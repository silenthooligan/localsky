//! Durable morning plans. These are decisions, never invented valve runs.
use std::sync::Arc;

use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::history::types::{DailyDecision, DailyZoneDecision};
use crate::model::IrrigationSnapshot;

#[derive(Clone)]
pub struct DailyIrrigationStore(Arc<Mutex<Connection>>);

impl DailyIrrigationStore {
    pub fn new(connection: Arc<Mutex<Connection>>) -> Self {
        Self(connection)
    }

    /// The scheduled decision survives forecast changes and process restarts.
    pub async fn record(
        &self,
        decision: DailyDecision,
    ) -> Result<(), Box<crate::failure::Failure>> {
        let json = serde_json::to_string(&decision).map_err(|e| {
            Box::new(crate::diagnostics::from_error(
                &e,
                "daily_irrigation.record",
            ))
        })?;
        let connection = self.0.clone();
        tokio::task::spawn_blocking(move || {
            connection
                .blocking_lock()
                .execute(
                    "INSERT INTO daily_irrigation(date_local, epoch, decision_json)
                 SELECT ?1, ?2, ?3 WHERE ?4 != 'missed_window' OR NOT EXISTS (
                     SELECT 1 FROM soil_morning_decisions WHERE date_local = ?1
                 )
                 ON CONFLICT(date_local) DO NOTHING",
                    params![decision.date_local, decision.epoch, json, decision.kind],
                )
                .map(|_| ())
                .map_err(|e| {
                    Box::new(crate::diagnostics::from_error(
                        &e,
                        "daily_irrigation.record",
                    ))
                })
        })
        .await
        .map_err(|e| {
            Box::new(crate::diagnostics::from_error(
                &e,
                "daily_irrigation.record",
            ))
        })?
    }

    pub async fn window(
        &self,
        from: i64,
        to: i64,
    ) -> Result<Vec<DailyDecision>, Box<crate::failure::Failure>> {
        let connection = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection.blocking_lock();
            let mut query = connection.prepare(
                "SELECT decision_json FROM daily_irrigation WHERE epoch >= ?1 AND epoch < ?2 ORDER BY epoch DESC"
            ).map_err(|e| Box::new(crate::diagnostics::from_error(&e, "daily_irrigation.window")))?;
            let rows = query.query_map(params![from, to], |row| row.get::<_, String>(0))
                .map_err(|e| Box::new(crate::diagnostics::from_error(&e, "daily_irrigation.window")))?;
            let mut result: Vec<DailyDecision> = rows.map(|row| {
                serde_json::from_str(&row.map_err(|e| Box::new(crate::diagnostics::from_error(&e, "daily_irrigation.window")))?).map_err(|e| Box::new(crate::diagnostics::from_error(&e, "daily_irrigation.window")))
            }).collect::<Result<_, Box<crate::failure::Failure>>>()?;
            let recorded: std::collections::HashSet<_> = result.iter().map(|d| d.date_local.clone()).collect();
            let mut legacy = std::collections::BTreeMap::<String, DailyDecision>::new();
            let mut query = connection.prepare(
                "SELECT date_local, epoch, zone_slug, outcome, reason_code FROM soil_morning_decisions WHERE epoch >= ?1 AND epoch < ?2 ORDER BY epoch"
            ).map_err(|e| Box::new(crate::diagnostics::from_error(&e, "daily_irrigation.window")))?;
            let rows = query.query_map(params![from, to], |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?,
                row.get::<_, String>(3)?, row.get::<_, String>(4)?,
            ))).map_err(|e| Box::new(crate::diagnostics::from_error(&e, "daily_irrigation.window")))?;
            for row in rows {
                let (date, epoch, zone, outcome, code) = row.map_err(|e| Box::new(crate::diagnostics::from_error(&e, "daily_irrigation.window")))?;
                if recorded.contains(&date) { continue; }
                let day = legacy.entry(date.clone()).or_insert_with(|| DailyDecision {
                    date_local: date, epoch, kind: "scheduled_legacy".into(), zones: Vec::new(),
                });
                let reason = match outcome.as_str() {
                    "not_due" => "The soil model did not request watering at the scheduled morning decision".into(),
                    "forecast_rain" => "Watering deferred because forecast rain was expected to cover the root-zone deficit".into(),
                    _ => format!("Morning decision recorded: {}", code.replace('_', " ")),
                };
                day.zones.push(DailyZoneDecision {
                    name: zone.replace('_', " "), zone, reason_code: code, reason,
                    ..Default::default()
                });
            }
            result.extend(legacy.into_values());
            Ok(result)
        }).await.map_err(|e| Box::new(crate::diagnostics::from_error(&e, "daily_irrigation.window")))?
    }
}

pub fn from_snapshot(
    snapshot: &IrrigationSnapshot,
    date: chrono::NaiveDate,
    epoch: i64,
    missed: bool,
) -> DailyDecision {
    DailyDecision {
        date_local: date.to_string(),
        epoch,
        kind: if missed { "missed_window" } else { "scheduled" }.into(),
        zones: snapshot.zones.iter().map(|zone| {
            let budget = snapshot.water_budgets.iter().find(|b| b.zone_slug == zone.slug);
            let verdict = zone.verdict.as_ref();
            let held = verdict.is_none_or(|v| !matches!(v.verdict.as_str(), "run" | "run_extended"));
            let water_need = budget.map(|b| b.today_reason.clone()).unwrap_or_else(|| "Water need unavailable".into());
            let (reason_code, reason) = if missed {
                ("missed_window".into(), "The morning window passed before the scheduler could record its decision. Current conditions cannot establish the earlier outcome.".into())
            } else if held {
                verdict.map(|v| (v.reason_code.clone(), v.reason.clone()))
                    .unwrap_or_else(|| ("unknown".into(), "Decision unavailable; watering held".into()))
            } else if zone.smart_suppressed.as_ref().is_some_and(|s| s.active_today) {
                ("manual_schedule".into(), "An owner schedule replaces automatic irrigation today".into())
            } else if zone.planned_run_seconds == 0 {
                ("water_balance".into(), water_need.clone())
            } else {
                ("water_needed".into(), water_need.clone())
            };
            DailyZoneDecision {
                zone: zone.slug.clone(), name: zone.name.clone(),
                planned_seconds: if held || missed { 0 } else { zone.planned_run_seconds },
                reason_code, reason, water_need,
            }
        }).collect(),
    }
}

/// Keep older evidence visible without turning a continuously evaluated weather
/// verdict into proof that the scheduler dispatched or skipped a valve run.
pub fn with_legacy_decisions(
    mut daily: Vec<DailyDecision>,
    rows: Vec<super::verdict_history::VerdictRow>,
) -> Vec<DailyDecision> {
    let recorded: std::collections::HashSet<_> =
        daily.iter().map(|d| d.date_local.clone()).collect();
    let mut legacy = std::collections::BTreeMap::<String, DailyDecision>::new();
    for row in rows {
        if recorded.contains(&row.date_local) || row.verdict != "skip" || row.reason.is_empty() {
            continue;
        }
        let day = legacy
            .entry(row.date_local.clone())
            .or_insert_with(|| DailyDecision {
                date_local: row.date_local,
                epoch: row.epoch,
                kind: "recorded_decision".into(),
                zones: Vec::new(),
            });
        day.epoch = day.epoch.min(row.epoch);
        if !day.zones.iter().any(|z| z.reason == row.reason) {
            day.zones.push(DailyZoneDecision {
                name: "Recorded hold".into(),
                reason: row.reason,
                ..Default::default()
            });
        }
    }
    daily.extend(legacy.into_values());
    daily.sort_by(|a, b| b.date_local.cmp(&a.date_local));
    daily
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn late_restart_cannot_hide_a_recorded_legacy_morning() {
        let mut connection = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut connection).unwrap();
        connection.execute(
            "INSERT INTO soil_morning_decisions VALUES ('2026-09-12', 'garden', 100, 'not_due', 'soil_not_due')",
            [],
        ).unwrap();
        let store = DailyIrrigationStore::new(Arc::new(Mutex::new(connection)));
        store
            .record(DailyDecision {
                date_local: "2026-09-12".into(),
                epoch: 200,
                kind: "missed_window".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .record(DailyDecision {
                date_local: "2026-09-13".into(),
                epoch: 300,
                kind: "missed_window".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let days = store.window(0, 400).await.unwrap();
        let known = days.iter().find(|d| d.date_local == "2026-09-12").unwrap();
        assert_eq!(known.kind, "scheduled_legacy");
        assert_eq!(known.epoch, 100);
        assert_eq!(known.zones[0].reason_code, "soil_not_due");
        assert_eq!(
            days.iter()
                .find(|d| d.date_local == "2026-09-13")
                .unwrap()
                .kind,
            "missed_window"
        );
    }

    #[tokio::test]
    async fn a_waterless_morning_survives_restart_and_later_forecast_changes() {
        let directory = std::env::temp_dir().join(format!(
            "localsky-daily-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("history.db");
        let history = crate::persistence::HistoryDb::open(path.clone()).unwrap();
        let store = DailyIrrigationStore::new(history.handle());
        let first = DailyDecision {
            date_local: "2026-09-12".into(),
            epoch: 100,
            kind: "scheduled".into(),
            zones: vec![DailyZoneDecision {
                zone: "garden".into(),
                reason: "Recent rain filled the root zone".into(),
                ..Default::default()
            }],
        };
        store.record(first.clone()).await.unwrap();
        drop(store);
        drop(history);
        let history = crate::persistence::HistoryDb::open(path).unwrap();
        let store = DailyIrrigationStore::new(history.handle());
        let mut changed = first.clone();
        changed.zones[0].planned_seconds = 600;
        store.record(changed).await.unwrap();
        assert_eq!(store.window(0, 200).await.unwrap(), vec![first]);
        drop(store);
        drop(history);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
