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
    /// given `date` plants both values; subsequent calls update only the
    /// observed side, since the morning prediction is what we're
    /// measuring against and shouldn't drift as the day progresses.
    ///
    /// The observed side is DAY-MAX, never clobbered down: a gauge going
    /// stale mid-storm (letting a lower model fill take over the merge)
    /// must not reset the day's recorded total. `observed_source` tags
    /// the writer that supplied the day's max value
    /// ('gauge'|'radar'|'model'|'none'); it only moves when the value
    /// does.
    pub async fn upsert(
        &self,
        date: NaiveDate,
        predicted_in: f64,
        observed_in: f64,
        observed_source: &str,
    ) -> Result<(), ForecastObservationsError> {
        let c = self.conn.clone();
        let date_str = date.format("%Y-%m-%d").to_string();
        let month = date.month() as i64;
        let source = observed_source.to_string();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            // Both SET expressions read the PRE-update row (SQLite upsert
            // semantics), so the source follows the value: it changes only
            // when the incoming observation is the new day max.
            conn.execute(
                "INSERT INTO forecast_observations
                    (date, predicted_in, observed_in, month, inserted_at_epoch, observed_source)
                 VALUES (?1, ?2, ?3, ?4, strftime('%s','now'), ?5)
                 ON CONFLICT(date) DO UPDATE SET
                    observed_in = MAX(observed_in, excluded.observed_in),
                    observed_source = CASE
                        WHEN excluded.observed_in >= observed_in THEN excluded.observed_source
                        ELSE observed_source
                    END,
                    inserted_at_epoch = excluded.inserted_at_epoch",
                params![date_str, predicted_in, observed_in, month, source],
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
                    // 'none' rows are fabricated placeholders (no rain-capable
                    // source that day), never evidence of wetness or dryness.
                    "SELECT date FROM forecast_observations
                     WHERE observed_in >= ?1 AND observed_source != 'none'
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
        // today" rather than going negative. Configured-timezone date, same
        // frame the ingest writer stamps rows with; anchoring on the
        // container's clock read one day ahead every evening in a UTC
        // container.
        let days = (crate::timeutil::now_local().date_naive() - date)
            .num_days()
            .max(0);
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
                // 'none' rows carry fabricated 0.0 observations (no
                // rain-capable source that day); letting them into the bias
                // fit would train the floor multiplier on days that measured
                // nothing.
                let mut stmt = conn.prepare(
                    "SELECT date, predicted_in, observed_in
                 FROM forecast_observations
                 WHERE inserted_at_epoch >= ?1 AND observed_source != 'none'
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

    /// Every observation between `from` and `to` (both inclusive), by
    /// DATE, ascending. `recent()` windows on `inserted_at_epoch` (the
    /// last write time), which is fine for rolling bias windows but not
    /// for a calendar-window read; the tuning report keys its rain-day
    /// and scorecard math on the row's own configured-tz date, so it
    /// queries the date column directly (the observed_rain_last_n_days
    /// pattern). Dates are TEXT 'YYYY-MM-DD', so string comparison is
    /// date order.
    pub async fn range(
        &self,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<BiasObservation>, ForecastObservationsError> {
        let c = self.conn.clone();
        let from_str = from.format("%Y-%m-%d").to_string();
        let to_str = to.format("%Y-%m-%d").to_string();
        let rows =
            tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<(String, f64, f64)>> {
                let conn = c.blocking_lock();
                // 'none' rows are placeholders (no rain-capable source
                // that day): excluded here so the tuning scorecard's
                // date-keyed reads treat those days as ABSENT (its
                // unscoreable path), never as measured-dry evidence.
                let mut stmt = conn.prepare(
                    "SELECT date, predicted_in, observed_in
                     FROM forecast_observations
                     WHERE date >= ?1 AND date <= ?2 AND observed_source != 'none'
                     ORDER BY date ASC",
                )?;
                let mapped = stmt
                    .query_map(params![from_str, to_str], |r| {
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

    /// Sum measured (gauge) daily rainfall over the last `window_days`,
    /// EXCLUDING today. The engine's observed-rain backstop already counts
    /// today's measured rain separately (`rain_today_in`), so this covers the
    /// preceding days only. It exists because the live ladder's observed-rain
    /// gate otherwise reads Open-Meteo's regional `past_daily` archive, which
    /// misses hyperlocal convection: a pop-up storm the gauge measured but the
    /// model never saw would not suppress the next morning's run. The caller
    /// max()es this against the model archive so neither source alone can hide
    /// real rain. Returns 0.0 when there are no rows in range.
    pub async fn observed_rain_last_n_days(
        &self,
        window_days: i64,
    ) -> Result<f64, ForecastObservationsError> {
        let c = self.conn.clone();
        // Configured-timezone date: rows are keyed by the ingest writer's
        // configured-tz day, so the window must anchor on the same frame (the
        // container clock double-counted "today" every evening under UTC).
        let today_naive = crate::timeutil::now_local().date_naive();
        let today = today_naive.format("%Y-%m-%d").to_string();
        let start = (today_naive - chrono::Duration::days(window_days.max(0)))
            .format("%Y-%m-%d")
            .to_string();
        let total = tokio::task::spawn_blocking(move || -> rusqlite::Result<f64> {
            let conn = c.blocking_lock();
            conn.query_row(
                "SELECT COALESCE(SUM(observed_in), 0.0)
                 FROM forecast_observations
                 WHERE date >= ?1 AND date < ?2 AND observed_source != 'none'",
                params![start, today],
                |r| r.get::<_, f64>(0),
            )
        })
        .await
        .map_err(|e| ForecastObservationsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| ForecastObservationsError::Sqlite(e.to_string()))?;
        Ok(total)
    }

    /// Per-source observed-rain sums AND row counts over the trailing
    /// `window_days` local days INCLUDING today (today's row is
    /// live-updated by the day-max writer). Feeds the water balance's
    /// observed-rain ladder, which resolves by COVERAGE: a measured rung
    /// with rows present wins even when its total is smaller than the
    /// model side (a dry gauge week is a measurement, not an absence).
    /// 'none' rows are placeholders and are excluded outright.
    pub async fn observed_rain_window_by_source(
        &self,
        window_days: i64,
    ) -> Result<ObservedRainWindow, ForecastObservationsError> {
        let c = self.conn.clone();
        let today_naive = crate::timeutil::now_local().date_naive();
        let end = today_naive.format("%Y-%m-%d").to_string();
        let start = (today_naive - chrono::Duration::days((window_days - 1).max(0)))
            .format("%Y-%m-%d")
            .to_string();
        let rows =
            tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<(String, f64, i64)>> {
                let conn = c.blocking_lock();
                let mut stmt = conn.prepare(
                    "SELECT observed_source, COALESCE(SUM(observed_in), 0.0), COUNT(*)
                 FROM forecast_observations
                 WHERE date >= ?1 AND date <= ?2 AND observed_source != 'none'
                 GROUP BY observed_source",
                )?;
                let mapped = stmt
                    .query_map(params![start, end], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, f64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(mapped)
            })
            .await
            .map_err(|e| ForecastObservationsError::Sqlite(format!("join: {e}")))?
            .map_err(|e| ForecastObservationsError::Sqlite(e.to_string()))?;
        let mut out = ObservedRainWindow::default();
        for (source, sum_in, days) in rows {
            let days = days.max(0) as u32;
            match source.as_str() {
                "gauge" => {
                    out.gauge_in += sum_in;
                    out.gauge_days += days;
                }
                "radar" => {
                    out.radar_in += sum_in;
                    out.radar_days += days;
                }
                "legacy" => {
                    out.legacy_in += sum_in;
                    out.legacy_days += days;
                }
                _ => out.model_in += sum_in,
            }
        }
        Ok(out)
    }
}

/// Per-source observed-rain sums (inches) + covered-day counts over a
/// trailing window. 'legacy' rows predate the provenance column: the
/// caller classifies them by install class (gauge-quality when a station
/// source exists, model-quality otherwise).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObservedRainWindow {
    pub gauge_in: f64,
    pub radar_in: f64,
    pub model_in: f64,
    pub legacy_in: f64,
    /// Day-row counts per measured source (a 0.0 row still counts: a dry
    /// gauge day is coverage).
    pub gauge_days: u32,
    pub radar_days: u32,
    pub legacy_days: u32,
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
    async fn range_reads_by_date_inclusive() {
        let s = fresh_store().await;
        let base = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        for i in 0..5 {
            s.upsert(
                base + chrono::Duration::days(i),
                0.1 * i as f64,
                0.0,
                "gauge",
            )
            .await
            .unwrap();
        }
        let rows = s
            .range(
                base + chrono::Duration::days(1),
                base + chrono::Duration::days(3),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 3, "both bounds inclusive");
        assert_eq!(rows[0].date, base + chrono::Duration::days(1));
        assert_eq!(rows[2].date, base + chrono::Duration::days(3));
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
        s.upsert(today, 0.0, 0.02, "gauge").await.unwrap();
        assert_eq!(s.days_since_observed_rain(0.05).await.unwrap(), None);
    }

    #[tokio::test]
    async fn days_since_observed_rain_counts_from_most_recent_wet_day() {
        let s = fresh_store().await;
        let today = chrono::Local::now().date_naive();
        // Three days ago soaked; yesterday drizzled below threshold.
        s.upsert(today - chrono::Duration::days(3), 0.1, 1.20, "gauge")
            .await
            .unwrap();
        s.upsert(today - chrono::Duration::days(1), 0.0, 0.01, "gauge")
            .await
            .unwrap();
        assert_eq!(s.days_since_observed_rain(0.05).await.unwrap(), Some(3));
        // A wet TODAY row pulls the counter to zero (the incident case:
        // the station gauge knows about rain the model missed).
        s.upsert(today, 0.0, 0.40, "gauge").await.unwrap();
        assert_eq!(s.days_since_observed_rain(0.05).await.unwrap(), Some(0));
    }

    /// The observed side is day-max: a stale gauge letting a lower model
    /// fill take the merge must not reset the day's recorded total, and
    /// the provenance tag follows the value that holds the max.
    #[tokio::test]
    async fn upsert_keeps_the_day_max_and_its_source() {
        let s = fresh_store().await;
        let day = chrono::Local::now().date_naive();
        s.upsert(day, 0.30, 0.55, "gauge").await.unwrap();
        // Gauge goes stale mid-storm; a lower model fill writes next.
        s.upsert(day, 0.30, 0.10, "model").await.unwrap();
        let rows = s.range(day, day).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            (rows[0].observed_in - 0.55).abs() < 1e-9,
            "day total must never clobber down, got {}",
            rows[0].observed_in
        );
        // The provenance stays with the writer that holds the max.
        let win = s.observed_rain_window_by_source(1).await.unwrap();
        assert!((win.gauge_in - 0.55).abs() < 1e-9);
        assert_eq!(win.model_in, 0.0);
        // A HIGHER later value moves both the total and the source.
        s.upsert(day, 0.30, 0.80, "radar").await.unwrap();
        let win = s.observed_rain_window_by_source(1).await.unwrap();
        assert!((win.radar_in - 0.80).abs() < 1e-9);
        assert_eq!(win.gauge_in, 0.0);
    }

    /// 'none' rows (no rain-capable source that day) are placeholders:
    /// they must not feed the bias fit, the dryness counters, or the
    /// balance's observed-rain sums.
    #[tokio::test]
    async fn none_rows_are_excluded_from_every_consumer() {
        let s = fresh_store().await;
        let today = chrono::Local::now().date_naive();
        // A fabricated dry day with a real prediction: exactly the row
        // that would train the bias floor if it leaked into the fit.
        s.upsert(today - chrono::Duration::days(2), 0.40, 0.0, "none")
            .await
            .unwrap();
        s.upsert(today - chrono::Duration::days(1), 0.40, 0.35, "gauge")
            .await
            .unwrap();
        let recent = s.recent(30).await.unwrap();
        assert_eq!(recent.len(), 1, "the 'none' row must not reach the fit");
        assert!((recent[0].observed_in - 0.35).abs() < 1e-9);
        // The trailing sum sees only the gauge day.
        let sum = s.observed_rain_last_n_days(7).await.unwrap();
        assert!((sum - 0.35).abs() < 1e-9, "got {sum}");
        let win = s.observed_rain_window_by_source(7).await.unwrap();
        assert!((win.gauge_in - 0.35).abs() < 1e-9);
        assert_eq!(win.model_in, 0.0);
    }

    /// The per-source window includes TODAY (the live day-max row) and
    /// buckets legacy rows separately for install-class resolution.
    #[tokio::test]
    async fn window_by_source_includes_today_and_buckets_legacy() {
        let s = fresh_store().await;
        let today = chrono::Local::now().date_naive();
        s.upsert(today, 0.0, 0.20, "gauge").await.unwrap();
        s.upsert(today - chrono::Duration::days(1), 0.0, 0.10, "legacy")
            .await
            .unwrap();
        s.upsert(today - chrono::Duration::days(3), 0.0, 0.40, "model")
            .await
            .unwrap();
        // A day outside the 7-day window stays out.
        s.upsert(today - chrono::Duration::days(9), 0.0, 2.00, "gauge")
            .await
            .unwrap();
        let win = s.observed_rain_window_by_source(7).await.unwrap();
        assert!((win.gauge_in - 0.20).abs() < 1e-9, "today's row counts");
        assert!((win.legacy_in - 0.10).abs() < 1e-9);
        assert!((win.model_in - 0.40).abs() < 1e-9);
        // Coverage counts ride along (a row is a covered day).
        assert_eq!(win.gauge_days, 1);
        assert_eq!(win.legacy_days, 1);
        assert_eq!(win.radar_days, 0);
    }

    /// A dry gauge day is COVERAGE: the day count registers even when
    /// the observed value is 0.0, which is what lets the ladder prefer a
    /// measured dry week over a wetter regional model.
    #[tokio::test]
    async fn window_counts_dry_measured_days_as_coverage() {
        let s = fresh_store().await;
        let today = chrono::Local::now().date_naive();
        s.upsert(today - chrono::Duration::days(1), 0.3, 0.0, "gauge")
            .await
            .unwrap();
        s.upsert(today - chrono::Duration::days(2), 0.0, 0.0, "gauge")
            .await
            .unwrap();
        let win = s.observed_rain_window_by_source(7).await.unwrap();
        assert_eq!(win.gauge_days, 2, "dry rows still count as coverage");
        assert_eq!(win.gauge_in, 0.0);
    }

    /// range() (the tuning scorecard's date-keyed read) excludes 'none'
    /// placeholders so those days read as absent (unscoreable), never as
    /// measured-dry evidence against a forecast skip.
    #[tokio::test]
    async fn range_excludes_none_placeholders() {
        let s = fresh_store().await;
        let base = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        s.upsert(base, 0.40, 0.0, "none").await.unwrap();
        s.upsert(base + chrono::Duration::days(1), 0.40, 0.35, "gauge")
            .await
            .unwrap();
        let rows = s
            .range(base, base + chrono::Duration::days(1))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "the 'none' day is absent from the map");
        assert_eq!(rows[0].date, base + chrono::Duration::days(1));
    }
}
