// Synoptic Data (MesoWest) real-station observation source,
// api.synopticdata.com.
//
// Requires a free public API token from synopticdata.com. Global coverage with
// especially dense US mesonet coverage (NWS/FAA, RAWS, mesonets, personal
// stations). Like NWS this is a REAL instrument observation from the nearest
// reporting station, not a model or an analysis, but Synoptic's network is far
// denser than the NWS ASOS/AWOS-only feed, so the nearest station is usually
// closer to the user's yard.
//
// Endpoint (single call per poll):
//   GET /v2/stations/nearesttime
//         ?token={token}
//         &vars=wind_speed,wind_direction,sea_level_pressure,air_temp,relative_humidity
//         &within=60                 (accept an obs up to 60 min old)
//         &units=english,speed|mph,pres|inhg   (see Units below; | -> %7C in URL)
//         &radius={lat},{lon},{radius_mi}   (nearest station in radius)
//     -- or, when a station id is pinned --
//         &stid={station_id}
//
// The response carries a STATION array (nearest first); each STATION has an
// OBSERVATIONS object whose per-variable latest sample is keyed `<var>_value_1`
// (or `<var>_value_1d` for a DERIVED value, e.g. sea-level pressure) as
// `{ "date_time": ..., "value": <number|null> }`. We parse the first STATION that
// yields any non-null mapped scalar into WeatherField values and emit them as one
// live current Observation on the same SourceEvent bus + reachability pattern
// nws.rs uses.
//
// Units (verified against the live API 2026-07-01): bare `units=english` returns
// wind_speed in KNOTS and sea_level_pressure in MILLIBARS, NOT the imperial units
// LocalSky expects. So we override the components: `units=english,speed|mph,
// pres|inhg` (the `|` percent-encoded as %7C so the raw URL parses). Then air_temp
// is F, wind_speed mph, sea_level_pressure inHg, RH percent, wind_direction
// degrees: every scalar is LocalSky canonical, no per-field conversion seam. A
// null value (the station did not report that variable) is SKIPPED, never 0.
//
// These emit as a live current Observation, but `live_current` stays FALSE in
// capabilities: it is a cloud current source (a remote station's reading), NOT a
// LAN station, so a real LAN sensor outranks it in the per-field merge. Synoptic
// is an observation source only: it emits no ForecastSnapshot and is excluded
// from `is_forecast()`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, SynopticConfig};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.synopticdata.com/v2";
const POLL_INTERVAL: Duration = Duration::from_secs(10 * 60); // 10 min
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Accept an observation up to this many minutes old (Synoptic `within` param).
const WITHIN_MINUTES: u32 = 60;

pub struct Synoptic {
    id: String,
    config: SynopticConfig,
    location: Location,
}

/// Top-level nearesttime response: a STATION array (nearest first).
#[derive(Debug, Deserialize)]
struct NearestTimeResponse {
    #[serde(rename = "STATION", default)]
    station: Vec<Station>,
}

#[derive(Debug, Deserialize)]
struct Station {
    /// The station id (STID) of this observation. Parsed for Debug/tracing
    /// parity with the API shape; the merge keys off the source id, not the
    /// station, so it is not read on the hot path.
    #[serde(rename = "STID", default)]
    #[allow(dead_code)]
    stid: Option<String>,
    #[serde(rename = "OBSERVATIONS", default)]
    observations: Option<Observations>,
}

/// Synoptic's OBSERVATIONS block. Each requested variable's latest sample is
/// keyed `<var>_value_1` (the "_1" set is the most-recent value set for
/// nearesttime). Each is a `{ date_time, value }` object; `value` is null when
/// the station did not report that variable this cycle.
#[derive(Debug, Deserialize)]
struct Observations {
    #[serde(rename = "air_temp_value_1", alias = "air_temp_value_1d", default)]
    air_temp: Option<ObsValue>,
    #[serde(
        rename = "relative_humidity_value_1",
        alias = "relative_humidity_value_1d",
        default
    )]
    relative_humidity: Option<ObsValue>,
    #[serde(rename = "wind_speed_value_1", alias = "wind_speed_value_1d", default)]
    wind_speed: Option<ObsValue>,
    #[serde(
        rename = "wind_direction_value_1",
        alias = "wind_direction_value_1d",
        default
    )]
    wind_direction: Option<ObsValue>,
    // Sea-level pressure is a DERIVED value at most stations (Synoptic computes it
    // from station pressure + elevation), so the live API keys it
    // `sea_level_pressure_value_1d` (confirmed against a live response), not
    // `_value_1`. Accept BOTH so a station that reports it directly still parses.
    #[serde(
        rename = "sea_level_pressure_value_1",
        alias = "sea_level_pressure_value_1d",
        default
    )]
    sea_level_pressure: Option<ObsValue>,
}

/// One Synoptic observation sample: `{ "date_time": "...", "value": <n|null> }`.
/// `value` null = the station did not report it (skip, never zero).
#[derive(Debug, Deserialize)]
struct ObsValue {
    value: Option<f64>,
}

impl Synoptic {
    pub fn new(id: impl Into<String>, config: SynopticConfig, location: Location) -> Self {
        Self {
            id: id.into(),
            config,
            location,
        }
    }

    /// Build the nearesttime request URL. When a station id is pinned we request
    /// that exact station (`stid`); otherwise we ask for the nearest station in
    /// the configured radius around the deployment lat/lon. Units matter: bare
    /// `units=english` returns wind in KNOTS and pressure in MILLIBARS (verified
    /// against the live API), so we override the components to LocalSky's
    /// canonical imperial: `speed|mph` and `pres|inhg` (the `|` percent-encoded
    /// as %7C so the raw URL parses). air_temp is F and RH is % either way.
    fn build_url(&self) -> String {
        let base = format!(
            "{API_BASE}/stations/nearesttime\
             ?token={token}\
             &vars=wind_speed,wind_direction,sea_level_pressure,air_temp,relative_humidity\
             &within={within}\
             &units=english,speed%7Cmph,pres%7Cinhg\
             &obtimezone=utc",
            token = self.config.token,
            within = WITHIN_MINUTES,
        );
        match self
            .config
            .station_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            // A pinned station id: request exactly it.
            Some(stid) => format!("{base}&stid={stid}"),
            // No pin: nearest station within `radius_mi` of the deployment point.
            // Synoptic's radius param is `lat,lon,miles`.
            None => format!(
                "{base}&radius={lat},{lon},{radius}",
                lat = self.location.lat,
                lon = self.location.lon,
                radius = self.config.radius_mi,
            ),
        }
    }

    /// Fetch + parse the nearesttime response. Routes through net::safe_fetch
    /// (SSRF-hardened: forbidden-target filter, resolved-IP pin, no redirects)
    /// like the other keyed cloud sources.
    async fn fetch(&self) -> anyhow::Result<NearestTimeResponse> {
        let url = self.build_url();
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(&url, FETCH_TIMEOUT).await?;
        let resp = client.get(safe_url).send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }
}

/// Map a Synoptic STATION's OBSERVATIONS block to live current scalars. We
/// requested `units=english`, so every value is already LocalSky canonical
/// imperial (air_temp F, wind_speed mph, sea_level_pressure inHg, RH percent,
/// wind_direction degrees); no conversion seam is needed. NULL-valued samples
/// are SKIPPED (never emitted as 0): a missing wind must not read as "0 mph".
fn map_observations(obs: &Observations) -> Vec<(WeatherField, f64)> {
    let mut fields: Vec<(WeatherField, f64)> = Vec::new();

    // Push a present, non-null sample; skip null/absent (never a 0).
    fn push(fields: &mut Vec<(WeatherField, f64)>, field: WeatherField, m: Option<&ObsValue>) {
        if let Some(v) = m.and_then(|o| o.value) {
            fields.push((field, v));
        }
    }

    push(&mut fields, WeatherField::AirTempF, obs.air_temp.as_ref());
    push(
        &mut fields,
        WeatherField::RhPct,
        obs.relative_humidity.as_ref(),
    );
    push(&mut fields, WeatherField::WindMph, obs.wind_speed.as_ref());
    push(
        &mut fields,
        WeatherField::WindBearingDeg,
        obs.wind_direction.as_ref(),
    );
    push(
        &mut fields,
        WeatherField::PressureInHg,
        obs.sea_level_pressure.as_ref(),
    );

    fields
}

/// Walk the nearest-first STATION list and return the mapped scalars from the
/// first station that yields any non-null field. Synoptic can return a nearest
/// station whose OBSERVATIONS are all null this cycle (a site that only reports
/// hourly), so we fall through to the next-nearest instead of going silent.
/// Returns an empty Vec when no station produced any field.
fn first_mapped_fields(resp: &NearestTimeResponse) -> Vec<(WeatherField, f64)> {
    for station in &resp.station {
        if let Some(obs) = &station.observations {
            let fields = map_observations(obs);
            if !fields.is_empty() {
                return fields;
            }
        }
    }
    Vec::new()
}

#[async_trait]
impl WeatherSource for Synoptic {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        // CURRENT scalars emitted from the nearest station's latest observation
        // (map_observations). These surface Synoptic in the per-field CURRENT
        // picker. Synoptic nearesttime carries no probability-of-precip and no
        // forecast, so Pop / ForecastDaily / ForecastHourly are intentionally
        // absent.
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindBearingDeg);
        fields.insert(WeatherField::PressureInHg);
        SourceCaps {
            // A REMOTE station's observation, not a LAN sensor, so live_current
            // stays false and a real LAN station outranks it in the merge.
            live_current: false,
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            // CURRENT scalars from the nearest mesonet station: a real
            // observation, so above model-derived clouds (Open-Meteo /
            // OpenWeather current at 25, Met.no at 20) yet below any LAN station
            // (live_current sources default ~80-100). 35 ranks Synoptic
            // alongside NWS as a strong cloud-current source without ever
            // displacing a local sensor.
            WeatherField::AirTempF
            | WeatherField::RhPct
            | WeatherField::WindMph
            | WeatherField::WindBearingDeg
            | WeatherField::PressureInHg => 35,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "Synoptic source started");
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
                            let fields = first_mapped_fields(&resp);
                            if !fields.is_empty() {
                                debug!(
                                    source_id = %self.id,
                                    fields_n = fields.len(),
                                    "Synoptic current observation updated"
                                );
                                let _ = bus.send(SourceEvent::Observation {
                                    source_id: self.id.clone(),
                                    fields,
                                    at_epoch: chrono::Utc::now().timestamp(),
                                });
                            } else {
                                debug!(
                                    source_id = %self.id,
                                    stations_n = resp.station.len(),
                                    "Synoptic stations reported no current scalars this cycle"
                                );
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "Synoptic fetch failed");
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
                        info!(source_id = %self.id, "Synoptic source shutdown");
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

    fn synoptic_test() -> Synoptic {
        Synoptic::new(
            "synoptic",
            SynopticConfig {
                token: "test".into(),
                station_id: None,
                radius_mi: 25.0,
            },
            Location {
                lat: 40.7128,
                lon: -74.006,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_list_current_scalars_only() {
        let s = synoptic_test();
        let caps = s.capabilities();
        // Exactly the current scalars map_observations emits.
        for f in [
            WeatherField::AirTempF,
            WeatherField::RhPct,
            WeatherField::WindMph,
            WeatherField::WindBearingDeg,
            WeatherField::PressureInHg,
        ] {
            assert!(
                caps.fields.contains(&f),
                "caps missing current scalar {f:?}"
            );
        }
        // Observation source only: no forecast caps, no POP.
        assert!(!caps.fields.contains(&WeatherField::ForecastDaily));
        assert!(!caps.fields.contains(&WeatherField::ForecastHourly));
        assert!(!caps.fields.contains(&WeatherField::Pop));
        // Remote-station current source, not a LAN sensor.
        assert!(!caps.live_current);
    }

    #[test]
    fn current_priority_above_model_clouds_below_lan() {
        let s = synoptic_test();
        // Above model-derived clouds (Open-Meteo/OpenWeather current = 25).
        assert!(s.priority(WeatherField::AirTempF) > 25);
        assert!(s.priority(WeatherField::WindMph) > 25);
        // Below a LAN station's live readings (Davis et al = 80).
        assert!(s.priority(WeatherField::AirTempF) < 80);
        // A field it does not provide is i32::MIN (never a candidate).
        assert_eq!(s.priority(WeatherField::ForecastDaily), i32::MIN);
    }

    #[test]
    fn nearest_url_carries_token_vars_and_radius() {
        let s = synoptic_test();
        let url = s.build_url();
        assert!(url.starts_with("https://api.synopticdata.com/v2/stations/nearesttime"));
        assert!(url.contains("token=test"));
        // Must override to imperial components: bare english is knots + millibars.
        assert!(
            url.contains("units=english,speed%7Cmph,pres%7Cinhg"),
            "url must request mph + inHg, not bare english (knots/mb): {url}"
        );
        // All five requested vars are present.
        for v in [
            "wind_speed",
            "wind_direction",
            "sea_level_pressure",
            "air_temp",
            "relative_humidity",
        ] {
            assert!(url.contains(v), "url missing var {v}");
        }
        // No pinned station -> radius search around the deployment point.
        assert!(url.contains("radius=40.7128,-74.006,25"));
        assert!(!url.contains("stid="));
    }

    #[test]
    fn pinned_station_url_uses_stid_not_radius() {
        let s = Synoptic::new(
            "synoptic",
            SynopticConfig {
                token: "tok".into(),
                station_id: Some("KSLC".into()),
                radius_mi: 25.0,
            },
            Location {
                lat: 40.7,
                lon: -111.9,
                elevation_m: None,
            },
        );
        let url = s.build_url();
        assert!(url.contains("stid=KSLC"), "pinned url must carry stid");
        assert!(
            !url.contains("radius="),
            "a pinned station must not also send a radius"
        );
    }

    // A minimal but realistic Synoptic /v2/stations/nearesttime response: two
    // STATIONs (nearest first). The nearest reports every variable null this
    // cycle (a site that only reports hourly), so the parser must fall through
    // to the next-nearest, which has a live reading.
    const NEARESTTIME_SAMPLE: &str = r#"{
        "SUMMARY": { "RESPONSE_CODE": 1, "RESPONSE_MESSAGE": "OK" },
        "STATION": [
            {
                "STID": "QUIET1",
                "NAME": "Silent Co-op",
                "OBSERVATIONS": {
                    "air_temp_value_1": { "date_time": "2026-06-24T18:00:00Z", "value": null },
                    "wind_speed_value_1": { "date_time": "2026-06-24T18:00:00Z", "value": null }
                }
            },
            {
                "STID": "KNYC",
                "NAME": "New York City Central Park",
                "OBSERVATIONS": {
                    "air_temp_value_1": { "date_time": "2026-06-24T18:15:00Z", "value": 84.2 },
                    "relative_humidity_value_1": { "date_time": "2026-06-24T18:15:00Z", "value": 55.0 },
                    "wind_speed_value_1": { "date_time": "2026-06-24T18:15:00Z", "value": 9.0 },
                    "wind_direction_value_1": { "date_time": "2026-06-24T18:15:00Z", "value": 210.0 },
                    "sea_level_pressure_value_1d": { "date_time": "2026-06-24T18:15:00Z", "value": 29.92 }
                }
            }
        ]
    }"#;

    #[test]
    fn parse_nearesttime_maps_english_units_to_weather_fields() {
        let resp: NearestTimeResponse =
            serde_json::from_str(NEARESTTIME_SAMPLE).expect("nearesttime sample parses");
        assert_eq!(resp.station.len(), 2);

        // The first (nearest) station is all-null this cycle, so the parser
        // falls through to the next-nearest KNYC with a live reading.
        let fields = first_mapped_fields(&resp);
        assert_eq!(
            fields.len(),
            5,
            "KNYC reports all five requested scalars, no nulls"
        );

        let get = |want: WeatherField| {
            fields
                .iter()
                .find(|(f, _)| *f == want)
                .map(|(_, v)| *v)
                .unwrap_or_else(|| panic!("missing field {want:?}"))
        };
        // units=english -> already LocalSky canonical imperial, no conversion.
        assert!((get(WeatherField::AirTempF) - 84.2).abs() < 0.001);
        assert!((get(WeatherField::RhPct) - 55.0).abs() < 0.001);
        assert!((get(WeatherField::WindMph) - 9.0).abs() < 0.001);
        assert!((get(WeatherField::WindBearingDeg) - 210.0).abs() < 0.001);
        assert!((get(WeatherField::PressureInHg) - 29.92).abs() < 0.001);
    }

    #[test]
    fn null_values_are_skipped_never_zero() {
        // A station whose only reported variable is null must yield no field
        // (a missing wind is not "0 mph"), so map_observations returns empty.
        let obs: Observations = serde_json::from_str(
            r#"{
                "air_temp_value_1": { "value": null },
                "wind_speed_value_1": { "value": null }
            }"#,
        )
        .expect("observations parse");
        assert!(map_observations(&obs).is_empty());
    }

    #[test]
    fn partial_station_emits_only_present_scalars() {
        // A station that reports only temp + wind (no pressure/RH/dir) emits
        // exactly those two, never zero-padded placeholders for the rest.
        let obs: Observations = serde_json::from_str(
            r#"{
                "air_temp_value_1": { "value": 70.0 },
                "wind_speed_value_1": { "value": 4.0 }
            }"#,
        )
        .expect("observations parse");
        let fields = map_observations(&obs);
        assert_eq!(fields.len(), 2);
        assert!(fields.iter().any(|(f, _)| *f == WeatherField::AirTempF));
        assert!(fields.iter().any(|(f, _)| *f == WeatherField::WindMph));
    }

    #[test]
    fn empty_station_list_yields_no_fields() {
        let resp: NearestTimeResponse =
            serde_json::from_str(r#"{ "STATION": [] }"#).expect("empty station list parses");
        assert!(first_mapped_fields(&resp).is_empty());
    }
}
