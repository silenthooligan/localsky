// MetNorway (met.no) weather source, api.met.no/weatherapi/locationforecast.
//
// Free, no API key. Global coverage. Requires a descriptive User-Agent
// per met.no terms of service. The compact endpoint returns ~9 days of
// hourly forecast in a single response, generous for free data.
//
// Endpoint:
//   GET /weatherapi/locationforecast/2.0/compact?lat={lat}&lon={lon}
//
// The compact response gives per-timestep `air_temperature`,
// `air_pressure_at_sea_level`, `relative_humidity`, `wind_speed`,
// `precipitation_amount` (in mm), and a `symbol_code` for the
// next_1_hours / next_6_hours / next_12_hours summary. No native
// probability-of-precipitation field; it has to be derived from the
// symbol code (and met.no compact has no POP at all -> left 0).
//
// All compact values are METRIC: temperatures in C, wind in m/s,
// precipitation in mm, times in UTC ISO8601. We convert to LocalSky's
// canonical imperial (degF, mph, inches) before publishing.
//
// HOURLY: one entry per timeseries step, anchored at "now", capped at 48.
// DAILY: timeseries grouped by UTC calendar day (met.no returns UTC),
// aggregated into temp max/min, precip sum, peak wind. POP/UV/sunrise/
// sunset are unavailable in compact and stay at Default (0). Capped at 7.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, MetNorwayConfig};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.met.no/weatherapi/locationforecast/2.0";
const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 min
const MAX_HOURLY: usize = 48;
const MAX_DAILY: usize = 7;

pub struct MetNorway {
    id: String,
    #[allow(dead_code)]
    // user_agent is consumed at construction; kept for parity with other sources
    config: MetNorwayConfig,
    location: Location,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    properties: ForecastProperties,
}

#[derive(Debug, Deserialize)]
struct ForecastProperties {
    timeseries: Vec<TimeStep>,
}

#[derive(Debug, Deserialize)]
struct TimeStep {
    /// ISO8601 UTC timestamp for this step, e.g. "2026-06-24T12:00:00Z".
    time: String,
    data: TimeStepData,
}

#[derive(Debug, Deserialize)]
struct TimeStepData {
    instant: InstantBlock,
    #[serde(rename = "next_1_hours")]
    next_1_hours: Option<NextBlock>,
}

#[derive(Debug, Deserialize)]
struct InstantBlock {
    details: InstantDetails,
}

#[derive(Debug, Deserialize)]
struct InstantDetails {
    air_temperature: Option<f64>,
    air_pressure_at_sea_level: Option<f64>,
    relative_humidity: Option<f64>,
    wind_speed: Option<f64>,
    wind_from_direction: Option<f64>,
    cloud_area_fraction: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct NextBlock {
    details: NextDetails,
    summary: Option<NextSummary>,
}

#[derive(Debug, Deserialize)]
struct NextDetails {
    precipitation_amount: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct NextSummary {
    /// Met.no symbol code text, e.g. "cloudy", "rain", "clearsky_day".
    symbol_code: Option<String>,
}

/// Map a met.no compact `symbol_code` text to a loose WMO weather code.
///
/// Met.no symbol codes carry a `_day` / `_night` / `_polartwilight`
/// suffix on the clear/partly-cloudy variants; we strip those and match
/// on the base condition. This is intentionally approximate: the UI has
/// a glyph fallback for code 0, so anything we can't classify maps to 0
/// rather than guessing wrong. WMO buckets used:
///   0 clear, 1 mostly clear, 2 partly cloudy, 3 overcast,
///   45 fog, 51 drizzle, 61 rain, 71 snow, 80 showers,
///   85 snow showers, 95 thunderstorm.
fn symbol_to_wmo(symbol: &str) -> u32 {
    // Strip the day/night/twilight suffix met.no appends to some codes.
    let base = symbol
        .trim_end_matches("_day")
        .trim_end_matches("_night")
        .trim_end_matches("_polartwilight");
    // Thunder variants ("rainandthunder", "heavyrainshowersandthunder", ...).
    if base.contains("thunder") {
        return 95;
    }
    if base.contains("sleet") {
        // Freezing rain / sleet bucket.
        return 66;
    }
    if base.contains("snowshowers") {
        return 85;
    }
    if base.contains("snow") {
        return 71;
    }
    if base.contains("rainshowers") {
        return 80;
    }
    if base.contains("rain") {
        return 61;
    }
    match base {
        "clearsky" => 0,
        "fair" => 1,
        "partlycloudy" => 2,
        "cloudy" => 3,
        "fog" => 45,
        "lightrain" | "drizzle" => 51,
        _ => 0,
    }
}

/// Parse an ISO8601 UTC timestamp to epoch seconds; None on malformed input.
fn iso8601_to_epoch(ts: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp())
}

/// True if a met.no `symbol_code` denotes any form of precipitation
/// (rain/sleet/snow/showers/thunder/drizzle), used to synthesize POP.
fn symbol_is_precip(code: &str) -> bool {
    ["rain", "sleet", "snow", "showers", "thunder", "drizzle"]
        .iter()
        .any(|k| code.contains(k))
}

/// Met.no compact is a DETERMINISTIC forecast with no probability of
/// precipitation. Synthesize a coarse POP so the engine's amount x probability
/// rain gates aren't permanently zeroed: a forecast precip amount means precip
/// is expected (100%); a precip-class symbol with no amount is a lighter signal
/// (50%); otherwise 0.
fn synth_pop(precip_in: f64, symbol_code: Option<&str>) -> u32 {
    if precip_in > 0.0 {
        100
    } else if symbol_code.is_some_and(symbol_is_precip) {
        50
    } else {
        0
    }
}

/// (year, ordinal) of an epoch in the given tz (UTC when None), for grouping
/// timeseries steps by the user's LOCAL calendar day instead of UTC (which
/// mis-buckets "today" for western-hemisphere users and drifts the daily date).
fn local_day_key(epoch: i64, tz: Option<chrono_tz::Tz>) -> (i32, u32) {
    use chrono::Datelike;
    let utc = DateTime::<Utc>::from_timestamp(epoch, 0).unwrap_or_default();
    match tz {
        Some(tz) => {
            let l = utc.with_timezone(&tz);
            (l.year(), l.ordinal())
        }
        None => (utc.year(), utc.ordinal()),
    }
}

/// Epoch of local noon for a (year, ordinal) day, used as the daily anchor so
/// the consumer's weekday/date label matches the user's wall clock. Falls back
/// to `fallback` if the date or local time can't be resolved.
fn local_noon_epoch(key: (i32, u32), tz: Option<chrono_tz::Tz>, fallback: i64) -> i64 {
    use chrono::{NaiveDate, TimeZone};
    let Some(noon) = NaiveDate::from_yo_opt(key.0, key.1).and_then(|d| d.and_hms_opt(12, 0, 0))
    else {
        return fallback;
    };
    match tz {
        Some(tz) => tz
            .from_local_datetime(&noon)
            .single()
            .map(|dt| dt.timestamp())
            .unwrap_or(fallback),
        None => Utc
            .from_local_datetime(&noon)
            .single()
            .map(|dt| dt.timestamp())
            .unwrap_or(fallback),
    }
}

/// Convert a timeseries (already-deserialized compact response) into a
/// canonical-imperial ForecastSnapshot. Pure + deterministic given
/// `now_epoch`, so the unit test can exercise it without a clock or
/// network.
fn build_snapshot(resp: &ForecastResponse, lat: f64, lon: f64, now_epoch: i64) -> ForecastSnapshot {
    let steps = &resp.properties.timeseries;
    // Resolve the deployment's local tz so daily windows match the user's wall
    // clock (met.no stamps everything UTC + supplies no tz string).
    let tz: Option<chrono_tz::Tz> =
        crate::timeutil::tz_name_for(lat, lon).and_then(|n| n.parse().ok());
    let tz_name = tz.map(|t| t.name().to_string()).unwrap_or_default();

    // ---- HOURLY: one row per step, canonical imperial, capped. ----
    let mut hourly: Vec<HourlyEntry> = Vec::new();
    for step in steps.iter().take(MAX_HOURLY) {
        let Some(time_epoch) = iso8601_to_epoch(&step.time) else {
            continue;
        };
        let d = &step.data.instant.details;
        let next = step.data.next_1_hours.as_ref();
        let symbol = next
            .and_then(|n| n.summary.as_ref())
            .and_then(|s| s.symbol_code.as_deref());
        let weather_code = symbol.map(symbol_to_wmo).unwrap_or(0);
        let precip_in = next
            .and_then(|n| n.details.precipitation_amount)
            .map(|mm| mm / 25.4) // mm -> in
            .unwrap_or(0.0);
        hourly.push(HourlyEntry {
            time_epoch,
            weather_code,
            temp_f: d.air_temperature.map(c_to_f).unwrap_or(0.0),
            // Compact has no apparent/"feels-like" temperature.
            apparent_temp_f: 0.0,
            precip_in,
            // Synthesized from precip presence (compact has no real POP).
            precip_probability: synth_pop(precip_in, symbol),
            wind_mph: d.wind_speed.map(ms_to_mph).unwrap_or(0.0),
            wind_dir_deg: d.wind_from_direction.map(|x| x.round() as u32).unwrap_or(0),
            humidity_pct: d.relative_humidity.map(|x| x.round() as u32).unwrap_or(0),
            cloud_cover_pct: d.cloud_area_fraction.map(|x| x.round() as u32).unwrap_or(0),
        });
    }

    // ---- DAILY: group steps by LOCAL calendar day, aggregate. ----
    // Accumulator keyed by local (year, ordinal-day), preserving first-seen order.
    struct DayAgg {
        temp_max_f: f64,
        temp_min_f: f64,
        precip_sum_in: f64,
        pop_max: u32,
        wind_max_mph: f64,
        // Dominant weather: take the worst (highest WMO) seen that day, a
        // crude "most significant condition" proxy since compact has no
        // daily summary.
        weather_code: u32,
        seen_temp: bool,
    }
    let mut order: Vec<(i32, u32)> = Vec::new();
    let mut days: std::collections::HashMap<(i32, u32), DayAgg> = std::collections::HashMap::new();

    for step in steps {
        let Some(time_epoch) = iso8601_to_epoch(&step.time) else {
            continue;
        };
        let key = local_day_key(time_epoch, tz);

        let d = &step.data.instant.details;
        let next = step.data.next_1_hours.as_ref();
        let symbol = next
            .and_then(|n| n.summary.as_ref())
            .and_then(|s| s.symbol_code.as_deref());
        let precip_in = next
            .and_then(|n| n.details.precipitation_amount)
            .map(|mm| mm / 25.4)
            .unwrap_or(0.0);
        let step_code = symbol.map(symbol_to_wmo).unwrap_or(0);
        let step_pop = synth_pop(precip_in, symbol);
        let temp_f = d.air_temperature.map(c_to_f);
        let wind_mph = d.wind_speed.map(ms_to_mph).unwrap_or(0.0);

        let agg = days.entry(key).or_insert_with(|| {
            order.push(key);
            DayAgg {
                temp_max_f: f64::MIN,
                temp_min_f: f64::MAX,
                precip_sum_in: 0.0,
                pop_max: 0,
                wind_max_mph: 0.0,
                weather_code: 0,
                seen_temp: false,
            }
        });
        if let Some(t) = temp_f {
            agg.temp_max_f = agg.temp_max_f.max(t);
            agg.temp_min_f = agg.temp_min_f.min(t);
            agg.seen_temp = true;
        }
        agg.precip_sum_in += precip_in;
        agg.pop_max = agg.pop_max.max(step_pop);
        agg.wind_max_mph = agg.wind_max_mph.max(wind_mph);
        agg.weather_code = agg.weather_code.max(step_code);
    }

    let mut daily: Vec<DailyEntry> = Vec::new();
    for key in order.into_iter().take(MAX_DAILY) {
        let agg = &days[&key];
        // Anchor the row at local noon so the consumer's weekday/date matches
        // the user's wall clock (fallback: the day's first step epoch).
        let anchor = local_noon_epoch(key, tz, now_epoch);
        daily.push(DailyEntry {
            time_epoch: anchor,
            weather_code: agg.weather_code,
            temp_max_f: if agg.seen_temp { agg.temp_max_f } else { 0.0 },
            temp_min_f: if agg.seen_temp { agg.temp_min_f } else { 0.0 },
            // Aggregated daily has no RH; filled from hourly by
            // backfill_daily_humidity below.
            humidity_pct: 0,
            precip_sum_in: agg.precip_sum_in,
            precip_probability_max: agg.pop_max,
            wind_max_mph: agg.wind_max_mph,
            // Compact has no gust field.
            wind_gust_max_mph: 0.0,
            // Compact has no UV index.
            uv_index_max: 0.0,
            // Compact has no sunrise/sunset (that's the sunrise/2.0 API).
            sunrise_epoch: 0,
            sunset_epoch: 0,
        });
    }

    let mut snap = ForecastSnapshot {
        last_refresh_epoch: now_epoch,
        source_reachable: true,
        source_label: "Met.no".to_string(),
        timezone: tz_name,
        daily,
        past_daily: vec![],
        hourly,
    };
    // Pair each day's high temp with THAT day's afternoon humidity (hourly).
    snap.backfill_daily_humidity();
    snap
}

#[inline]
fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

#[inline]
fn ms_to_mph(ms: f64) -> f64 {
    ms * 2.236_936
}

impl MetNorway {
    pub fn new(id: impl Into<String>, config: MetNorwayConfig, location: Location) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(config.user_agent.clone())
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            config,
            location,
            client,
        }
    }

    async fn fetch_forecast(&self) -> anyhow::Result<ForecastResponse> {
        let url = format!(
            "{API_BASE}/compact?lat={lat}&lon={lon}",
            lat = self.location.lat,
            lon = self.location.lon
        );
        let resp: ForecastResponse = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }
}

#[async_trait]
impl WeatherSource for MetNorway {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        // MetNorway's compact response is current+forecast in one
        // response. The "instant" block at the first timestep is the
        // closest thing to a current observation, but it is still a model
        // forecast (live_current=false below), so a real LAN station always
        // outranks it.
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindBearingDeg);
        fields.insert(WeatherField::PressureInHg);
        fields.insert(WeatherField::ForecastDaily);
        fields.insert(WeatherField::ForecastHourly);
        SourceCaps {
            live_current: false,
            hourly_forecast_hours: 216, // compact = ~9 days
            daily_forecast_days: 9,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            WeatherField::ForecastHourly | WeatherField::ForecastDaily => 55,
            // Numeric live values are present in the first timestep but
            // it's a model forecast, not an actual instrument; very low
            // priority vs any LAN sensor.
            WeatherField::AirTempF
            | WeatherField::RhPct
            | WeatherField::WindMph
            | WeatherField::WindBearingDeg
            | WeatherField::PressureInHg => 20,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "MetNorway source started");
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match self.fetch_forecast().await {
                        Ok(forecast) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            // First timestep = "now" (instant). This is NOT an
                            // observation: it is the model forecast valued at the
                            // current hour, the same deterministic run that drives
                            // the hourly/daily snapshot below. We publish it as a
                            // current scalar (cloud tier, live_current=false) so a
                            // no-hardware user still gets honest current values, but
                            // any real LAN station always outranks it (priority 20)
                            // and the UI badges it Forecast. Cloud cover here is
                            // likewise a forecast quantity, not a sky-camera/ceilometer
                            // reading.
                            if let Some(step) = forecast.properties.timeseries.first() {
                                let d = &step.data.instant.details;
                                let mut fields = Vec::new();
                                if let Some(t_c) = d.air_temperature {
                                    fields.push((WeatherField::AirTempF, c_to_f(t_c)));
                                }
                                if let Some(rh) = d.relative_humidity {
                                    fields.push((WeatherField::RhPct, rh));
                                }
                                if let Some(p_hpa) = d.air_pressure_at_sea_level {
                                    // hPa -> inHg
                                    fields.push((WeatherField::PressureInHg, p_hpa * 0.02953));
                                }
                                if let Some(ws_ms) = d.wind_speed {
                                    // m/s -> mph
                                    fields.push((WeatherField::WindMph, ms_to_mph(ws_ms)));
                                }
                                if let Some(wd) = d.wind_from_direction {
                                    fields.push((WeatherField::WindBearingDeg, wd));
                                }
                                if !fields.is_empty() {
                                    debug!(
                                        source_id = %self.id,
                                        fields_n = fields.len(),
                                        "MetNorway forecast updated"
                                    );
                                    let _ = bus.send(SourceEvent::Observation {
                                        source_id: self.id.clone(),
                                        fields,
                                        at_epoch: chrono::Utc::now().timestamp(),
                                    });
                                }
                            }

                            // Forecast snapshot: daily + hourly built from the
                            // SAME compact response, published to the forecast
                            // bridge by source priority.
                            let now = chrono::Utc::now().timestamp();
                            let snapshot =
                                build_snapshot(&forecast, self.location.lat, self.location.lon, now);
                            if !snapshot.hourly.is_empty() || !snapshot.daily.is_empty() {
                                debug!(
                                    source_id = %self.id,
                                    daily_n = snapshot.daily.len(),
                                    hourly_n = snapshot.hourly.len(),
                                    "MetNorway forecast snapshot built"
                                );
                                let _ = bus.send(SourceEvent::Forecast {
                                    source_id: self.id.clone(),
                                    snapshot,
                                    at_epoch: now,
                                });
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "MetNorway forecast fetch failed");
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
                        info!(source_id = %self.id, "MetNorway source shutdown");
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

    fn met_test() -> MetNorway {
        MetNorway::new(
            "met",
            MetNorwayConfig {
                user_agent: "LocalSky test (test@example.com)".into(),
            },
            Location {
                lat: 59.9139,
                lon: 10.7522,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_advertise_forecast() {
        let m = met_test();
        let caps = m.capabilities();
        assert!(caps.fields.contains(&WeatherField::ForecastHourly));
        assert!(caps.fields.contains(&WeatherField::ForecastDaily));
        assert!(caps.fields.contains(&WeatherField::PressureInHg));
        // It is a forecast value, not an instrument: live_current must stay false.
        assert!(!caps.live_current);
        assert_eq!(caps.daily_forecast_days, 9);
    }

    #[test]
    fn forecast_higher_priority_than_live() {
        let m = met_test();
        assert!(m.priority(WeatherField::ForecastHourly) > m.priority(WeatherField::AirTempF));
    }

    #[test]
    fn symbol_code_maps_loosely() {
        assert_eq!(symbol_to_wmo("clearsky_day"), 0);
        assert_eq!(symbol_to_wmo("fair_night"), 1);
        assert_eq!(symbol_to_wmo("partlycloudy_day"), 2);
        assert_eq!(symbol_to_wmo("cloudy"), 3);
        assert_eq!(symbol_to_wmo("fog"), 45);
        assert_eq!(symbol_to_wmo("rain"), 61);
        assert_eq!(symbol_to_wmo("rainshowers_day"), 80);
        assert_eq!(symbol_to_wmo("snow"), 71);
        assert_eq!(symbol_to_wmo("heavyrainandthunder"), 95);
        assert_eq!(symbol_to_wmo("totally_unknown_glyph"), 0);
    }

    // A tiny two-step compact sample spanning two UTC calendar days, so
    // the parser exercises both hourly mapping and daily grouping. METRIC
    // inputs -> canonical imperial outputs. Deterministic (no clock/net).
    const SAMPLE: &str = r#"{
      "properties": {
        "timeseries": [
          {
            "time": "2026-06-24T12:00:00Z",
            "data": {
              "instant": {
                "details": {
                  "air_temperature": 20.0,
                  "air_pressure_at_sea_level": 1013.0,
                  "relative_humidity": 55.0,
                  "wind_speed": 5.0,
                  "wind_from_direction": 180.0,
                  "cloud_area_fraction": 40.0
                }
              },
              "next_1_hours": {
                "summary": { "symbol_code": "rain" },
                "details": { "precipitation_amount": 25.4 }
              }
            }
          },
          {
            "time": "2026-06-24T13:00:00Z",
            "data": {
              "instant": {
                "details": {
                  "air_temperature": 25.0,
                  "air_pressure_at_sea_level": 1012.0,
                  "relative_humidity": 50.0,
                  "wind_speed": 10.0,
                  "wind_from_direction": 200.0,
                  "cloud_area_fraction": 10.0
                }
              },
              "next_1_hours": {
                "summary": { "symbol_code": "clearsky_day" },
                "details": { "precipitation_amount": 0.0 }
              }
            }
          },
          {
            "time": "2026-06-25T00:00:00Z",
            "data": {
              "instant": {
                "details": {
                  "air_temperature": 15.0,
                  "wind_speed": 2.0,
                  "wind_from_direction": 90.0,
                  "relative_humidity": 80.0,
                  "cloud_area_fraction": 90.0
                }
              },
              "next_1_hours": {
                "summary": { "symbol_code": "cloudy" },
                "details": { "precipitation_amount": 0.0 }
              }
            }
          }
        ]
      }
    }"#;

    #[test]
    fn forecast_maps_cloud_cover_fraction_to_pct() {
        // Forecast path only: cloud_area_fraction (percent) rounds into the
        // hourly snapshot cloud_cover_pct. (Met.no has no current cloud field.)
        let resp: ForecastResponse = serde_json::from_str(SAMPLE).expect("sample parses");
        let snap = build_snapshot(&resp, 0.0, 0.0, 1_700_000_000);
        assert_eq!(snap.hourly[0].cloud_cover_pct, 40);
    }

    #[test]
    fn parse_maps_hourly_and_daily() {
        let resp: ForecastResponse = serde_json::from_str(SAMPLE).expect("sample parses");
        // (0,0) -> tz None -> UTC bucketing + empty tz, keeping epochs deterministic.
        let snap = build_snapshot(&resp, 0.0, 0.0, 1_700_000_000);

        assert_eq!(snap.last_refresh_epoch, 1_700_000_000);
        assert!(snap.source_reachable);
        assert_eq!(snap.timezone, "");
        assert!(snap.past_daily.is_empty());

        // ----- hourly[0] -----
        assert_eq!(snap.hourly.len(), 3);
        let h0 = &snap.hourly[0];
        // 2026-06-24T12:00:00Z = 1782302400 epoch.
        assert_eq!(h0.time_epoch, 1_782_302_400);
        assert!(h0.time_epoch > 1_700_000_000); // sane future epoch
                                                // 20C -> 68F
        assert!((h0.temp_f - 68.0).abs() < 0.01, "temp {}", h0.temp_f);
        // 5 m/s -> ~11.18 mph
        assert!(
            (h0.wind_mph - 11.184_68).abs() < 0.01,
            "wind {}",
            h0.wind_mph
        );
        assert_eq!(h0.wind_dir_deg, 180);
        assert_eq!(h0.humidity_pct, 55);
        assert_eq!(h0.cloud_cover_pct, 40);
        // 25.4mm -> 1.0 in
        assert!(
            (h0.precip_in - 1.0).abs() < 0.001,
            "precip {}",
            h0.precip_in
        );
        assert_eq!(h0.weather_code, 61); // "rain"
        assert_eq!(h0.precip_probability, 100); // synthesized: precip present
        assert_eq!(h0.apparent_temp_f, 0.0); // no feels-like in compact

        // ----- daily grouping: 2026-06-24 and 2026-06-25 -----
        assert_eq!(snap.daily.len(), 2);
        let d0 = &snap.daily[0];
        // Anchored at UTC noon of 2026-06-24 (tz None), which equals the first
        // step here since it lands at 12:00:00Z.
        assert_eq!(d0.time_epoch, 1_782_302_400);
        // Day 0 temps: 20C(68F) and 25C(77F)
        assert!((d0.temp_max_f - 77.0).abs() < 0.01, "max {}", d0.temp_max_f);
        assert!((d0.temp_min_f - 68.0).abs() < 0.01, "min {}", d0.temp_min_f);
        // Day 0 precip sum: 25.4mm + 0mm -> 1.0 in
        assert!(
            (d0.precip_sum_in - 1.0).abs() < 0.001,
            "sum {}",
            d0.precip_sum_in
        );
        // Day 0 wind max: max(5,10) m/s -> ~22.37 mph
        assert!(
            (d0.wind_max_mph - 22.369_36).abs() < 0.01,
            "wmax {}",
            d0.wind_max_mph
        );
        // Worst-condition proxy: max(61 rain, 0 clear) = 61
        assert_eq!(d0.weather_code, 61);
        // Synthesized from the wet step: max(100, 0) = 100.
        assert_eq!(d0.precip_probability_max, 100);
        assert_eq!(d0.wind_gust_max_mph, 0.0);
        assert_eq!(d0.uv_index_max, 0.0);
        assert_eq!(d0.sunrise_epoch, 0);
        assert_eq!(d0.sunset_epoch, 0);

        let d1 = &snap.daily[1];
        // Anchored at UTC noon of 2026-06-25 = 1782345600 + 12h = 1782388800.
        assert_eq!(d1.time_epoch, 1_782_388_800);
        assert!((d1.temp_max_f - 59.0).abs() < 0.01); // 15C -> 59F
        assert_eq!(d1.weather_code, 3); // "cloudy"
    }
}
