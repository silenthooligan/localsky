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
