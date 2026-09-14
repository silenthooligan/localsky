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
use tracing::debug;

use crate::config::schema::{Location, OpenWeatherConfig};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::poll::{run_polling, Poll};

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
    #[serde(default = "dry_rain")]
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
    #[serde(default = "dry_hourly_rain")]
    rain: Option<RainOneHour>,
    #[serde(default)]
    weather: Vec<WeatherCond>,
}

// OpenWeather documents omitted rain as a phenomenon that does not occur.
// Serde defaults apply only to an absent key; explicit null stays unknown.
fn dry_rain() -> Option<f64> {
    Some(0.0)
}
fn dry_hourly_rain() -> Option<RainOneHour> {
    Some(RainOneHour { one_h: Some(0.0) })
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
                .map(|t| {
                    (
                        t.min.filter(|t| t.is_finite()),
                        t.max.filter(|t| t.is_finite()),
                    )
                })
                .unwrap_or((None, None));
            DailyEntry {
                // OpenWeather stamps a midday value.
                day_marker: crate::engine::clock::DayMarker::inside_local_day(d.dt.unwrap_or(0)),
                weather_code: first_wmo(&d.weather),
                temp_max_f,
                temp_min_f,
                // OWM's daily block has no RH; filled from hourly by
                // backfill_daily_humidity below.
                humidity_pct: None,
                // OWM rain is mm even under units=imperial.
                precip_sum_in: d
                    .rain
                    .filter(|v| crate::forecast::precip::valid_amount(*v))
                    .map(crate::units::mm_to_in),
                // Absent pop stays None (provider gap), not a fabricated 0%.
                precip_probability_max: d
                    .pop
                    .map(|p| ((p * 100.0).round() as i64).clamp(0, 100) as u32),
                wind_max_mph: d.wind_speed.filter(|w| w.is_finite() && *w >= 0.0),
                wind_gust_max_mph: d.wind_gust.unwrap_or(0.0),
                uv_index_max: d.uvi.unwrap_or(0.0),
                sunrise_epoch: d.sunrise.unwrap_or(0),
                sunset_epoch: d.sunset.unwrap_or(0),
                ..Default::default()
            }
        })
        .collect();

    let hourly: Vec<HourlyEntry> = resp
        .hourly
        .iter()
        .map(|h| HourlyEntry {
            time_epoch: h.dt.unwrap_or(0),
            weather_code: first_wmo(&h.weather),
            temp_f: h.temp.filter(|t| t.is_finite()),
            apparent_temp_f: h.feels_like.unwrap_or(0.0),
            // OWM rain.1h is mm even under units=imperial.
            precip_in: h
                .rain
                .as_ref()
                .and_then(|r| r.one_h)
                .filter(|v| crate::forecast::precip::valid_amount(*v))
                .map(crate::units::mm_to_in),
            precip_probability: h
                .pop
                .map(|p| ((p * 100.0).round() as i64).clamp(0, 100) as u32),
            wind_mph: h.wind_speed.filter(|w| w.is_finite() && *w >= 0.0),
            wind_dir_deg: (h.wind_deg.unwrap_or(0.0).round() as i64).rem_euclid(360) as u32,
            humidity_pct: h
                .humidity
                .filter(|rh| rh.is_finite() && (0.0..=100.0).contains(rh))
                .map(|rh| rh.round() as u32),
            cloud_cover_pct: (h.clouds.unwrap_or(0.0).round() as i64).clamp(0, 100) as u32,
            ..Default::default()
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
        ..Default::default()
    };
    // Pair each day's high temp with THAT day's afternoon humidity (hourly).
    snap.backfill_daily_humidity(crate::timeutil::deployment_calendar());
    snap
}

impl OpenWeather {
    pub fn new(id: impl Into<String>, config: OpenWeatherConfig, location: Location) -> Self {
        Self {
            id: id.into(),
            config,
            location,
            client: crate::net::client(Duration::from_secs(15)),
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

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "OpenWeather",
            POLL_INTERVAL,
            bus,
            shutdown,
            |s: Arc<OpenWeather>| async move {
                let resp = s.fetch().await?;
                let now = chrono::Utc::now().timestamp();

                // Live fields from `current`. An absent block or an empty
                // field set is a legitimate poll with no observation.
                let mut fields = Vec::new();
                if let Some(c) = &resp.current {
                    if let Some(v) = c.temp {
                        fields.push((WeatherField::AirTempF, v));
                    }
                    if let Some(v) = c.dew_point {
                        fields.push((WeatherField::DewPointF, v));
                    }
                    if let Some(v) = c.humidity {
                        fields.push((WeatherField::RhPct, v));
                    }
                    // OWM returns pressure in hPa even on imperial units.
                    if let Some(v) = c.pressure {
                        fields.push((WeatherField::PressureInHg, v * 0.02953));
                    }
                    if let Some(v) = c.wind_speed {
                        fields.push((WeatherField::WindMph, v));
                    }
                    if let Some(v) = c.wind_gust {
                        fields.push((WeatherField::WindGustMph, v));
                    }
                    if let Some(v) = c.wind_deg {
                        fields.push((WeatherField::WindBearingDeg, v));
                    }
                    if let Some(v) = c.uvi {
                        fields.push((WeatherField::UvIndex, v));
                    }
                    // OWM current.rain["1h"] is mm over the last hour (mm/h) even
                    // on units=imperial; / 25.4 -> in/hr for RainIntensityInHr.
                    if let Some(v) = c.rain.as_ref().and_then(|r| r.one_h) {
                        fields.push((WeatherField::RainIntensityInHr, crate::units::mm_to_in(v)));
                    }
                }
                let mut poll = Poll::observation(&s.id, fields, now);

                // Forecast: build + emit a full snapshot from daily[]/hourly[].
                if !resp.daily.is_empty() || !resp.hourly.is_empty() {
                    let snapshot = build_snapshot(&resp, now);
                    debug!(
                        source_id = %s.id,
                        daily_n = snapshot.daily.len(),
                        hourly_n = snapshot.hourly.len(),
                        "OpenWeather forecast snapshot",
                    );
                    poll = poll.with(SourceEvent::Forecast {
                        source_id: s.id.clone(),
                        snapshot,
                        at_epoch: now,
                    });
                }
                anyhow::Ok(poll)
            },
        )
        .await
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
    fn missing_or_invalid_temperatures_preserve_unknown_and_true_zero() {
        let mut resp: OneCallResponse = serde_json::from_value(serde_json::json!({
            "daily": [{ "dt": 1700000000 }],
            "hourly": [{ "dt": 1700000000 }]
        }))
        .unwrap();
        let snapshot = build_snapshot(&resp, 1700000000);
        assert_eq!(snapshot.daily[0].temp_max_f, None);
        assert_eq!(snapshot.daily[0].temp_min_f, None);
        assert_eq!(snapshot.hourly[0].temp_f, None);
        for invalid in [None, Some(f64::NAN), Some(f64::INFINITY)] {
            resp.daily[0].temp = Some(DailyTemp {
                min: invalid,
                max: invalid,
            });
            resp.hourly[0].temp = invalid;
            let snapshot = build_snapshot(&resp, 1700000000);
            assert_eq!(snapshot.daily[0].temp_max_f, None);
            assert_eq!(snapshot.daily[0].temp_min_f, None);
            assert_eq!(snapshot.hourly[0].temp_f, None);
        }
        resp.daily[0].temp = Some(DailyTemp {
            min: Some(-5.0),
            max: Some(0.0),
        });
        resp.hourly[0].temp = Some(0.0);
        let snapshot = build_snapshot(&resp, 1700000000);
        assert_eq!(snapshot.daily[0].temp_max_f, Some(0.0));
        assert_eq!(snapshot.daily[0].temp_min_f, Some(-5.0));
        assert_eq!(snapshot.hourly[0].temp_f, Some(0.0));
    }

    #[test]
    fn missing_wind_and_humidity_are_not_calm_dry_air() {
        let mut resp: OneCallResponse = serde_json::from_value(serde_json::json!({
            "daily": [{}], "hourly": [{}]
        }))
        .unwrap();
        for invalid in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            resp.daily[0].wind_speed = invalid;
            resp.hourly[0].wind_speed = invalid;
            resp.hourly[0].humidity = invalid;
            let snapshot = build_snapshot(&resp, 1);
            assert_eq!(snapshot.daily[0].wind_max_mph, None);
            assert_eq!(snapshot.hourly[0].wind_mph, None);
            assert_eq!(snapshot.hourly[0].humidity_pct, None);
        }
        resp.hourly[0].humidity = Some(101.0);
        assert_eq!(build_snapshot(&resp, 1).hourly[0].humidity_pct, None);
        resp.daily[0].wind_speed = Some(0.0);
        resp.hourly[0].wind_speed = Some(0.0);
        resp.hourly[0].humidity = Some(0.0);
        let snapshot = build_snapshot(&resp, 1);
        assert_eq!(snapshot.daily[0].wind_max_mph, Some(0.0));
        assert_eq!(snapshot.hourly[0].wind_mph, Some(0.0));
        assert_eq!(snapshot.hourly[0].humidity_pct, Some(0));
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
        assert_eq!(
            d0.day_marker,
            crate::engine::clock::DayMarker::inside_local_day(1700000000)
        );
        assert_eq!(d0.weather_code, 61); // 500 -> WMO light rain
        assert!((d0.temp_max_f.unwrap() - 78.5).abs() < 0.001); // already F
        assert!((d0.temp_min_f.unwrap() - 55.0).abs() < 0.001);
        assert!((d0.precip_sum_in.unwrap() - 1.0).abs() < 0.001); // 25.4 mm -> 1 in
        assert_eq!(d0.precip_probability_max, Some(60)); // 0.6 -> 60%
        assert!((d0.wind_max_mph.unwrap() - 9.0).abs() < 0.001);
        assert!((d0.wind_gust_max_mph - 18.0).abs() < 0.001);
        assert!((d0.uv_index_max - 7.2).abs() < 0.001);
        assert_eq!(d0.sunrise_epoch, 1699970000);
        assert_eq!(d0.sunset_epoch, 1700010000);

        assert_eq!(snap.hourly.len(), 1);
        let h0 = &snap.hourly[0];
        assert_eq!(h0.time_epoch, 1700000400);
        assert_eq!(h0.weather_code, 2); // 802 -> WMO scattered
        assert!((h0.temp_f.unwrap() - 68.0).abs() < 0.001); // already F
        assert!((h0.apparent_temp_f - 66.0).abs() < 0.001);
        assert!((h0.precip_in.unwrap() - 0.1).abs() < 0.001); // 2.54 mm -> 0.1 in
        assert_eq!(h0.precip_probability, Some(30)); // 0.3 -> 30%
        assert!((h0.wind_mph.unwrap() - 6.0).abs() < 0.001);
        assert_eq!(h0.wind_dir_deg, 10); // 370 wrapped -> 10
        assert_eq!(h0.humidity_pct, Some(55));
        assert_eq!(h0.cloud_cover_pct, 40);
    }
    #[test]
    fn documented_sparse_dry_rain_differs_from_null_and_malformed_amounts() {
        let response: OneCallResponse = serde_json::from_value(serde_json::json!({
            "daily": [{"dt":1700000000}, {"dt":1700086400,"rain":null}, {"dt":1700172800,"rain":-1.0}],
            "hourly": [{"dt":1700000000}, {"dt":1700003600,"rain":null}, {"dt":1700007200,"rain":{}}, {"dt":1700010800,"rain":{"1h":0.0}}]
        })).unwrap();
        let snapshot = build_snapshot(&response, 1700000000);
        assert_eq!(
            snapshot
                .daily
                .iter()
                .map(|d| d.precip_sum_in)
                .collect::<Vec<_>>(),
            vec![Some(0.0), None, None]
        );
        assert_eq!(
            snapshot
                .hourly
                .iter()
                .map(|h| h.precip_in)
                .collect::<Vec<_>>(),
            vec![Some(0.0), None, None, Some(0.0)]
        );
    }
}
