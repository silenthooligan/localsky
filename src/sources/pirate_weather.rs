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
use tracing::debug;

use crate::config::schema::{Location, PirateWeatherConfig};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::poll::{run_polling, Poll};

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
    /// Liquid rain accumulation, inches (units=us, version=2). The older
    /// precipAccumulation mixes rain with physical snow/ice depth.
    #[serde(rename = "liquidAccumulation")]
    liquid_accumulation: Option<f64>,
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
        // Keyed API: the derived per-install User-Agent is the right identity.
        let client = crate::net::client(Duration::from_secs(15));
        Self {
            id: id.into(),
            config,
            location,
            client,
        }
    }

    async fn fetch(&self) -> anyhow::Result<ForecastResponse> {
        let url = format!(
            "{API_BASE}/{key}/{lat},{lon}?units=us&version=2",
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

    /// Map the `currently` block onto canonical fields. units=us means the
    /// scalars are already imperial; humidity and precipProbability are
    /// Dark-Sky 0..1 fractions and are scaled to percent here. Pure, so the
    /// conversion is unit-testable without a wall clock.
    fn current_fields(c: &CurrentBlock) -> Vec<(WeatherField, f64)> {
        let mut fields = Vec::new();
        if let Some(v) = c.temperature {
            fields.push((WeatherField::AirTempF, v));
        }
        if let Some(v) = c.dew_point {
            fields.push((WeatherField::DewPointF, v));
        }
        // Dark-Sky-compatible: humidity is 0..1, convert to %.
        if let Some(v) = c.humidity {
            fields.push((WeatherField::RhPct, v * 100.0));
        }
        if let Some(v) = c.pressure {
            fields.push((WeatherField::PressureInHg, v * 0.02953));
        }
        if let Some(v) = c.wind_speed {
            fields.push((WeatherField::WindMph, v));
        }
        if let Some(v) = c.wind_gust {
            fields.push((WeatherField::WindGustMph, v));
        }
        if let Some(v) = c.wind_bearing {
            fields.push((WeatherField::WindBearingDeg, v));
        }
        if let Some(v) = c.uv_index {
            fields.push((WeatherField::UvIndex, v));
        }
        if let Some(v) = c.precip_intensity {
            fields.push((WeatherField::RainIntensityInHr, v));
        }
        // 0..1 precip probability -> 0..100 percent. Pirate's precip is a
        // model blend (HRRR/NBM/GEFS), NOT RTMA-RU radar, so this Pop is a
        // forecast value, not a measured nowcast.
        if let Some(v) = c.precip_probability {
            fields.push((WeatherField::Pop, (v * 100.0).clamp(0.0, 100.0)));
        }
        fields
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
                        // Pirate Weather stamps 00:00 local.
                        day_marker: crate::engine::clock::DayMarker::inside_local_day(
                            d.time.unwrap_or(0),
                        ),
                        weather_code: icon_to_wmo(d.icon.as_deref()),
                        temp_max_f: d.temperature_high.filter(|t| t.is_finite()),
                        temp_min_f: d.temperature_low.filter(|t| t.is_finite()),
                        // Daily block carries no usable per-day RH; filled from
                        // hourly by backfill_daily_humidity below.
                        humidity_pct: None,
                        // Day's liquid accumulation (inches). precipIntensity is
                        // a peak RATE (in/hr), not a daily total, so it is NOT a
                        // valid fallback here; absent accumulation remains unknown.
                        precip_sum_in: d
                            .liquid_accumulation
                            .filter(|v| crate::forecast::precip::valid_amount(*v)),
                        // Absent probability stays None, not a fabricated 0%.
                        precip_probability_max: d.precip_probability.map(frac_to_pct),
                        wind_max_mph: d.wind_speed.filter(|w| w.is_finite() && *w >= 0.0),
                        wind_gust_max_mph: d.wind_gust.unwrap_or(0.0),
                        uv_index_max: d.uv_index.unwrap_or(0.0),
                        sunrise_epoch: d.sunrise_time.unwrap_or(0),
                        sunset_epoch: d.sunset_time.unwrap_or(0),
                        ..Default::default()
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
                        temp_f: h.temperature.filter(|t| t.is_finite()),
                        apparent_temp_f: h.apparent_temperature.unwrap_or(0.0),
                        precip_in: h
                            .precip_intensity
                            .filter(|v| crate::forecast::precip::valid_amount(*v)),
                        precip_probability: h.precip_probability.map(frac_to_pct),
                        wind_mph: h.wind_speed.filter(|w| w.is_finite() && *w >= 0.0),
                        wind_dir_deg: h
                            .wind_bearing
                            .map(|b| b.round().clamp(0.0, 360.0) as u32)
                            .unwrap_or(0),
                        humidity_pct: h
                            .humidity
                            .filter(|rh| rh.is_finite() && (0.0..=1.0).contains(rh))
                            .map(frac_to_pct),
                        cloud_cover_pct: h.cloud_cover.map(frac_to_pct).unwrap_or(0),
                        ..Default::default()
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
            ..Default::default()
        };
        // Pair each day's high temp with THAT day's afternoon humidity (hourly).
        snap.backfill_daily_humidity(crate::timeutil::deployment_calendar());
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

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        // The loop (tick, missed-tick policy, fetch metric, reachability
        // edges in both directions, shutdown) is `run_polling`'s; this
        // closure is one poll: fetch, map the current block, build the
        // forecast snapshot from the SAME response.
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "PirateWeather",
            POLL_INTERVAL,
            bus,
            shutdown,
            |s: Arc<Self>| async move {
                let resp = s.fetch().await?;
                let now = chrono::Utc::now().timestamp();
                let fields = resp
                    .currently
                    .as_ref()
                    .map(Self::current_fields)
                    .unwrap_or_default();
                let mut poll = Poll::observation(&s.id, fields, now);
                // Forecast: build + publish the daily/hourly snapshot from the
                // SAME response the currently block came from.
                let snapshot = Self::build_snapshot(&resp, now);
                if !snapshot.daily.is_empty() || !snapshot.hourly.is_empty() {
                    debug!(
                        source_id = %s.id,
                        daily_n = snapshot.daily.len(),
                        hourly_n = snapshot.hourly.len(),
                        "PirateWeather forecast updated"
                    );
                    poll = poll.with(SourceEvent::Forecast {
                        source_id: s.id.clone(),
                        snapshot,
                        at_epoch: now,
                    });
                }
                Ok(poll)
            },
        )
        .await
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
        // currently.precipProbability is Dark-Sky 0..1; current_fields scales
        // it to 0..100 percent on emit.
        let resp: ForecastResponse = serde_json::from_str(SAMPLE).expect("parse sample");
        let c = resp.currently.expect("currently block present");
        let raw = c
            .precip_probability
            .expect("currently precipProbability present");
        assert!((raw - 0.42).abs() < 1e-6, "raw 0..1 fraction preserved");
        let fields = PirateWeather::current_fields(&c);
        let pop_pct = fields
            .iter()
            .find(|(f, _)| *f == WeatherField::Pop)
            .map(|(_, v)| *v)
            .expect("Pop emitted from the currently block");
        assert!((pop_pct - 42.0).abs() < 1e-6, "0.42 -> 42% Pop");
        assert!(
            fields.contains(&(WeatherField::AirTempF, 75.0)),
            "temperature passes through unconverted (units=us)"
        );
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
                    "liquidAccumulation": 0.12,
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
    fn missing_or_invalid_temperatures_do_not_fabricate_freeze() {
        let mut resp: ForecastResponse = serde_json::from_str(SAMPLE).unwrap();
        for invalid in [None, Some(f64::NAN), Some(f64::INFINITY)] {
            let day = &mut resp.daily.as_mut().unwrap().data[0];
            day.temperature_high = invalid;
            day.temperature_low = invalid;
            resp.hourly.as_mut().unwrap().data[0].temperature = invalid;
            let snapshot = PirateWeather::build_snapshot(&resp, 1_700_000_123);
            assert_eq!(snapshot.daily[0].temp_max_f, None);
            assert_eq!(snapshot.daily[0].temp_min_f, None);
            assert_eq!(snapshot.hourly[0].temp_f, None);
            assert!(
                snapshot.daily[0].precip_sum_in.unwrap() > 0.0,
                "other evidence remains"
            );
        }
        resp.daily.as_mut().unwrap().data[0].temperature_high = Some(0.0);
        assert_eq!(
            PirateWeather::build_snapshot(&resp, 1).daily[0].temp_max_f,
            Some(0.0)
        );
    }

    #[test]
    fn wind_and_humidity_gaps_cannot_be_calm_or_dry_evidence() {
        let mut resp: ForecastResponse = serde_json::from_str(SAMPLE).unwrap();
        for invalid in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            resp.daily.as_mut().unwrap().data[0].wind_speed = invalid;
            let hour = &mut resp.hourly.as_mut().unwrap().data[0];
            hour.wind_speed = invalid;
            hour.humidity = invalid;
            let snapshot = PirateWeather::build_snapshot(&resp, 1_700_000_123);
            assert_eq!(snapshot.daily[0].wind_max_mph, None);
            assert_eq!(snapshot.hourly[0].wind_mph, None);
            assert_eq!(snapshot.hourly[0].humidity_pct, None);
        }
        resp.hourly.as_mut().unwrap().data[0].humidity = Some(1.1);
        assert_eq!(
            PirateWeather::build_snapshot(&resp, 1).hourly[0].humidity_pct,
            None
        );
        resp.daily.as_mut().unwrap().data[0].wind_speed = Some(0.0);
        let hour = &mut resp.hourly.as_mut().unwrap().data[0];
        hour.wind_speed = Some(0.0);
        hour.humidity = Some(0.0);
        let snapshot = PirateWeather::build_snapshot(&resp, 1);
        assert_eq!(snapshot.daily[0].wind_max_mph, Some(0.0));
        assert_eq!(snapshot.hourly[0].wind_mph, Some(0.0));
        assert_eq!(snapshot.hourly[0].humidity_pct, Some(0));
    }

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
        assert_eq!(
            d.day_marker,
            crate::engine::clock::DayMarker::inside_local_day(1_700_000_000)
        );
        assert_eq!(d.sunrise_epoch, 1_700_022_000);
        assert_eq!(d.sunset_epoch, 1_700_060_400);
        assert!(
            (d.temp_max_f.unwrap() - 82.4).abs() < 1e-6,
            "high already in F"
        );
        assert!(
            (d.temp_min_f.unwrap() - 61.2).abs() < 1e-6,
            "low already in F"
        );
        assert!((d.precip_sum_in.unwrap() - 0.12).abs() < 1e-6);
        assert_eq!(d.precip_probability_max, Some(35)); // 0.35 -> 35%
        assert!((d.wind_max_mph.unwrap() - 9.0).abs() < 1e-6);
        assert!((d.wind_gust_max_mph - 21.5).abs() < 1e-6);
        assert!((d.uv_index_max - 7.0).abs() < 1e-6);
        assert_eq!(d.weather_code, 63); // "rain" -> WMO 63

        // hourly[0]
        assert_eq!(snap.hourly.len(), 1);
        let h = &snap.hourly[0];
        assert_eq!(h.time_epoch, 1_700_001_000);
        assert!((h.temp_f.unwrap() - 70.0).abs() < 1e-6, "temp already in F");
        assert!((h.apparent_temp_f - 72.5).abs() < 1e-6);
        assert!((h.precip_in.unwrap() - 0.04).abs() < 1e-6);
        assert_eq!(h.precip_probability, Some(50)); // 0.5 -> 50%
        assert!((h.wind_mph.unwrap() - 6.0).abs() < 1e-6);
        assert_eq!(h.wind_dir_deg, 180);
        assert_eq!(h.humidity_pct, Some(66)); // 0.66 -> 66%
        assert_eq!(h.cloud_cover_pct, 90); // 0.9 -> 90%
        assert_eq!(h.weather_code, 2); // "partly-cloudy-day" -> WMO 2
    }
    #[test]
    fn physical_snow_accumulation_and_missing_sentinels_are_not_liquid_rain() {
        let mut resp: ForecastResponse = serde_json::from_value(serde_json::json!({
            "daily": { "data": [{"time": 1700000000, "precipAccumulation": 10.0}] },
            "hourly": { "data": [{"time": 1700000000}] }
        }))
        .unwrap();
        assert_eq!(
            PirateWeather::build_snapshot(&resp, 1000).daily[0].precip_sum_in,
            None
        );
        for value in [
            None,
            Some(-999.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(0.0),
        ] {
            resp.daily.as_mut().unwrap().data[0].liquid_accumulation = value;
            resp.hourly.as_mut().unwrap().data[0].precip_intensity = value;
            let snapshot = PirateWeather::build_snapshot(&resp, 1000);
            let expected = if value == Some(0.0) { Some(0.0) } else { None };
            assert_eq!(snapshot.daily[0].precip_sum_in, expected);
            assert_eq!(snapshot.hourly[0].precip_in, expected);
        }
    }
}
