// PirateWeather source, api.pirateweather.net, the open Dark-Sky-API
// replacement. Same response shape as the original Dark Sky API, free
// tier 10k/day. Useful for users who built tooling against Dark Sky
// before Apple shut it down.
//
// Endpoint:
//   GET /forecast/{key}/{lat},{lon}?units=us
//
// The `currently` block has live values; `daily` + `hourly` blocks have
// the forecast arrays. units=us -> values are already canonical imperial
// (degF, mph, in, in/hr); probability/humidity/cloudCover are 0..1 and
// get scaled to percent.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, PirateWeatherConfig};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.pirateweather.net/forecast";
const POLL_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub struct PirateWeather {
    id: String,
    config: PirateWeatherConfig,
    location: Location,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    currently: Option<CurrentBlock>,
    /// IANA timezone string, e.g. "America/New_York".
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    daily: Option<DailyBlock>,
    #[serde(default)]
    hourly: Option<HourlyBlock>,
}

#[derive(Debug, Deserialize)]
struct CurrentBlock {
    temperature: Option<f64>,
    #[serde(rename = "apparentTemperature")]
    #[allow(dead_code)] // kept to mirror the API shape
    apparent_temperature: Option<f64>,
    #[serde(rename = "dewPoint")]
    dew_point: Option<f64>,
    humidity: Option<f64>, // 0..1 in Dark-Sky-compatible APIs
    pressure: Option<f64>, // hPa
    #[serde(rename = "windSpeed")]
    wind_speed: Option<f64>,
    #[serde(rename = "windGust")]
    wind_gust: Option<f64>,
    #[serde(rename = "windBearing")]
    wind_bearing: Option<f64>,
    #[serde(rename = "uvIndex")]
    uv_index: Option<f64>,
    #[serde(rename = "precipIntensity")]
    precip_intensity: Option<f64>, // in/hr (units=us)
    // Probability of precip for the current block, 0..1 (Dark-Sky-compatible).
    // IMPORTANT: in CONUS/Canada the `currently` block draws temperature,
    // dewPoint, humidity and wind from the RTMA-RU analysis (good current
    // scalars), but the PRECIPITATION fields (precipIntensity, precipProbability)
    // are a model blend (HRRR_SubH / NBM / GEFS ensemble per Pirate upstream),
    // NOT the RTMA-RU radar+station analysis. So this Pop is a forecast value,
    // not a measurement. Scaled to 0..100 percent on emit.
    #[serde(rename = "precipProbability")]
    precip_probability: Option<f64>, // 0..1
}

#[derive(Debug, Deserialize)]
struct DailyBlock {
    #[serde(default)]
    data: Vec<DailyDatum>,
}

#[derive(Debug, Deserialize)]
struct DailyDatum {
    time: Option<i64>, // unix seconds (00:00 local for the day)
    #[serde(rename = "sunriseTime")]
    sunrise_time: Option<i64>,
    #[serde(rename = "sunsetTime")]
    sunset_time: Option<i64>,
    #[serde(rename = "temperatureHigh")]
    temperature_high: Option<f64>,
    #[serde(rename = "temperatureLow")]
    temperature_low: Option<f64>,
    #[serde(rename = "precipProbability")]
    precip_probability: Option<f64>, // 0..1
    /// Liquid-equivalent accumulation for the day, inches (units=us).
    #[serde(rename = "precipAccumulation")]
    precip_accumulation: Option<f64>,
    // Daily precipIntensity (in/hr peak rate) is intentionally NOT read: it's a
    // rate, not a daily total, so it's not a valid precip_sum_in fallback.
    #[serde(rename = "windSpeed")]
    wind_speed: Option<f64>, // mph
    #[serde(rename = "windGust")]
    wind_gust: Option<f64>, // mph
    #[serde(rename = "uvIndex")]
    uv_index: Option<f64>,
    icon: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HourlyBlock {
    #[serde(default)]
    data: Vec<HourlyDatum>,
}

#[derive(Debug, Deserialize)]
struct HourlyDatum {
    time: Option<i64>, // unix seconds
    temperature: Option<f64>,
    #[serde(rename = "apparentTemperature")]
    apparent_temperature: Option<f64>,
    #[serde(rename = "precipProbability")]
    precip_probability: Option<f64>, // 0..1
    #[serde(rename = "precipIntensity")]
    precip_intensity: Option<f64>, // in/hr (units=us)
    #[serde(rename = "windSpeed")]
    wind_speed: Option<f64>, // mph
    #[serde(rename = "windBearing")]
    wind_bearing: Option<f64>, // deg
    humidity: Option<f64>, // 0..1
    #[serde(rename = "cloudCover")]
    cloud_cover: Option<f64>, // 0..1
    icon: Option<String>,
}

/// Map a Dark-Sky / PirateWeather `icon` string to the nearest WMO weather
/// code (the UI's glyph table keys on WMO). Dark Sky's icon set is coarse, so
/// this is intentionally loose: unknown / absent icons fall back to 0, which
/// the UI renders with a generic-cloud glyph fallback. We do NOT block on a
/// perfect WMO map here.
fn icon_to_wmo(icon: Option<&str>) -> u32 {
    match icon.unwrap_or("") {
        "clear-day" | "clear-night" | "clear" => 0,
        "partly-cloudy-day" | "partly-cloudy-night" | "partly-cloudy" => 2,
        "cloudy" => 3,
        "fog" => 45,
        "drizzle" => 51,
        "sleet" | "freezing-rain" | "freezing-drizzle" => 66,
        "rain" => 63,
        "snow" | "flurries" => 73,
        "thunderstorm" | "tstorm" => 95,
        "hail" => 96,
        // wind / breezy / dangerous-wind / tornado / smoke / haze / mist and
        // anything unrecognized: no clean WMO equivalent -> 0 (glyph fallback).
        _ => 0,
    }
}

/// Convert a 0..1 Dark-Sky probability/fraction into a clamped 0..100 percent.
fn frac_to_pct(v: f64) -> u32 {
    (v * 100.0).round().clamp(0.0, 100.0) as u32
}

impl PirateWeather {
    pub fn new(id: impl Into<String>, config: PirateWeatherConfig, location: Location) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            config,
            location,
            client,
        }
    }

    async fn fetch(&self) -> anyhow::Result<ForecastResponse> {
        let url = format!(
            "{API_BASE}/{key}/{lat},{lon}?units=us",
            key = self.config.api_key,
            lat = self.location.lat,
            lon = self.location.lon,
        );
        Ok(self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Build a ForecastSnapshot from a parsed PirateWeather response. Pure +
    /// deterministic (the caller stamps `now`), so it is unit-testable without
    /// network or a wall clock. units=us means values are already canonical
    /// imperial; only the 0..1 fractions need scaling.
    fn build_snapshot(resp: &ForecastResponse, now: i64) -> ForecastSnapshot {
        let daily = resp
            .daily
            .as_ref()
            .map(|b| {
                b.data
                    .iter()
                    .map(|d| DailyEntry {
                        time_epoch: d.time.unwrap_or(0),
                        weather_code: icon_to_wmo(d.icon.as_deref()),
                        temp_max_f: d.temperature_high.unwrap_or(0.0),
                        temp_min_f: d.temperature_low.unwrap_or(0.0),
                        // Daily block carries no usable per-day RH; filled from
                        // hourly by backfill_daily_humidity below.
                        humidity_pct: 0,
                        // Day's liquid accumulation (inches). precipIntensity is
                        // a peak RATE (in/hr), not a daily total, so it is NOT a
                        // valid fallback here; default to 0 when accumulation is
                        // absent rather than conflating a rate with a sum.
                        precip_sum_in: d.precip_accumulation.unwrap_or(0.0),
                        precip_probability_max: d.precip_probability.map(frac_to_pct).unwrap_or(0),
                        wind_max_mph: d.wind_speed.unwrap_or(0.0),
                        wind_gust_max_mph: d.wind_gust.unwrap_or(0.0),
                        uv_index_max: d.uv_index.unwrap_or(0.0),
                        sunrise_epoch: d.sunrise_time.unwrap_or(0),
                        sunset_epoch: d.sunset_time.unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let hourly = resp
            .hourly
            .as_ref()
            .map(|b| {
                b.data
                    .iter()
                    .map(|h| HourlyEntry {
                        time_epoch: h.time.unwrap_or(0),
                        weather_code: icon_to_wmo(h.icon.as_deref()),
                        temp_f: h.temperature.unwrap_or(0.0),
                        apparent_temp_f: h.apparent_temperature.unwrap_or(0.0),
                        precip_in: h.precip_intensity.unwrap_or(0.0),
                        precip_probability: h.precip_probability.map(frac_to_pct).unwrap_or(0),
                        wind_mph: h.wind_speed.unwrap_or(0.0),
                        wind_dir_deg: h
                            .wind_bearing
                            .map(|b| b.round().clamp(0.0, 360.0) as u32)
                            .unwrap_or(0),
                        humidity_pct: h.humidity.map(frac_to_pct).unwrap_or(0),
                        cloud_cover_pct: h.cloud_cover.map(frac_to_pct).unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut snap = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            source_label: "Pirate Weather".to_string(),
            timezone: resp.timezone.clone().unwrap_or_default(),
            daily,
            past_daily: vec![],
            hourly,
        };
        // Pair each day's high temp with THAT day's afternoon humidity (hourly).
        snap.backfill_daily_humidity();
        snap
    }
}

#[async_trait]
impl WeatherSource for PirateWeather {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::DewPointF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::PressureInHg);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindGustMph);
        fields.insert(WeatherField::WindBearingDeg);
        fields.insert(WeatherField::UvIndex);
        fields.insert(WeatherField::RainIntensityInHr);
        // Current-block precip probability. NOTE: Pirate's precip fields are a
        // model blend (HRRR/NBM/GEFS), not RTMA-RU radar, so this is a forecast
        // Pop, not a measured nowcast.
        fields.insert(WeatherField::Pop);
        fields.insert(WeatherField::ForecastDaily);
        fields.insert(WeatherField::ForecastHourly);
        SourceCaps {
            live_current: false,
            hourly_forecast_hours: 48,
            daily_forecast_days: 7,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            WeatherField::ForecastDaily | WeatherField::ForecastHourly => 50,
            // Pirate's current temp/dewpoint/humidity/wind come from the RTMA-RU
            // analysis (good current scalars, kept at 25). But its precipitation
            // (RainIntensityInHr, Pop) is a model blend (HRRR/NBM/GEFS), NOT
            // radar, so it is a forecast, not a measurement. It is pinned to the
            // model tier (25) so it can never outrank a real measured or radar
            // rain signal (gauge ~80-100, MRMS radar QPE, NWS observation).
            WeatherField::Pop
            | WeatherField::AirTempF
            | WeatherField::DewPointF
            | WeatherField::RhPct
            | WeatherField::PressureInHg
            | WeatherField::WindMph
            | WeatherField::WindGustMph
            | WeatherField::WindBearingDeg
            | WeatherField::UvIndex
            | WeatherField::RainIntensityInHr => 25,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "PirateWeather source started");
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match self.fetch().await {
                        Ok(resp) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            if let Some(c) = &resp.currently {
                                let mut fields = Vec::new();
                                if let Some(v) = c.temperature { fields.push((WeatherField::AirTempF, v)); }
                                if let Some(v) = c.dew_point { fields.push((WeatherField::DewPointF, v)); }
                                // Dark-Sky-compatible: humidity is 0..1, convert to %.
                                if let Some(v) = c.humidity { fields.push((WeatherField::RhPct, v * 100.0)); }
                                if let Some(v) = c.pressure { fields.push((WeatherField::PressureInHg, v * 0.02953)); }
                                if let Some(v) = c.wind_speed { fields.push((WeatherField::WindMph, v)); }
                                if let Some(v) = c.wind_gust { fields.push((WeatherField::WindGustMph, v)); }
                                if let Some(v) = c.wind_bearing { fields.push((WeatherField::WindBearingDeg, v)); }
                                if let Some(v) = c.uv_index { fields.push((WeatherField::UvIndex, v)); }
                                if let Some(v) = c.precip_intensity { fields.push((WeatherField::RainIntensityInHr, v)); }
                                // 0..1 precip probability -> 0..100 percent. Pirate's
                                // precip is a model blend (HRRR/NBM/GEFS), NOT RTMA-RU
                                // radar, so this Pop is a forecast value, not a
                                // measured nowcast.
                                if let Some(v) = c.precip_probability { fields.push((WeatherField::Pop, (v * 100.0).clamp(0.0, 100.0))); }
                                if !fields.is_empty() {
                                    debug!(source_id = %self.id, fields_n = fields.len(), "PirateWeather updated");
                                    let _ = bus.send(SourceEvent::Observation {
                                        source_id: self.id.clone(),
                                        fields,
                                        at_epoch: chrono::Utc::now().timestamp(),
                                    });
                                }
                            }
                            // Forecast: build + publish the daily/hourly snapshot
                            // from the SAME response the currently block came from.
                            let now = chrono::Utc::now().timestamp();
                            let snapshot = Self::build_snapshot(&resp, now);
                            if !snapshot.daily.is_empty() || !snapshot.hourly.is_empty() {
                                debug!(
                                    source_id = %self.id,
                                    daily_n = snapshot.daily.len(),
                                    hourly_n = snapshot.hourly.len(),
                                    "PirateWeather forecast updated"
                                );
                                let _ = bus.send(SourceEvent::Forecast {
                                    source_id: self.id.clone(),
                                    snapshot,
                                    at_epoch: now,
                                });
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "PirateWeather fetch failed");
                            if last_reachable != Some(false) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: false,
                                });
                                last_reachable = Some(false);
                            }
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source_id = %self.id, "PirateWeather shutdown");
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw_test() -> PirateWeather {
        PirateWeather::new(
            "pw",
            PirateWeatherConfig {
                api_key: "test".into(),
            },
            Location {
                lat: 30.0,
                lon: -81.0,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_include_rain_intensity() {
        let p = pw_test();
        assert!(p
            .capabilities()
            .fields
            .contains(&WeatherField::RainIntensityInHr));
    }

    #[test]
    fn caps_include_current_pop() {
        // The `currently` block emits a Pop (a model-blend forecast value, not
        // a measured nowcast), so the capability must advertise it for the
        // per-field picker + catalog.
        let p = pw_test();
        assert!(p.capabilities().fields.contains(&WeatherField::Pop));
    }

    #[test]
    fn current_pop_is_model_tier() {
        // Pirate's precip (Pop, RainIntensityInHr) is a model blend (HRRR/NBM/
        // GEFS), NOT RTMA-RU radar, so it sits at the model-scalar tier (25),
        // never above it. It must not outrank a real measured or radar rain.
        let p = pw_test();
        assert_eq!(p.priority(WeatherField::Pop), 25);
        assert_eq!(
            p.priority(WeatherField::Pop),
            p.priority(WeatherField::UvIndex)
        );
        assert_eq!(
            p.priority(WeatherField::Pop),
            p.priority(WeatherField::RainIntensityInHr)
        );
    }

    #[test]
    fn current_block_parses_pop_as_percent() {
        // currently.precipProbability is Dark-Sky 0..1; the run loop scales it
        // to 0..100 percent on emit (same conversion exercised here).
        let resp: ForecastResponse = serde_json::from_str(SAMPLE).expect("parse sample");
        let c = resp.currently.expect("currently block present");
        let raw = c
            .precip_probability
            .expect("currently precipProbability present");
        assert!((raw - 0.42).abs() < 1e-6, "raw 0..1 fraction preserved");
        let pop_pct = (raw * 100.0).clamp(0.0, 100.0);
        assert!((pop_pct - 42.0).abs() < 1e-6, "0.42 -> 42% Pop");
    }

    // Small literal sample of a units=us PirateWeather response. Drives the
    // parse + snapshot mapping without network or a wall clock.
    const SAMPLE: &str = r#"{
        "timezone": "America/New_York",
        "currently": { "temperature": 75.0, "precipProbability": 0.42 },
        "daily": {
            "data": [
                {
                    "time": 1700000000,
                    "sunriseTime": 1700022000,
                    "sunsetTime": 1700060400,
                    "temperatureHigh": 82.4,
                    "temperatureLow": 61.2,
                    "precipProbability": 0.35,
                    "precipAccumulation": 0.12,
                    "windSpeed": 9.0,
                    "windGust": 21.5,
                    "uvIndex": 7,
                    "icon": "rain"
                }
            ]
        },
        "hourly": {
            "data": [
                {
                    "time": 1700001000,
                    "temperature": 70.0,
                    "apparentTemperature": 72.5,
                    "precipProbability": 0.5,
                    "precipIntensity": 0.04,
                    "windSpeed": 6.0,
                    "windBearing": 180,
                    "humidity": 0.66,
                    "cloudCover": 0.9,
                    "icon": "partly-cloudy-day"
                }
            ]
        }
    }"#;

    #[test]
    fn parses_daily_and_hourly_forecast() {
        let resp: ForecastResponse = serde_json::from_str(SAMPLE).expect("parse sample");
        let snap = PirateWeather::build_snapshot(&resp, 1_700_000_123);

        assert_eq!(snap.timezone, "America/New_York");
        assert!(snap.source_reachable);
        assert_eq!(snap.last_refresh_epoch, 1_700_000_123);
        assert!(snap.past_daily.is_empty());

        // daily[0]
        assert_eq!(snap.daily.len(), 1);
        let d = &snap.daily[0];
        assert_eq!(d.time_epoch, 1_700_000_000);
        assert_eq!(d.sunrise_epoch, 1_700_022_000);
        assert_eq!(d.sunset_epoch, 1_700_060_400);
        assert!((d.temp_max_f - 82.4).abs() < 1e-6, "high already in F");
        assert!((d.temp_min_f - 61.2).abs() < 1e-6, "low already in F");
        assert!((d.precip_sum_in - 0.12).abs() < 1e-6);
        assert_eq!(d.precip_probability_max, 35); // 0.35 -> 35%
        assert!((d.wind_max_mph - 9.0).abs() < 1e-6);
        assert!((d.wind_gust_max_mph - 21.5).abs() < 1e-6);
        assert!((d.uv_index_max - 7.0).abs() < 1e-6);
        assert_eq!(d.weather_code, 63); // "rain" -> WMO 63

        // hourly[0]
        assert_eq!(snap.hourly.len(), 1);
        let h = &snap.hourly[0];
        assert_eq!(h.time_epoch, 1_700_001_000);
        assert!((h.temp_f - 70.0).abs() < 1e-6, "temp already in F");
        assert!((h.apparent_temp_f - 72.5).abs() < 1e-6);
        assert!((h.precip_in - 0.04).abs() < 1e-6);
        assert_eq!(h.precip_probability, 50); // 0.5 -> 50%
        assert!((h.wind_mph - 6.0).abs() < 1e-6);
        assert_eq!(h.wind_dir_deg, 180);
        assert_eq!(h.humidity_pct, 66); // 0.66 -> 66%
        assert_eq!(h.cloud_cover_pct, 90); // 0.9 -> 90%
        assert_eq!(h.weather_code, 2); // "partly-cloudy-day" -> WMO 2
    }
}
