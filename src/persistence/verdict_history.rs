// Verdict history store. Replaces the legacy `decisions` table with
// `verdict_history` (M0005): adds date_local + inputs_json columns so
// any historical decision can be replayed through the current engine.

use std::sync::Arc;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum VerdictHistoryError {
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("inputs serialize: {0}")]
    Serialize(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictRow {
    pub id: i64,
    pub epoch: i64,
    pub date_local: String,
    pub verdict: String,
    pub reason: String,
    pub inputs_json: String,
}

#[derive(Debug, Clone)]
pub struct NewVerdict {
    pub epoch: i64,
    pub date_local: String,
    pub verdict: String,
    pub reason: String,
    /// Optional raw Inputs blob for replay. Serialize the
    /// engine::skip_rules::Inputs struct to JSON. Pass an empty object
    /// string ("{}") if not capturing.
    pub inputs_json: String,
}

/// P3-4: score one day's decision against the rain that actually fell, honestly.
/// A day is scored (`Some`) only when rain was a real factor: the morning
/// forecast called for >= SIG, or the station observed >= WET. A dry default day
/// or a non-rain skip (restriction / freeze / soil) returns `None` and is shown
/// for context but never counted, so the headline tally can't be inflated by
/// trivially-correct dry runs.
pub fn assess_day(
    verdict: &str,
    predicted_in: Option<f64>,
    observed_in: Option<f64>,
) -> (String, Option<bool>) {
    const WET: f64 = 0.05; // a meaningful rain total
    const SIG: f64 = 0.10; // enough rain that watering through it was wasteful
    let obs = match observed_in {
        Some(o) => o,
        None => return ("no rain total recorded".to_string(), None),
    };
    let pred = predicted_in.unwrap_or(0.0);
    let skip = verdict.starts_with("skip");
    if obs < WET && pred < SIG {
        // The common dry day: a correct default, but not a notable rain call.
        return (
            if skip {
                "skipped (dry, not a rain call)".to_string()
            } else {
                "watered (dry)".to_string()
            },
            None,
        );
    }
    if skip {
        if obs >= WET {
            (format!("skipped, {obs:.2}\" rain arrived"), Some(true))
        } else {
            (
                format!("skipped for a {pred:.2}\" forecast that missed"),
                Some(false),
            )
        }
    } else if obs >= WET {
        // Run while a meaningful total fell: over-watered. Use WET here too, so the
        // same rain amount is judged consistently on both the skip and run paths
        // (the old SIG threshold scored a 0.05-0.10" run as a clean win).
        (format!("watered, then {obs:.2}\" rain fell"), Some(false))
    } else {
        // Stayed dry. This branch is only reached when the day is rain-relevant
        // via the forecast (pred >= SIG), so the "forecast that missed" label is
        // always coherent (pred is never 0 here).
        (
            format!("watered through a {pred:.2}\" forecast that missed"),
            Some(true),
        )
    }
}

/// P1 (units architecture): classify a PERSISTED baked verdict reason back to its
/// stable rule id, for the forecast-accuracy scoreboard. The scoreboard rebuilds
/// days from `verdict_history` rows, which store only verdict + reason text (no
/// reason_code column, and the plan does no history migration), so the code is
/// DERIVED from the reason string rather than re-emitted by the engine. Matches on
/// the distinctive prefix each `engine::skip_rules` baked reason uses; an empty
/// reason on a "run" verdict is a clean run ("run"); anything unrecognized
/// (custom-condition reasons, older wording) classifies as "" so the client falls
/// back to the baked string. Kept here (not the engine) because it's a
/// history-reconstruction concern, not part of the live decision.
pub fn classify_reason_code(verdict: &str, reason: &str) -> String {
    let r = reason.trim();
    if r.is_empty() {
        // Empty reason: a clean run (or run-extended with no reason). Skips always
        // carry a reason, so an empty-reason skip is unexpected -> leave blank.
        return if verdict.starts_with("run") {
            "run".to_string()
        } else {
            String::new()
        };
    }
    // Order: the more specific "Already wet (... in the last N day(s))" before the
    // plain "Already wet (... today)" so observed_rain isn't shadowed.
    let code = if r.starts_with("Manual override") {
        "override"
    } else if r.starts_with("Paused (vacation until") {
        "pause_until"
    } else if r.starts_with("Paused") {
        "paused"
    } else if r.starts_with("Live weather unavailable") {
        "live_data"
    } else if r.starts_with("Currently raining") {
        "rain_now"
    } else if r.starts_with("Freeze risk now") {
        "freeze_now"
    } else if r.starts_with("Overnight freeze") {
        "overnight_freeze"
    } else if r.starts_with("Soil frost") {
        "soil_frost"
    } else if r.starts_with("Wind too high now") {
        "wind_now"
    } else if r.starts_with("Windy day forecast") {
        "wind_forecast"
    } else if r.contains("rain in the last") {
        // "Already wet ({:.2}\" rain in the last {} day(s))"
        "observed_rain"
    } else if r.starts_with("Already wet") {
        "already_wet"
    } else if r.starts_with("All zones soil-saturated") {
        "soil_saturation"
    } else if r.starts_with("Rain expected within 4h") {
        "rain_next_4h"
    } else if r.starts_with("Tomorrow rain") {
        "tomorrow_rain"
    } else if r.starts_with("Heavy rain in next 3 days") {
        "rain_3day"
    } else if r.starts_with("Heat advisory") {
        "heat_advisory"
    } else if r.starts_with("Dry-run mode") {
        "dry_run"
    } else {
        // Watering-restriction reasons are operator-authored free text, and custom
        // condition reasons are user-defined; both (plus any older wording) fall
        // through here. "" tells the client to render the baked string verbatim.
        ""
    };
    code.to_string()
}

#[derive(Clone)]
pub struct VerdictHistoryStore {
    conn: Arc<Mutex<Connection>>,
}

impl VerdictHistoryStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn insert(&self, v: NewVerdict) -> Result<(), VerdictHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO verdict_history(epoch, date_local, verdict, reason, inputs_json)
                 VALUES (?, ?, ?, ?, ?)",
                params![v.epoch, v.date_local, v.verdict, v.reason, v.inputs_json],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| VerdictHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| VerdictHistoryError::Sqlite(e.to_string()))
    }

    pub async fn window(
        &self,
        from_epoch: i64,
        to_epoch: i64,
    ) -> Result<Vec<VerdictRow>, VerdictHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<VerdictRow>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, epoch, date_local, verdict, reason, inputs_json
                 FROM verdict_history
                 WHERE epoch >= ? AND epoch < ?
                 ORDER BY epoch ASC",
            )?;
            let rows = stmt
                .query_map(params![from_epoch, to_epoch], |r| {
                    Ok(VerdictRow {
                        id: r.get(0)?,
                        epoch: r.get(1)?,
                        date_local: r.get(2)?,
                        verdict: r.get(3)?,
                        reason: r.get(4)?,
                        inputs_json: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| VerdictHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| VerdictHistoryError::Sqlite(e.to_string()))
    }

    /// P3-4: the forecast-accuracy scoreboard. One row per LOCAL calendar day,
    /// the morning verdict (the day's earliest transition) paired with that day's
    /// predicted-vs-observed rain from `forecast_observations`, plus the honest
    /// tally from `assess_day`. Ordered newest-first.
    ///
    /// The day key is derived here in `chrono::Local` from each verdict's epoch,
    /// NOT from the stored `date_local` column. `date_local` is written via SQLite
    /// `strftime(..., 'unixepoch')` (UTC), while `forecast_observations.date` is
    /// the refresher's `chrono::Local` day. Joining on those two columns directly
    /// (the prior implementation) keyed a UTC day against a local day, so on any
    /// deploy west of UTC an evening verdict mis-joined to the next day's rain (or
    /// to NULL), and a UTC-day MIN(epoch) preferentially selected the *previous*
    /// local evening's transition rather than the morning one. Re-deriving the
    /// local day in the same calendar `forecast_observations` uses fixes both.
    pub async fn accuracy_window(
        &self,
        from_epoch: i64,
    ) -> Result<crate::ha::snapshot::AccuracyResult, VerdictHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(
            move || -> rusqlite::Result<crate::ha::snapshot::AccuracyResult> {
                use chrono::TimeZone;
                use std::collections::{BTreeMap, HashMap};
                let conn = c.blocking_lock();

                // Raw verdict transitions in the window, oldest first.
                let mut vstmt = conn.prepare(
                    "SELECT epoch, verdict, reason FROM verdict_history
                     WHERE epoch >= ?1 ORDER BY epoch ASC",
                )?;
                let vrows = vstmt
                    .query_map(params![from_epoch], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                // Predicted-vs-observed rain, keyed by the LOCAL day the refresher
                // stored (chrono::Local). REAL NOT NULL columns => both present.
                let mut fstmt = conn
                    .prepare("SELECT date, predicted_in, observed_in FROM forecast_observations")?;
                let obs: HashMap<String, (f64, f64)> = fstmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            (r.get::<_, f64>(1)?, r.get::<_, f64>(2)?),
                        ))
                    })?
                    .collect::<rusqlite::Result<HashMap<_, _>>>()?;

                // Group by LOCAL day, keep the earliest (morning) verdict per day.
                let mut by_day: BTreeMap<String, (i64, String, String)> = BTreeMap::new();
                for (epoch, verdict, reason) in vrows {
                    let date = match chrono::Local.timestamp_opt(epoch, 0).single() {
                        Some(dt) => dt.format("%Y-%m-%d").to_string(),
                        None => continue,
                    };
                    by_day
                        .entry(date)
                        .and_modify(|cur| {
                            if epoch < cur.0 {
                                *cur = (epoch, verdict.clone(), reason.clone());
                            }
                        })
                        .or_insert((epoch, verdict, reason));
                }

                // BTreeMap iterates ascending; reverse for newest-first.
                let mut days = Vec::with_capacity(by_day.len());
                let (mut scored, mut matched) = (0u32, 0u32);
                for (date, (_epoch, verdict, reason)) in by_day.into_iter().rev() {
                    let (predicted_in, observed_in) = match obs.get(&date) {
                        Some((p, o)) => (Some(*p), Some(*o)),
                        None => (None, None),
                    };
                    let (assessment, correct) = assess_day(&verdict, predicted_in, observed_in);
                    if let Some(ok) = correct {
                        scored += 1;
                        if ok {
                            matched += 1;
                        }
                    }
                    // P1: derive the rule id from the persisted reason (additive;
                    // no DB migration). Computed before the moves below.
                    let reason_code = classify_reason_code(&verdict, &reason);
                    days.push(crate::ha::snapshot::ScoreboardDay {
                        date,
                        verdict,
                        reason,
                        reason_code,
                        predicted_in,
                        observed_in,
                        assessment,
                        correct,
                    });
                }
                Ok(crate::ha::snapshot::AccuracyResult {
                    days,
                    scored,
                    matched,
                })
            },
        )
        .await
        .map_err(|e| VerdictHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| VerdictHistoryError::Sqlite(e.to_string()))
    }

    /// All verdicts for a specific local date. Useful for the daily
    /// dashboard verdict tile.
    pub async fn for_date(
        &self,
        date_local: String,
    ) -> Result<Vec<VerdictRow>, VerdictHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<VerdictRow>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, epoch, date_local, verdict, reason, inputs_json
                 FROM verdict_history WHERE date_local = ?
                 ORDER BY epoch ASC",
            )?;
            let rows = stmt
                .query_map(params![date_local], |r| {
                    Ok(VerdictRow {
                        id: r.get(0)?,
                        epoch: r.get(1)?,
                        date_local: r.get(2)?,
                        verdict: r.get(3)?,
                        reason: r.get(4)?,
                        inputs_json: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| VerdictHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| VerdictHistoryError::Sqlite(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;

    async fn fresh_store() -> VerdictHistoryStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        VerdictHistoryStore::new(Arc::new(Mutex::new(c)))
    }

    #[test]
    fn classify_reason_code_recovers_rule_ids() {
        // Clean run + run_extended with no reason -> "run".
        assert_eq!(classify_reason_code("run", ""), "run");
        assert_eq!(classify_reason_code("run_extended", ""), "run");
        // The distinctive prefix of each baked engine reason maps to its id. These
        // strings are the EXACT format!() output of engine::skip_rules.
        assert_eq!(
            classify_reason_code("skip", "Currently raining (0.05 in/hr)"),
            "rain_now"
        );
        assert_eq!(
            classify_reason_code("skip", "Wind too high now (20.0 mph > 10 mph)"),
            "wind_now"
        );
        assert_eq!(
            classify_reason_code("skip", "Freeze risk now (30°F < 38°F)"),
            "freeze_now"
        );
        // observed_rain ("...rain in the last N day(s)") must NOT be shadowed by
        // the plain "Already wet (... today)" branch.
        assert_eq!(
            classify_reason_code("skip", "Already wet (1.50\" rain in the last 2 day(s))"),
            "observed_rain"
        );
        assert_eq!(
            classify_reason_code("skip", "Already wet (0.10\" today)"),
            "already_wet"
        );
        assert_eq!(
            classify_reason_code(
                "skip",
                "All zones soil-saturated (tightest: back yard shrubs 90% ≥ 85% threshold)"
            ),
            "soil_saturation"
        );
        assert_eq!(
            classify_reason_code("skip", "Rain expected within 4h (0.20\" forecast)"),
            "rain_next_4h"
        );
        assert_eq!(
            classify_reason_code("skip", "Tomorrow rain (0.40\" × 90% confidence)"),
            "tomorrow_rain"
        );
        assert_eq!(
            classify_reason_code("skip", "Heavy rain in next 3 days (1.00\" weighted)"),
            "rain_3day"
        );
        assert_eq!(
            classify_reason_code(
                "run_extended",
                "Heat advisory: running planned + 15% (peak 98°F)"
            ),
            "heat_advisory"
        );
        assert_eq!(classify_reason_code("skip", "Dry-run mode"), "dry_run");
        assert_eq!(
            classify_reason_code("skip", "Paused (vacation mode)"),
            "paused"
        );
        assert_eq!(
            classify_reason_code("skip", "Paused (vacation until Mon Jan 1, 9 AM)"),
            "pause_until"
        );
        assert_eq!(
            classify_reason_code("skip", "Manual override: skip"),
            "override"
        );
        // Operator restriction free-text / unrecognized -> "" (client renders the
        // baked string verbatim).
        assert_eq!(
            classify_reason_code("skip", "No watering on odd days (Smithtown ord. 12)"),
            ""
        );
    }

    #[test]
    fn assess_day_scores_only_rain_relevant_days() {
        // Skipped and the rain came: a win.
        let (label, ok) = assess_day("skip", Some(0.4), Some(0.30));
        assert_eq!(ok, Some(true));
        assert!(label.contains("rain arrived"), "got: {label}");
        // Skipped for a forecast that missed: an honest miss.
        assert_eq!(assess_day("skip", Some(0.3), Some(0.0)).1, Some(false));
        // Watered, then it rained: over-watered.
        assert_eq!(assess_day("run", Some(0.0), Some(0.20)).1, Some(false));
        // A run with a 0.05-0.10" total: meaningful rain fell while watering, so
        // it's over-watering, judged the same as the skip side uses WET -- NOT a
        // clean win as the old SIG-based run branch scored it.
        let (band_label, band_ok) = assess_day("run", Some(0.0), Some(0.07));
        assert_eq!(band_ok, Some(false));
        assert!(band_label.contains("rain fell"), "got: {band_label}");
        // run_extended counts as a run.
        assert_eq!(assess_day("run_extended", Some(0.0), Some(0.0)).1, None);
        // Dry default days are shown but NEVER scored (can't inflate the tally).
        assert_eq!(assess_day("run", Some(0.0), Some(0.0)).1, None);
        assert_eq!(assess_day("skip", Some(0.0), Some(0.0)).1, None);
        // No rain total recorded: unscored.
        assert_eq!(assess_day("skip", Some(0.5), None).1, None);
    }

    #[tokio::test]
    async fn accuracy_window_joins_and_tallies_honestly() {
        use chrono::TimeZone;
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        let conn = Arc::new(Mutex::new(c));
        let store = VerdictHistoryStore::new(conn.clone());
        // Derive each day's LOCAL key exactly as accuracy_window does, so the test
        // is deterministic regardless of the runner's TZ. Two midday epochs ~24h
        // apart land on different local days.
        let day = |e: i64| {
            chrono::Local
                .timestamp_opt(e, 0)
                .single()
                .unwrap()
                .format("%Y-%m-%d")
                .to_string()
        };
        let epoch_a = 1_750_000_000; // older
        let epoch_b = epoch_a + 86_400; // ~next local day
        let date_a = day(epoch_a);
        let date_b = day(epoch_b);
        assert_ne!(date_a, date_b, "epochs must land on different local days");
        // Day A: skipped, rain came. Day B: watered, dry. Also write an evening
        // transition on day A AFTER the morning one to prove MIN(epoch)-per-LOCAL
        // -day keeps the morning verdict (the old UTC grouping picked the evening).
        store
            .insert(NewVerdict {
                epoch: epoch_a,
                date_local: date_a.clone(),
                verdict: "skip".into(),
                reason: "Rain expected".into(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap();
        store
            .insert(NewVerdict {
                epoch: epoch_a + 3600,
                date_local: date_a.clone(),
                verdict: "run".into(),
                reason: "later same-day flip".into(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap();
        store
            .insert(NewVerdict {
                epoch: epoch_b,
                date_local: date_b.clone(),
                verdict: "run".into(),
                reason: String::new(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap();
        {
            let lock = conn.lock().await;
            lock.execute(
                "INSERT INTO forecast_observations(date, predicted_in, observed_in, month, inserted_at_epoch)
                 VALUES (?1, 0.40, 0.30, 6, 0), (?2, 0.0, 0.0, 6, 0)",
                params![date_a, date_b],
            )
            .unwrap();
        }
        let res = store.accuracy_window(0).await.unwrap();
        assert_eq!(res.days.len(), 2);
        // Newest first.
        assert_eq!(res.days[0].date, date_b);
        assert_eq!(res.days[1].date, date_a);
        // Day A keeps the MORNING verdict (skip), not the later same-day flip.
        assert_eq!(res.days[1].verdict, "skip");
        // Day A scored + matched (skip, rain arrived); Day B unscored (dry run).
        assert_eq!(res.days[1].correct, Some(true));
        assert_eq!(res.days[1].observed_in, Some(0.30));
        assert_eq!(res.days[0].correct, None);
        assert_eq!(res.scored, 1);
        assert_eq!(res.matched, 1);
    }

    #[tokio::test]
    async fn legacy_decisions_carry_into_verdict_history() {
        // Simulate a legacy v0.1 DB with rows in `decisions`, then run
        // migrations. M0005 should copy rows forward.
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE decisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                epoch INTEGER NOT NULL,
                verdict TEXT NOT NULL,
                reason TEXT NOT NULL,
                UNIQUE(epoch)
            );
            INSERT INTO decisions(epoch, verdict, reason)
                VALUES (1700000000, 'skip', 'Already wet'), (1700086400, 'run', '');",
        )
        .unwrap();
        runner::run(&mut c).unwrap();

        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM verdict_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        // legacy decisions table dropped
        let still_there: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='decisions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_there, 0);
    }

    #[tokio::test]
    async fn insert_then_for_date_roundtrip() {
        let s = fresh_store().await;
        s.insert(NewVerdict {
            epoch: 1700000000,
            date_local: "2023-11-14".into(),
            verdict: "skip".into(),
            reason: "Rain expected".into(),
            inputs_json: "{}".into(),
        })
        .await
        .unwrap();
        let rows = s.for_date("2023-11-14".into()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, "skip");
    }

    #[tokio::test]
    async fn window_queries_inclusive_from_exclusive_to() {
        let s = fresh_store().await;
        for i in 0..5 {
            s.insert(NewVerdict {
                epoch: 1000 + i,
                date_local: "x".into(),
                verdict: "run".into(),
                reason: "".into(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap();
        }
        let win = s.window(1001, 1004).await.unwrap();
        assert_eq!(win.len(), 3);
        assert_eq!(win.first().unwrap().epoch, 1001);
        assert_eq!(win.last().unwrap().epoch, 1003);
    }
}
