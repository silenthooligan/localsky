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
// DAILY: timeseries grouped by LOCAL calendar day, aggregated into temp
// max/min, precip sum, peak wind. Steps past the ~48-60h horizon are
// 6-hourly and carry only next_6_hours (then a summary-only next_12_hours
// on the far tail), so the daily aggregation reads the finest next_*
// block present per step. UV/sunrise/sunset are unavailable in compact
// and stay at Default (0). Capped at 7.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tracing::debug;

use crate::config::schema::{Location, MetNorwayConfig};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::poll::{run_polling, Poll};
use crate::units::{c_to_f, ms_to_mph};
use crate::units::{hpa_to_inhg, mm_to_in};

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
    /// Past the hourly horizon (~48-60h out) the steps go 6-hourly and
    /// this is the ONLY precip-carrying block on them.
    #[serde(rename = "next_6_hours")]
    next_6_hours: Option<NextBlock>,
    /// Summary-only in compact (details carry no precipitation_amount);
    /// the last-resort symbol/POP signal on the far tail of the series.
    #[serde(rename = "next_12_hours")]
    next_12_hours: Option<NextBlock>,
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

fn rain_day_bounds(key: (i32, u32), tz: Option<chrono_tz::Tz>) -> Option<(i64, i64)> {
    use chrono::TimeZone;
    let day = chrono::NaiveDate::from_yo_opt(key.0, key.1)?;
    let start = day.and_hms_opt(0, 0, 0)?;
    let end = day.succ_opt()?.and_hms_opt(0, 0, 0)?;
    match tz {
        Some(tz) => Some((
            tz.from_local_datetime(&start).earliest()?.timestamp(),
            tz.from_local_datetime(&end).earliest()?.timestamp(),
        )),
        None => Some((start.and_utc().timestamp(), end.and_utc().timestamp())),
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
            .filter(|v| crate::forecast::precip::valid_amount(*v))
            .map(mm_to_in);
        hourly.push(HourlyEntry {
            time_epoch,
            weather_code,
            temp_f: d.air_temperature.map(c_to_f).filter(|t| t.is_finite()),
            // Compact has no apparent/"feels-like" temperature.
            apparent_temp_f: 0.0,
            precip_in,
            // Synthesized from precip presence (compact has no real POP).
            // Synthesized, but a real estimate: Some, never a provider gap.
            precip_probability: precip_in.map(|amount| synth_pop(amount, symbol)),
            wind_mph: d
                .wind_speed
                .map(ms_to_mph)
                .filter(|w| w.is_finite() && *w >= 0.0),
            wind_dir_deg: d.wind_from_direction.map(|x| x.round() as u32).unwrap_or(0),
            humidity_pct: d
                .relative_humidity
                .filter(|rh| rh.is_finite() && (0.0..=100.0).contains(rh))
                .map(|rh| rh.round() as u32),
            cloud_cover_pct: d.cloud_area_fraction.map(|x| x.round() as u32).unwrap_or(0),
            ..Default::default()
        });
    }

    // Choose the finest declared interval once. A 6h/12h accumulation can
    // establish a daily total without pretending to be a one-hour dry value.
    let rain_intervals: Vec<_> = steps
        .iter()
        .filter_map(|step| {
            let start = iso8601_to_epoch(&step.time)?;
            let (hours, period) = step
                .data
                .next_1_hours
                .as_ref()
                .map(|p| (1, p))
                .or_else(|| step.data.next_6_hours.as_ref().map(|p| (6, p)))
                .or_else(|| step.data.next_12_hours.as_ref().map(|p| (12, p)))?;
            let amount = period
                .details
                .precipitation_amount
                .filter(|v| crate::forecast::precip::valid_amount(*v))
                .map(mm_to_in);
            Some((start, start.checked_add(hours * 3600)?, amount))
        })
        .collect();

    // ---- DAILY: group steps by LOCAL calendar day, aggregate. ----
    // Accumulator keyed by local (year, ordinal-day), preserving first-seen order.
    struct DayAgg {
        temp_max_f: f64,
        temp_min_f: f64,
        pop_max: u32,
        wind_max_mph: Option<f64>,
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
        // Finest-window fallback: hourly steps carry next_1_hours, but past
        // the ~48-60h horizon met.no switches to 6-hourly steps carrying only
        // next_6_hours (and a summary-only next_12_hours on the far tail).
        // Take exactly ONE block per step, the finest present, so consecutive
        // steps contribute non-overlapping windows: an hourly step never adds
        // its co-present 6h block on top (that would 6x the sum), and a
        // 6-hourly step's 6h window tiles exactly against its neighbors.
        // Without this, days 3+ always summed to 0.00 in / 0% POP.
        let next = step
            .data
            .next_1_hours
            .as_ref()
            .or(step.data.next_6_hours.as_ref())
            .or(step.data.next_12_hours.as_ref());
        let symbol = next
            .and_then(|n| n.summary.as_ref())
            .and_then(|s| s.symbol_code.as_deref());
        let precip_in = next
            .and_then(|n| n.details.precipitation_amount)
            .filter(|v| crate::forecast::precip::valid_amount(*v))
            .map(mm_to_in);
        let step_code = symbol.map(symbol_to_wmo).unwrap_or(0);
        let step_pop = precip_in
            .map(|amount| synth_pop(amount, symbol))
            .unwrap_or_else(|| {
                if symbol.is_some_and(symbol_is_precip) {
                    50
                } else {
                    0
                }
            });
        let temp_f = d.air_temperature.map(c_to_f).filter(|t| t.is_finite());
        let wind_mph = d
            .wind_speed
            .map(ms_to_mph)
            .filter(|w| w.is_finite() && *w >= 0.0);

        let agg = days.entry(key).or_insert_with(|| {
            order.push(key);
            DayAgg {
                temp_max_f: f64::MIN,
                temp_min_f: f64::MAX,
                pop_max: 0,
                wind_max_mph: Some(0.0),
                weather_code: 0,
                seen_temp: false,
            }
        });
        if let Some(t) = temp_f {
            agg.temp_max_f = agg.temp_max_f.max(t);
            agg.temp_min_f = agg.temp_min_f.min(t);
            agg.seen_temp = true;
        }
        agg.pop_max = agg.pop_max.max(step_pop);
        // An incomplete series cannot claim a calm/safe daily maximum.
        // Once a contributing step lacks wind, the daily peak is unknown.
        agg.wind_max_mph = agg
            .wind_max_mph
            .zip(wind_mph)
            .map(|(peak, wind)| peak.max(wind));
        agg.weather_code = agg.weather_code.max(step_code);
    }

    let mut daily: Vec<DailyEntry> = Vec::new();
    for key in order.into_iter().take(MAX_DAILY) {
        let agg = &days[&key];
        // Anchor the row at local noon so the consumer's weekday/date matches
        // the user's wall clock (fallback: the day's first step epoch).
        let anchor = local_noon_epoch(key, tz, now_epoch);
        daily.push(DailyEntry {
            // met.no is anchored on local noon.
            day_marker: crate::engine::clock::DayMarker::inside_local_day(anchor),
            weather_code: agg.weather_code,
            temp_max_f: agg.seen_temp.then_some(agg.temp_max_f),
            temp_min_f: agg.seen_temp.then_some(agg.temp_min_f),
            // Aggregated daily has no RH; filled from hourly by
            // backfill_daily_humidity below.
            humidity_pct: None,
            precip_sum_in: rain_day_bounds(key, tz).and_then(|(start, end)| {
                (end > now_epoch)
                    .then(|| {
                        crate::forecast::precip::total_over(
                            rain_intervals.iter().copied(),
                            start.max(now_epoch),
                            end,
                        )
                    })
                    .flatten()
            }),
            precip_probability_max: Some(agg.pop_max),
            wind_max_mph: agg.wind_max_mph,
            // Compact has no gust field.
            wind_gust_max_mph: 0.0,
            // Compact has no UV index.
            uv_index_max: 0.0,
            // Compact has no sunrise/sunset (that's the sunrise/2.0 API).
            sunrise_epoch: 0,
            sunset_epoch: 0,
            ..Default::default()
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
        ..Default::default()
    };
    // Pair each day's high temp with THAT day's afternoon humidity (hourly).
    snap.backfill_daily_humidity(crate::timeutil::deployment_calendar());
    snap
}

impl MetNorway {
    pub fn new(id: impl Into<String>, config: MetNorwayConfig, location: Location) -> Self {
        // api.met.no's terms require an identifying UA. An empty or
        // historical-placeholder value derives the real instance identity
        // at request time; the config is never rewritten (see
        // sources::resolve_outbound_user_agent). A keyless authority that
        // carries the operator's own contact string is the `client_with`
        // case, not the plain derived-identity `client`.
        let client = crate::net::client_with(
            Duration::from_secs(15),
            &crate::sources::resolve_outbound_user_agent(&config.user_agent),
        );
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

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        // The loop (tick, missed-tick policy, fetch metric, reachability
        // edges in both directions, shutdown) is `run_polling`'s; this
        // closure is one poll: fetch the compact response, publish its
        // first timestep as the current scalar, build the forecast
        // snapshot from the SAME response.
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "MetNorway",
            POLL_INTERVAL,
            bus,
            shutdown,
            |s: Arc<Self>| async move {
                let forecast = s.fetch_forecast().await?;
                let now = chrono::Utc::now().timestamp();

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
                let mut fields = Vec::new();
                if let Some(step) = forecast.properties.timeseries.first() {
                    let d = &step.data.instant.details;
                    if let Some(t_c) = d.air_temperature {
                        fields.push((WeatherField::AirTempF, c_to_f(t_c)));
                    }
                    if let Some(rh) = d.relative_humidity {
                        fields.push((WeatherField::RhPct, rh));
                    }
                    if let Some(p_hpa) = d.air_pressure_at_sea_level {
                        // hPa -> inHg
                        fields.push((WeatherField::PressureInHg, hpa_to_inhg(p_hpa)));
                    }
                    if let Some(ws_ms) = d.wind_speed {
                        // m/s -> mph
                        fields.push((WeatherField::WindMph, ms_to_mph(ws_ms)));
                    }
                    if let Some(wd) = d.wind_from_direction {
                        fields.push((WeatherField::WindBearingDeg, wd));
                    }
                }
                // Poll::observation publishes nothing when `fields` is empty.
                let mut poll = Poll::observation(&s.id, fields, now);

                // Forecast snapshot: daily + hourly built from the SAME
                // compact response, published to the forecast bridge by
                // source priority.
                let snapshot = build_snapshot(&forecast, s.location.lat, s.location.lon, now);
                if !snapshot.hourly.is_empty() || !snapshot.daily.is_empty() {
                    debug!(
                        source_id = %s.id,
                        daily_n = snapshot.daily.len(),
                        hourly_n = snapshot.hourly.len(),
                        "MetNorway forecast snapshot built"
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
    fn daily_aggregation_needs_real_finite_temperature_samples() {
        let mut resp: ForecastResponse = serde_json::from_str(SAMPLE).unwrap();
        for invalid in [None, Some(f64::NAN), Some(f64::INFINITY), Some(f64::MAX)] {
            for step in &mut resp.properties.timeseries {
                step.data.instant.details.air_temperature = invalid;
            }
            let snapshot = build_snapshot(&resp, 0.0, 0.0, 1_700_000_000);
            assert!(!snapshot.daily.is_empty());
            assert!(snapshot
                .daily
                .iter()
                .all(|d| d.temp_max_f.is_none() && d.temp_min_f.is_none()));
            assert!(snapshot.hourly.iter().all(|h| h.temp_f.is_none()));
            assert!(
                snapshot
                    .hourly
                    .iter()
                    .any(|h| h.precip_in.is_some_and(|v| v > 0.0)),
                "missing temperature leaves real hourly rain evidence intact"
            );
        }
        resp.properties.timeseries[0]
            .data
            .instant
            .details
            .air_temperature = Some(0.0);
        let snapshot = build_snapshot(&resp, 0.0, 0.0, 1_700_000_000);
        assert_eq!(snapshot.daily[0].temp_max_f, Some(32.0));
        assert_eq!(snapshot.daily[0].temp_min_f, Some(32.0));
        assert_eq!(snapshot.hourly[0].temp_f, Some(32.0));
        assert_eq!(snapshot.daily[1].temp_max_f, None);
    }

    #[test]
    fn partial_wind_day_is_unknown_while_reported_zero_survives() {
        let mut resp: ForecastResponse = serde_json::from_str(SAMPLE).unwrap();
        for invalid in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            let details = &mut resp.properties.timeseries[0].data.instant.details;
            details.wind_speed = invalid;
            details.relative_humidity = invalid;
            let snapshot = build_snapshot(&resp, 0.0, 0.0, 1_700_000_000);
            assert_eq!(snapshot.hourly[0].wind_mph, None);
            assert_eq!(snapshot.hourly[0].humidity_pct, None);
            assert_eq!(
                snapshot.daily[0].wind_max_mph, None,
                "valid later wind cannot establish the whole daily peak"
            );
        }
        resp.properties.timeseries[0]
            .data
            .instant
            .details
            .relative_humidity = Some(101.0);
        assert_eq!(
            build_snapshot(&resp, 0.0, 0.0, 1).hourly[0].humidity_pct,
            None
        );
        for step in &mut resp.properties.timeseries {
            step.data.instant.details.wind_speed = Some(0.0);
            step.data.instant.details.relative_humidity = Some(0.0);
        }
        let snapshot = build_snapshot(&resp, 0.0, 0.0, 1);
        assert!(snapshot.daily.iter().all(|d| d.wind_max_mph == Some(0.0)));
        assert!(snapshot
            .hourly
            .iter()
            .all(|h| h.wind_mph == Some(0.0) && h.humidity_pct == Some(0)));
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
        assert!(
            (h0.temp_f.unwrap() - 68.0).abs() < 0.01,
            "temp {:?}",
            h0.temp_f
        );
        // 5 m/s -> ~11.18 mph
        assert!(
            (h0.wind_mph.unwrap() - 11.184_68).abs() < 0.01,
            "wind {:?}",
            h0.wind_mph
        );
        assert_eq!(h0.wind_dir_deg, 180);
        assert_eq!(h0.humidity_pct, Some(55));
        assert_eq!(h0.cloud_cover_pct, 40);
        // 25.4mm -> 1.0 in
        assert!(
            (h0.precip_in.unwrap() - 1.0).abs() < 0.001,
            "precip {}",
            h0.precip_in.unwrap()
        );
        assert_eq!(h0.weather_code, 61); // "rain"
        assert_eq!(h0.precip_probability, Some(100)); // synthesized: precip present
        assert_eq!(h0.apparent_temp_f, 0.0); // no feels-like in compact

        // ----- daily grouping: 2026-06-24 and 2026-06-25 -----
        assert_eq!(snap.daily.len(), 2);
        let d0 = &snap.daily[0];
        // Anchored at UTC noon of 2026-06-24 (tz None), which equals the first
        // step here since it lands at 12:00:00Z.
        assert_eq!(
            d0.day_marker,
            crate::engine::clock::DayMarker::inside_local_day(1_782_302_400)
        );
        // Day 0 temps: 20C(68F) and 25C(77F)
        assert!(
            (d0.temp_max_f.unwrap() - 77.0).abs() < 0.01,
            "max {:?}",
            d0.temp_max_f
        );
        assert!(
            (d0.temp_min_f.unwrap() - 68.0).abs() < 0.01,
            "min {:?}",
            d0.temp_min_f
        );
        // Two known hours are not the full-day accumulation.
        assert_eq!(
            d0.precip_sum_in, None,
            "a short sample does not cover a full day"
        );
        // Day 0 wind max: max(5,10) m/s -> ~22.37 mph
        assert!(
            (d0.wind_max_mph.unwrap() - 22.369_36).abs() < 0.01,
            "wmax {:?}",
            d0.wind_max_mph
        );
        // Worst-condition proxy: max(61 rain, 0 clear) = 61
        assert_eq!(d0.weather_code, 61);
        // Synthesized from the wet step: max(100, 0) = 100.
        assert_eq!(d0.precip_probability_max, Some(100));
        assert_eq!(d0.wind_gust_max_mph, 0.0);
        assert_eq!(d0.uv_index_max, 0.0);
        assert_eq!(d0.sunrise_epoch, 0);
        assert_eq!(d0.sunset_epoch, 0);

        let d1 = &snap.daily[1];
        // Anchored at UTC noon of 2026-06-25 = 1782345600 + 12h = 1782388800.
        assert_eq!(
            d1.day_marker,
            crate::engine::clock::DayMarker::inside_local_day(1_782_388_800)
        );
        assert!((d1.temp_max_f.unwrap() - 59.0).abs() < 0.01); // 15C -> 59F
        assert_eq!(d1.weather_code, 3); // "cloudy"
    }

    // The compact response past the hourly horizon: day one is hourly steps
    // carrying BOTH next_1_hours and next_6_hours (the finest window must win
    // or the co-present 6h amounts double-count), day two is 6-hourly steps
    // carrying only next_6_hours, day three is a summary-only next_12_hours
    // tail step (no precipitation_amount in compact).
    const SIX_HOURLY_SAMPLE: &str = r#"{
      "properties": {
        "timeseries": [
          {
            "time": "2026-06-24T12:00:00Z",
            "data": {
              "instant": { "details": { "air_temperature": 20.0 } },
              "next_1_hours": {
                "summary": { "symbol_code": "rain" },
                "details": { "precipitation_amount": 2.54 }
              },
              "next_6_hours": {
                "summary": { "symbol_code": "rain" },
                "details": { "precipitation_amount": 25.4 }
              }
            }
          },
          {
            "time": "2026-06-24T13:00:00Z",
            "data": {
              "instant": { "details": { "air_temperature": 21.0 } },
              "next_1_hours": {
                "summary": { "symbol_code": "rain" },
                "details": { "precipitation_amount": 2.54 }
              },
              "next_6_hours": {
                "summary": { "symbol_code": "rain" },
                "details": { "precipitation_amount": 22.86 }
              }
            }
          },
          {
            "time": "2026-06-25T00:00:00Z",
            "data": {
              "instant": { "details": { "air_temperature": 15.0 } },
              "next_6_hours": {
                "summary": { "symbol_code": "rain" },
                "details": { "precipitation_amount": 12.7 }
              }
            }
          },
          {
            "time": "2026-06-25T06:00:00Z",
            "data": {
              "instant": { "details": { "air_temperature": 16.0 } },
              "next_6_hours": {
                "summary": { "symbol_code": "rain" },
                "details": { "precipitation_amount": 12.7 }
              }
            }
          },
          {
            "time": "2026-06-25T12:00:00Z",
            "data": {
              "instant": { "details": { "air_temperature": 18.0 } },
              "next_6_hours": {
                "summary": { "symbol_code": "cloudy" },
                "details": { "precipitation_amount": 0.0 }
              }
            }
          },
          {
            "time": "2026-06-26T00:00:00Z",
            "data": {
              "instant": { "details": { "air_temperature": 17.0 } },
              "next_12_hours": {
                "summary": { "symbol_code": "rain" },
                "details": {}
              }
            }
          }
        ]
      }
    }"#;

    #[test]
    fn daily_precip_falls_back_to_coarser_windows_past_hourly_horizon() {
        let resp: ForecastResponse =
            serde_json::from_str(SIX_HOURLY_SAMPLE).expect("sample parses");
        // (0,0) -> tz None -> UTC bucketing, keeping the grouping deterministic.
        let snap = build_snapshot(&resp, 0.0, 0.0, 1_700_000_000);
        assert_eq!(snap.daily.len(), 3);

        // These abbreviated samples do not cover whole calendar days. Do
        // not advertise their known partial sums as complete dry/wet days.
        assert_eq!(snap.daily[0].precip_sum_in, None);
        let d1 = &snap.daily[1];
        assert_eq!(d1.precip_sum_in, None);
        assert_eq!(d1.precip_probability_max, Some(100)); // synth: amount present
        assert_eq!(d1.weather_code, 61); // "rain" from the 6h summary

        // Day 2 (summary-only next_12_hours tail): nothing to sum, but the
        // precip-class symbol still drives the synthesized POP + condition.
        let d2 = &snap.daily[2];
        assert_eq!(d2.precip_sum_in, None);
        assert_eq!(d2.precip_probability_max, Some(50));
        assert_eq!(d2.weather_code, 61);

        // Hourly rows keep the 1h-only read: a 6-hourly step (no
        // next_1_hours) contributes no hourly precip.
        assert_eq!(snap.hourly[2].precip_in, None);
    }
    #[test]
    fn six_hour_rain_tiles_a_day_without_inventing_hourly_coverage() {
        let start = iso8601_to_epoch("2026-06-24T00:00:00Z").unwrap();
        let steps: Vec<_> = (0..4).map(|i| serde_json::json!({
            "time": chrono::DateTime::from_timestamp(start + i * 6 * 3600, 0).unwrap().to_rfc3339(),
            "data": {"instant":{"details":{}}, "next_6_hours":{"details":{"precipitation_amount":6.35}}}
        })).collect();
        let mut json = serde_json::json!({"properties":{"timeseries":steps}});
        let resp: ForecastResponse = serde_json::from_value(json.clone()).unwrap();
        let snapshot = build_snapshot(&resp, 0.0, 0.0, start);
        assert!((snapshot.daily[0].precip_sum_in.unwrap() - 1.0).abs() < 1e-9);
        assert!(snapshot.hourly.iter().all(|h| h.precip_in.is_none()));
        json["properties"]["timeseries"][2]["data"]["next_6_hours"]["details"]
            ["precipitation_amount"] = serde_json::Value::Null;
        let resp: ForecastResponse = serde_json::from_value(json).unwrap();
        assert_eq!(
            build_snapshot(&resp, 0.0, 0.0, start).daily[0].precip_sum_in,
            None
        );
    }
}
