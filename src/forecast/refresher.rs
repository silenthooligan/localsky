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
use crate::ports::config_store::ConfigStore;
use crate::ports::weather_source::{SourceEvent, WeatherField};
use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

const REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Backoff ceiling for upstream failures. 16 doublings of the base
/// interval would overshoot the refresh cadence; cap at 30 minutes so
/// the refresher never sleeps longer than the happy-path interval.
const BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

/// Spawn the Open-Meteo refresher loop. It is now a bus FORECAST source like
/// any other: instead of writing the ForecastStore directly it emits
/// `SourceEvent::Forecast` (+ `Reachability`) on `bus`, and the `forecast_bridge`
/// arbitrates it against any other configured forecast source by priority.
/// `source_id` is the Open-Meteo source entry's id (or the default). `boot_coords`
/// is the wizard-configured deployment.location at boot (None on fresh installs);
/// `cfg_store` is the live config handle re-read each tick so location edits
/// apply without a restart.
pub fn spawn_forecast_refresher(
    bus: broadcast::Sender<SourceEvent>,
    source_id: String,
    boot_coords: Option<(f64, f64)>,
    cfg_store: Option<Arc<FileConfigStore>>,
) {
    tokio::spawn(async move {
        // Last successful snapshot, so a failure can re-emit it flagged
        // unreachable (preserving the old store-prev-with-source_reachable=false
        // behavior) without wiping the forecast.
        let mut last_good: Option<ForecastSnapshot> = None;
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
                Ok((snap, current)) => {
                    let at = snap.last_refresh_epoch;
                    last_good = Some(snap.clone());
                    let _ = bus.send(SourceEvent::Forecast {
                        source_id: source_id.clone(),
                        snapshot: snap,
                        at_epoch: at,
                    });
                    // LIVE current conditions: emit the parsed `current` block as
                    // an Observation on the SAME bus. The snapshot bridge routes
                    // it to TempestStore::apply_source_fields; Open-Meteo's id is
                    // absent from the live_current map, so it fills as a
                    // forecast-derived (live_current=false) low-priority source
                    // that a LAN station outranks by default, yet a per-field
                    // override (e.g. WIND = open_meteo) can pin. Forecast path is
                    // unchanged whether or not a current block is present.
                    if let Some((fields, cur_at)) = current {
                        let _ = bus.send(SourceEvent::Observation {
                            source_id: source_id.clone(),
                            fields,
                            at_epoch: cur_at,
                        });
                    }
                    if degraded {
                        tracing::info!(consecutive_failures, "forecast source recovered");
                        let _ = bus.send(SourceEvent::Reachability {
                            source_id: source_id.clone(),
                            reachable: true,
                        });
                        degraded = false;
                    }
                    consecutive_failures = 0;
                    REFRESH_INTERVAL
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    // Re-emit the last good forecast flagged unreachable so the UI
                    // shows the stale forecast + a degraded badge (the bridge
                    // accepts it as the same owner refreshing itself).
                    if let Some(prev) = last_good.clone() {
                        let at = prev.last_refresh_epoch;
                        let mut prev = prev;
                        prev.source_reachable = false;
                        let _ = bus.send(SourceEvent::Forecast {
                            source_id: source_id.clone(),
                            snapshot: prev,
                            at_epoch: at,
                        });
                    }
                    let _ = bus.send(SourceEvent::Reachability {
                        source_id: source_id.clone(),
                        reachable: false,
                    });
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
         current=temperature_2m,relative_humidity_2m,dew_point_2m,apparent_temperature,wind_speed_10m,wind_gusts_10m,wind_direction_10m,surface_pressure,precipitation,precipitation_probability,weather_code,cloud_cover,shortwave_radiation,uv_index&\
         daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,precipitation_probability_max,wind_speed_10m_max,wind_gusts_10m_max,uv_index_max,et0_fao_evapotranspiration,sunrise,sunset&\
         hourly=weather_code,temperature_2m,apparent_temperature,precipitation,precipitation_probability,wind_speed_10m,wind_direction_10m,relative_humidity_2m,cloud_cover&\
         temperature_unit=fahrenheit&\
         wind_speed_unit=mph&\
         precipitation_unit=inch&\
         past_days={PAST_DAYS}&\
         forecast_days=7&\
         forecast_hours=48&\
         timezone=auto&\
         timeformat=unixtime"
    );
    if model != DEFAULT_MODEL {
        url.push_str("&models=");
        url.push_str(model);
    }
    url
}

/// Fetch + parse one Open-Meteo response into the forecast snapshot AND the
/// optional LIVE current-conditions scalars (`current=` block). Both come from
/// the SAME single HTTP request, mirroring how the WeatherSource adapters
/// (NWS/OpenWeather) emit a Forecast and an Observation from one fetch.
async fn refresh_once(
    client: &Client,
    lat: f64,
    lon: f64,
    model: &str,
) -> Result<(ForecastSnapshot, Option<(Vec<(WeatherField, f64)>, i64)>)> {
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
    let current = resp.current_fields();
    Ok((resp.into_snapshot(), current))
}

// Open-Meteo response shape: parallel arrays per series.

#[derive(Deserialize)]
struct Raw {
    timezone: String,
    daily: RawDaily,
    hourly: RawHourly,
    /// LIVE current-conditions block (`current=`). Absent on older cached
    /// responses or if Open-Meteo ever drops the parameter, so it is optional:
    /// no `current` block simply means no live scalars this tick (the forecast
    /// path is unaffected).
    #[serde(default)]
    current: Option<RawCurrent>,
    /// The units Open-Meteo echoes for the `current` block. We request
    /// fahrenheit / mph / inch, but we HONOR whatever the response declares so a
    /// future unit drift can't silently feed (e.g.) Celsius into a Fahrenheit
    /// field. Absent -> assume the requested imperial units.
    #[serde(default)]
    current_units: Option<RawCurrentUnits>,
}

/// Open-Meteo `current` block: one scalar per requested variable, stamped with
/// the observation `time` (epoch, `timeformat=unixtime`). Every reading is
/// optional so a partial block still yields the fields that ARE present.
#[derive(Deserialize)]
struct RawCurrent {
    /// Observation time (epoch seconds). 0/absent -> fall back to "now" at emit.
    #[serde(default)]
    time: i64,
    /// The accumulation interval (seconds) the `current` block covers. Open-Meteo
    /// echoes this alongside `time`; `precipitation` is the total accumulated over
    /// THIS interval (typically 900s = 15 min), NOT a per-hour rate. We use it to
    /// normalize precipitation to in/hr (see current_fields). Absent/0 -> assume
    /// the legacy hourly bucket (treat the value as already in/hr).
    #[serde(default)]
    interval: i64,
    #[serde(default)]
    temperature_2m: Option<f64>,
    #[serde(default)]
    relative_humidity_2m: Option<f64>,
    /// Dew point at 2m. Requested in fahrenheit (we honor the echoed unit and
    /// convert C->F exactly like temperature_2m). Gives cloud-only deployments a
    /// real dew point so the comfort/feels-like context isn't blank when no LAN
    /// station owns DewPointF.
    #[serde(default)]
    dew_point_2m: Option<f64>,
    #[serde(default)]
    apparent_temperature: Option<f64>,
    #[serde(default)]
    wind_speed_10m: Option<f64>,
    #[serde(default)]
    wind_gusts_10m: Option<f64>,
    #[serde(default)]
    wind_direction_10m: Option<f64>,
    #[serde(default)]
    surface_pressure: Option<f64>,
    #[serde(default)]
    precipitation: Option<f64>,
    /// Current-hour precipitation probability (percent, 0-100). Open-Meteo emits
    /// this in the current block as the chance of precip for the current hour;
    /// surfaced as Pop so the picker can pin "current Pop -> Open-Meteo".
    #[serde(default)]
    precipitation_probability: Option<f64>,
    #[serde(default)]
    cloud_cover: Option<f64>,
    /// Shortwave (global horizontal) solar radiation, W/m². Gives cloud-only
    /// deployments a real solar reading (the hero condition + Solar panel), since
    /// no LAN station owns solar_w_m2 in that case.
    #[serde(default)]
    shortwave_radiation: Option<f64>,
    /// Current UV index (unitless). Open-Meteo supports `uv_index` in the current
    /// block; surfaced for cloud-only deployments.
    #[serde(default)]
    uv_index: Option<f64>,
}

/// The unit strings Open-Meteo echoes for the `current` block (e.g.
/// `temperature_2m: "°F"`, `wind_speed_10m: "mph"`, `surface_pressure: "hPa"`).
/// We request imperial, but reading these lets us convert if the server ever
/// hands back metric. Only the units that need a possible conversion are read.
#[derive(Deserialize)]
struct RawCurrentUnits {
    #[serde(default)]
    temperature_2m: Option<String>,
    #[serde(default)]
    dew_point_2m: Option<String>,
    #[serde(default)]
    wind_speed_10m: Option<String>,
    #[serde(default)]
    wind_gusts_10m: Option<String>,
    #[serde(default)]
    surface_pressure: Option<String>,
    #[serde(default)]
    precipitation: Option<String>,
}

#[derive(Deserialize)]
struct RawDaily {
    // timeformat=unixtime: true epochs (local-day-start instants), so no
    // client-side timezone math and no DST edge cases.
    time: Vec<i64>,
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
    /// FAO-56 reference evapotranspiration (ET0), inches/day under the requested
    /// precipitation_unit=inch. Open-Meteo computes this from its own model
    /// fields, so a cloud-only deployment gets a real daily ET0 (the Et0Today
    /// current scalar reads today's index) without a LAN station owning it.
    #[serde(default)]
    et0_fao_evapotranspiration: Vec<Option<f64>>,
    sunrise: Vec<i64>,
    sunset: Vec<i64>,
}

#[derive(Deserialize)]
struct RawHourly {
    time: Vec<i64>,
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

        let hourly: Vec<HourlyEntry> = (0..self.hourly.time.len())
            .map(|i| HourlyEntry {
                time_epoch: self.hourly.time.get(i).copied().unwrap_or(0),
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

        // Build every daily entry, then split: the first `PAST_DAYS`
        // are past_daily, the remainder is the future-facing daily.
        // past_days=3 means [t-3, t-2, t-1, t-0, ..., t+6].
        let all_daily: Vec<DailyEntry> = (0..self.daily.time.len())
            .map(|i| DailyEntry {
                time_epoch: self.daily.time.get(i).copied().unwrap_or(0),
                weather_code: pick(&self.daily.weather_code, i),
                temp_max_f: pick(&self.daily.temperature_2m_max, i),
                temp_min_f: pick(&self.daily.temperature_2m_min, i),
                // Filled below by backfill_daily_humidity from the hourly window
                // (Open-Meteo's daily rollup has no RH field).
                humidity_pct: 0,
                precip_sum_in: pick(&self.daily.precipitation_sum, i),
                precip_probability_max: pick(&self.daily.precipitation_probability_max, i)
                    .unwrap_or(0),
                wind_max_mph: pick(&self.daily.wind_speed_10m_max, i),
                wind_gust_max_mph: pick(&self.daily.wind_gusts_10m_max, i),
                uv_index_max: pick(&self.daily.uv_index_max, i).unwrap_or(0.0),
                sunrise_epoch: self.daily.sunrise.get(i).copied().unwrap_or(0),
                sunset_epoch: self.daily.sunset.get(i).copied().unwrap_or(0),
            })
            .collect();
        let split = PAST_DAYS.min(all_daily.len());
        let past_daily = all_daily[..split].to_vec();
        let daily = all_daily[split..].to_vec();

        let mut snap = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            source_label: "Open-Meteo".to_string(),
            timezone: self.timezone,
            daily,
            past_daily,
            hourly,
        };
        // Pair each day's high temp with THAT day's afternoon humidity (from the
        // hourly window) so the 3-day peak heat index is physically valid.
        snap.backfill_daily_humidity();
        snap
    }
}

fn pick<T: Clone + Default>(v: &[T], i: usize) -> T {
    v.get(i).cloned().unwrap_or_default()
}

/// True when an Open-Meteo unit string denotes Celsius (so a temperature scalar
/// needs F conversion). We request fahrenheit, so the common case is `"°F"`
/// (or absent) -> no conversion; this only triggers on a server unit drift.
fn unit_is_celsius(u: Option<&str>) -> bool {
    matches!(u, Some(s) if s.contains('C') && !s.contains('F'))
}

/// True when an Open-Meteo speed unit string denotes a non-mph unit we must
/// convert from. We request mph; metric responses come back as `"km/h"` or
/// `"m/s"`. `"mph"`/absent -> no conversion.
fn speed_to_mph_factor(u: Option<&str>) -> f64 {
    match u {
        Some(s) if s.contains("km/h") || s.contains("kmh") => 0.621_371,
        Some(s) if s.contains("m/s") || s.contains("ms") => 2.236_94,
        // "mph" or unknown/absent: assume the requested mph (no scaling).
        _ => 1.0,
    }
}

/// Every WeatherField the Open-Meteo `current_fields()` emitter can produce, in
/// emit order. This is the SINGLE source of truth for "what Open-Meteo current
/// conditions are capable + selectable": `current_fields()` only ever pushes
/// fields from this list (each push is conditioned on the matching scalar being
/// present, so a partial `current`/`daily` block emits a SUBSET, never a field
/// outside this set), and `runtime.rs` source_field_names returns this exact
/// const verbatim from its OpenMeteo arm. Collapsing the two formerly
/// hand-synced lists here makes capable == emitted == selectable, so the picker
/// can never offer (or drop) a field the emitter doesn't (or does) produce.
///
/// CROSS-AGENT CONTRACT (A1): runtime.rs source_field_names ~:417 returns this
/// path verbatim from the OpenMeteo arm. Keep the two in lockstep by referencing
/// this const, never by re-typing the list.
pub const OPEN_METEO_CURRENT_FIELDS: &[crate::ports::weather_source::WeatherField] = &[
    WeatherField::AirTempF,
    WeatherField::DewPointF,
    WeatherField::RhPct,
    WeatherField::WindMph,
    WeatherField::WindGustMph,
    WeatherField::WindBearingDeg,
    WeatherField::PressureInHg,
    WeatherField::RainIntensityInHr,
    WeatherField::RainTodayIn,
    WeatherField::Pop,
    WeatherField::SolarWm2,
    WeatherField::UvIndex,
    WeatherField::Et0Today,
];

impl Raw {
    /// LIVE current-conditions scalars from the `current` block, mapped to the
    /// merge's `WeatherField` set, honoring the response's declared units (we
    /// request imperial, but convert if the server hands back metric). Returns
    /// the `(fields, at_epoch)` pair, or `None` when there is no usable current
    /// block (so the forecast-only path is unchanged). `surface_pressure` is
    /// always hPa from Open-Meteo (the API has no inHg option) and is converted
    /// to inHg through the canonical unit seam. `precipitation` is the last
    /// INTERVAL's accumulation (inch over `current.interval` seconds, not a
    /// per-hour rate); we normalize it to in/hr via the interval before emitting
    /// RainIntensityInHr. `shortwave_radiation`/`uv_index` give cloud-only
    /// deployments a solar/UV reading. All of these emit as live_current=false
    /// forecast fills, so they never read as live station data in the engine.
    fn current_fields(&self) -> Option<(Vec<(WeatherField, f64)>, i64)> {
        let cur = self.current.as_ref()?;
        let units = self.current_units.as_ref();
        let mut fields: Vec<(WeatherField, f64)> = Vec::new();

        // Temperature (F). Convert only if the server declared Celsius.
        if let Some(t) = cur.temperature_2m {
            let t = if unit_is_celsius(units.and_then(|u| u.temperature_2m.as_deref())) {
                t * 9.0 / 5.0 + 32.0
            } else {
                t
            };
            fields.push((WeatherField::AirTempF, t));
        }
        // Dew point (F). Converted from Celsius only on a server unit drift,
        // exactly like temperature_2m (the dew_point unit echoes alongside it).
        if let Some(dp) = cur.dew_point_2m {
            let dp = if unit_is_celsius(units.and_then(|u| u.dew_point_2m.as_deref())) {
                dp * 9.0 / 5.0 + 32.0
            } else {
                dp
            };
            fields.push((WeatherField::DewPointF, dp));
        }
        if let Some(rh) = cur.relative_humidity_2m {
            fields.push((WeatherField::RhPct, rh));
        }
        // Apparent temperature -> feels-like. The merge recomputes feels-like
        // from temp/rh/wind, so we don't have a dedicated field for it; skipping
        // it keeps the merge's derivation authoritative. (Requested for parity
        // with the forecast hourly apparent_temperature, but not merged.)
        let _ = cur.apparent_temperature;
        if let Some(w) = cur.wind_speed_10m {
            let f = speed_to_mph_factor(units.and_then(|u| u.wind_speed_10m.as_deref()));
            fields.push((WeatherField::WindMph, w * f));
        }
        if let Some(g) = cur.wind_gusts_10m {
            let f = speed_to_mph_factor(units.and_then(|u| u.wind_gusts_10m.as_deref()));
            fields.push((WeatherField::WindGustMph, g * f));
        }
        if let Some(d) = cur.wind_direction_10m {
            fields.push((WeatherField::WindBearingDeg, d));
        }
        if let Some(p) = cur.surface_pressure {
            // Open-Meteo surface_pressure is hPa (no inHg unit option). Route
            // through the canonical unit seam instead of a hardcoded truncated
            // factor so the conversion matches every other adapter (and full
            // precision). Absent unit -> assume hPa (the API's only output).
            let unit = units
                .and_then(|u| u.surface_pressure.as_deref())
                .unwrap_or("hPa");
            let inhg =
                crate::sources::units::to_canonical(WeatherField::PressureInHg, p, Some(unit));
            fields.push((WeatherField::PressureInHg, inhg));
        }
        if let Some(pr) = cur.precipitation {
            // Requested precipitation_unit=inch; the current bucket is the last
            // INTERVAL's accumulation (Open-Meteo's `current.interval`, typically
            // 900s = 15 min), NOT a per-hour rate. Mislabeling a 15-min accumulation
            // as in/hr under-reports the live rain rate ~4x. Normalize to in/hr via
            // the interval; absent/0 interval -> assume the legacy hourly bucket
            // (value already in/hr). Convert mm -> inch first if the server declared it.
            let inch = crate::sources::units::to_canonical(
                WeatherField::RainIntensityInHr,
                pr,
                units.and_then(|u| u.precipitation.as_deref()),
            );
            let in_hr = if cur.interval > 0 {
                inch * 3600.0 / cur.interval as f64
            } else {
                inch
            };
            // Open-Meteo current (live_current=false) emits this as a forecast
            // FILL, so it can never read as live station rain (fix #5's
            // rain_live_epoch gate); it only fills the dashboard rate when no live
            // source owns rain.
            fields.push((WeatherField::RainIntensityInHr, in_hr));
        }
        // RainTodayIn: today's accumulated precipitation, read from the DAILY
        // rollup at the today index (past_days=PAST_DAYS shifts today to index
        // PAST_DAYS in the daily arrays: [t-3, t-2, t-1, t-0, ...]). This is the
        // field LAN stations (Tempest/Ecowitt) emit natively; before this, NO
        // cloud emitted rain_today_in, so pinning "rain_today_in -> Open-Meteo"
        // in the picker was a silent no-op (fix: now Open-Meteo owns it as a
        // live_current=false fill). precipitation_unit=inch is requested, so the
        // daily sum is already inches (canonical); no conversion.
        if let Some(rain_today) = self.daily.precipitation_sum.get(PAST_DAYS).copied() {
            fields.push((WeatherField::RainTodayIn, rain_today));
        }
        // Pop: current-hour precipitation probability (percent) from the current
        // block. Surfaced so the picker can pin "current Pop -> Open-Meteo".
        if let Some(pop) = cur.precipitation_probability {
            fields.push((WeatherField::Pop, pop));
        }
        // Shortwave radiation (W/m²) -> SolarWm2: gives a cloud-only deployment a
        // real solar reading so the Weather home isn't a moon-at-noon / dead Solar
        // panel. Open-Meteo reports W/m² (canonical), so no conversion.
        if let Some(s) = cur.shortwave_radiation {
            fields.push((WeatherField::SolarWm2, s));
        }
        // UV index (unitless, canonical) -> UvIndex, same cloud-only motivation.
        if let Some(uv) = cur.uv_index {
            fields.push((WeatherField::UvIndex, uv));
        }
        // Et0Today: today's FAO-56 reference ET0 (inches/day) from the daily
        // rollup at the today index (PAST_DAYS, same shift as RainTodayIn). The
        // current block has no ET0 scalar, so we read it from daily; a cloud-only
        // deployment gets a real Et0Today fill without a native ET0 station. The
        // value is a daily Option (gaps -> the day is skipped, not zero-filled).
        if let Some(Some(et0)) = self.daily.et0_fao_evapotranspiration.get(PAST_DAYS) {
            fields.push((WeatherField::Et0Today, *et0));
        }
        // weather_code / cloud_cover are not scalar merge fields; cloud_cover is
        // requested for parity but has no WeatherField, so it is not emitted.
        let _ = cur.cloud_cover;

        // Invariant (the list-collapse contract): every field emitted here is a
        // member of OPEN_METEO_CURRENT_FIELDS, the const runtime.rs returns as the
        // selectable set. A new push that forgets to extend the const (or a const
        // entry the emitter never produces) is the exact drift this collapse
        // removes; the debug assert catches it in tests/dev without a release cost.
        debug_assert!(
            fields
                .iter()
                .all(|(f, _)| OPEN_METEO_CURRENT_FIELDS.contains(f)),
            "current_fields emitted a field outside OPEN_METEO_CURRENT_FIELDS"
        );

        if fields.is_empty() {
            return None;
        }
        let at = if cur.time > 0 {
            cur.time
        } else {
            Utc::now().timestamp()
        };
        Some((fields, at))
    }
}

// Open-Meteo time fields are requested as `timeformat=unixtime`, so daily/hourly
// time and sunrise/sunset arrive as true epoch integers (the local-day-start
// instant under timezone=auto). No client-side timezone parsing, and no DST edge
// cases: the previous string parser appended ":00Z" to offset-less local strings
// and mis-dated every entry by the local UTC offset.

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_model_url_is_stable_and_includes_current_block() {
        // The exact URL the best_match refresher fetches. The `current=` block
        // (added so Open-Meteo feeds LIVE current conditions into the merge) sits
        // before `daily=`; the rest is unchanged from the pre-model-selection URL.
        // Matching imperial units (fahrenheit / mph / inch) are requested so the
        // current scalars need no conversion (we still honor the response units).
        let expected = concat!(
            "https://api.open-meteo.com/v1/forecast?latitude=28.5&longitude=-81.4",
            "&current=temperature_2m,relative_humidity_2m,dew_point_2m,apparent_temperature,",
            "wind_speed_10m,wind_gusts_10m,wind_direction_10m,surface_pressure,",
            "precipitation,precipitation_probability,weather_code,cloud_cover,shortwave_radiation,uv_index",
            "&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,",
            "precipitation_probability_max,wind_speed_10m_max,wind_gusts_10m_max,uv_index_max,",
            "et0_fao_evapotranspiration,sunrise,sunset",
            "&hourly=weather_code,temperature_2m,apparent_temperature,precipitation,",
            "precipitation_probability,wind_speed_10m,wind_direction_10m,",
            "relative_humidity_2m,cloud_cover",
            "&temperature_unit=fahrenheit&wind_speed_unit=mph&precipitation_unit=inch",
            "&past_days=3&forecast_days=7&forecast_hours=48&timezone=auto&timeformat=unixtime"
        );
        assert_eq!(forecast_url(28.5, -81.4, "best_match"), expected);
    }

    #[test]
    fn non_default_model_appends_models_param_only() {
        let base = forecast_url(48.14, 11.58, "best_match");
        let pinned = forecast_url(48.14, 11.58, "icon_seamless");
        assert_eq!(pinned, format!("{base}&models=icon_seamless"));
    }

    /// A representative Open-Meteo response with a `current` block in the units
    /// we request (imperial). Only the fields current_fields reads are checked.
    const CURRENT_SAMPLE: &str = r#"{
        "timezone": "America/New_York",
        "current_units": {
            "time": "unixtime",
            "temperature_2m": "°F",
            "relative_humidity_2m": "%",
            "dew_point_2m": "°F",
            "apparent_temperature": "°F",
            "wind_speed_10m": "mph",
            "wind_gusts_10m": "mph",
            "wind_direction_10m": "°",
            "surface_pressure": "hPa",
            "precipitation": "inch",
            "cloud_cover": "%"
        },
        "current": {
            "time": 1750000000,
            "interval": 900,
            "temperature_2m": 78.5,
            "relative_humidity_2m": 61.0,
            "dew_point_2m": 64.4,
            "apparent_temperature": 80.1,
            "wind_speed_10m": 9.0,
            "wind_gusts_10m": 17.0,
            "wind_direction_10m": 210.0,
            "surface_pressure": 1013.0,
            "precipitation": 0.02,
            "precipitation_probability": 40.0,
            "weather_code": 3,
            "cloud_cover": 75.0,
            "shortwave_radiation": 520.0,
            "uv_index": 6.5
        },
        "daily": {
            "time": [1, 2, 3, 4],
            "weather_code": [0, 0, 0, 0],
            "temperature_2m_max": [80, 80, 80, 80],
            "temperature_2m_min": [60, 60, 60, 60],
            "precipitation_sum": [0.0, 0.0, 0.0, 0.31],
            "precipitation_probability_max": [0, 0, 0, 0],
            "wind_speed_10m_max": [0, 0, 0, 0],
            "wind_gusts_10m_max": [0, 0, 0, 0],
            "uv_index_max": [0, 0, 0, 0],
            "et0_fao_evapotranspiration": [0.10, 0.11, 0.12, 0.18],
            "sunrise": [0, 0, 0, 0], "sunset": [0, 0, 0, 0]
        },
        "hourly": {
            "time": [], "weather_code": [], "temperature_2m": [],
            "apparent_temperature": [], "precipitation": [],
            "precipitation_probability": [], "wind_speed_10m": [],
            "wind_direction_10m": [], "relative_humidity_2m": [], "cloud_cover": []
        }
    }"#;

    #[test]
    fn current_block_maps_to_live_scalar_fields() {
        use crate::ports::weather_source::WeatherField as F;
        let raw: Raw = serde_json::from_str(CURRENT_SAMPLE).expect("sample parses");
        let (fields, at) = raw.current_fields().expect("a current block yields fields");
        let m: std::collections::HashMap<_, _> = fields.into_iter().collect();
        // Imperial units echoed -> no temp/wind conversion.
        assert_eq!(m.get(&F::AirTempF), Some(&78.5));
        assert_eq!(m.get(&F::RhPct), Some(&61.0));
        // dew_point_2m -> DewPointF (F echoed, no conversion).
        assert_eq!(m.get(&F::DewPointF), Some(&64.4));
        assert_eq!(m.get(&F::WindMph), Some(&9.0));
        assert_eq!(m.get(&F::WindGustMph), Some(&17.0));
        assert_eq!(m.get(&F::WindBearingDeg), Some(&210.0));
        // surface_pressure is always hPa from Open-Meteo -> inHg via to_canonical.
        let inhg = m.get(&F::PressureInHg).copied().unwrap();
        assert!((inhg - 29.913_89).abs() < 1e-3, "pressure inHg = {inhg}");
        // precipitation (0.02" over a 900s interval) -> normalized to in/hr:
        // 0.02 * 3600 / 900 = 0.08 in/hr (NOT mislabeled as 0.02 in/hr).
        let in_hr = m.get(&F::RainIntensityInHr).copied().unwrap();
        assert!((in_hr - 0.08).abs() < 1e-9, "rain in/hr = {in_hr}");
        // RainTodayIn: today's daily precipitation_sum at index PAST_DAYS (=3),
        // the field LAN stations emit; this is what makes the Rain->OM pin real.
        assert_eq!(m.get(&F::RainTodayIn), Some(&0.31));
        // precipitation_probability -> Pop (current-hour chance, percent).
        assert_eq!(m.get(&F::Pop), Some(&40.0));
        // shortwave_radiation -> SolarWm2 (W/m², no conversion).
        assert_eq!(m.get(&F::SolarWm2), Some(&520.0));
        // uv_index -> UvIndex (unitless).
        assert_eq!(m.get(&F::UvIndex), Some(&6.5));
        // Et0Today: today's daily et0_fao_evapotranspiration at index PAST_DAYS.
        assert_eq!(m.get(&F::Et0Today), Some(&0.18));
        // Stamped with the observation time, not "now".
        assert_eq!(at, 1_750_000_000);
        // apparent_temperature / cloud_cover / weather_code are not merge fields.
        assert!(!m.contains_key(&F::ForecastDaily));
        // Every emitted field is a member of OPEN_METEO_CURRENT_FIELDS (the
        // list-collapse invariant the picker relies on).
        for (f, _) in m.iter() {
            assert!(
                OPEN_METEO_CURRENT_FIELDS.contains(f),
                "emitted {f:?} not in OPEN_METEO_CURRENT_FIELDS"
            );
        }
    }

    #[test]
    fn current_block_honors_metric_response_units() {
        // Defensive: if Open-Meteo ever ignores our imperial request and echoes
        // metric units, current_fields converts from the DECLARED units (C, km/h,
        // hPa, mm) so the merge never gets the wrong magnitude.
        use crate::ports::weather_source::WeatherField as F;
        let json = r#"{
            "timezone": "UTC",
            "current_units": {
                "temperature_2m": "°C", "dew_point_2m": "°C",
                "wind_speed_10m": "km/h",
                "wind_gusts_10m": "km/h", "surface_pressure": "hPa",
                "precipitation": "mm"
            },
            "current": {
                "time": 1750000000,
                "temperature_2m": 25.0, "relative_humidity_2m": 50.0,
                "dew_point_2m": 0.0,
                "wind_speed_10m": 16.09344, "wind_gusts_10m": 32.18688,
                "wind_direction_10m": 90.0, "surface_pressure": 1000.0,
                "precipitation": 25.4
            },
            "daily": {"time": [], "weather_code": [], "temperature_2m_max": [],
                "temperature_2m_min": [], "precipitation_sum": [],
                "precipitation_probability_max": [], "wind_speed_10m_max": [],
                "wind_gusts_10m_max": [], "uv_index_max": [], "sunrise": [], "sunset": []},
            "hourly": {"time": [], "weather_code": [], "temperature_2m": [],
                "apparent_temperature": [], "precipitation": [],
                "precipitation_probability": [], "wind_speed_10m": [],
                "wind_direction_10m": [], "relative_humidity_2m": [], "cloud_cover": []}
        }"#;
        let raw: Raw = serde_json::from_str(json).unwrap();
        let (fields, _) = raw.current_fields().unwrap();
        let m: std::collections::HashMap<_, _> = fields.into_iter().collect();
        // 25 C -> 77 F.
        assert!((m.get(&F::AirTempF).unwrap() - 77.0).abs() < 1e-6);
        // dew_point_2m 0 C -> 32 F (converted exactly like temperature_2m).
        assert!((m.get(&F::DewPointF).unwrap() - 32.0).abs() < 1e-6);
        // 16.09344 km/h -> 10 mph.
        assert!((m.get(&F::WindMph).unwrap() - 10.0).abs() < 1e-4);
        // 32.18688 km/h -> 20 mph.
        assert!((m.get(&F::WindGustMph).unwrap() - 20.0).abs() < 1e-4);
        // 25.4 mm -> 1.0 inch.
        assert!((m.get(&F::RainIntensityInHr).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn precipitation_without_interval_is_treated_as_in_hr() {
        // Back-compat: a response with no `interval` (legacy hourly bucket) keeps
        // the value as in/hr (no 4x under-report from a missing divisor).
        use crate::ports::weather_source::WeatherField as F;
        let json = r#"{
            "timezone": "UTC",
            "current_units": {"precipitation": "inch"},
            "current": {"time": 1750000000, "precipitation": 0.10},
            "daily": {"time": [], "weather_code": [], "temperature_2m_max": [],
                "temperature_2m_min": [], "precipitation_sum": [],
                "precipitation_probability_max": [], "wind_speed_10m_max": [],
                "wind_gusts_10m_max": [], "uv_index_max": [], "sunrise": [], "sunset": []},
            "hourly": {"time": [], "weather_code": [], "temperature_2m": [],
                "apparent_temperature": [], "precipitation": [],
                "precipitation_probability": [], "wind_speed_10m": [],
                "wind_direction_10m": [], "relative_humidity_2m": [], "cloud_cover": []}
        }"#;
        let raw: Raw = serde_json::from_str(json).unwrap();
        let (fields, _) = raw.current_fields().unwrap();
        let m: std::collections::HashMap<_, _> = fields.into_iter().collect();
        assert_eq!(m.get(&F::RainIntensityInHr), Some(&0.10));
    }

    #[test]
    fn no_current_block_yields_none() {
        // A forecast-only response (no `current`) leaves the live merge untouched:
        // current_fields returns None, so the refresher emits no Observation.
        let json = r#"{
            "timezone": "UTC",
            "daily": {"time": [], "weather_code": [], "temperature_2m_max": [],
                "temperature_2m_min": [], "precipitation_sum": [],
                "precipitation_probability_max": [], "wind_speed_10m_max": [],
                "wind_gusts_10m_max": [], "uv_index_max": [], "sunrise": [], "sunset": []},
            "hourly": {"time": [], "weather_code": [], "temperature_2m": [],
                "apparent_temperature": [], "precipitation": [],
                "precipitation_probability": [], "wind_speed_10m": [],
                "wind_direction_10m": [], "relative_humidity_2m": [], "cloud_cover": []}
        }"#;
        let raw: Raw = serde_json::from_str(json).unwrap();
        assert!(raw.current_fields().is_none());
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

    #[test]
    fn open_meteo_current_fields_const_matches_full_emit_set() {
        // The list-collapse contract: with a FULLY populated current + daily
        // block, current_fields emits exactly OPEN_METEO_CURRENT_FIELDS (same set,
        // no extras, no omissions). runtime.rs returns this same const as the
        // selectable picker set, so capable == emitted == selectable. CURRENT_SAMPLE
        // populates every var the emitter reads.
        let raw: Raw = serde_json::from_str(CURRENT_SAMPLE).expect("sample parses");
        let (fields, _) = raw.current_fields().expect("a current block yields fields");
        let emitted: std::collections::HashSet<_> = fields.iter().map(|(f, _)| *f).collect();
        let declared: std::collections::HashSet<_> =
            OPEN_METEO_CURRENT_FIELDS.iter().copied().collect();
        assert_eq!(
            emitted, declared,
            "current_fields full-block emit set must equal OPEN_METEO_CURRENT_FIELDS"
        );
    }

    #[test]
    fn open_meteo_current_fields_const_is_the_expected_set() {
        // Pin the exact const contents so a drift here (or an out-of-band edit to
        // runtime.rs's mirrored arm) is caught. RainTodayIn is load-bearing: it is
        // what makes pinning rain_today_in -> Open-Meteo a real override (no cloud
        // emitted it before), so it MUST be present.
        use crate::ports::weather_source::WeatherField as F;
        let expected = [
            F::AirTempF,
            F::DewPointF,
            F::RhPct,
            F::WindMph,
            F::WindGustMph,
            F::WindBearingDeg,
            F::PressureInHg,
            F::RainIntensityInHr,
            F::RainTodayIn,
            F::Pop,
            F::SolarWm2,
            F::UvIndex,
            F::Et0Today,
        ];
        assert_eq!(OPEN_METEO_CURRENT_FIELDS, &expected[..]);
        assert!(OPEN_METEO_CURRENT_FIELDS.contains(&F::RainTodayIn));
    }
}
