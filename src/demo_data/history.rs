//! Sample history belongs to calendar dates, not to the first boot of a volume.
//! Only the demo boot path starts this task. Existing history is never deleted.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Days};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::engine::calendar::Calendar;
use crate::history::types::{DailyDecision, DailyZoneDecision};

const ZONES: [(&str, &str, u32); 4] = [
    ("back_yard", "Back yard", 3600),
    ("front_yard", "Front yard", 1800),
    ("side_yard", "Side yard", 1800),
    ("back_yard_shrubs", "Back yard shrubs", 1320),
];

pub(super) async fn maintain(conn: Arc<Mutex<Connection>>) {
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let now = chrono::Utc::now().timestamp();
        let connection = conn.clone();
        let result = tokio::task::spawn_blocking(move || {
            seed(
                &mut connection.blocking_lock(),
                now,
                crate::timeutil::deployment_calendar(),
            )
        })
        .await;
        match result {
            Ok(Ok(0)) => {}
            Ok(Ok(days)) => {
                tracing::info!(days, "demo history added missing sample days");
                match recent_watering(&conn, now).await {
                    Ok(watered) => super::seed_tuning_signals(conn.clone(), now, &watered).await,
                    Err(error) => {
                        tracing::warn!(diagnostic = %crate::diagnostics::from_error(&error, "demo.history.read_watering"), "demo history probe seed failed")
                    }
                }
            }
            Ok(Err(error)) => {
                tracing::warn!(diagnostic = %crate::diagnostics::from_error(&error, "demo.history.seed"), "demo history refresh failed; retrying next hour")
            }
            Err(error) => {
                tracing::warn!(diagnostic = %crate::diagnostics::from_error(&error, "demo.history.task"), "demo history task failed; retrying next hour")
            }
        }
    }
}

/// All rows for the missing days commit together. An interrupted or failed
/// refresh can retry without duplicates or partly populated daily accounts.
fn seed(conn: &mut Connection, now: i64, calendar: Calendar) -> rusqlite::Result<usize> {
    let today = calendar.local_date(now).ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(
            "demo history clock is outside the supported date range".into(),
        )
    })?;
    let tx = conn.transaction()?;
    let mut added = 0;
    for back in (1..=365).rev() {
        let Some(date) = today.checked_sub_days(Days::new(back)) else {
            continue;
        };
        let Some((start, end)) = calendar.day_bounds_datetime(date) else {
            continue;
        };
        let (start, end) = (start.timestamp(), end.timestamp());
        // Preserve earlier demo seeds and any imported/non-demo rows. A day
        // with existing evidence is never retroactively given a different story.
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM daily_irrigation WHERE date_local = ?1)
                OR EXISTS(SELECT 1 FROM runs WHERE start_epoch >= ?2 AND start_epoch < ?3)
                OR EXISTS(SELECT 1 FROM verdict_history WHERE epoch >= ?2 AND epoch < ?3)",
            params![date.to_string(), start, end],
            |row| row.get(0),
        )?;
        if exists {
            continue;
        }
        // Use the date ordinal so revisiting the same date next week cannot
        // turn a former rain day into a watering day.
        let index = date.num_days_from_ce();
        let kind = index.rem_euclid(7);
        let (verdict, reason_code, reason) = match kind {
            2 => (
                "skip",
                "rain_soon",
                "Rain expected within 4h (0.31 in forecast)",
            ),
            5 => (
                "skip",
                "wind_now",
                "Wind too high now: 14 mph above the 10 mph limit",
            ),
            0 | 3 => (
                "skip",
                "soil_not_due",
                "Soil water is above the watering trigger",
            ),
            _ => (
                "run",
                "water_needed",
                "Soil water is below the watering trigger",
            ),
        };
        let epoch = start + 6 * 3600;
        let mut cursor = epoch;
        let mut zones = Vec::new();
        for (slug, name, duration) in ZONES {
            let capped = slug == "back_yard" && kind == 4;
            let seconds = if verdict == "run" && !capped {
                (i64::from(duration) + i64::from(index.rem_euclid(90)) - 45).max(300) as u32
            } else {
                0
            };
            let zone_reason = if capped {
                "Soil water is above the watering trigger"
            } else {
                reason
            };
            // Match the scheduler's recorded outcome shape. A held zone is
            // explicitly skipped, never a zero-duration completed watering.
            tx.execute(
                "INSERT INTO runs (zone_slug, start_epoch, end_epoch, duration_s, source, controller_id, status, skip_reason, note)
                 VALUES (?1, ?2, ?3, ?4, 'smart_morning', 'demo_controller', ?5, ?6, 'Synthetic demo history')",
                params![slug, cursor, cursor + i64::from(seconds), seconds,
                    if seconds > 0 { "completed" } else { "skipped" },
                    if seconds > 0 { None } else { Some(zone_reason) }],
            )?;
            if seconds > 0 {
                cursor += i64::from(seconds) + 300;
            }
            zones.push(DailyZoneDecision {
                zone: slug.into(),
                name: name.into(),
                planned_seconds: seconds,
                reason_code: if capped { "soil_not_due" } else { reason_code }.into(),
                reason: if capped {
                    "Soil water is above the watering trigger"
                } else {
                    reason
                }
                .into(),
                water_need: "Synthetic demo history".into(),
            });
        }
        let decision = DailyDecision {
            date_local: date.to_string(),
            epoch,
            kind: "scheduled".into(),
            zones,
        };
        let json = serde_json::to_string(&decision)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        tx.execute(
            "INSERT INTO daily_irrigation (date_local, epoch, decision_json) VALUES (?1, ?2, ?3)",
            params![decision.date_local, epoch, json],
        )?;
        tx.execute("INSERT INTO verdict_history (epoch, date_local, verdict, reason, inputs_json, trace_json) VALUES (?1, ?2, ?3, ?4, '{}', '')", params![epoch, date.to_string(), verdict, reason])?;
        let predicted = if kind == 2 { 0.31 } else { 0.0 };
        let observed = if kind == 2 && index.rem_euclid(3) != 0 {
            0.33
        } else {
            0.0
        };
        tx.execute("INSERT OR IGNORE INTO forecast_observations (date, predicted_in, observed_in, month, inserted_at_epoch, observed_source) VALUES (?1, ?2, ?3, ?4, ?5, 'gauge')", params![date.to_string(), predicted, observed, date.month(), now])?;
        added += 1;
    }
    tx.commit()?;
    Ok(added)
}

async fn recent_watering(
    conn: &Arc<Mutex<Connection>>,
    now: i64,
) -> rusqlite::Result<HashMap<&'static str, Vec<(i64, i64)>>> {
    let connection = conn.lock().await;
    let mut query = connection.prepare("SELECT zone_slug, start_epoch, end_epoch FROM runs WHERE controller_id = 'demo_controller' AND duration_s > 0 AND start_epoch >= ?1 AND end_epoch <= ?2 ORDER BY start_epoch")?;
    let mut watered: HashMap<&'static str, Vec<(i64, i64)>> = HashMap::new();
    let rows = query.query_map(params![now - 30 * 86400, now], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (zone, start, end) = row?;
        if let Some((slug, _, _)) = ZONES.iter().find(|(slug, _, _)| *slug == zone) {
            watered.entry(*slug).or_default().push((start, end));
        }
    }
    Ok(watered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
        conn
    }

    fn now() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2026-09-22T12:00:00Z")
            .unwrap()
            .timestamp()
    }

    #[test]
    fn persisted_history_recovers_after_months_and_does_not_duplicate_on_restart() {
        let mut conn = database();
        let now = now();
        conn.execute("INSERT INTO runs (zone_slug, start_epoch, duration_s, source, controller_id, status) VALUES ('old_zone', ?1, 600, 'owner', 'real_controller', 'completed')", [now - 110 * 86400]).unwrap();
        assert_eq!(seed(&mut conn, now, Calendar::utc()).unwrap(), 364);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .unwrap();
        let recent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE start_epoch >= ?1",
                [now - 30 * 86400],
                |row| row.get(0),
            )
            .unwrap();
        assert!(recent > 20);
        assert_eq!(seed(&mut conn, now + 3600, Calendar::utc()).unwrap(), 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            count
        );
        assert_eq!(seed(&mut conn, now + 86400, Calendar::utc()).unwrap(), 1);
        assert_eq!(
            conn.query_row(
                "SELECT duration_s FROM runs WHERE zone_slug = 'old_zone'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            600
        );
        let months: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT substr(date_local, 1, 7)) FROM daily_irrigation",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(months >= 12);
    }

    #[test]
    fn daily_log_has_run_and_skip_reasons_without_fake_completed_sessions() {
        for offset in [-5 * 3600, 19800, 12 * 3600] {
            let calendar = Calendar::fixed_offset(offset).unwrap();
            let mut conn = database();
            seed(&mut conn, now(), calendar).unwrap();
            let mut rows = conn
                .prepare("SELECT decision_json FROM daily_irrigation")
                .unwrap();
            let decisions = rows.query_map([], |row| row.get::<_, String>(0)).unwrap();
            let mut saw_skip = false;
            let mut saw_run = false;
            for json in decisions {
                let decision: DailyDecision = serde_json::from_str(&json.unwrap()).unwrap();
                assert_eq!(
                    decision.date_local,
                    calendar.local_date(decision.epoch).unwrap().to_string()
                );
                assert!(decision.epoch < now());
                for zone in decision.zones {
                    assert!(!zone.reason.is_empty());
                    saw_skip |= zone.planned_seconds == 0;
                    saw_run |= zone.planned_seconds > 0;
                }
            }
            assert!(saw_skip && saw_run);
            let fake: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM runs WHERE (status = 'completed' AND duration_s <= 0) OR end_epoch > ?1 OR (status = 'skipped' AND (duration_s != 0 OR skip_reason IS NULL))",
                    [now()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(fake, 0);
        }
    }

    #[test]
    fn failed_refresh_rolls_back_and_can_retry() {
        let mut conn = database();
        conn.execute_batch("CREATE TRIGGER reject_demo BEFORE INSERT ON runs BEGIN SELECT RAISE(ABORT, 'test storage failure'); END;").unwrap();
        assert!(seed(&mut conn, now(), Calendar::utc()).is_err());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM daily_irrigation", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM verdict_history", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        conn.execute_batch("DROP TRIGGER reject_demo").unwrap();
        assert_eq!(seed(&mut conn, now(), Calendar::utc()).unwrap(), 365);
    }
}
