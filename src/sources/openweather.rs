// OpenWeatherMap weather source, api.openweathermap.org "One Call API 3.0".
//
// Requires a paid API key (free tier covers 1000 calls/day = poll
// every ~90 seconds). Global coverage. Standard pick for users without
// a LAN station or a free regional service.
//
// Endpoint:
//   GET /data/3.0/onecall?lat={lat}&lon={lon}&appid={key}&units=imperial
//
// One Call returns current + minutely (1h) + hourly (48h) + daily (8d)
// in a single response. We emit live observation fields from `current`,
// a full ForecastSnapshot from `daily[]` + `hourly[]`, and reachability
// on success/failure.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, OpenWeatherConfig};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.openweathermap.org/data/3.0";
const POLL_INTERVAL: Duration = Duration::from_secs(10 * 60); // 10 min (free-tier safe)

pub struct OpenWeather {
    id: String,
    config: OpenWeatherConfig,
    location: Location,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct OneCallResponse {
    /// IANA timezone name for the requested point (e.g. "America/New_York").
    #[serde(default)]
    timezone: Option<String>,
    current: Option<CurrentBlock>,
    #[serde(default)]
    daily: Vec<DailyBlock>,
    #[serde(default)]
    hourly: Vec<HourlyBlock>,
}

#[derive(Debug, Deserialize)]
struct CurrentBlock {
    temp: Option<f64>,
    #[allow(dead_code)] // kept to mirror the API shape
    feels_like: Option<f64>,
    pressure: Option<f64>, // hPa
    humidity: Option<f64>,
    dew_point: Option<f64>,
    uvi: Option<f64>,
    wind_speed: Option<f64>, // mph (imperial)
    wind_gust: Option<f64>,
    wind_deg: Option<f64>,
    /// Rain volume for the last hour. Same shape as the hourly block:
    /// an object `{ "1h": <mm> }`. OWM reports rain in mm even under
    /// units=imperial, so this mm/h reading is converted to in/hr downstream.
    rain: Option<RainOneHour>,
}

#[derive(Debug, Deserialize)]
struct WeatherCond {
    /// OpenWeather condition code (2xx/3xx/5xx/6xx/7xx/80x). Mapped loosely
    /// to a WMO code for the glyph registry.
    id: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct DailyTemp {
    min: Option<f64>,
    max: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DailyBlock {
    dt: Option<i64>,
    sunrise: Option<i64>,
    sunset: Option<i64>,
    temp: Option<DailyTemp>,
    wind_speed: Option<f64>, // mph (imperial)
    wind_gust: Option<f64>,  // mph (imperial)
    pop: Option<f64>,        // 0..1
    uvi: Option<f64>,
    /// Daily precip accumulation. OWM reports rain in mm even on
    /// units=imperial, so this is converted to inches downstream.
    rain: Option<f64>,
    #[serde(default)]
    weather: Vec<WeatherCond>,
}

/// One Call rain object `{ "1h": <mm> }`. Shared by the `current` and
/// `hourly` blocks; OWM reports the value in mm even on units=imperial.
#[derive(Debug, Deserialize)]
struct RainOneHour {
    #[serde(rename = "1h")]
    one_h: Option<f64>, // mm even on units=imperial
}

#[derive(Debug, Deserialize)]
struct HourlyBlock {
    dt: Option<i64>,
    temp: Option<f64>,
    feels_like: Option<f64>,
    humidity: Option<f64>,
    clouds: Option<f64>,
    wind_speed: Option<f64>, // mph (imperial)
    wind_deg: Option<f64>,
    pop: Option<f64>, // 0..1
    rain: Option<RainOneHour>,
    #[serde(default)]
    weather: Vec<WeatherCond>,
}

/// Map an OpenWeather condition code (`weather[0].id`) to a WMO weather code
/// so it resolves through the shared glyph registry. Loose mapping; unknown
/// codes fall back to 0 (the UI has a glyph fallback for unmapped codes).
/// OpenWeather code reference: https://openweathermap.org/weather-conditions
fn owm_to_wmo(code: u32) -> u32 {
    match code {
        // 2xx Thunderstorm
        200..=202 | 230..=232 => 95, // thunderstorm with rain/drizzle
        210..=221 => 95,             // plain thunderstorm
        // 3xx Drizzle
        300 | 310 => 51, // light drizzle
        301 | 311 | 313 | 321 => 53,
        302 | 312 | 314 => 55, // heavy drizzle
        // 5xx Rain
        500 => 61, // light rain
        501 => 63, // moderate rain
        502..=504 => 65,
        511 => 66,       // freezing rain
        520 => 80,       // light shower rain
        521 => 81,       // shower rain
        522 | 531 => 82, // heavy / ragged shower rain
        // 6xx Snow
        600 | 620 => 71, // light snow / light snow showers
        601 | 621 => 73,
        602 | 622 => 75, // heavy snow / heavy snow showers
        611..=613 => 66, // sleet
        615..=616 => 67, // rain + snow
        // 7xx Atmosphere (mist/smoke/haze/fog/sand/dust/ash/squall/tornado)
        701 | 711 | 721 | 731 | 741 | 751 | 761 | 762 | 771 | 781 => 45,
        // 80x Clouds
        800 => 0,       // clear sky
        801 => 1,       // few clouds
        802 => 2,       // scattered clouds
        803 | 804 => 3, // broken / overcast clouds
        _ => 0,
    }
}

fn first_wmo(weather: &[WeatherCond]) -> u32 {
    weather
        .first()
        .and_then(|w| w.id)
        .map(owm_to_wmo)
        .unwrap_or(0)
}

/// Build a ForecastSnapshot from a parsed One Call response. Pulls the
/// timezone + daily/hourly arrays; `now_epoch` stamps last_refresh.
/// units=imperial already gives temps in F and wind in mph; rain is the
/// documented exception (mm), so it is divided by 25.4 → inches.
fn build_snapshot(resp: &OneCallResponse, now_epoch: i64) -> ForecastSnapshot {
    let timezone = resp.timezone.clone().unwrap_or_default();

    let daily: Vec<DailyEntry> = resp
        .daily
        .iter()
        .map(|d| {
            let (temp_min_f, temp_max_f) = d
                .temp
                .as_ref()
                .map(|t| (t.min.unwrap_or(0.0), t.max.unwrap_or(0.0)))
                .unwrap_or((0.0, 0.0));
            DailyEntry {
                time_epoch: d.dt.unwrap_or(0),
                weather_code: first_wmo(&d.weather),
                temp_max_f,
                temp_min_f,
                // OWM's daily block has no RH; filled from hourly by
                // backfill_daily_humidity below.
                humidity_pct: 0,
                // OWM rain is mm even under units=imperial.
                precip_sum_in: d.rain.unwrap_or(0.0) / 25.4,
                precip_probability_max: ((d.pop.unwrap_or(0.0) * 100.0).round() as i64)
                    .clamp(0, 100) as u32,
                wind_max_mph: d.wind_speed.unwrap_or(0.0),
                wind_gust_max_mph: d.wind_gust.unwrap_or(0.0),
                uv_index_max: d.uvi.unwrap_or(0.0),
                sunrise_epoch: d.sunrise.unwrap_or(0),
                sunset_epoch: d.sunset.unwrap_or(0),
            }
        })
        .collect();

    let hourly: Vec<HourlyEntry> = resp
        .hourly
        .iter()
        .map(|h| HourlyEntry {
            time_epoch: h.dt.unwrap_or(0),
            weather_code: first_wmo(&h.weather),
            temp_f: h.temp.unwrap_or(0.0),
            apparent_temp_f: h.feels_like.unwrap_or(0.0),
            // OWM rain.1h is mm even under units=imperial.
            precip_in: h.rain.as_ref().and_then(|r| r.one_h).unwrap_or(0.0) / 25.4,
            precip_probability: ((h.pop.unwrap_or(0.0) * 100.0).round() as i64).clamp(0, 100)
                as u32,
            wind_mph: h.wind_speed.unwrap_or(0.0),
            wind_dir_deg: (h.wind_deg.unwrap_or(0.0).round() as i64).rem_euclid(360) as u32,
            humidity_pct: (h.humidity.unwrap_or(0.0).round() as i64).clamp(0, 100) as u32,
            cloud_cover_pct: (h.clouds.unwrap_or(0.0).round() as i64).clamp(0, 100) as u32,
        })
        .collect();

    let mut snap = ForecastSnapshot {
        last_refresh_epoch: now_epoch,
        source_reachable: true,
        source_label: "OpenWeather".to_string(),
        timezone,
        daily,
        past_daily: vec![],
        hourly,
    };
    // Pair each day's high temp with THAT day's afternoon humidity (hourly).
    snap.backfill_daily_humidity();
    snap
}

impl OpenWeather {
    pub fn new(id: impl Into<String>, config: OpenWeatherConfig, location: Location) -> Self {
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

    async fn fetch(&self) -> anyhow::Result<OneCallResponse> {
        let url = format!(
            "{API_BASE}/onecall?lat={lat}&lon={lon}&appid={key}&units=imperial",
            lat = self.location.lat,
            lon = self.location.lon,
            key = self.config.api_key,
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
}

#[async_trait]
impl WeatherSource for OpenWeather {
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
        fields.insert(WeatherField::ForecastDaily);
        fields.insert(WeatherField::ForecastHourly);
        SourceCaps {
            live_current: false,
            hourly_forecast_hours: 48,
            daily_forecast_days: 8,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            // Forecast: solid commercial source.
            WeatherField::ForecastDaily | WeatherField::ForecastHourly => 50,
            // Live values: model-derived, low vs any LAN station.
            WeatherField::AirTempF
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
        info!(source_id = %self.id, "OpenWeather source started");
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
                            if let Some(c) = &resp.current {
                                let mut fields = Vec::new();
                                if let Some(v) = c.temp { fields.push((WeatherField::AirTempF, v)); }
                                if let Some(v) = c.dew_point { fields.push((WeatherField::DewPointF, v)); }
                                if let Some(v) = c.humidity { fields.push((WeatherField::RhPct, v)); }
                                // OWM returns pressure in hPa even on imperial units.
                                if let Some(v) = c.pressure { fields.push((WeatherField::PressureInHg, v * 0.02953)); }
                                if let Some(v) = c.wind_speed { fields.push((WeatherField::WindMph, v)); }
                                if let Some(v) = c.wind_gust { fields.push((WeatherField::WindGustMph, v)); }
                                if let Some(v) = c.wind_deg { fields.push((WeatherField::WindBearingDeg, v)); }
                                if let Some(v) = c.uvi { fields.push((WeatherField::UvIndex, v)); }
                                // OWM current.rain["1h"] is mm over the last hour (mm/h) even
                                // on units=imperial; / 25.4 -> in/hr for RainIntensityInHr.
                                if let Some(v) = c.rain.as_ref().and_then(|r| r.one_h) {
                                    fields.push((WeatherField::RainIntensityInHr, v / 25.4));
                                }
                                if !fields.is_empty() {
                                    debug!(source_id = %self.id, fields_n = fields.len(), "OpenWeather updated");
                                    let _ = bus.send(SourceEvent::Observation {
                                        source_id: self.id.clone(),
                                        fields,
                                        at_epoch: chrono::Utc::now().timestamp(),
                                    });
                                }
                            }
                            // Forecast: build + emit a full snapshot from daily[]/hourly[].
                            if !resp.daily.is_empty() || !resp.hourly.is_empty() {
                                let now = chrono::Utc::now().timestamp();
                                let snapshot = build_snapshot(&resp, now);
                                debug!(
                                    source_id = %self.id,
                                    daily_n = snapshot.daily.len(),
                                    hourly_n = snapshot.hourly.len(),
                                    "OpenWeather forecast snapshot",
                                );
                                let _ = bus.send(SourceEvent::Forecast {
                                    source_id: self.id.clone(),
                                    snapshot,
                                    at_epoch: now,
                                });
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "OpenWeather fetch failed");
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
                        info!(source_id = %self.id, "OpenWeather shutdown");
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

    fn ow_test() -> OpenWeather {
        OpenWeather::new(
            "ow",
            OpenWeatherConfig {
                api_key: "test".into(),
            },
            Location {
                lat: 40.7128,
                lon: -74.006,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_include_forecast_and_uv() {
        let o = ow_test();
        let caps = o.capabilities();
        assert!(caps.fields.contains(&WeatherField::ForecastDaily));
        assert!(caps.fields.contains(&WeatherField::UvIndex));
        assert!(caps.fields.contains(&WeatherField::RainIntensityInHr));
        assert_eq!(caps.hourly_forecast_hours, 48);
    }

    #[test]
    fn current_rain_one_h_maps_mm_per_hour_to_in_per_hr() {
        // One Call 3.0 current block carries rain as `{ "1h": <mm> }`,
        // a mm/h reading even under units=imperial. run() divides by 25.4
        // to emit RainIntensityInHr (in/hr).
        let json = r#"{
            "timezone": "America/New_York",
            "current": {
                "temp": 60.0,
                "humidity": 90,
                "rain": { "1h": 5.08 }
            }
        }"#;

        let resp: OneCallResponse = serde_json::from_str(json).expect("parse current sample");
        let c = resp.current.expect("current block present");
        let rain_mm_h = c
            .rain
            .and_then(|r| r.one_h)
            .expect("current.rain[1h] present");
        // 5.08 mm/h / 25.4 = 0.2 in/hr.
        let rain_in_hr = rain_mm_h / 25.4;
        assert!((rain_in_hr - 0.2).abs() < 0.001);
    }

    #[test]
    fn current_without_rain_omits_intensity() {
        // No `rain` key on the current block -> None, so no RainIntensityInHr
        // observation is pushed (a dry hour reads absent, not 0).
        let json = r#"{ "current": { "temp": 72.0 } }"#;
        let resp: OneCallResponse = serde_json::from_str(json).expect("parse current sample");
        let c = resp.current.expect("current block present");
        assert!(c.rain.and_then(|r| r.one_h).is_none());
    }

    #[test]
    fn owm_condition_codes_map_to_wmo() {
        assert_eq!(owm_to_wmo(800), 0); // clear
        assert_eq!(owm_to_wmo(802), 2); // scattered clouds
        assert_eq!(owm_to_wmo(804), 3); // overcast
        assert_eq!(owm_to_wmo(500), 61); // light rain
        assert_eq!(owm_to_wmo(211), 95); // thunderstorm
        assert_eq!(owm_to_wmo(741), 45); // fog
        assert_eq!(owm_to_wmo(601), 73); // snow
        assert_eq!(owm_to_wmo(999999), 0); // unknown -> 0
    }

    #[test]
    fn parse_forecast_arrays_maps_units() {
        // Minimal One Call 3.0 response: units=imperial -> temp F, wind mph,
        // but rain stays mm; pop is 0..1.
        let json = r#"{
            "timezone": "America/New_York",
            "current": { "temp": 70.0 },
            "daily": [
                {
                    "dt": 1700000000,
                    "sunrise": 1699970000,
                    "sunset": 1700010000,
                    "temp": { "min": 55.0, "max": 78.5 },
                    "wind_speed": 9.0,
                    "wind_gust": 18.0,
                    "wind_deg": 200,
                    "pop": 0.6,
                    "uvi": 7.2,
                    "rain": 25.4,
                    "weather": [ { "id": 500 } ]
                }
            ],
            "hourly": [
                {
                    "dt": 1700000400,
                    "temp": 68.0,
                    "feels_like": 66.0,
                    "humidity": 55,
                    "clouds": 40,
                    "wind_speed": 6.0,
                    "wind_deg": 370,
                    "pop": 0.3,
                    "rain": { "1h": 2.54 },
                    "weather": [ { "id": 802 } ]
                }
            ]
        }"#;

        let resp: OneCallResponse = serde_json::from_str(json).expect("parse One Call sample");
        let snap = build_snapshot(&resp, 1700001234);

        assert_eq!(snap.timezone, "America/New_York");
        assert_eq!(snap.last_refresh_epoch, 1700001234);
        assert!(snap.source_reachable);
        assert!(snap.past_daily.is_empty());

        assert_eq!(snap.daily.len(), 1);
        let d0 = &snap.daily[0];
        assert_eq!(d0.time_epoch, 1700000000);
        assert_eq!(d0.weather_code, 61); // 500 -> WMO light rain
        assert!((d0.temp_max_f - 78.5).abs() < 0.001); // already F
        assert!((d0.temp_min_f - 55.0).abs() < 0.001);
        assert!((d0.precip_sum_in - 1.0).abs() < 0.001); // 25.4 mm -> 1 in
        assert_eq!(d0.precip_probability_max, 60); // 0.6 -> 60%
        assert!((d0.wind_max_mph - 9.0).abs() < 0.001);
        assert!((d0.wind_gust_max_mph - 18.0).abs() < 0.001);
        assert!((d0.uv_index_max - 7.2).abs() < 0.001);
        assert_eq!(d0.sunrise_epoch, 1699970000);
        assert_eq!(d0.sunset_epoch, 1700010000);

        assert_eq!(snap.hourly.len(), 1);
        let h0 = &snap.hourly[0];
        assert_eq!(h0.time_epoch, 1700000400);
        assert_eq!(h0.weather_code, 2); // 802 -> WMO scattered
        assert!((h0.temp_f - 68.0).abs() < 0.001); // already F
        assert!((h0.apparent_temp_f - 66.0).abs() < 0.001);
        assert!((h0.precip_in - 0.1).abs() < 0.001); // 2.54 mm -> 0.1 in
        assert_eq!(h0.precip_probability, 30); // 0.3 -> 30%
        assert!((h0.wind_mph - 6.0).abs() < 0.001);
        assert_eq!(h0.wind_dir_deg, 10); // 370 wrapped -> 10
        assert_eq!(h0.humidity_pct, 55);
        assert_eq!(h0.cloud_cover_pct, 40);
    }
}
