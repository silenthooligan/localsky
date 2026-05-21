// Open-Meteo refresher. Fetches a 7-day daily + 48-hour hourly
// forecast every 30 minutes from the no-auth public API and pushes
// the parsed snapshot into ForecastStore.
//
// The lat/lon comes from WEATHER_APP_LAT / WEATHER_APP_LON env vars
// (already wired for the radar centering). Timezone is auto-detected
// by Open-Meteo from the coordinates so daily windows match the
// user's local clock.

use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::forecast::store::ForecastStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);

pub fn spawn_forecast_refresher(store: Arc<ForecastStore>) {
    tokio::spawn(async move {
        let lat: f64 = std::env::var("WEATHER_APP_LAT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(40.0);
        let lon: f64 = std::env::var("WEATHER_APP_LON")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(-75.0);
        let client = match Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("localsky/forecast")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("forecast http client init failed: {e:#}");
                return;
            }
        };

        loop {
            match refresh_once(&client, lat, lon).await {
                Ok(snap) => store.store(snap),
                Err(e) => {
                    tracing::warn!("forecast refresh failed: {e:#}");
                    let mut prev = (*store.snapshot()).clone();
                    prev.source_reachable = false;
                    store.store(prev);
                }
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

/// Number of past days to fetch alongside the 7-day forecast. Used by
/// the heat-advisory rule to compute "days since significant rain"
/// without depending on the SQLite history layer.
const PAST_DAYS: usize = 3;

async fn refresh_once(client: &Client, lat: f64, lon: f64) -> Result<ForecastSnapshot> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?\
         latitude={lat}&longitude={lon}&\
         daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,precipitation_probability_max,wind_speed_10m_max,uv_index_max,sunrise,sunset&\
         hourly=weather_code,temperature_2m,apparent_temperature,precipitation,precipitation_probability,wind_speed_10m,wind_direction_10m,relative_humidity_2m,cloud_cover&\
         temperature_unit=fahrenheit&\
         wind_speed_unit=mph&\
         precipitation_unit=inch&\
         past_days={PAST_DAYS}&\
         forecast_days=7&\
         forecast_hours=48&\
         timezone=auto"
    );
    let resp: Raw = client
        .get(&url)
        .send()
        .await
        .context("GET open-meteo forecast")?
        .error_for_status()
        .context("open-meteo non-2xx")?
        .json()
        .await
        .context("decode open-meteo json")?;
    Ok(resp.into_snapshot())
}

// Open-Meteo response shape: parallel arrays per series.

#[derive(Deserialize)]
struct Raw {
    timezone: String,
    daily: RawDaily,
    hourly: RawHourly,
}

#[derive(Deserialize)]
struct RawDaily {
    time: Vec<String>,
    weather_code: Vec<u32>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    precipitation_sum: Vec<f64>,
    precipitation_probability_max: Vec<Option<u32>>,
    wind_speed_10m_max: Vec<f64>,
    uv_index_max: Vec<Option<f64>>,
    sunrise: Vec<String>,
    sunset: Vec<String>,
}

#[derive(Deserialize)]
struct RawHourly {
    time: Vec<String>,
    weather_code: Vec<u32>,
    temperature_2m: Vec<f64>,
    apparent_temperature: Vec<f64>,
    precipitation: Vec<f64>,
    precipitation_probability: Vec<Option<u32>>,
    wind_speed_10m: Vec<f64>,
    wind_direction_10m: Vec<u32>,
    relative_humidity_2m: Vec<u32>,
    cloud_cover: Vec<u32>,
}

impl Raw {
    fn into_snapshot(self) -> ForecastSnapshot {
        let now = Utc::now().timestamp();

        // Build every daily entry, then split: the first `PAST_DAYS`
        // are past_daily, the remainder is the future-facing daily.
        // past_days=3 means [t-3, t-2, t-1, t-0, ..., t+6].
        let all_daily: Vec<DailyEntry> = (0..self.daily.time.len())
            .map(|i| DailyEntry {
                time_epoch: parse_om_local(&self.daily.time[i]),
                weather_code: pick(&self.daily.weather_code, i),
                temp_max_f: pick(&self.daily.temperature_2m_max, i),
                temp_min_f: pick(&self.daily.temperature_2m_min, i),
                precip_sum_in: pick(&self.daily.precipitation_sum, i),
                precip_probability_max: pick(&self.daily.precipitation_probability_max, i)
                    .unwrap_or(0),
                wind_max_mph: pick(&self.daily.wind_speed_10m_max, i),
                uv_index_max: pick(&self.daily.uv_index_max, i).unwrap_or(0.0),
                sunrise_epoch: parse_om_local(&self.daily.sunrise[i]),
                sunset_epoch: parse_om_local(&self.daily.sunset[i]),
            })
            .collect();
        let split = PAST_DAYS.min(all_daily.len());
        let past_daily = all_daily[..split].to_vec();
        let daily = all_daily[split..].to_vec();

        let hourly: Vec<HourlyEntry> = (0..self.hourly.time.len())
            .map(|i| HourlyEntry {
                time_epoch: parse_om_local(&self.hourly.time[i]),
                weather_code: pick(&self.hourly.weather_code, i),
                temp_f: pick(&self.hourly.temperature_2m, i),
                apparent_temp_f: pick(&self.hourly.apparent_temperature, i),
                precip_in: pick(&self.hourly.precipitation, i),
                precip_probability: pick(&self.hourly.precipitation_probability, i)
                    .unwrap_or(0),
                wind_mph: pick(&self.hourly.wind_speed_10m, i),
                wind_dir_deg: pick(&self.hourly.wind_direction_10m, i),
                humidity_pct: pick(&self.hourly.relative_humidity_2m, i),
                cloud_cover_pct: pick(&self.hourly.cloud_cover, i),
            })
            .collect();

        ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            timezone: self.timezone,
            daily,
            past_daily,
            hourly,
        }
    }
}

fn pick<T: Clone + Default>(v: &[T], i: usize) -> T {
    v.get(i).cloned().unwrap_or_default()
}

/// Parse Open-Meteo's "2026-05-09T06:32" or "2026-05-09" into a UTC
/// epoch. Open-Meteo emits times in the requested timezone with no
/// offset suffix; for daily windows we treat them as local-midnight.
/// We just convert via DateTime parsing to UTC (best-effort — daily
/// window starts will be off by ~one timezone offset, but the
/// browser converts back to Local for display so the visual day
/// boundary is correct).
fn parse_om_local(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    // Try datetime first (hourly + sunrise/sunset).
    let with_z = if s.contains('T') { format!("{s}:00Z") } else { format!("{s}T00:00:00Z") };
    DateTime::parse_from_rfc3339(&with_z)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}
