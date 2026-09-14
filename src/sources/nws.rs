// National Weather Service (US) weather source, api.weather.gov.
//
// Free, no API key. US-only coverage. Requires a descriptive
// User-Agent header per the api.weather.gov terms of service.
//
// Two-stage lookup the first time we see a (lat, lon):
//   GET /points/{lat},{lon}                                  -> gridId + gridX + gridY
//                                                               + observationStations (URL)
//   GET /gridpoints/{gridId}/{gridX},{gridY}/forecast        -> 7-day text forecast
//   GET /gridpoints/{gridId}/{gridX},{gridY}/forecast/hourly -> per-hour forecast
//   GET {observationStations}                                -> nearest station collection
//   GET /stations/{stationId}/observations/latest            -> REAL current observation
//
// We cache the (gridId, gridX, gridY) since it's stable for a given
// location, then poll the forecast endpoints every 30 min. The free
// daily-forecast call returns probabilityOfPrecipitation per period,
// temperature high/low (Fahrenheit), wind speed (mph as a string like
// "10 mph" / "5 to 10 mph"), and a long text shortForecast.
//
// CURRENT OBSERVATIONS: separately from the forecast, NWS exposes the
// latest METAR-style observation from the nearest ASOS/AWOS station. We
// resolve that station id once (points -> observationStations collection ->
// nearest station collection) and cache it, then every five minutes GET
// /stations/{id}/observations/latest and map properties to live current
// scalars (temperature -> AirTempF, relativeHumidity -> RhPct, windSpeed ->
// WindMph, windGust -> WindGustMph, windDirection -> WindBearingDeg,
// barometricPressure/seaLevelPressure -> PressureInHg, dewpoint ->
// DewPointF, precipitationLastHour -> RainIntensityInHr). precipitationLastHour
// is a REAL measured gauge total for the last hour (NWS reports it in METERS,
// `wmoUnit:m`); because it is a full last-hour accumulation it reads as an
// in/hr rate over that hour, so it emits as RainIntensityInHr (meters are
// normalized to mm, then the units seam takes mm -> in). Each NWS measured
// value carries its own `unitCode`
// (wmoUnit:degC, wmoUnit:percent, wmoUnit:m_s-1, wmoUnit:km_h-1, wmoUnit:Pa,
// wmoUnit:degree_(angle)), which we map to a label `units::to_canonical`
// understands so the conversion seam stays single-sourced. These emit as a
// live current Observation, but `live_current` stays FALSE in capabilities:
// it is a cloud current source (a remote station's reading), NOT a LAN
// station, so a real LAN sensor outranks it in the per-field merge.
//
// Emits the current scalars above as one Observation, a full ForecastSnapshot
// built from the daily PERIODS (paired day+night -> one DailyEntry) plus the
// hourly endpoint, and Reachability flips on network errors.
//
// Unit notes: NWS /forecast and /forecast/hourly already return imperial
// (temperature in F, windSpeed strings in mph). No conversion needed for
// the FORECAST values we read. There is no QPF precip amount in /forecast, so
// daily precip_sum_in is unknown until gridpoint QPF covers its period. NWS has no reliable UV / cloud-cover /
// apparent-temp in these endpoints, so those default. WMO weather codes
// are mapped loosely from the shortForecast text (else 0; the UI has a
// glyph fallback). The CURRENT /observations/latest feed is the opposite:
// it reports METRIC (degC, m/s or km/h, Pa, percent, degrees) with an
// explicit per-field unitCode, so each scalar is routed through
// units::to_canonical to reach LocalSky's canonical imperial unit.

use futures::{FutureExt, StreamExt};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::schema::{Location, NwsConfig};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::poll::{run_polling, Poll};

const API_BASE: &str = "https://api.weather.gov";
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const FORECAST_INTERVAL_S: i64 = 30 * 60;
// Bound API work while allowing a nearby station to fill a missing field.
const OBSERVATION_STATION_LIMIT: usize = 6;

pub struct Nws {
    id: String,
    #[allow(dead_code)]
    // user_agent is consumed at construction; kept for parity with other sources
    config: NwsConfig,
    location: Location,
    client: Client,
    api_base: String,
    /// Cached (gridId, gridX, gridY) from /points/{lat},{lon}.
    grid_cache: Arc<Mutex<Option<GridPoint>>>,
    /// Cached observation-station ids, nearest-first (e.g. ["KNYC", "KJFK",
    /// ...]), resolved once from /points -> observationStations collection.
    /// Stable for a given location, so we never re-walk the collection per
    /// poll. We try them in order each cycle: if the nearest station's
    /// /observations/latest is empty (a station that reports no scalars this
    /// hour), we fall through to the next-nearest instead of going silent.
    station_cache: Arc<Mutex<Option<Vec<String>>>>,
    last_forecast_epoch: AtomicI64,
}

#[derive(Debug, Clone)]
struct GridPoint {
    grid_id: String,
    grid_x: u32,
    grid_y: u32,
}

#[derive(Debug, Deserialize)]
struct PointsResponse {
    properties: PointsProperties,
}

#[derive(Debug, Deserialize)]
struct PointsProperties {
    #[serde(rename = "gridId")]
    grid_id: String,
    #[serde(rename = "gridX")]
    grid_x: u32,
    #[serde(rename = "gridY")]
    grid_y: u32,
    /// URL of the observation-stations collection for this point
    /// (e.g. ".../gridpoints/OKX/33,35/stations"). Absent on some legacy
    /// payloads, so optional; the current-observation path is best-effort.
    #[serde(rename = "observationStations")]
    observation_stations: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    properties: ForecastProperties,
}

#[derive(Debug, Deserialize)]
struct ForecastProperties {
    periods: Vec<ForecastPeriod>,
}

#[derive(Debug, Deserialize)]
struct ForecastPeriod {
    #[serde(rename = "startTime")]
    start_time: Option<String>,
    #[serde(rename = "isDaytime")]
    is_daytime: bool,
    temperature: Option<f64>,
    #[serde(rename = "probabilityOfPrecipitation")]
    pop: Option<PopObj>,
    #[serde(rename = "windSpeed")]
    wind_speed: Option<String>,
    #[serde(rename = "windDirection")]
    wind_direction: Option<String>,
    #[serde(rename = "relativeHumidity")]
    relative_humidity: Option<PopObj>,
    #[serde(rename = "shortForecast")]
    short_forecast: Option<String>,
}

/// Shared shape for `{ "value": <number|null> }` sub-objects
/// (probabilityOfPrecipitation, relativeHumidity, dewpoint, ...).
#[derive(Debug, Deserialize)]
struct PopObj {
    value: Option<f64>,
}

/// Raw gridpoint payload (`/gridpoints/{id}/{x},{y}`, no `/forecast` suffix).
/// This is the only NWS endpoint that carries quantitative precipitation (QPF),
/// which the human `/forecast` text feed omits. Without it an NWS-owned forecast
/// has unknown rain amounts; missing QPF cannot be treated as dry weather.
#[derive(Debug, Deserialize)]
struct RawGridResponse {
    properties: RawGridProperties,
}

#[derive(Debug, Deserialize)]
struct RawGridProperties {
    #[serde(rename = "quantitativePrecipitation")]
    qpf: Option<QpfBlock>,
}

#[derive(Debug, Deserialize)]
struct QpfBlock {
    uom: Option<String>,
    /// Each value covers an ISO8601 interval (`validTime = "<start>/<duration>"`)
    /// and carries mm of liquid over that interval (uom `wmoUnit:mm`).
    #[serde(default)]
    values: Vec<QpfValue>,
}

#[derive(Debug, Deserialize)]
struct QpfValue {
    #[serde(rename = "validTime")]
    valid_time: Option<String>,
    value: Option<f64>,
}

/// Observation-stations collection (`GET {observationStations}`): a GeoJSON
/// FeatureCollection ordered nearest-first, each feature carrying a
/// `stationIdentifier` (e.g. "KNYC"). Nearby stations fill missing fields.
#[derive(Debug, Deserialize)]
struct StationsResponse {
    #[serde(default)]
    features: Vec<StationFeature>,
}

#[derive(Debug, Deserialize)]
struct StationFeature {
    properties: StationProps,
}

#[derive(Debug, Deserialize)]
struct StationProps {
    #[serde(rename = "stationIdentifier")]
    station_identifier: Option<String>,
}

/// Latest observation (`GET /stations/{id}/observations/latest`): a single
/// GeoJSON Feature whose `properties` carry the current measured scalars. Each
/// measured field is a `{ value, unitCode }` sub-object; `value` is null when
/// the station did not report it (NWS routinely nulls windGust/dewpoint).
#[derive(Debug, Deserialize)]
struct ObservationResponse {
    properties: ObservationProperties,
}

#[derive(Debug, Deserialize)]
struct ObservationProperties {
    timestamp: Option<String>,
    temperature: Option<Measured>,
    dewpoint: Option<Measured>,
    #[serde(rename = "relativeHumidity")]
    relative_humidity: Option<Measured>,
    #[serde(rename = "windDirection")]
    wind_direction: Option<Measured>,
    #[serde(rename = "windSpeed")]
    wind_speed: Option<Measured>,
    #[serde(rename = "windGust")]
    wind_gust: Option<Measured>,
    #[serde(rename = "barometricPressure")]
    barometric_pressure: Option<Measured>,
    #[serde(rename = "seaLevelPressure")]
    sea_level_pressure: Option<Measured>,
    /// Measured liquid accumulation over the last hour from the station gauge.
    /// NWS reports this in METERS (`unitCode: wmoUnit:m`); some stations use
    /// `wmoUnit:mm`. Since it is the full last-hour total it is effectively an
    /// in/hr rate, so we emit it as RainIntensityInHr. Null when the gauge did
    /// not report (dry hour or no gauge) -> skipped, never emitted as 0.
    #[serde(rename = "precipitationLastHour")]
    precipitation_last_hour: Option<Measured>,
}

/// One NWS measured quantity: `{ "value": <number|null>, "unitCode": "wmoUnit:degC" }`.
/// `value` null = the station didn't report it this cycle (skip, never zero).
#[derive(Debug, Deserialize)]
struct Measured {
    value: Option<f64>,
    #[serde(rename = "unitCode")]
    unit_code: Option<String>,
}

/// Translate an NWS `unitCode` (WMO/QUDT vocabulary, e.g. `wmoUnit:degC`,
/// `wmoUnit:m_s-1`, `wmoUnit:km_h-1`, `wmoUnit:Pa`, `wmoUnit:percent`,
/// `wmoUnit:degree_(angle)`) into the lowercase unit label `units::to_canonical`
/// understands, so the conversion seam is single-sourced. Returns None for an
/// absent/unrecognized code, which `to_canonical` treats as "already canonical"
/// (pass-through) when handed through as None.
fn nws_unit_label(unit_code: Option<&str>) -> Option<&'static str> {
    // Strip the `wmoUnit:` / `qudtUnit:` namespace prefix, lowercase.
    let raw = unit_code?;
    let bare = raw.rsplit(':').next().unwrap_or(raw).trim();
    let lower = bare.to_ascii_lowercase();
    Some(match lower.as_str() {
        "degc" | "celsius" => "c",
        "degf" | "fahrenheit" => "f",
        "k" | "kelvin" => "k",
        "percent" => "%",
        "m_s-1" | "m s-1" | "m/s" => "m/s",
        "km_h-1" | "km h-1" | "km/h" => "km/h",
        "pa" => "pa",
        "hpa" => "hpa",
        // windDirection / non-converted angle units have no canonical mapping;
        // degrees are already canonical, so signal pass-through (None).
        _ => return None,
    })
}

impl Nws {
    pub fn new(id: impl Into<String>, config: NwsConfig, location: Location) -> Self {
        // api.weather.gov requires an operator-identifying UA. An empty or
        // historical-placeholder value ("you@example.com") derives the real
        // instance identity at request time; the config is never rewritten
        // (see sources::resolve_outbound_user_agent). That contact string is
        // why this is `client_with` and not the derived-UA `client`.
        let client = crate::net::client_with(
            Duration::from_secs(15),
            &crate::sources::resolve_outbound_user_agent(&config.user_agent),
        );
        Self {
            id: id.into(),
            config,
            location,
            client,
            api_base: API_BASE.into(),
            grid_cache: Arc::new(Mutex::new(None)),
            station_cache: Arc::new(Mutex::new(None)),
            last_forecast_epoch: AtomicI64::new(0),
        }
    }

    async fn resolve_grid(&self) -> anyhow::Result<GridPoint> {
        if let Some(cached) = self.grid_cache.lock().await.clone() {
            return Ok(cached);
        }
        let url = format!(
            "{}/points/{lat},{lon}",
            self.api_base,
            lat = self.location.lat,
            lon = self.location.lon
        );
        let resp: PointsResponse = self
            .client
            .get(&url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let grid = GridPoint {
            grid_id: resp.properties.grid_id,
            grid_x: resp.properties.grid_x,
            grid_y: resp.properties.grid_y,
        };
        *self.grid_cache.lock().await = Some(grid.clone());
        Ok(grid)
    }

    async fn fetch_forecast(&self, grid: &GridPoint) -> anyhow::Result<ForecastResponse> {
        let url = format!(
            "{}/gridpoints/{}/{},{}/forecast",
            self.api_base, grid.grid_id, grid.grid_x, grid.grid_y
        );
        let resp: ForecastResponse = self
            .client
            .get(&url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    async fn fetch_hourly(&self, grid: &GridPoint) -> anyhow::Result<ForecastResponse> {
        let url = format!(
            "{}/gridpoints/{}/{},{}/forecast/hourly",
            self.api_base, grid.grid_id, grid.grid_x, grid.grid_y
        );
        let resp: ForecastResponse = self
            .client
            .get(&url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    /// Fetch the raw gridpoint payload for QPF (precip amount). Separate from
    /// `/forecast` because only the raw endpoint carries quantitativePrecipitation.
    async fn fetch_raw_grid(&self, grid: &GridPoint) -> anyhow::Result<RawGridResponse> {
        let url = format!(
            "{}/gridpoints/{}/{},{}",
            self.api_base, grid.grid_id, grid.grid_x, grid.grid_y
        );
        let resp: RawGridResponse = self
            .client
            .get(&url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    /// Resolve (once) the nearest-first list of observation-station ids for this
    /// location and cache it. Walks /points -> observationStations collection ->
    /// every feature's stationIdentifier (collection order is nearest-first).
    /// Returns an empty Vec when the point has no stations collection or the
    /// collection is empty/foreign (the current-observation path is best-effort;
    /// US-only). A 404/403 outside the US surfaces as an Err from the HTTP
    /// layer, which the caller treats as "no current", never a panic.
    async fn resolve_stations(&self) -> anyhow::Result<Vec<String>> {
        if let Some(cached) = self.station_cache.lock().await.clone() {
            return Ok(cached);
        }
        // The points payload carries the stations-collection URL. Reuse the
        // same /points endpoint resolve_grid hits, but read observationStations.
        let url = format!(
            "{}/points/{lat},{lon}",
            self.api_base,
            lat = self.location.lat,
            lon = self.location.lon
        );
        let points: PointsResponse = self
            .client
            .get(&url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let Some(stations_url) = points.properties.observation_stations else {
            return Ok(Vec::new());
        };
        let stations: StationsResponse = self
            .client
            .get(&stations_url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let ids = station_ids(&stations);
        if !ids.is_empty() {
            *self.station_cache.lock().await = Some(ids.clone());
        }
        Ok(ids)
    }

    /// Fetch a bounded nearby set concurrently. Preserve provider report times
    /// and choose each field once; a station missing wind cannot hide a nearby
    /// report. The common arbiter applies the configured report-age limit.
    async fn fetch_current_fields(&self, station_ids: &[String]) -> Poll {
        let requests: Vec<_> = station_ids
            .iter()
            .take(OBSERVATION_STATION_LIMIT)
            .enumerate()
            .map(|(rank, station)| {
                let station = station.clone();
                let client = self.client.clone();
                let api_base = self.api_base.clone();
                async move {
                    let response =
                        Self::fetch_latest_observation(client, api_base, station.clone()).await;
                    (rank, station, response)
                }
                .boxed()
            })
            .collect();
        let responses = futures::stream::iter(requests)
            .buffer_unordered(3)
            .collect::<Vec<_>>()
            .await;
        // Select each field once before publishing, so two stations reporting
        // at the same epoch cannot disagree between history and the live store.
        let now = chrono::Utc::now().timestamp();
        let mut poll = Poll::none().unreachable();
        let mut reports = Vec::new();
        for (rank, station, response) in responses {
            match response {
                Ok(obs) => {
                    poll.reachable = Some(true);
                    debug!(source_id = %self.id, %station, "NWS observed fields received");
                    reports.push((rank, obs.properties));
                }
                Err(error) => {
                    warn!(source_id = %self.id, %station, %error, "NWS observation fetch failed")
                }
            }
        }
        poll.events = merge_current_observations(&self.id, &reports, now);
        poll
    }

    async fn poll_once(&self) -> Poll {
        // Current observations are independent of forecast availability and
        // cadence. A forecast outage must not discard measured wind.
        let mut poll = match self.resolve_stations().await {
            Ok(stations) if !stations.is_empty() => self.fetch_current_fields(&stations).await,
            Ok(_) => Poll::none(),
            Err(error) => {
                warn!(source_id = %self.id, %error, "NWS station lookup failed");
                Poll::none().unreachable()
            }
        };
        let now = chrono::Utc::now().timestamp();
        if forecast_due(self.last_forecast_epoch.load(Ordering::Relaxed), now) {
            match self.poll_forecast().await {
                Ok(snapshot) => {
                    let at_epoch = chrono::Utc::now().timestamp();
                    self.last_forecast_epoch.store(at_epoch, Ordering::Relaxed);
                    poll.reachable = Some(true);
                    poll.events.push(SourceEvent::Forecast {
                        source_id: self.id.clone(),
                        snapshot,
                        at_epoch,
                    });
                }
                Err(error) => {
                    warn!(source_id = %self.id, error = %format!("{error:#}"), "NWS forecast fetch failed; retaining current observations")
                }
            }
        }
        poll
    }

    async fn poll_forecast(&self) -> anyhow::Result<ForecastSnapshot> {
        let grid = self.resolve_grid().await?;
        let forecast = self.fetch_forecast(&grid).await?;
        let hourly = match self.fetch_hourly(&grid).await {
            Ok(value) => Some(value),
            Err(error) => {
                warn!(source_id = %self.id, %error, "NWS hourly fetch failed");
                None
            }
        };
        let qpf = match self.fetch_raw_grid(&grid).await {
            Ok(value) => Some(value),
            Err(error) => {
                warn!(source_id = %self.id, %error, "NWS QPF fetch failed");
                None
            }
        };
        Ok(self.build_snapshot(
            &forecast,
            hourly.as_ref(),
            qpf.as_ref(),
            chrono::Utc::now().timestamp(),
        ))
    }

    /// Fetch the latest observation for a resolved station id.
    async fn fetch_latest_observation(
        client: Client,
        api_base: String,
        station_id: String,
    ) -> anyhow::Result<ObservationResponse> {
        let url = format!("{api_base}/stations/{station_id}/observations/latest");
        let resp: ObservationResponse = client
            .get(&url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }
}

fn forecast_due(last_success: i64, now: i64) -> bool {
    last_success <= 0
        || last_success > now
        || now.saturating_sub(last_success) >= FORECAST_INTERVAL_S
}

fn merge_current_observations(
    id: &str,
    reports: &[(usize, ObservationProperties)],
    now: i64,
) -> Vec<SourceEvent> {
    use std::collections::{BTreeMap, HashMap};
    let mut selected: HashMap<WeatherField, (i64, usize, f64)> = HashMap::new();
    for (rank, props) in reports {
        let epoch = parse_epoch(props.timestamp.as_deref());
        if epoch <= 0 || epoch > now {
            continue;
        }
        for (field, value) in map_current_observation(props) {
            if !value.is_finite() {
                continue;
            }
            // Freshest nearby report; nearest station breaks equal-time ties.
            if selected
                .get(&field)
                .is_none_or(|(old, old_rank, _)| epoch > *old || (epoch == *old && rank < old_rank))
            {
                selected.insert(field, (epoch, *rank, value));
            }
        }
    }
    let mut batches: BTreeMap<i64, Vec<_>> = BTreeMap::new();
    for (field, (epoch, _, value)) in selected {
        batches.entry(epoch).or_default().push((field, value));
    }
    batches
        .into_iter()
        .map(|(at_epoch, mut fields)| {
            fields.sort_by_key(|(field, _)| field.name());
            SourceEvent::Observation {
                source_id: id.into(),
                fields,
                at_epoch,
            }
        })
        .collect()
}

/// Convert an NWS `precipitationLastHour` reading to inches, honoring the
/// declared depth unitCode. NWS reports this gauge total in METERS
/// (`wmoUnit:m`), though some stations emit `wmoUnit:mm`. The units seam's
/// rain arm understands mm/cm/in but NOT meters (where `m` collides with the
/// distance unit), so meters is normalized to mm here (x1000) before routing
/// through `units::to_canonical`; mm/cm/in fall straight through to the seam.
/// An absent/unrecognized unitCode is assumed canonical (inches), pass-through.
fn precip_last_hour_in(m: &Measured) -> Option<f64> {
    let v = m.value?;
    let bare = m.unit_code.as_deref().map(|c| {
        c.rsplit(':')
            .next()
            .unwrap_or(c)
            .trim()
            .to_ascii_lowercase()
    });
    use crate::sources::units::to_canonical;
    Some(match bare.as_deref() {
        // Meters of depth -> mm, then mm -> in via the single seam.
        Some("m") | Some("meter") | Some("meters") | Some("metre") | Some("metres") => {
            to_canonical(WeatherField::RainIntensityInHr, v * 1000.0, Some("mm"))
        }
        // mm / cm / in: hand the seam the label it already understands.
        Some(u) => to_canonical(WeatherField::RainIntensityInHr, v, Some(u)),
        // No unitCode: assume already canonical inches (pass-through).
        None => v,
    })
}

/// All station ids from a stations collection in nearest-first order, skipping
/// features that omit a stationIdentifier. Empty for a foreign/empty point.
fn station_ids(stations: &StationsResponse) -> Vec<String> {
    stations
        .features
        .iter()
        .filter_map(|f| f.properties.station_identifier.clone())
        .collect()
}

/// Map an NWS latest-observation payload to live current scalars, converting
/// each value from its declared `unitCode` to LocalSky's canonical imperial
/// unit via `units::to_canonical`. NULL-valued fields are SKIPPED (never
/// emitted as 0): a missing windGust must not read as "no wind gust = 0 mph".
/// windDirection is degrees in NWS and degrees canonically, so it passes
/// through unconverted. barometricPressure is preferred for PressureInHg;
/// seaLevelPressure is the fallback when the station omits the station-level
/// pressure.
fn map_current_observation(props: &ObservationProperties) -> Vec<(WeatherField, f64)> {
    let mut fields: Vec<(WeatherField, f64)> = Vec::new();

    // Present value + convert from its own unitCode. A null/absent value is
    // skipped, never pushed as 0.
    fn push(fields: &mut Vec<(WeatherField, f64)>, field: WeatherField, m: Option<&Measured>) {
        if let Some(meas) = m {
            if let Some(v) = meas.value {
                let Some(unit) = nws_unit_label(meas.unit_code.as_deref()) else {
                    return;
                };
                let converted = crate::sources::units::to_canonical(field, v, Some(unit));
                if converted.is_finite()
                    && (!matches!(field, WeatherField::WindMph | WeatherField::WindGustMph)
                        || converted >= 0.0)
                    && (field != WeatherField::RhPct || (0.0..=100.0).contains(&converted))
                {
                    fields.push((field, converted));
                }
            }
        }
    }

    push(
        &mut fields,
        WeatherField::AirTempF,
        props.temperature.as_ref(),
    );
    push(
        &mut fields,
        WeatherField::DewPointF,
        props.dewpoint.as_ref(),
    );
    push(
        &mut fields,
        WeatherField::RhPct,
        props.relative_humidity.as_ref(),
    );
    push(
        &mut fields,
        WeatherField::WindMph,
        props.wind_speed.as_ref(),
    );
    push(
        &mut fields,
        WeatherField::WindGustMph,
        props.wind_gust.as_ref(),
    );
    // windDirection: degrees in == degrees out (no unit conversion path).
    if let Some(m) = &props.wind_direction {
        if let Some(v) = m.value {
            fields.push((WeatherField::WindBearingDeg, v));
        }
    }
    // Pressure: prefer station-level barometricPressure, fall back to
    // seaLevelPressure when the station omits a non-null station-level value.
    // Both are Pa per unitCode -> inHg.
    let pressure = match &props.barometric_pressure {
        Some(m) if m.value.is_some() => Some(m),
        _ => props.sea_level_pressure.as_ref(),
    };
    push(&mut fields, WeatherField::PressureInHg, pressure);

    // REAL measured rain: the station gauge's last-hour accumulation. Since it
    // is a full last-hour total it reads as an in/hr rate over that hour, so we
    // emit it as RainIntensityInHr. NWS declares the depth unit per-field
    // (usually wmoUnit:m); precip_last_hour_in normalizes it to inches. A null
    // gauge (dry hour / no gauge) is skipped, never pushed as 0 (which would
    // falsely read as "confirmed no rain").
    if let Some(m) = &props.precipitation_last_hour {
        if let Some(in_hr) = precip_last_hour_in(m) {
            fields.push((WeatherField::RainIntensityInHr, in_hr));
        }
    }

    fields
}

/// Parse an RFC3339/ISO-8601 timestamp (NWS `startTime`, e.g.
/// `2026-06-24T06:00:00-04:00`) into a UTC epoch. Returns 0 on parse
/// failure or when absent, which downstream treats as "unknown time".
fn parse_epoch(start_time: Option<&str>) -> i64 {
    start_time
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// NWS gridpoint intervals use integral day/hour durations. Unsupported or
/// malformed durations remain unknown; never invent a one-hour interval.
fn iso_duration_hours(s: &str) -> Option<i64> {
    let rest = s.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let days = if date.is_empty() {
        0
    } else {
        date.strip_suffix('D')?.parse::<i64>().ok()?
    };
    let hours = if time.is_empty() {
        0
    } else {
        time.strip_suffix('H')?.parse::<i64>().ok()?
    };
    let total = days.checked_mul(24)?.checked_add(hours)?;
    (days >= 0 && hours >= 0 && total > 0).then_some(total)
}

/// Preserve the exact start and end; rounding would claim uncovered minutes.
fn parse_qpf_interval(valid_time: &str) -> Option<(i64, i64)> {
    let (start, dur) = valid_time.split_once('/')?;
    let start = chrono::DateTime::parse_from_rfc3339(start)
        .ok()?
        .timestamp();
    let end = start.checked_add(iso_duration_hours(dur)?.checked_mul(3600)?)?;
    Some((start, end))
}

fn qpf_intervals(qpf: &QpfBlock) -> Vec<(i64, i64, Option<f64>)> {
    if qpf.uom.as_deref() != Some("wmoUnit:mm") {
        return Vec::new();
    }
    qpf.values
        .iter()
        .filter_map(|v| {
            let (start, end) = parse_qpf_interval(v.valid_time.as_deref()?)?;
            let amount = v
                .value
                .filter(|v| crate::forecast::precip::valid_amount(*v))
                .map(crate::units::mm_to_in);
            Some((start, end, amount))
        })
        .collect()
}

/// Parse the max sustained wind from an NWS wind string. NWS reports
/// `windSpeed` as text: `"10 mph"`, `"5 to 10 mph"`, `"15 to 25 mph"`,
/// occasionally `""`. We take the LARGEST integer in the string (the
/// upper bound of a range), already in mph. Missing or malformed values
/// stay unknown. An explicit "Calm" or "0 mph" is a real zero.
fn parse_wind_max_mph(s: Option<&str>) -> Option<f64> {
    let text = s?.trim().to_ascii_lowercase();
    if text == "calm" {
        return Some(0.0);
    }
    let values: Vec<_> = text.strip_suffix("mph")?.split_whitespace().collect();
    let number = |text: &str| {
        text.parse::<f64>()
            .ok()
            .filter(|w| w.is_finite() && *w >= 0.0)
    };
    match values.as_slice() {
        [value] => number(value),
        [low, "to", high] => Some(number(low)?.max(number(high)?)),
        _ => None,
    }
}

/// Convert an NWS compass `windDirection` abbreviation ("N", "NNE",
/// "ESE", ...) to degrees. Returns 0 for unknown/empty (also north;
/// acceptable since wind dir is informational on the forecast strip).
fn wind_dir_to_deg(s: Option<&str>) -> u32 {
    match s.map(|d| d.trim().to_ascii_uppercase()).as_deref() {
        Some("N") => 0,
        Some("NNE") => 23,
        Some("NE") => 45,
        Some("ENE") => 68,
        Some("E") => 90,
        Some("ESE") => 113,
        Some("SE") => 135,
        Some("SSE") => 158,
        Some("S") => 180,
        Some("SSW") => 203,
        Some("SW") => 225,
        Some("WSW") => 248,
        Some("W") => 270,
        Some("WNW") => 293,
        Some("NW") => 315,
        Some("NNW") => 338,
        _ => 0,
    }
}

/// Loosely map an NWS `shortForecast` phrase to a WMO weather code. NWS
/// has no machine code in /forecast, so we keyword-match the human text.
/// Anything we can't classify returns 0 (the UI has a glyph fallback);
/// we deliberately do NOT block on a perfect WMO table.
fn wmo_from_short(s: Option<&str>) -> u32 {
    let Some(s) = s else { return 0 };
    let t = s.to_ascii_lowercase();
    // Order matters: check the more specific / severe phrases first.
    if t.contains("thunder") {
        95 // thunderstorm
    } else if t.contains("snow") || t.contains("flurr") || t.contains("blizzard") {
        73 // snow fall, moderate
    } else if t.contains("sleet") || t.contains("ice") || t.contains("freezing") {
        67 // freezing rain, heavy
    } else if t.contains("rain") && t.contains("light") {
        61 // rain, slight
    } else if t.contains("rain") || t.contains("showers") {
        63 // rain, moderate
    } else if t.contains("drizzle") {
        51 // drizzle, light
    } else if t.contains("fog") || t.contains("haze") {
        45 // fog
    } else if t.contains("mostly cloudy") || t.contains("overcast") {
        3 // overcast
    } else if t.contains("partly") || t.contains("mostly sunny") || t.contains("mostly clear") {
        2 // partly cloudy
    } else if t.contains("cloud") {
        3 // overcast / cloudy
    } else {
        // "sunny" / "clear" / "fair" and anything unmapped: clear-sky / fallback
        // (WMO 0; the glyph registry treats 0 as clear).
        0
    }
}

/// Pair consecutive NWS periods into DailyEntry rows. NWS returns 12h
/// PERIODS alternating daytime/nighttime ("Today"/"Tonight",
/// "Monday"/"Monday Night"). A day period carries the high temp; the
/// following night period carries the low. We anchor each DailyEntry on
/// a daytime period and fold in the next period when it's nighttime.
///
/// `precip_sum_in` stays unknown (the /forecast endpoint has no QPF amount).
fn parse_daily(periods: &[ForecastPeriod]) -> Vec<DailyEntry> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < periods.len() {
        let p = &periods[i];
        // A lone leading night period (forecast issued after dark) becomes
        // its own day with only a min temp.
        if !p.is_daytime {
            out.push(DailyEntry {
                // NWS stamps the period start: 06:00 local for a daytime
                // period, 18:00 for a lone night row.
                day_marker: crate::engine::clock::DayMarker::inside_local_day(parse_epoch(
                    p.start_time.as_deref(),
                )),
                weather_code: wmo_from_short(p.short_forecast.as_deref()),
                temp_max_f: None,
                temp_min_f: p.temperature.filter(|t| t.is_finite()),
                // Lone night period; backfill_daily_humidity fills from hourly.
                humidity_pct: None,
                precip_sum_in: None,
                // An absent POP stays None (NWS omits it on dry periods AND
                // on gaps); a reported value, 0 included, is kept.
                precip_probability_max: p
                    .pop
                    .as_ref()
                    .and_then(|o| o.value)
                    .map(|v| v.round() as u32),
                wind_max_mph: parse_wind_max_mph(p.wind_speed.as_deref()),
                wind_gust_max_mph: 0.0,
                uv_index_max: 0.0,
                sunrise_epoch: 0,
                sunset_epoch: 0,
                ..Default::default()
            });
            i += 1;
            continue;
        }

        // Daytime period: high temp, drives the day's code + wind.
        let mut entry = DailyEntry {
            // NWS stamps the period start: 06:00 local for a daytime
            // period, 18:00 for a lone night row.
            day_marker: crate::engine::clock::DayMarker::inside_local_day(parse_epoch(
                p.start_time.as_deref(),
            )),
            weather_code: wmo_from_short(p.short_forecast.as_deref()),
            temp_max_f: p.temperature.filter(|t| t.is_finite()),
            temp_min_f: None,
            // The daytime period's RH co-occurs with the day's high temp, so
            // it's the right pairing for the heat-index calc. None lets
            // backfill_daily_humidity fall back to the hourly window.
            humidity_pct: p
                .relative_humidity
                .as_ref()
                .and_then(|o| o.value)
                .filter(|rh| rh.is_finite() && (0.0..=100.0).contains(rh))
                .map(|rh| rh.round() as u32),
            precip_sum_in: None,
            precip_probability_max: p
                .pop
                .as_ref()
                .and_then(|o| o.value)
                .map(|v| v.round() as u32),
            wind_max_mph: parse_wind_max_mph(p.wind_speed.as_deref()),
            wind_gust_max_mph: 0.0,
            uv_index_max: 0.0,
            sunrise_epoch: 0,
            sunset_epoch: 0,
            ..Default::default()
        };

        // Fold in the paired night period for the low temp + max POP.
        if let Some(night) = periods.get(i + 1) {
            if !night.is_daytime {
                entry.temp_min_f = night.temperature.filter(|t| t.is_finite());
                // Max of the day/night POPs; either side absent defers to the
                // other, both absent stays None.
                let night_pop = night
                    .pop
                    .as_ref()
                    .and_then(|o| o.value)
                    .map(|v| v.round() as u32);
                entry.precip_probability_max = match (entry.precip_probability_max, night_pop) {
                    (Some(d), Some(n)) => Some(d.max(n)),
                    (a, b) => a.or(b),
                };
                let night_wind = parse_wind_max_mph(night.wind_speed.as_deref());
                // Both periods contribute to this peak. A known calm half
                // cannot establish a safe daily maximum over an unknown half.
                entry.wind_max_mph = entry
                    .wind_max_mph
                    .zip(night_wind)
                    .map(|(day, night)| day.max(night));
                i += 2;
                out.push(entry);
                continue;
            }
        }
        // No paired night (last period of the feed): emit day-only.
        i += 1;
        out.push(entry);
    }
    out
}

/// Map NWS hourly periods to HourlyEntry rows. The hourly endpoint
/// returns one period per hour, all daytime/nighttime flagged, with
/// temperature (F), POP, windSpeed string, windDirection, and
/// relativeHumidity.value. Rain amounts need separate QPF coverage;
/// apparent temperature and cloud cover retain their advisory defaults.
fn parse_hourly(periods: &[ForecastPeriod]) -> Vec<HourlyEntry> {
    periods
        .iter()
        .map(|p| HourlyEntry {
            time_epoch: parse_epoch(p.start_time.as_deref()),
            weather_code: wmo_from_short(p.short_forecast.as_deref()),
            temp_f: p.temperature.filter(|t| t.is_finite()),
            apparent_temp_f: 0.0,
            precip_in: None,
            precip_probability: p
                .pop
                .as_ref()
                .and_then(|o| o.value)
                .map(|v| v.round() as u32),
            wind_mph: parse_wind_max_mph(p.wind_speed.as_deref()),
            wind_dir_deg: wind_dir_to_deg(p.wind_direction.as_deref()),
            humidity_pct: p
                .relative_humidity
                .as_ref()
                .and_then(|o| o.value)
                .filter(|rh| rh.is_finite() && (0.0..=100.0).contains(rh))
                .map(|rh| rh.round() as u32),
            cloud_cover_pct: 0,
            ..Default::default()
        })
        .collect()
}

impl Nws {
    /// Build a ForecastSnapshot from the daily-period feed plus the
    /// (optional) hourly + QPF feeds. `now` is the refresh epoch. QPF
    /// (quantitative precipitation) fills the daily/hourly precip amounts the
    /// text `/forecast` feed lacks, so the engine's forecast-rain skips work.
    fn build_snapshot(
        &self,
        forecast: &ForecastResponse,
        hourly: Option<&ForecastResponse>,
        qpf: Option<&RawGridResponse>,
        now: i64,
    ) -> ForecastSnapshot {
        let mut daily = parse_daily(&forecast.properties.periods);
        let mut hourly = hourly
            .map(|h| parse_hourly(&h.properties.periods))
            .unwrap_or_default();
        if let Some(block) = qpf.and_then(|r| r.properties.qpf.as_ref()) {
            let cal = crate::timeutil::deployment_calendar();
            let intervals = qpf_intervals(block);
            for d in &mut daily {
                let Some((start, end)) = cal
                    .day_of(d.day_marker)
                    .and_then(|day| cal.day_bounds_utc(day))
                else {
                    continue;
                };
                // Day zero forecasts the remaining day, as NWS does not send
                // past QPF. Full future civil days require complete coverage,
                // including a 23/25-hour day across a clock change.
                if end > now {
                    d.precip_sum_in = crate::forecast::precip::total_over(
                        intervals.iter().copied(),
                        start.max(now),
                        end,
                    );
                }
            }
            for h in &mut hourly {
                h.precip_in = crate::forecast::precip::total_over(
                    intervals.iter().copied(),
                    h.time_epoch,
                    h.time_epoch.saturating_add(3600),
                );
            }
        }
        let mut snap = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            source_label: "NWS".to_string(),
            // NWS does not echo an IANA zone, so resolve it from the
            // point we are fetching for.
            //
            // This shipped as String::new() under a comment arguing the
            // field was unnecessary because the epochs are absolute. The
            // epochs are absolute; the field is not for display. The
            // extended-series graft uses it as its location-safety gate,
            // and an empty string disarms that gate on every default US
            // install, which is exactly the population that relies on the
            // graft.
            timezone: crate::timeutil::tz_name_for(self.location.lat, self.location.lon)
                .unwrap_or_default(),
            daily,
            past_daily: vec![],
            hourly,
            ..Default::default()
        };
        // Backfill any daily entry still missing RH from the hourly window
        // (the daytime period's relativeHumidity already populates most).
        snap.backfill_daily_humidity(crate::timeutil::deployment_calendar());
        snap
    }
}

#[async_trait]
impl WeatherSource for Nws {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        // CURRENT scalars emitted from the nearest station's latest
        // observation (map_current_observation). These surface NWS in the
        // per-field CURRENT picker (A1's from_caps path). They are NOT POP:
        // NWS observations/latest carries no probability-of-precip, so Pop is
        // a forecast-only quantity and is intentionally absent here.
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::DewPointF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindGustMph);
        fields.insert(WeatherField::WindBearingDeg);
        fields.insert(WeatherField::PressureInHg);
        // REAL measured rain from the station gauge (precipitationLastHour ->
        // RainIntensityInHr). A live in/hr rate over the last hour, so it ranks
        // as a current scalar in the per-field CURRENT picker.
        fields.insert(WeatherField::RainIntensityInHr);
        // FORECAST capabilities (drive is_forecast() picker + forecast bridge).
        fields.insert(WeatherField::ForecastDaily);
        fields.insert(WeatherField::ForecastHourly);
        SourceCaps {
            // The current scalars come from a REMOTE station's observation, not
            // a LAN sensor, so this is a cloud current source: live_current
            // stays false and a real LAN station outranks it in the merge.
            live_current: false,
            hourly_forecast_hours: 156, // NWS hourly goes ~6.5 days out
            daily_forecast_days: 7,
            radar_tiles: false,
            // NWS doesn't compute reference ET; the engine derives it.
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            // Forecast fields: solid US-government truth, higher than
            // Open-Meteo but lower than a LAN station's live readings.
            WeatherField::Pop | WeatherField::ForecastDaily | WeatherField::ForecastHourly => 60,
            // CURRENT scalars from the nearest ASOS/AWOS station: a real
            // government observation, so above model-derived clouds
            // (Open-Meteo / OpenWeather current at 25, Met.no at 20) yet below
            // any LAN station (live_current sources default ~80-100). 35 ranks
            // NWS as the strongest US-region cloud-current source without ever
            // displacing a local sensor.
            WeatherField::AirTempF
            | WeatherField::DewPointF
            | WeatherField::RhPct
            | WeatherField::WindMph
            | WeatherField::WindGustMph
            | WeatherField::WindBearingDeg
            | WeatherField::PressureInHg
            | WeatherField::RainIntensityInHr => 35,
            _ => i32::MIN,
        }
    }

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "NWS",
            POLL_INTERVAL,
            bus,
            shutdown,
            |source: Arc<Nws>| async move { Ok(source.poll_once().await) },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(epoch: i64, fields: serde_json::Value) -> ObservationProperties {
        let mut fields = fields;
        fields["timestamp"] = serde_json::json!(chrono::DateTime::from_timestamp(epoch, 0)
            .unwrap()
            .to_rfc3339());
        serde_json::from_value(fields).unwrap()
    }

    #[tokio::test]
    async fn forecast_http_failure_does_not_discard_measured_current_wind() {
        let now = chrono::Utc::now().timestamp();
        let payload = serde_json::json!({"properties": {
            "timestamp": chrono::DateTime::from_timestamp(now - 300, 0).unwrap().to_rfc3339(),
            "windSpeed": {"value": 9.252, "unitCode": "wmoUnit:km_h-1"}
        }});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route(
                "/stations/TEST/observations/latest",
                axum::routing::get(move || {
                    let payload = payload.clone();
                    async move { axum::Json(payload) }
                }),
            )
            .fallback(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE });
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let config = serde_json::from_str("{}").unwrap();
        let mut source = Nws::new("weather", config, Location::default());
        source.api_base = format!("http://{addr}");
        *source.station_cache.lock().await = Some(vec!["TEST".into()]);
        *source.grid_cache.lock().await = Some(GridPoint {
            grid_id: "TEST".into(),
            grid_x: 1,
            grid_y: 1,
        });
        let poll = source.poll_once().await;
        server.abort();
        assert_eq!(poll.reachable, Some(true));
        assert_eq!(poll.events.len(), 1);
        let SourceEvent::Observation {
            fields, at_epoch, ..
        } = &poll.events[0]
        else {
            panic!("observation")
        };
        assert_eq!(*at_epoch, now - 300);
        assert!((fields[0].1 - 5.7489).abs() < 0.001);
        assert_eq!(source.last_forecast_epoch.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn unrecognized_units_or_invalid_wind_cannot_claim_a_measurement() {
        for (value, unit) in [(8.0, "unknown"), (-1.0, "wmoUnit:m_s-1")] {
            let props = report(
                1_700_000_000,
                serde_json::json!({"windSpeed":{"value":value,"unitCode":unit}}),
            );
            assert!(map_current_observation(&props).is_empty());
        }
    }

    #[test]
    fn observations_preserve_report_age_and_fill_missing_fields_from_nearby_stations() {
        let now = 1_700_000_000;
        let reports = vec![
            (
                0,
                report(
                    now - 600,
                    serde_json::json!({"temperature":{"value":20,"unitCode":"wmoUnit:degC"}}),
                ),
            ),
            (
                1,
                report(
                    now - 300,
                    serde_json::json!({"windSpeed":{"value":18,"unitCode":"wmoUnit:km_h-1"}}),
                ),
            ),
        ];
        let events = merge_current_observations("weather", &reports, now);
        assert_eq!(events.len(), 2);
        let mut actual = std::collections::HashMap::new();
        for event in events {
            let SourceEvent::Observation {
                fields, at_epoch, ..
            } = event
            else {
                panic!("observation")
            };
            for (field, value) in fields {
                actual.insert(field, (value, at_epoch));
            }
        }
        assert_eq!(actual[&WeatherField::AirTempF], (68.0, now - 600));
        assert!((actual[&WeatherField::WindMph].0 - 11.18468).abs() < 0.001);
        assert_eq!(actual[&WeatherField::WindMph].1, now - 300);
    }

    #[test]
    fn freshest_report_wins_and_equal_time_uses_nearest_without_duplicate_history_keys() {
        let now = 1_700_000_000;
        let wind =
            |value| serde_json::json!({"windSpeed":{"value":value,"unitCode":"wmoUnit:m_s-1"}});
        let reports = vec![
            (0, report(now - 600, wind(10))),
            (2, report(now - 300, wind(5))),
            (1, report(now - 300, wind(0))),
        ];
        let events = merge_current_observations("weather", &reports, now);
        assert_eq!(events.len(), 1);
        let SourceEvent::Observation {
            fields, at_epoch, ..
        } = &events[0]
        else {
            panic!("observation")
        };
        assert_eq!(fields, &vec![(WeatherField::WindMph, 0.0)]);
        assert_eq!(*at_epoch, now - 300);
    }

    #[test]
    fn unknown_and_future_report_times_cannot_become_current_nws_observations() {
        let now = 1_700_000_000;
        let fields = serde_json::json!({"windSpeed":{"value":5,"unitCode":"wmoUnit:m_s-1"}});
        let missing = serde_json::from_value(fields.clone()).unwrap();
        assert!(merge_current_observations(
            "weather",
            &[(0, missing), (1, report(now + 60, fields))],
            now
        )
        .is_empty());
    }

    #[test]
    fn current_poll_does_not_require_a_forecast_fetch_each_time() {
        let now = 1_700_000_000;
        assert!(forecast_due(0, now));
        assert!(!forecast_due(now - POLL_INTERVAL.as_secs() as i64, now));
        assert!(forecast_due(now - FORECAST_INTERVAL_S, now));
        assert!(forecast_due(now + 1, now));
    }

    fn nws_test() -> Nws {
        Nws::new(
            "nws",
            NwsConfig {
                user_agent: "LocalSky test (test@example.com)".into(),
            },
            Location {
                lat: 40.7128,
                lon: -74.006,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_list_current_scalars_and_forecast() {
        let n = nws_test();
        let caps = n.capabilities();
        // Exactly the current scalars map_current_observation emits.
        for f in [
            WeatherField::AirTempF,
            WeatherField::DewPointF,
            WeatherField::RhPct,
            WeatherField::WindMph,
            WeatherField::WindGustMph,
            WeatherField::WindBearingDeg,
            WeatherField::PressureInHg,
            WeatherField::RainIntensityInHr,
        ] {
            assert!(
                caps.fields.contains(&f),
                "caps missing current scalar {f:?}"
            );
        }
        // Forecast capabilities still advertised (drive the forecast picker).
        assert!(caps.fields.contains(&WeatherField::ForecastDaily));
        assert!(caps.fields.contains(&WeatherField::ForecastHourly));
        // POP is forecast-only (no current observation carries it); not a cap.
        assert!(!caps.fields.contains(&WeatherField::Pop));
        // Remote-station current source, not a LAN sensor.
        assert!(!caps.live_current);
    }

    #[test]
    fn current_priority_above_model_clouds_below_lan() {
        let n = nws_test();
        // Above model-derived clouds (Open-Meteo/OpenWeather current = 25).
        assert!(n.priority(WeatherField::AirTempF) > 25);
        assert!(n.priority(WeatherField::WindMph) > 25);
        // Below a LAN station's live readings (Davis et al = 80).
        assert!(n.priority(WeatherField::AirTempF) < 80);
    }

    #[test]
    fn pop_priority_above_air_temp() {
        let n = nws_test();
        assert!(n.priority(WeatherField::Pop) > n.priority(WeatherField::AirTempF));
    }

    #[test]
    fn wind_string_parsing() {
        assert_eq!(parse_wind_max_mph(Some("10 mph")), Some(10.0));
        assert_eq!(parse_wind_max_mph(Some("5 to 10 mph")), Some(10.0));
        assert_eq!(parse_wind_max_mph(Some("15 to 25 mph")), Some(25.0));
        assert_eq!(parse_wind_max_mph(Some("0 mph")), Some(0.0));
        assert_eq!(parse_wind_max_mph(Some("Calm")), Some(0.0));
        assert_eq!(parse_wind_max_mph(Some("4.5 mph")), Some(4.5));
        for invalid in [
            None,
            Some(""),
            Some("-5 mph"),
            Some("NaN mph"),
            Some("inf mph"),
            Some("10 km/h"),
            Some("unknown 10 mph"),
        ] {
            assert_eq!(parse_wind_max_mph(invalid), None);
        }
    }

    #[test]
    fn wind_dir_mapping() {
        assert_eq!(wind_dir_to_deg(Some("N")), 0);
        assert_eq!(wind_dir_to_deg(Some("E")), 90);
        assert_eq!(wind_dir_to_deg(Some("sw")), 225);
        assert_eq!(wind_dir_to_deg(Some("ESE")), 113);
        assert_eq!(wind_dir_to_deg(None), 0);
    }

    #[test]
    fn wmo_keyword_mapping() {
        assert_eq!(wmo_from_short(Some("Sunny")), 0);
        assert_eq!(wmo_from_short(Some("Mostly Cloudy")), 3);
        assert_eq!(wmo_from_short(Some("Chance Showers And Thunderstorms")), 95);
        assert_eq!(wmo_from_short(Some("Light Rain")), 61);
        assert_eq!(wmo_from_short(Some("Snow")), 73);
        assert_eq!(wmo_from_short(None), 0);
    }

    // Minimal literal sample of an NWS /forecast response: one daytime
    // period paired with its night, then a standalone trailing day.
    const FORECAST_SAMPLE: &str = r#"{
        "properties": {
            "periods": [
                {
                    "startTime": "2026-06-24T06:00:00-04:00",
                    "isDaytime": true,
                    "temperature": 84,
                    "probabilityOfPrecipitation": { "value": 20 },
                    "windSpeed": "5 to 10 mph",
                    "windDirection": "SW",
                    "relativeHumidity": { "value": 55 },
                    "shortForecast": "Mostly Sunny"
                },
                {
                    "startTime": "2026-06-24T18:00:00-04:00",
                    "isDaytime": false,
                    "temperature": 67,
                    "probabilityOfPrecipitation": { "value": 40 },
                    "windSpeed": "10 to 15 mph",
                    "windDirection": "S",
                    "relativeHumidity": { "value": 70 },
                    "shortForecast": "Chance Showers And Thunderstorms"
                },
                {
                    "startTime": "2026-06-25T06:00:00-04:00",
                    "isDaytime": true,
                    "temperature": 80,
                    "probabilityOfPrecipitation": { "value": 10 },
                    "windSpeed": "5 mph",
                    "windDirection": "N",
                    "relativeHumidity": { "value": 50 },
                    "shortForecast": "Sunny"
                }
            ]
        }
    }"#;

    // Minimal literal sample of an NWS /forecast/hourly response.
    const HOURLY_SAMPLE: &str = r#"{
        "properties": {
            "periods": [
                {
                    "startTime": "2026-06-24T06:00:00-04:00",
                    "isDaytime": true,
                    "temperature": 72,
                    "probabilityOfPrecipitation": { "value": 15 },
                    "windSpeed": "8 mph",
                    "windDirection": "WSW",
                    "relativeHumidity": { "value": 62 },
                    "shortForecast": "Partly Cloudy"
                },
                {
                    "startTime": "2026-06-24T07:00:00-04:00",
                    "isDaytime": true,
                    "temperature": 74,
                    "probabilityOfPrecipitation": { "value": 18 },
                    "windSpeed": "9 mph",
                    "windDirection": "W",
                    "relativeHumidity": { "value": 60 },
                    "shortForecast": "Partly Cloudy"
                }
            ]
        }
    }"#;

    #[test]
    fn parse_daily_pairs_day_and_night() {
        let fc: ForecastResponse =
            serde_json::from_str(FORECAST_SAMPLE).expect("forecast sample parses");
        let daily = parse_daily(&fc.properties.periods);
        assert_eq!(daily.len(), 2);

        let d0 = &daily[0];
        // High from the daytime period, low from the paired night.
        assert_eq!(d0.temp_max_f, Some(84.0));
        assert_eq!(d0.temp_min_f, Some(67.0));
        // Max POP across the pair (day 20, night 40 -> 40).
        assert_eq!(d0.precip_probability_max, Some(40));
        // Max sustained wind across the pair (day max 10, night max 15).
        assert_eq!(d0.wind_max_mph, Some(15.0));
        // Code from the daytime shortForecast ("Mostly Sunny" -> 2).
        assert_eq!(d0.weather_code, 2);
        // Epoch is the daytime period start (sane, > 2026-01-01).
        assert!(d0.day_marker.provenance_epoch_not_an_instant().unwrap_or(0) > 1_767_225_600);

        // Trailing standalone day (no night to pair): high only.
        let d1 = &daily[1];
        assert_eq!(d1.temp_max_f, Some(80.0));
        assert_eq!(d1.temp_min_f, None);
        assert_eq!(d1.weather_code, 0); // "Sunny"
    }

    #[test]
    fn missing_periods_and_temperatures_remain_unknown() {
        // The live after-dark failure: tonight's 75°F low became a day
        // with a fabricated 0°F high. Preserve the actual low alone.
        let mut fc: ForecastResponse = serde_json::from_str(FORECAST_SAMPLE).unwrap();
        let night = &mut fc.properties.periods[1];
        night.temperature = Some(75.0);
        let daily = parse_daily(std::slice::from_ref(night));
        assert_eq!(daily[0].temp_max_f, None);
        assert_eq!(daily[0].temp_min_f, Some(75.0));
        let json = serde_json::to_value(&daily[0]).unwrap();
        assert!(json["temp_max_f"].is_null());
        assert_eq!(json["temp_min_f"], 75.0);

        // A real freezing reading is evidence, including precisely zero.
        fc.properties.periods[0].temperature = Some(0.0);
        let day = parse_daily(&fc.properties.periods[..1]);
        assert_eq!(day[0].temp_max_f, Some(0.0));
        assert_eq!(day[0].temp_min_f, None);
        assert_eq!(
            parse_hourly(&fc.properties.periods[..1])[0].temp_f,
            Some(0.0)
        );

        for invalid in [
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
        ] {
            fc.properties.periods[0].temperature = invalid;
            fc.properties.periods[1].temperature = invalid;
            let daily = parse_daily(&fc.properties.periods[..2]);
            assert_eq!(daily[0].temp_max_f, None);
            assert_eq!(daily[0].temp_min_f, None);
            assert_eq!(parse_hourly(&fc.properties.periods[..1])[0].temp_f, None);
            assert_eq!(
                parse_daily(&fc.properties.periods[1..2])[0].temp_min_f,
                None
            );
        }
    }

    #[test]
    fn unknown_wind_period_and_humidity_cannot_become_calm_dry_day() {
        let mut fc: ForecastResponse = serde_json::from_str(FORECAST_SAMPLE).unwrap();
        fc.properties.periods[0].wind_speed = Some("Calm".into());
        fc.properties.periods[1].wind_speed = None;
        let daily = parse_daily(&fc.properties.periods[..2]);
        assert_eq!(
            daily[0].wind_max_mph, None,
            "half a calm day is not a known daily peak"
        );
        assert_eq!(parse_hourly(&fc.properties.periods)[0].wind_mph, Some(0.0));
        assert_eq!(parse_hourly(&fc.properties.periods)[1].wind_mph, None);
        fc.properties.periods[1].wind_speed = Some("0 mph".into());
        assert_eq!(
            parse_daily(&fc.properties.periods[..2])[0].wind_max_mph,
            Some(0.0)
        );
        for invalid in [
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(-1.0),
            Some(101.0),
        ] {
            fc.properties.periods[0].relative_humidity = Some(PopObj { value: invalid });
            assert_eq!(
                parse_daily(&fc.properties.periods[..1])[0].humidity_pct,
                None
            );
            assert_eq!(
                parse_hourly(&fc.properties.periods[..1])[0].humidity_pct,
                None
            );
        }
        fc.properties.periods[0].relative_humidity = Some(PopObj { value: Some(0.0) });
        assert_eq!(
            parse_daily(&fc.properties.periods[..1])[0].humidity_pct,
            Some(0)
        );
        assert_eq!(
            parse_hourly(&fc.properties.periods[..1])[0].humidity_pct,
            Some(0)
        );
    }

    #[test]
    fn parse_hourly_maps_first_row() {
        let h: ForecastResponse =
            serde_json::from_str(HOURLY_SAMPLE).expect("hourly sample parses");
        let hourly = parse_hourly(&h.properties.periods);
        assert_eq!(hourly.len(), 2);

        let h0 = &hourly[0];
        assert_eq!(h0.temp_f, Some(72.0)); // already Fahrenheit
        assert_eq!(h0.precip_probability, Some(15));
        assert_eq!(h0.wind_mph, Some(8.0));
        assert_eq!(h0.wind_dir_deg, 248); // WSW
        assert_eq!(h0.humidity_pct, Some(62));
        assert_eq!(h0.weather_code, 2); // "Partly Cloudy"
        assert!(h0.time_epoch > 1_767_225_600);
    }

    #[test]
    fn build_snapshot_assembles_daily_and_hourly() {
        let n = nws_test();
        let fc: ForecastResponse = serde_json::from_str(FORECAST_SAMPLE).unwrap();
        let h: ForecastResponse = serde_json::from_str(HOURLY_SAMPLE).unwrap();
        let snap = n.build_snapshot(&fc, Some(&h), None, 1_900_000_000);
        assert_eq!(snap.last_refresh_epoch, 1_900_000_000);
        assert!(snap.source_reachable);
        assert_eq!(snap.daily.len(), 2);
        assert_eq!(snap.hourly.len(), 2);
        assert!(snap.past_daily.is_empty());
        // Daily-only fallback when hourly fetch failed upstream.
        let snap_daily_only = n.build_snapshot(&fc, None, None, 1_900_000_000);
        assert!(snap_daily_only.hourly.is_empty());
        assert_eq!(snap_daily_only.daily.len(), 2);
    }

    #[test]
    fn iso_duration_parsing() {
        assert_eq!(iso_duration_hours("PT1H"), Some(1));
        assert_eq!(iso_duration_hours("PT6H"), Some(6));
        assert_eq!(iso_duration_hours("P1DT6H"), Some(30));
        assert_eq!(iso_duration_hours("P1D"), Some(24));
        assert_eq!(iso_duration_hours("garbage"), None);
        assert_eq!(iso_duration_hours("PT0H"), None);
        assert_eq!(iso_duration_hours("PT-1H"), None);
    }

    #[test]
    fn qpf_fills_daily_and_hourly_precip() {
        // Two 6h QPF intervals, 12.7mm (0.5") then 25.4mm (1.0"), starting at a
        // known top-of-hour. Daily sum for that local day = 1.5"; each hour of
        // the first interval gets 0.5/6".
        let qpf_json = r#"{"properties":{"quantitativePrecipitation":{"uom":"wmoUnit:mm","values":[
            {"validTime":"2024-06-01T12:00:00+00:00/PT6H","value":12.7},
            {"validTime":"2024-06-01T18:00:00+00:00/PT6H","value":25.4}
        ]}}}"#;
        let raw: RawGridResponse = serde_json::from_str(qpf_json).unwrap();
        let block = raw.properties.qpf.as_ref().unwrap();
        let intervals = qpf_intervals(block);
        let start = parse_from_rfc3339_secs("2024-06-01T12:00:00+00:00");
        let total = crate::forecast::precip::total_over(
            intervals.iter().copied(),
            start,
            start + 12 * 3600,
        )
        .unwrap();
        assert!((total - 1.5).abs() < 1e-6);
        let hour =
            crate::forecast::precip::total_over(intervals.iter().copied(), start, start + 3600)
                .unwrap();
        assert!((hour - 0.5 / 6.0).abs() < 1e-6);
        assert_eq!(
            crate::forecast::precip::total_over(intervals, start, start + 24 * 3600),
            None
        );
    }

    fn parse_from_rfc3339_secs(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp()
    }

    // -------- current-observation path --------

    // A representative /stations/{id}/observations/latest payload. Metric units
    // (degC, m/s, Pa, percent, degrees) with explicit unitCodes, plus a null
    // windGust and null dewpoint (NWS routinely omits these).
    const OBS_SAMPLE: &str = r#"{
        "properties": {
            "temperature":        { "value": 21.1,    "unitCode": "wmoUnit:degC" },
            "dewpoint":           { "value": null,    "unitCode": "wmoUnit:degC" },
            "relativeHumidity":   { "value": 55.0,    "unitCode": "wmoUnit:percent" },
            "windDirection":      { "value": 230.0,   "unitCode": "wmoUnit:degree_(angle)" },
            "windSpeed":          { "value": 5.0,     "unitCode": "wmoUnit:m_s-1" },
            "windGust":           { "value": null,    "unitCode": "wmoUnit:m_s-1" },
            "barometricPressure": { "value": 101325.0,"unitCode": "wmoUnit:Pa" },
            "seaLevelPressure":   { "value": 101500.0,"unitCode": "wmoUnit:Pa" },
            "precipitationLastHour": { "value": 0.00254, "unitCode": "wmoUnit:m" }
        }
    }"#;

    fn find(fields: &[(WeatherField, f64)], f: WeatherField) -> Option<f64> {
        fields.iter().find(|(k, _)| *k == f).map(|(_, v)| *v)
    }

    #[test]
    fn observation_maps_scalars_with_unit_conversion() {
        let obs: ObservationResponse =
            serde_json::from_str(OBS_SAMPLE).expect("observation sample parses");
        let fields = map_current_observation(&obs.properties);

        // 21.1 C -> 69.98 F
        let t = find(&fields, WeatherField::AirTempF).expect("air temp present");
        assert!((t - 69.98).abs() < 0.01, "air temp = {t}");
        // RH passes through (percent -> %).
        assert_eq!(find(&fields, WeatherField::RhPct), Some(55.0));
        // 5 m/s -> ~11.18 mph
        let w = find(&fields, WeatherField::WindMph).expect("wind present");
        assert!((w - 11.18).abs() < 0.01, "wind mph = {w}");
        // Direction degrees pass through unconverted.
        assert_eq!(find(&fields, WeatherField::WindBearingDeg), Some(230.0));
        // 101325 Pa -> ~29.92 inHg (station-level barometricPressure preferred).
        let p = find(&fields, WeatherField::PressureInHg).expect("pressure present");
        assert!((p - 29.92).abs() < 0.02, "pressure inHg = {p}");
        // 0.00254 m last-hour gauge -> 2.54 mm -> 0.1 in/hr.
        let r = find(&fields, WeatherField::RainIntensityInHr).expect("rain present");
        assert!((r - 0.1).abs() < 1e-6, "rain in/hr = {r}");
    }

    #[test]
    fn precip_last_hour_unit_conversions() {
        // Meters (the usual NWS unitCode): 0.00254 m = 2.54 mm = 0.1 in.
        let m = Measured {
            value: Some(0.00254),
            unit_code: Some("wmoUnit:m".into()),
        };
        let r = precip_last_hour_in(&m).expect("meters convert");
        assert!((r - 0.1).abs() < 1e-9, "meters -> in = {r}");
        // Millimeters (some stations): 25.4 mm = 1.0 in.
        let mm = Measured {
            value: Some(25.4),
            unit_code: Some("wmoUnit:mm".into()),
        };
        let r = precip_last_hour_in(&mm).expect("mm convert");
        assert!((r - 1.0).abs() < 1e-9, "mm -> in = {r}");
        // No unitCode -> assumed canonical inches (pass-through).
        let bare = Measured {
            value: Some(0.25),
            unit_code: None,
        };
        assert_eq!(precip_last_hour_in(&bare), Some(0.25));
        // Null value -> None (skipped, never zeroed).
        let null = Measured {
            value: None,
            unit_code: Some("wmoUnit:m".into()),
        };
        assert!(precip_last_hour_in(&null).is_none());
    }

    #[test]
    fn null_precip_last_hour_is_skipped() {
        // A dry hour reports a null gauge -> RainIntensityInHr absent, NOT 0
        // (0 would falsely read as a confirmed "no rain" measurement).
        let json = r#"{"properties":{
            "precipitationLastHour": { "value": null, "unitCode": "wmoUnit:m" }
        }}"#;
        let obs: ObservationResponse = serde_json::from_str(json).unwrap();
        let fields = map_current_observation(&obs.properties);
        assert!(find(&fields, WeatherField::RainIntensityInHr).is_none());
    }

    #[test]
    fn null_fields_are_skipped_not_zeroed() {
        let obs: ObservationResponse = serde_json::from_str(OBS_SAMPLE).unwrap();
        let fields = map_current_observation(&obs.properties);
        // windGust and dewpoint were null -> absent entirely, not 0.0.
        assert!(
            find(&fields, WeatherField::WindGustMph).is_none(),
            "null windGust must be skipped, not emitted as 0"
        );
        assert!(
            find(&fields, WeatherField::DewPointF).is_none(),
            "null dewpoint must be skipped, not emitted as 0"
        );
    }

    #[test]
    fn km_h_wind_unit_converts() {
        // Newer stations report windSpeed in km/h.
        let json = r#"{"properties":{
            "windSpeed": { "value": 18.0, "unitCode": "wmoUnit:km_h-1" }
        }}"#;
        let obs: ObservationResponse = serde_json::from_str(json).unwrap();
        let fields = map_current_observation(&obs.properties);
        // 18 km/h -> ~11.18 mph
        let w = find(&fields, WeatherField::WindMph).expect("wind present");
        assert!((w - 11.18).abs() < 0.02, "km/h wind = {w}");
    }

    #[test]
    fn pressure_falls_back_to_sea_level_when_barometric_null() {
        // Station omits station-level pressure (null) but reports sea-level.
        let json = r#"{"properties":{
            "barometricPressure": { "value": null,     "unitCode": "wmoUnit:Pa" },
            "seaLevelPressure":   { "value": 101500.0, "unitCode": "wmoUnit:Pa" }
        }}"#;
        let obs: ObservationResponse = serde_json::from_str(json).unwrap();
        let fields = map_current_observation(&obs.properties);
        let p = find(&fields, WeatherField::PressureInHg).expect("pressure present via fallback");
        // 101500 Pa -> ~29.97 inHg
        assert!((p - 29.97).abs() < 0.02, "fallback pressure = {p}");
    }

    #[test]
    fn empty_observation_yields_no_current() {
        // A foreign/empty observation (all null or absent) emits nothing, so
        // the run loop sends no Observation (US-only: no panic, no zeros).
        let json = r#"{"properties":{
            "temperature": { "value": null, "unitCode": "wmoUnit:degC" }
        }}"#;
        let obs: ObservationResponse = serde_json::from_str(json).unwrap();
        assert!(map_current_observation(&obs.properties).is_empty());
    }

    #[test]
    fn nws_unit_labels_map_to_canonical_inputs() {
        assert_eq!(nws_unit_label(Some("wmoUnit:degC")), Some("c"));
        assert_eq!(nws_unit_label(Some("wmoUnit:percent")), Some("%"));
        assert_eq!(nws_unit_label(Some("wmoUnit:m_s-1")), Some("m/s"));
        assert_eq!(nws_unit_label(Some("wmoUnit:km_h-1")), Some("km/h"));
        assert_eq!(nws_unit_label(Some("wmoUnit:Pa")), Some("pa"));
        // Angle / unknown -> None (pass-through in to_canonical).
        assert_eq!(nws_unit_label(Some("wmoUnit:degree_(angle)")), None);
        assert_eq!(nws_unit_label(None), None);
    }

    #[test]
    fn station_ids_preserve_nearest_first_order() {
        let json = r#"{
            "features": [
                { "properties": { "stationIdentifier": "KNYC" } },
                { "properties": { "stationIdentifier": "KJFK" } }
            ]
        }"#;
        let stations: StationsResponse = serde_json::from_str(json).unwrap();
        // Full nearest-first list (so the run loop can fall through to KJFK
        // when KNYC reports no scalars).
        assert_eq!(station_ids(&stations), vec!["KNYC", "KJFK"]);
        // Empty collection -> empty Vec (point with no stations, e.g. foreign).
        let empty: StationsResponse = serde_json::from_str(r#"{"features":[]}"#).unwrap();
        assert!(station_ids(&empty).is_empty());
    }

    #[test]
    fn points_payload_exposes_observation_stations_url() {
        // The /points response now also carries the stations-collection URL.
        let json = r#"{
            "properties": {
                "gridId": "OKX",
                "gridX": 33,
                "gridY": 35,
                "observationStations": "https://api.weather.gov/gridpoints/OKX/33,35/stations"
            }
        }"#;
        let p: PointsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(p.properties.grid_id, "OKX");
        assert_eq!(
            p.properties.observation_stations.as_deref(),
            Some("https://api.weather.gov/gridpoints/OKX/33,35/stations")
        );
    }
    #[test]
    fn missing_qpf_keeps_fresh_forecast_rain_unknown() {
        let fc: ForecastResponse = serde_json::from_str(FORECAST_SAMPLE).unwrap();
        let hourly: ForecastResponse = serde_json::from_str(HOURLY_SAMPLE).unwrap();
        let snapshot = nws_test().build_snapshot(&fc, Some(&hourly), None, 1700000000);
        assert!(snapshot.source_reachable);
        assert!(snapshot.daily.iter().all(|d| d.precip_sum_in.is_none()));
        assert!(snapshot.hourly.iter().all(|h| h.precip_in.is_none()));
        assert_eq!(
            snapshot.next_n_hours_precip_in(4, snapshot.hourly[0].time_epoch),
            None
        );
    }

    #[test]
    fn qpf_zero_has_coverage_while_null_invalid_and_duplicate_intervals_do_not() {
        let start = parse_from_rfc3339_secs("2026-06-24T10:00:00Z");
        let mut block: QpfBlock = serde_json::from_value(serde_json::json!({
            "uom":"wmoUnit:mm", "values":[{"validTime":"2026-06-24T10:00:00Z/PT6H", "value":0.0}]
        }))
        .unwrap();
        assert_eq!(
            crate::forecast::precip::total_over(qpf_intervals(&block), start, start + 6 * 3600),
            Some(0.0)
        );
        assert_eq!(
            crate::forecast::precip::total_over(qpf_intervals(&block), start, start + 7 * 3600),
            None
        );
        for value in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            block.values[0].value = value;
            assert_eq!(
                crate::forecast::precip::total_over(qpf_intervals(&block), start, start + 3600),
                None
            );
        }
        block.values[0].value = Some(0.0);
        block.values.push(QpfValue {
            valid_time: block.values[0].valid_time.clone(),
            value: Some(0.0),
        });
        assert_eq!(
            crate::forecast::precip::total_over(qpf_intervals(&block), start, start + 3600),
            None
        );
        block.values.truncate(1);
        block.uom = Some("unknown".into());
        assert!(qpf_intervals(&block).is_empty());
    }

    #[test]
    fn qpf_daily_total_requires_remaining_today_and_complete_future_day() {
        let fc: ForecastResponse = serde_json::from_str(FORECAST_SAMPLE).unwrap();
        let cal = crate::timeutil::deployment_calendar();
        let day = cal
            .day_of(parse_daily(&fc.properties.periods)[0].day_marker)
            .unwrap();
        let (start, end) = cal.day_bounds_utc(day).unwrap();
        let now = start + 12 * 3600;
        let stamp = chrono::DateTime::from_timestamp(now, 0)
            .unwrap()
            .to_rfc3339();
        let qpf: RawGridResponse = serde_json::from_value(serde_json::json!({"properties":{
            "quantitativePrecipitation":{"uom":"wmoUnit:mm", "values":[
                {"validTime":format!("{stamp}/PT{}H", (end-now)/3600),"value":0.0}
            ]}
        }}))
        .unwrap();
        let snapshot = nws_test().build_snapshot(&fc, None, Some(&qpf), now);
        assert_eq!(snapshot.daily[0].precip_sum_in, Some(0.0));
        assert_eq!(snapshot.daily[1].precip_sum_in, None);
    }
}
