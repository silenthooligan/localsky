//! Durable evidence of what held each actual scheduled morning.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::NaiveDate;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::engine::soil_decisions::{MorningOutcome, PastMorning, ZoneMorningDecision};

#[derive(Debug, thiserror::Error)]
#[error("soil morning decisions: {0}")]
pub struct SoilDecisionsError(#[source] Box<crate::failure::Failure>);

#[derive(Clone)]
pub struct SoilDecisionsStore {
    conn: Arc<Mutex<Connection>>,
}

impl SoilDecisionsStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// First scheduled decision wins. A restart or later changed forecast cannot
    /// retroactively spend the same morning twice or replace a restriction hold.
    pub async fn record_morning(
        &self,
        date: NaiveDate,
        epoch: i64,
        decisions: Vec<ZoneMorningDecision>,
    ) -> Result<(), SoilDecisionsError> {
        let connection = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let mut connection = connection.blocking_lock();
            let transaction = connection.transaction()?;
            for decision in decisions {
                transaction.execute(
                    "INSERT INTO soil_morning_decisions(date_local, zone_slug, epoch, outcome, reason_code)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(date_local, zone_slug) DO NOTHING",
                    params![date.to_string(), decision.zone_slug, epoch,
                        decision.outcome.as_str(), decision.reason_code],
                )?;
            }
            transaction.commit()
        }).await.map_err(|error| SoilDecisionsError(Box::new(crate::diagnostics::from_error(&error, "soil_decisions.record_morning"))))?
            .map_err(|error| SoilDecisionsError(Box::new(crate::diagnostics::from_error(&error, "soil_decisions.record_morning"))))
    }

    /// Only completed local dates are returned. Today's partial soil replay can
    /// change the current deficit, but never becomes a past forecast failure.
    pub async fn completed_window(
        &self,
        first: NaiveDate,
        today: NaiveDate,
    ) -> Result<HashMap<String, Vec<PastMorning>>, SoilDecisionsError> {
        let connection = self.conn.clone();
        tokio::task::spawn_blocking(
            move || -> rusqlite::Result<HashMap<String, Vec<PastMorning>>> {
                let connection = connection.blocking_lock();
                let mut statement = connection.prepare(
                    "SELECT date_local, zone_slug, outcome FROM soil_morning_decisions
                 WHERE date_local >= ?1 AND date_local < ?2 ORDER BY date_local",
                )?;
                let rows =
                    statement.query_map(params![first.to_string(), today.to_string()], |row| {
                        let date: String = row.get(0)?;
                        let date =
                            NaiveDate::parse_from_str(&date, "%Y-%m-%d").map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    0,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                        let slug: String = row.get(1)?;
                        let outcome: String = row.get(2)?;
                        Ok((
                            slug,
                            PastMorning {
                                date,
                                outcome: MorningOutcome::from_record(&outcome),
                            },
                        ))
                    })?;
                let mut result: HashMap<String, Vec<PastMorning>> = HashMap::new();
                for row in rows {
                    let (slug, morning) = row?;
                    result.entry(slug).or_default().push(morning);
                }
                Ok(result)
            },
        )
        .await
        .map_err(|error| {
            SoilDecisionsError(Box::new(crate::diagnostics::from_error(
                &error,
                "soil_decisions.completed_window",
            )))
        })?
        .map_err(|error| {
            SoilDecisionsError(Box::new(crate::diagnostics::from_error(
                &error,
                "soil_decisions.completed_window",
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restart_preserves_the_first_decision_and_reads_only_completed_dates() {
        let mut connection = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut connection).unwrap();
        let connection = Arc::new(Mutex::new(connection));
        let store = SoilDecisionsStore::new(connection.clone());
        let first = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let decision = |outcome| ZoneMorningDecision {
            zone_slug: "front".into(),
            outcome,
            reason_code: outcome.as_str().into(),
        };
        store
            .record_morning(first, 1, vec![decision(MorningOutcome::OtherHold)])
            .await
            .unwrap();
        let restarted = SoilDecisionsStore::new(connection);
        restarted
            .record_morning(first, 2, vec![decision(MorningOutcome::ForecastRain)])
            .await
            .unwrap();
        restarted
            .record_morning(
                first.succ_opt().unwrap(),
                3,
                vec![decision(MorningOutcome::ForecastRain)],
            )
            .await
            .unwrap();
        restarted
            .record_morning(today, 4, vec![decision(MorningOutcome::ForecastRain)])
            .await
            .unwrap();
        let rows = restarted.completed_window(first, today).await.unwrap();
        assert_eq!(rows["front"].len(), 2);
        assert_eq!(rows["front"][0].outcome, MorningOutcome::OtherHold);
        assert_eq!(rows["front"][1].outcome, MorningOutcome::ForecastRain);
    }
}
