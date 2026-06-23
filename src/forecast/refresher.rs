// Open-Meteo refresher. Fetches a 7-day daily + 48-hour hourly
// forecast every 30 minutes from the no-auth public API and pushes
// the parsed snapshot into ForecastStore.
//
// Coordinates resolve per refresh tick, in priority order:
//   1. deployment.location from the live config store (so a wizard
//      location change applies on the next tick, no restart),
//   2. the boot-config coordinates passed by main.rs,
//   3. WEATHER_APP_LAT / WEATHER_APP_LON env vars (legacy v0.1 path),
//   4. 40.0 / -75.0 as the last-ditch default.
// (0, 0) config coordinates are treated as "never set" so a blank
// config cannot point the forecast at Null Island.
// Timezone is auto-detected by Open-Meteo from the coordinates so
// daily windows match the user's local clock.

use crate::config::schema::{Config, SourceKind};
use crate::config::FileConfigStore;
use crate::forecast::model_catalog::DEFAULT_MODEL;
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::forecast::store::ForecastStore;
use crate::ports::config_store::ConfigStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Backoff ceiling for upstream failures. 16 doublings of the base
/// interval would overshoot the refresh cadence; cap at 30 minutes so
/// the refresher never sleeps longer than the happy-path interval.
const BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

/// Spawn the refresher loop. `boot_coords` is the wizard-configured
/// deployment.location at boot (None on fresh installs); `cfg_store`
/// is the live config handle re-read each tick so location edits apply
/// without a restart.
pub fn spawn_forecast_refresher(
    store: Arc<ForecastStore>,
    boot_coords: Option<(f64, f64)>,
    cfg_store: Option<Arc<FileConfigStore>>,
) {
    tokio::spawn(async move {
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

        // Circuit-breaker state: count consecutive failures, double the
        // sleep on each, and emit one degraded-mode / recovered log per
        // state transition rather than per failure.
        let mut consecutive_failures: u32 = 0;
        let mut degraded: bool = false;

        loop {
            let (lat, lon) = resolve_coords(cfg_store.as_deref(), boot_coords).await;
            let model = resolve_model(cfg_store.as_deref()).await;
            let sleep_for = match refresh_once(&client, lat, lon, &model).await {
                Ok(snap) => {
                    store.store(snap);
                    if degraded {
                        tracing::info!(consecutive_failures, "forecast source recovered");
                        degraded = false;
                    }
                    consecutive_failures = 0;
                    REFRESH_INTERVAL
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let mut prev = (*store.snapshot()).clone();
                    prev.source_reachable = false;
                    store.store(prev);
                    if !degraded {
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "forecast source unreachable; entering degraded mode"
                        );
                        degraded = true;
                    } else {
                        tracing::debug!(
                            consecutive_failures,
                            error = %format!("{e:#}"),
                            "forecast still unreachable"
                        );
                    }
                    backoff(consecutive_failures)
                }
            };
            tokio::time::sleep(sleep_for).await;
        }
    });
}

/// Resolve forecast coordinates for one refresh tick. Live config wins
/// (wizard changes apply without restart), then the boot-config coords,
/// then the legacy env vars, then the 40/-75 default. (0, 0) means
/// "location never set" and falls through.
async fn resolve_coords(
    cfg_store: Option<&FileConfigStore>,
    boot_coords: Option<(f64, f64)>,
) -> (f64, f64) {
    if let Some(store) = cfg_store {
        if let Ok(cfg) = store.load().await {
            let (lat, lon) = (cfg.deployment.location.lat, cfg.deployment.location.lon);
            if lat != 0.0 || lon != 0.0 {
                return (lat, lon);
            }
        }
    }
    if let Some((lat, lon)) = boot_coords {
        if lat != 0.0 || lon != 0.0 {
            return (lat, lon);
        }
    }
    env_coords()
}

/// Resolve the Open-Meteo model for one refresh tick, live-config
/// first (a model change in settings applies on the next tick, no
/// restart, same contract as resolve_coords).
async fn resolve_model(cfg_store: Option<&FileConfigStore>) -> String {
    if let Some(store) = cfg_store {
        if let Ok(cfg) = store.load().await {
            return configured_open_meteo_model(&cfg);
        }
    }
    DEFAULT_MODEL.to_string()
}

/// The configured Open-Meteo model: the first open_meteo source entry's
/// `model`, defaulting to best_match when none is configured. Enabled
/// state is deliberately ignored: the refresher runs unconditionally
/// today regardless of the source flag, and the model should travel
/// with the entry the operator edited. Shared with the /radar/windgrid
/// handler so both fetches pin the same model.
pub fn configured_open_meteo_model(cfg: &Config) -> String {
    cfg.sources
        .iter()
        .find_map(|s| match &s.source {
            SourceKind::OpenMeteo(c) => Some(c.model.clone()),
            _ => None,
        })
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Legacy v0.1 coordinate source: WEATHER_APP_LAT/LON env vars with the
/// historical 40.0 / -75.0 fallback.
fn env_coords() -> (f64, f64) {
    let lat: f64 = std::env::var("WEATHER_APP_LAT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40.0);
    let lon: f64 = std::env::var("WEATHER_APP_LON")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-75.0);
    (lat, lon)
}

/// Exponential backoff with jitter, capped at BACKOFF_MAX. base = 30s,
/// doubling each consecutive failure (60s, 120s, 240s, ...).
fn backoff(n: u32) -> Duration {
    let base = 30u64;
    let mult = 1u64.checked_shl(n.min(16)).unwrap_or(u64::MAX);
    let secs = base.saturating_mul(mult).min(BACKOFF_MAX.as_secs());
    // Lightweight jitter so a fleet of restarting LocalSkys doesn't
    // synchronize their retries at upstream.
    let jitter = (secs / 10).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let off = nanos % (2 * jitter + 1);
    Duration::from_secs(secs.saturating_sub(jitter).saturating_add(off))
}

/// Number of past days to fetch alongside the 7-day forecast. Used by
/// the heat-advisory rule to compute "days since significant rain"
/// without depending on the SQLite history layer.
const PAST_DAYS: usize = 3;

/// Build the forecast URL. `&models=` is appended ONLY for a
/// non-default model so the best_match URL stays byte-identical to the
/// pre-model-selection one (same upstream behavior, same cache keys).
fn forecast_url(lat: f64, lon: f64, model: &str) -> String {
    let mut url = format!(
        "https://api.open-meteo.com/v1/forecast?\
         latitude={lat}&longitude={lon}&\
         daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,precipitation_probability_max,wind_speed_10m_max,wind_gusts_10m_max,uv_index_max,sunrise,sunset&\
         hourly=weather_code,temperature_2m,apparent_temperature,precipitation,precipitation_probability,wind_speed_10m,wind_direction_10m,relative_humidity_2m,cloud_cover&\
         temperature_unit=fahrenheit&\
         wind_speed_unit=mph&\
         precipitation_unit=inch&\
         past_days={PAST_DAYS}&\
         forecast_days=7&\
         forecast_hours=48&\
         timezone=auto"
    );
    if model != DEFAULT_MODEL {
        url.push_str("&models=");
        url.push_str(model);
    }
    url
}

async fn refresh_once(
    client: &Client,
    lat: f64,
    lon: f64,
    model: &str,
) -> Result<ForecastSnapshot> {
    let url = forecast_url(lat, lon, model);
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
    /// Daily peak gust (mph); runs well above sustained wind_speed_10m_max and
    /// is the signal the high-wind alert uses (the Tempest under-reads gusts).
    wind_gusts_10m_max: Vec<f64>,
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
                wind_gust_max_mph: pick(&self.daily.wind_gusts_10m_max, i),
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
                precip_probability: pick(&self.hourly.precipitation_probability, i).unwrap_or(0),
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
/// offset suffix.
///
/// For date-only entries (daily windows), anchor at NOON UTC of that
/// calendar date. Anchoring at midnight UTC would push the resulting
/// instant onto the previous local day in any UTC-X timezone
/// (e.g. "2026-05-26" -> 00:00 UTC -> 2026-05-25 20:00 EDT, weekday
/// Mon). The 7-day verdict strip and the restriction-weekday check
/// both consume time_epoch via `Local.timestamp_opt(..)`, so a date
/// that drifts onto the previous local day breaks the entire weekday
/// gate: every cell evaluates against yesterday's weekday and SJRWMD
/// "even = Thu+Sun" ends up rejecting every day of the week.
/// Anchoring at noon UTC keeps the local date stable for any timezone
/// inside +/- 11 hours of UTC.
fn parse_om_local(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    let with_z = if s.contains('T') {
        format!("{s}:00Z")
    } else {
        format!("{s}T12:00:00Z")
    };
    DateTime::parse_from_rfc3339(&with_z)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    #[test]
    fn date_only_anchors_at_noon_utc_stays_on_local_day() {
        // "2026-05-26" should land on May 26 in any timezone within
        // +/- 11h. Midnight-UTC anchoring would push it onto May 25 in
        // the western hemisphere; noon-UTC keeps it stable.
        let epoch = parse_om_local("2026-05-26");
        for offset_h in -11..=11 {
            let tz = FixedOffset::east_opt(offset_h * 3600).unwrap();
            let dt = tz.timestamp_opt(epoch, 0).single().unwrap();
            assert_eq!(
                dt.format("%Y-%m-%d").to_string(),
                "2026-05-26",
                "tz offset {offset_h}h drifted off the expected local date"
            );
        }
    }

    #[test]
    fn datetime_pass_through_preserves_clock() {
        // Hourly entries already include a clock; preserve them
        // verbatim (treated as UTC, same as before the fix).
        let epoch = parse_om_local("2026-05-26T14:30");
        let utc = chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0).expect("valid epoch");
        assert_eq!(utc.format("%Y-%m-%dT%H:%M").to_string(), "2026-05-26T14:30");
    }

    #[test]
    fn empty_string_returns_zero() {
        assert_eq!(parse_om_local(""), 0);
    }

    #[test]
    fn default_model_url_is_byte_identical_to_pre_model_url() {
        // The exact URL the refresher fetched before model selection
        // existed. best_match MUST keep producing these bytes.
        let expected = concat!(
            "https://api.open-meteo.com/v1/forecast?latitude=28.5&longitude=-81.4",
            "&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,",
            "precipitation_probability_max,wind_speed_10m_max,wind_gusts_10m_max,uv_index_max,sunrise,sunset",
            "&hourly=weather_code,temperature_2m,apparent_temperature,precipitation,",
            "precipitation_probability,wind_speed_10m,wind_direction_10m,",
            "relative_humidity_2m,cloud_cover",
            "&temperature_unit=fahrenheit&wind_speed_unit=mph&precipitation_unit=inch",
            "&past_days=3&forecast_days=7&forecast_hours=48&timezone=auto"
        );
        assert_eq!(forecast_url(28.5, -81.4, "best_match"), expected);
    }

    #[test]
    fn non_default_model_appends_models_param_only() {
        let base = forecast_url(48.14, 11.58, "best_match");
        let pinned = forecast_url(48.14, 11.58, "icon_seamless");
        assert_eq!(pinned, format!("{base}&models=icon_seamless"));
    }

    #[test]
    fn configured_model_reads_first_open_meteo_source() {
        let mut cfg = crate::config::schema::Config::default();
        assert_eq!(configured_open_meteo_model(&cfg), "best_match");
        let entry: crate::config::schema::SourceEntry = serde_json::from_value(serde_json::json!({
            "id": "open_meteo",
            "kind": "open_meteo",
            "config": { "model": "gfs_seamless" },
        }))
        .unwrap();
        cfg.sources.push(entry);
        assert_eq!(configured_open_meteo_model(&cfg), "gfs_seamless");
    }

    #[tokio::test]
    async fn resolve_coords_prefers_live_config_location() {
        let dir = std::env::temp_dir().join(format!(
            "ls-forecast-coords-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localsky.toml");
        let store = FileConfigStore::new(&path);
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        store.save(&cfg).await.unwrap();
        // Config beats the (Philadelphia-defaulting) boot coords.
        let got = resolve_coords(Some(&store), Some((40.0, -75.0))).await;
        assert!((got.0 - 28.5).abs() < 1e-9);
        assert!((got.1 - (-81.4)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn resolve_coords_falls_back_to_boot_coords_without_config() {
        let dir = std::env::temp_dir().join(format!(
            "ls-forecast-nocfg-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileConfigStore::new(dir.join("missing.toml"));
        let got = resolve_coords(Some(&store), Some((28.5, -81.4))).await;
        assert!((got.0 - 28.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn resolve_coords_treats_null_island_as_unset() {
        let dir = std::env::temp_dir().join(format!(
            "ls-forecast-zero-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localsky.toml");
        let store = FileConfigStore::new(&path);
        let cfg = crate::config::schema::Config::default(); // lat/lon 0.0
        store.save(&cfg).await.unwrap();
        let got = resolve_coords(Some(&store), Some((28.5, -81.4))).await;
        assert!(
            (got.0 - 28.5).abs() < 1e-9,
            "zero config coords must fall through"
        );
    }
}
