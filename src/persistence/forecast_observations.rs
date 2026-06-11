// Forecast-observation persistence. One row per local calendar day with
// the predicted-vs-observed rain pair feeding engine::forecast_bias.
//
// Idempotent on `date` (UPSERT) so a partial-day insert is replaced by
// the end-of-day total as the refresher updates through the day.

use std::sync::Arc;

use chrono::{Datelike, NaiveDate};
use rusqlite::{params, Connection};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::engine::forecast_bias::Observation as BiasObservation;

#[derive(Debug, Error)]
pub enum ForecastObservationsError {
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("bad date string: {0}")]
    Date(String),
}

#[derive(Debug, Clone)]
pub struct ForecastObservationsStore {
    conn: Arc<Mutex<Connection>>,
}

impl ForecastObservationsStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Record the day's predicted+observed tuple. The first call for a
    /// given `date` plants both values; subsequent calls update only
    /// `observed_in`, since the morning prediction is what we're
    /// measuring against and shouldn't drift as the day progresses.
    pub async fn upsert(
        &self,
        date: NaiveDate,
        predicted_in: f64,
        observed_in: f64,
    ) -> Result<(), ForecastObservationsError> {
        let c = self.conn.clone();
        let date_str = date.format("%Y-%m-%d").to_string();
        let month = date.month() as i64;
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO forecast_observations
                    (date, predicted_in, observed_in, month, inserted_at_epoch)
                 VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))
                 ON CONFLICT(date) DO UPDATE SET
                    observed_in = excluded.observed_in,
                    inserted_at_epoch = excluded.inserted_at_epoch",
                params![date_str, predicted_in, observed_in, month],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| ForecastObservationsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| ForecastObservationsError::Sqlite(e.to_string()))
    }

    /// Days since the most recent local day whose OBSERVED rain met
    /// `threshold_in`, per the station gauge totals this table records.
    /// `Ok(None)` when no recorded day ever met the threshold. The
    /// refresher min's this against the regional model's counter so a
    /// hyperlocal storm the model never resolved still counts as recent
    /// rain (2026-06-11 incident: heat-advisory extend the morning after
    /// a soaking).
    pub async fn days_since_observed_rain(
        &self,
        threshold_in: f64,
    ) -> Result<Option<u32>, ForecastObservationsError> {
        let c = self.conn.clone();
        let last_wet: Option<String> =
            tokio::task::spawn_blocking(move || -> rusqlite::Result<Option<String>> {
                let conn = c.blocking_lock();
                match conn.query_row(
                    "SELECT date FROM forecast_observations
                     WHERE observed_in >= ?1
                     ORDER BY date DESC LIMIT 1",
                    params![threshold_in],
                    |r| r.get::<_, String>(0),
                ) {
                    Ok(d) => Ok(Some(d)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e),
                }
            })
            .await
            .map_err(|e| ForecastObservationsError::Sqlite(format!("join: {e}")))?
            .map_err(|e| ForecastObservationsError::Sqlite(e.to_string()))?;
        let Some(date_str) = last_wet else {
            return Ok(None);
        };
        let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
            .map_err(|e| ForecastObservationsError::Date(format!("{date_str}: {e}")))?;
        // Clamp at zero: a (clock-skewed) future-dated row reads as "wet
        // today" rather than going negative.
        let days = (chrono::Local::now().date_naive() - date).num_days().max(0);
        Ok(Some(days as u32))
    }

    /// Load every observation in the last `window_days`. The engine
    /// caller passes the slice into `BiasModel::from_observations`.
    pub async fn recent(
        &self,
        window_days: i64,
    ) -> Result<Vec<BiasObservation>, ForecastObservationsError> {
        let c = self.conn.clone();
        let rows =
            tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<(String, f64, f64)>> {
                let conn = c.blocking_lock();
                let cutoff_epoch = chrono::Utc::now().timestamp() - window_days * 86400;
                let mut stmt = conn.prepare(
                    "SELECT date, predicted_in, observed_in
                 FROM forecast_observations
                 WHERE inserted_at_epoch >= ?1
                 ORDER BY date ASC",
                )?;
                let mapped = stmt
                    .query_map(params![cutoff_epoch], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, f64>(1)?,
                            r.get::<_, f64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(mapped)
            })
            .await
            .map_err(|e| ForecastObservationsError::Sqlite(format!("join: {e}")))?
            .map_err(|e| ForecastObservationsError::Sqlite(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for (date_str, predicted_in, observed_in) in rows {
            let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                .map_err(|e| ForecastObservationsError::Date(format!("{date_str}: {e}")))?;
            out.push(BiasObservation::new(date, predicted_in, observed_in));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;
    use rusqlite::Connection;

    async fn fresh_store() -> ForecastObservationsStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        ForecastObservationsStore::new(Arc::new(Mutex::new(c)))
    }

    #[tokio::test]
    async fn days_since_observed_rain_empty_table_is_none() {
        let s = fresh_store().await;
        assert_eq!(s.days_since_observed_rain(0.05).await.unwrap(), None);
    }

    #[tokio::test]
    async fn days_since_observed_rain_ignores_sub_threshold_days() {
        let s = fresh_store().await;
        let today = chrono::Local::now().date_naive();
        // Today drizzled 0.02": below the 0.05" significance floor.
        s.upsert(today, 0.0, 0.02).await.unwrap();
        assert_eq!(s.days_since_observed_rain(0.05).await.unwrap(), None);
    }

    #[tokio::test]
    async fn days_since_observed_rain_counts_from_most_recent_wet_day() {
        let s = fresh_store().await;
        let today = chrono::Local::now().date_naive();
        // Three days ago soaked; yesterday drizzled below threshold.
        s.upsert(today - chrono::Duration::days(3), 0.1, 1.20)
            .await
            .unwrap();
        s.upsert(today - chrono::Duration::days(1), 0.0, 0.01)
            .await
            .unwrap();
        assert_eq!(s.days_since_observed_rain(0.05).await.unwrap(), Some(3));
        // A wet TODAY row pulls the counter to zero (the incident case:
        // the station gauge knows about rain the model missed).
        s.upsert(today, 0.0, 0.40).await.unwrap();
        assert_eq!(s.days_since_observed_rain(0.05).await.unwrap(), Some(0));
    }
}
