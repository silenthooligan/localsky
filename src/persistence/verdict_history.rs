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
    Sqlite(#[source] Box<crate::failure::Failure>),
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
    /// The structured skip-ladder trace captured when the verdict
    /// changed, as stored. Empty on rows written before it existed, and
    /// on the queries that do not ask for it.
    pub trace_json: String,
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

/// Score one day's decision against the rain that actually fell, honestly.
/// A day is scored (`Some`) only when rain was a real factor: the morning
/// forecast called for >= SIG, or the station observed >= WET. A dry default day
/// or a non-rain skip (restriction / freeze / soil) returns `None`: it stays in
/// the scoreboard payload with its label, but never counts toward the tally, so
/// the headline can't be inflated by trivially-correct dry runs.
///
/// "Stays in the payload" is not the same as "is shown". The only renderer,
/// `components::historyview`, currently drops every `correct == None` day; see
/// the client note on [`VerdictHistoryStore::accuracy_window`].
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
        .map_err(|e| VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(&e, "verdict_history.insert"))))?
        .map_err(|e| VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(&e, "verdict_history.insert"))))
    }

    /// A verdict CHANGE, with the trace that explains it. The day is the
    /// deployment's calendar day, derived here rather than by SQLite's
    /// `strftime(..., 'unixepoch')`, which is a UTC day and was being
    /// written into a column named `date_local`: the accuracy scoreboard
    /// knew, and re-derived the day from the epoch by hand rather than
    /// trust it.
    pub async fn insert_transition(
        &self,
        epoch: i64,
        verdict: String,
        reason: String,
        trace_json: String,
    ) -> Result<(), VerdictHistoryError> {
        let c = self.conn.clone();
        let date_local = crate::timeutil::local_date(epoch)
            .map(|d| d.to_string())
            .unwrap_or_default();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO verdict_history
                    (epoch, date_local, verdict, reason, inputs_json, trace_json)
                 VALUES (?1, ?2, ?3, ?4, '{}', ?5)",
                params![epoch, date_local, verdict, reason, trace_json],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.insert_transition",
            )))
        })?
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.insert_transition",
            )))
        })
    }

    /// Drop verdict rows older than `cutoff_epoch`, on the same retention
    /// switch the runs table follows.
    pub async fn prune_older_than(&self, cutoff_epoch: i64) -> Result<usize, VerdictHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let conn = c.blocking_lock();
            conn.execute(
                "DELETE FROM verdict_history WHERE epoch < ?",
                params![cutoff_epoch],
            )
        })
        .await
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.prune_older_than",
            )))
        })?
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.prune_older_than",
            )))
        })
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
                "SELECT id, epoch, date_local, verdict, reason, inputs_json, trace_json
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
                        trace_json: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.window",
            )))
        })?
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.window",
            )))
        })
    }

    /// The forecast-accuracy scoreboard. One row per LOCAL calendar day,
    /// the morning verdict (the day's earliest transition) paired with that day's
    /// predicted-vs-observed rain from `forecast_observations`, plus the honest
    /// tally from `assess_day`. Ordered newest-first.
    ///
    /// A day whose ledger row carries the fabricated 'none' observation keeps
    /// its FORECAST and reports `observed_in: None`, so `assess_day` returns
    /// "no rain total recorded" and the day goes unscored rather than being
    /// judged against a 0.0 nobody read. The day stays in the payload with its
    /// verdict, its reason and the forecast it was made against: an ungraded
    /// rain call, not a call that was never made.
    ///
    /// What the CLIENT does with an unscored day is a separate question, and
    /// today the answer is "drops it". `components::historyview` filters the
    /// rendered list on `d.correct.is_some()` and short-circuits the whole
    /// section to its empty state when `scored == 0`, so a fully gaugeless
    /// window renders no list at all rather than a column of ungraded calls.
    /// This function's job is to describe the day honestly, which it now does;
    /// surfacing the unscored rows is a client change that has not landed.
    ///
    /// The day key is derived here via `timeutil::local_date` (the CONFIGURED
    /// timezone) from each verdict's epoch, NOT from the stored `date_local`
    /// column. `date_local` is written via SQLite `strftime(..., 'unixepoch')`
    /// (UTC), while `forecast_observations.date` is the refresher's
    /// configured-tz day. Joining on those two columns directly (the prior
    /// implementation) keyed a UTC day against a local day, so on any deploy
    /// west of UTC an evening verdict mis-joined to the next day's rain (or
    /// to NULL), and a UTC-day MIN(epoch) preferentially selected the *previous*
    /// local evening's transition rather than the morning one. Re-deriving the
    /// day in the same calendar `forecast_observations` now uses (the writer
    /// moved to the configured tz too) keeps writer and reader agreeing even
    /// in a UTC container.
    pub async fn accuracy_window(
        &self,
        from_epoch: i64,
        as_of_epoch: i64,
    ) -> Result<crate::model::AccuracyResult, VerdictHistoryError> {
        // Rain totals remain partial until the configured local date ends.
        // Capture the query's clock once so a midnight rollover cannot grade
        // different rows against different days.
        let as_of_date = crate::timeutil::local_date(as_of_epoch);
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<crate::model::AccuracyResult> {
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
            // stored (the configured timezone, the same frame the verdict day
            // is derived in below). All three value columns are NOT NULL, so
            // the reads below are total.
            //
            // EVERY row is read, 'none' rows included, and the fabrication is
            // filtered per-COLUMN instead. `recent`, `range` and
            // `days_since_observed_rain` drop the whole row because they build
            // a series of observations and a fabricated one has no place in
            // it. The scoreboard's unit is the DAY: dropping the row throws
            // away the day's real forecast along with its fake observation,
            // and a rain call the yard genuinely made stops appearing as a
            // call at all. Only the observed side of a 'none' row is
            // fabricated, so only the observed side is nulled.
            //
            // What 'none' means (M0015 plus the day-MAX upsert): no
            // rain-capable source's label survived to the day's last write. On
            // a gaugeless install that is every day, and scoring one pits a
            // real forecast against a measurement nobody took -- a 0.0 under a
            // 0.40" forecast reads as a rain call that missed, so the
            // scoreboard blames the forecast source for a day it had no gauge
            // for.
            //
            // It is WIDER than "nothing measured this day". The upsert moves
            // the label whenever `excluded.observed_in >= observed_in`
            // (forecast_observations.rs), so on a day whose max stays 0.0
            // every write ties and the LAST writer owns the label: a gauge
            // that read 0.00" all day and went stale before configured-tz
            // midnight ends the day stamped 'none' (observers.rs returns
            // 0.0/'none' for a stale owner). Such a day is genuinely measured
            // and is lost here -- but it is indistinguishable from a gaugeless
            // day at the row level, both being exactly 0.0/'none'. Listing it
            // unscored is the honest read of that ambiguity; guessing it was
            // measured would re-open the defect this exclusion exists to
            // close. The separation belongs in the WRITER (a 'none' write that
            // only TIES must not take the label off a 'gauge'/'radar' row),
            // which is a change to forecast_observations.rs with its own test.
            let mut fstmt = conn.prepare(
                "SELECT date, predicted_in, observed_in, observed_source
                     FROM forecast_observations",
            )?;
            let obs: HashMap<String, (Option<f64>, Option<f64>)> = fstmt
                .query_map([], |r| {
                    let date = r.get::<_, String>(0)?;
                    let predicted = r.get::<_, f64>(1)?;
                    let observed = r.get::<_, f64>(2)?;
                    let source = r.get::<_, String>(3)?;
                    Ok((
                        date,
                        (
                            // -1.0 is `upsert_et0`'s placeholder for "no rain
                            // writer has supplied this day's forecast yet",
                            // not a forecast of minus an inch. It used to be
                            // unreachable here because those rows are stamped
                            // 'none' and the row was dropped whole; now that
                            // the row is read, the sentinel needs its own gate.
                            (predicted >= 0.0).then_some(predicted),
                            (source != "none").then_some(observed),
                        ),
                    ))
                })?
                .collect::<rusqlite::Result<HashMap<_, _>>>()?;

            // Group by LOCAL day, keep the earliest (morning) verdict per day.
            let mut by_day: BTreeMap<String, (i64, String, String)> = BTreeMap::new();
            for (epoch, verdict, reason) in vrows {
                let date = match crate::timeutil::local_date(epoch) {
                    Some(d) => d.format("%Y-%m-%d").to_string(),
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
            for (date, (epoch, verdict, reason)) in by_day.into_iter().rev() {
                // No ledger row at all reads like a fabricated observation:
                // nothing to grade the day against. Both halves are already
                // Option, so the row's own per-column verdict passes through.
                let (predicted_in, observed_in) = match obs.get(&date) {
                    Some((p, o)) => (*p, *o),
                    None => (None, None),
                };
                let complete = crate::timeutil::local_date(epoch)
                    .zip(as_of_date)
                    .is_some_and(|(day, today)| day < today);
                let (assessment, correct) = if complete {
                    assess_day(&verdict, predicted_in, observed_in)
                } else {
                    ("Waiting for the local day to finish".into(), None)
                };
                if let Some(ok) = correct {
                    scored += 1;
                    if ok {
                        matched += 1;
                    }
                }
                // P1: derive the rule id from the persisted reason (additive;
                // no DB migration). Computed before the moves below.
                let reason_code = classify_reason_code(&verdict, &reason);
                days.push(crate::model::ScoreboardDay {
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
            Ok(crate::model::AccuracyResult {
                days,
                scored,
                matched,
            })
        })
        .await
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.accuracy_window",
            )))
        })?
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.accuracy_window",
            )))
        })
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
                        trace_json: String::new(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.for_date",
            )))
        })?
        .map_err(|e| {
            VerdictHistoryError::Sqlite(Box::new(crate::diagnostics::from_error(
                &e,
                "verdict_history.for_date",
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;

    /// The trace the Rule Lab replays survives the round trip: written by
    /// the transition insert, read back by the window, parsed into the
    /// wire shape.
    #[tokio::test]
    async fn a_verdict_transition_carries_its_trace_back_out() {
        use crate::model::{DecisionTrace, RuleEval};
        let store = fresh_store().await;
        let trace = DecisionTrace {
            verdict: "skip".into(),
            reason: "Already wet (0.10\" today)".into(),
            degraded: false,
            reason_code: "already_wet".into(),
            rules: vec![RuleEval {
                id: "already_wet".into(),
                label: "Already wet today".into(),
                category: "weather".into(),
                detail: "0.10\" today vs 0.05\" floor".into(),
                outcome: "fired".into(),
                verdict: Some("skip".into()),
                margin_label: None,
                ..Default::default()
            }],
        };
        store
            .insert_transition(
                1_700_000_000,
                "skip".into(),
                "Already wet (0.10\" today)".into(),
                serde_json::to_string(&trace).unwrap(),
            )
            .await
            .unwrap();
        let rows = store.window(1_600_000_000, 1_800_000_000).await.unwrap();
        assert_eq!(rows.len(), 1);
        let d = crate::history::types::DecisionRecord::from(rows[0].clone());
        assert_eq!(d.verdict, "skip");
        assert_eq!(d.trace.expect("trace present"), trace);
    }

    /// A row written before traces existed carries none, rather than
    /// failing the window for every row beside it.
    #[tokio::test]
    async fn a_row_with_no_trace_reads_as_none() {
        let store = fresh_store().await;
        store
            .insert_transition(1_700_000_500, "run".into(), String::new(), String::new())
            .await
            .unwrap();
        let rows = store.window(1_600_000_000, 1_800_000_000).await.unwrap();
        let d = crate::history::types::DecisionRecord::from(rows[0].clone());
        assert!(d.trace.is_none());
    }

    /// Prune must target `verdict_history`, the name the v2 migration gave
    /// the table. The pre-fix code said `DELETE FROM decisions`, which
    /// errors on a migrated database and let the history grow forever.
    #[tokio::test]
    async fn prune_trims_verdict_history_after_the_migration() {
        let store = fresh_store().await;
        store
            .insert_transition(1_000_000_000, "skip".into(), "old".into(), String::new())
            .await
            .unwrap();
        store
            .insert_transition(1_900_000_000, "run".into(), "recent".into(), String::new())
            .await
            .unwrap();
        let removed = store.prune_older_than(1_500_000_000).await.unwrap();
        assert_eq!(removed, 1, "exactly the one stale row pruned");
        let rows = store.window(0, 2_000_000_000).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, "run");
    }

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

    /// engine::tuning's scorecard mirrors assess_day's WET/SIG so the
    /// tuning report and this scoreboard can never disagree on what
    /// counts as rain. assess_day's consts are function-local; this pins
    /// both sides to the same literals.
    #[test]
    fn assess_day_thresholds_match_engine_tuning() {
        assert_eq!(crate::engine::tuning::WET_IN, 0.05);
        assert_eq!(crate::engine::tuning::SIG_IN, 0.10);
        // Behavior pin: a skip with observed exactly at WET is confirmed.
        assert_eq!(assess_day("skip", Some(0.0), Some(0.05)).1, Some(true));
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
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        let conn = Arc::new(Mutex::new(c));
        let store = VerdictHistoryStore::new(conn.clone());
        // Derive each day's LOCAL key exactly as accuracy_window does, so the test
        // is deterministic regardless of the runner's TZ. Two midday epochs ~24h
        // apart land on different local days.
        let day = |e: i64| {
            crate::timeutil::local_date(e)
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
        let res = store.accuracy_window(0, 2_000_000_000).await.unwrap();
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
    async fn accuracy_waits_for_actual_local_midnight_without_losing_evidence() {
        // The full gate runs these dates in IANA zones with 23/25-hour days,
        // southern-hemisphere DST and fractional offsets. Never add 86,400
        // seconds to decide whether a local day is complete.
        for (month, day) in [(3, 8), (4, 5), (9, 27), (11, 1)] {
            let date = chrono::NaiveDate::from_ymd_opt(2026, month, day).unwrap();
            let store = fresh_store().await;
            for (row_date, rain) in [
                (date.pred_opt().unwrap(), 0.3),
                (date, 0.0),
                (date.succ_opt().unwrap(), 0.0),
            ] {
                let (start, _) = crate::timeutil::local_day_bounds_utc(row_date).unwrap();
                store
                    .insert(NewVerdict {
                        epoch: start.timestamp() + 1,
                        date_local: row_date.to_string(),
                        verdict: "skip".into(),
                        reason: "Rain expected".into(),
                        inputs_json: "{}".into(),
                    })
                    .await
                    .unwrap();
                store.conn.lock().await.execute(
                    "INSERT INTO forecast_observations(date, predicted_in, observed_in, month, inserted_at_epoch, observed_source)
                     VALUES (?1, 0.4, ?2, ?3, ?4, 'gauge')",
                    params![row_date.to_string(), rain, month, start.timestamp()],
                ).unwrap();
            }
            let (_, end) = crate::timeutil::local_day_bounds_utc(date).unwrap();
            let pending = store.accuracy_window(0, end.timestamp() - 1).await.unwrap();
            assert_eq!((pending.scored, pending.matched), (1, 1));
            assert_eq!(pending.days.len(), 3);
            for row in pending.days.iter().take(2) {
                assert_eq!(row.correct, None);
                assert_eq!(row.assessment, "Waiting for the local day to finish");
                assert_eq!(row.predicted_in, Some(0.4));
                assert_eq!(row.observed_in, Some(0.0));
            }
            let complete = store.accuracy_window(0, end.timestamp()).await.unwrap();
            assert_eq!((complete.scored, complete.matched), (2, 1));
            assert_eq!(complete.days[1].date, date.to_string());
            assert_eq!(complete.days[1].correct, Some(false));
            assert_eq!(complete.days[0].correct, None);
        }
    }

    /// A gaugeless install (the out-of-the-box configuration: a forecast
    /// source, no station and no radar) writes a FABRICATED 0.0 observation
    /// stamped 'none' every day. That 0.0 must never reach `assess_day` as a
    /// measurement: scored, it turns every rain skip into a red X and tells
    /// the owner his forecast source is wrong when nothing ever measured the
    /// rain. The day and the forecast it was judged against DO survive, so the
    /// window still shows how many rain calls were made -- just not how they
    /// graded. A real 'gauge' day beside it still scores, so the exclusion
    /// stays narrow.
    #[tokio::test]
    async fn accuracy_window_leaves_fabricated_none_days_unscored() {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        let conn = Arc::new(Mutex::new(c));
        let store = VerdictHistoryStore::new(conn.clone());
        // Same local-day derivation accuracy_window uses, so the test is
        // deterministic regardless of the runner's TZ.
        let day = |e: i64| {
            crate::timeutil::local_date(e)
                .unwrap()
                .format("%Y-%m-%d")
                .to_string()
        };
        let epoch_a = 1_750_000_000; // the gaugeless day
        let epoch_b = epoch_a + 86_400; // ~next local day, measured
        let date_a = day(epoch_a);
        let date_b = day(epoch_b);
        assert_ne!(date_a, date_b, "epochs must land on different local days");
        for (epoch, date) in [(epoch_a, &date_a), (epoch_b, &date_b)] {
            store
                .insert(NewVerdict {
                    epoch,
                    date_local: date.clone(),
                    verdict: "skip".into(),
                    reason: "Rain expected within 4h (0.40\" forecast)".into(),
                    inputs_json: "{}".into(),
                })
                .await
                .unwrap();
        }
        {
            let lock = conn.lock().await;
            // Day A: the placeholder the refresher writes when nothing can
            // measure rain. Day B: real numbers from a gauge.
            lock.execute(
                "INSERT INTO forecast_observations(date, predicted_in, observed_in, month, inserted_at_epoch, observed_source)
                 VALUES (?1, 0.40, 0.0, 6, 0, 'none'), (?2, 0.40, 0.30, 6, 0, 'gauge')",
                params![date_a, date_b],
            )
            .unwrap();
        }
        let res = store.accuracy_window(0, 2_000_000_000).await.unwrap();
        assert_eq!(
            res.days.len(),
            2,
            "both verdict days are still in the payload (what the CLIENT \
             renders is a separate question; see accuracy_window's doc)"
        );
        // Newest first: day B (gauge), then day A (fabricated).
        assert_eq!(res.days[0].date, date_b);
        assert_eq!(res.days[1].date, date_a);
        // Day A keeps the forecast it was judged against -- the prediction is
        // real, only the observation was fabricated -- and reports no rain
        // total, so it reads as an ungraded call.
        //
        // Two behaviours are pinned here. Against the ORIGINAL code the day
        // read predicted 0.40 / observed 0.00 and scored "skipped for a 0.40\"
        // forecast that missed": a confident verdict built on the sentinel
        // M0015 writes to MARK fabrication. Against the first-pass fix, which
        // dropped the whole row, `predicted_in` came back None and the day
        // stopped looking like a rain call had been made at all.
        assert_eq!(res.days[1].predicted_in, Some(0.40));
        assert_eq!(res.days[1].observed_in, None);
        assert_eq!(res.days[1].assessment, "no rain total recorded");
        assert_eq!(res.days[1].correct, None);
        // Day B is untouched by the exclusion: measured rain still scores.
        assert_eq!(res.days[0].observed_in, Some(0.30));
        assert_eq!(res.days[0].correct, Some(true));
        assert_eq!(res.days[0].predicted_in, Some(0.40));
        // The tally counts only the measured day. Before the fix: 2 of 2
        // scored, 1 matched.
        assert_eq!(res.scored, 1);
        assert_eq!(res.matched, 1);
    }

    /// A day a gauge really did read as dry still scores, on BOTH sides of the
    /// call. The fabricated-day exclusion keys on the row's LABEL, never on
    /// its VALUE, and this is the pin for that: the day's observed total is
    /// 0.00" -- identical to the placeholder's -- and it is graded anyway.
    /// Widening the exclusion to `observed_in <= 0.0` (or to any predicate
    /// that reads the number instead of the provenance) would silently delete
    /// every forecast that missed.
    ///
    /// It also pins which way the exclusion's residual cost runs. A dry day is
    /// a MISS when the yard skipped for the forecast and a WIN when it watered
    /// through it, so losing dry days to a late source lapse does not bias
    /// `matched / scored` in one direction.
    #[tokio::test]
    async fn a_gauge_measured_dry_day_is_still_scored_on_both_sides() {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        let conn = Arc::new(Mutex::new(c));
        let store = VerdictHistoryStore::new(conn.clone());
        let day = |e: i64| {
            crate::timeutil::local_date(e)
                .unwrap()
                .format("%Y-%m-%d")
                .to_string()
        };
        let epoch_a = 1_750_000_000; // skipped for the forecast: a miss
        let epoch_b = epoch_a + 86_400; // watered through it: a win
        let date_a = day(epoch_a);
        let date_b = day(epoch_b);
        assert_ne!(date_a, date_b, "epochs must land on different local days");
        store
            .insert(NewVerdict {
                epoch: epoch_a,
                date_local: date_a.clone(),
                verdict: "skip".into(),
                reason: "Rain expected within 4h (0.40\" forecast)".into(),
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
            // Both days: a 0.40" forecast and a station that stayed live and
            // measured 0.00". Nothing fabricated on either row.
            lock.execute(
                "INSERT INTO forecast_observations(date, predicted_in, observed_in, month, inserted_at_epoch, observed_source)
                 VALUES (?1, 0.40, 0.0, 6, 0, 'gauge'), (?2, 0.40, 0.0, 6, 0, 'gauge')",
                params![date_a, date_b],
            )
            .unwrap();
        }
        let res = store.accuracy_window(0, 2_000_000_000).await.unwrap();
        assert_eq!(res.days.len(), 2);
        // Newest first.
        assert_eq!(res.days[0].date, date_b);
        assert_eq!(res.days[1].date, date_a);
        // The measured 0.00" is reported as a measurement, not as absence.
        assert_eq!(res.days[1].observed_in, Some(0.0));
        assert_eq!(res.days[0].observed_in, Some(0.0));
        // Skipped for a forecast that missed: a miss.
        assert_eq!(res.days[1].correct, Some(false));
        assert_eq!(
            res.days[1].assessment,
            "skipped for a 0.40\" forecast that missed"
        );
        // Watered through the same forecast: a win. Dry days score on both
        // sides, so dropping them is not a one-way tally bias.
        assert_eq!(res.days[0].correct, Some(true));
        assert_eq!(res.scored, 2);
        assert_eq!(res.matched, 1);
    }

    /// `upsert_et0` plants a day's row with predicted -1.0 when no rain writer
    /// has supplied that morning's forecast yet, alongside the 0.0/'none' rain
    /// placeholder (forecast_observations.rs). Reading 'none' rows instead of
    /// dropping them puts that sentinel on the scoreboard's path for the first
    /// time, so it needs its own gate: a day with no forecast recorded reports
    /// no forecast, never a call for minus an inch of rain.
    #[tokio::test]
    async fn the_et0_placeholder_prediction_is_not_reported_as_a_forecast() {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        let conn = Arc::new(Mutex::new(c));
        let store = VerdictHistoryStore::new(conn.clone());
        let epoch = 1_750_000_000;
        let date = crate::timeutil::local_date(epoch)
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();
        store
            .insert(NewVerdict {
                epoch,
                date_local: date.clone(),
                verdict: "run".into(),
                reason: String::new(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap();
        {
            let lock = conn.lock().await;
            // Exactly what `upsert_et0` plants on a day no rain writer has
            // reached yet: the -1.0 prediction sentinel and the 'none' rain
            // placeholder.
            lock.execute(
                "INSERT INTO forecast_observations(date, predicted_in, observed_in, month, inserted_at_epoch, observed_source, et0_mm, et0_source)
                 VALUES (?1, -1.0, 0.0, 6, 0, 'none', 4.2, 'localsky_engine')",
                params![date],
            )
            .unwrap();
        }
        let res = store.accuracy_window(0, 2_000_000_000).await.unwrap();
        assert_eq!(res.days.len(), 1);
        assert_eq!(res.days[0].predicted_in, None, "sentinel is not a forecast");
        assert_eq!(res.days[0].observed_in, None);
        assert_eq!(res.days[0].assessment, "no rain total recorded");
        assert_eq!(res.days[0].correct, None);
        assert_eq!(res.scored, 0);
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
