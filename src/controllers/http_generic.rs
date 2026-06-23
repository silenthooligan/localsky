// Generic HTTP/REST controller for DIY / ESP32 irrigation boards.
//
// LocalSky drives the board over a tiny, documented REST contract. Any board
// that implements these five endpoints is a first-class controller, with full
// status readback, zone discovery, and a wizard "test connection" experience,
// unlike the fire-and-forget mqtt_command sink.
//
//   GET  {base}/status          -> { "firmware": "1.0",                  (optional)
//                                     "zones": [ { "id": "1",
//                                                  "running": true,
//                                                  "remaining_s": 120 } ],
//                                     "flow_gpm": 0.0,                    (optional)
//                                     "rain": false }                    (optional)
//   GET  {base}/zones           -> { "zones": [ { "id": "1", "name": "Back Yard" } ] }
//   POST {base}/zone/{id}/run     body { "seconds": 600 }
//   POST {base}/zone/{id}/stop
//   POST {base}/stop_all
//
// Auth is an optional bearer token sent as `Authorization: Bearer <token>`.
// Success is any HTTP 2xx; 401 is mapped to AuthFailed.
//
// Zone <-> board mapping: the board identifies each zone by an `id` (the field
// it returns in /status and /zones, and the {id} segment in run/stop URLs). On
// the LocalSky side that same id is stored as the zone's `controller_station`.
// So: board `id` == LocalSky `controller_station` (any string the board uses,
// e.g. "1" or "back_yard").
//
// Every request goes through net::safe_fetch (SSRF-hardened, IP-pinned, no
// redirects) so an operator-supplied base_url can't be turned into an exfil
// channel. RFC1918/ULA stays allowed because DIY boards live on the LAN.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use crate::config::schema::HttpGenericConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, DiscoveredZone,
    IrrigationController, RunHandle, RunRecord, ZoneRuntimeStatus,
};

/// Per-request HTTP timeout. DIY boards are on the LAN; keep it tight.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on a single zone run (2h), matching the API's RUN_SECONDS_MAX.
const MAX_RUN_SECONDS: u32 = 7200;

/// Percent-encode a station id for safe use as a single URL path segment.
/// Station ids come from operator config (`controller_station`), so encoding
/// stops a stray `/`, `?`, `#`, or space from injecting path/query structure
/// into the request. Unreserved chars (RFC 3986) pass through untouched.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct HttpGeneric {
    id: String,
    config: HttpGenericConfig,
    /// zone_slug -> board station id. Built from config.zones before
    /// construction (mirrors OpenSprinklerDirect).
    zone_to_station: Arc<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    #[serde(default)]
    firmware: Option<String>,
    #[serde(default)]
    zones: Vec<StatusZone>,
    #[serde(default)]
    flow_gpm: Option<f64>,
    #[serde(default)]
    rain: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct StatusZone {
    id: String,
    #[serde(default)]
    running: bool,
    #[serde(default)]
    remaining_s: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ZonesResponse {
    #[serde(default)]
    zones: Vec<ZonesEntry>,
}

#[derive(Debug, Deserialize)]
struct ZonesEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

impl HttpGeneric {
    pub fn new(
        id: impl Into<String>,
        config: HttpGenericConfig,
        zone_to_station: HashMap<String, String>,
    ) -> Result<Self, ControllerError> {
        Ok(Self {
            id: id.into(),
            config,
            zone_to_station: Arc::new(zone_to_station),
        })
    }

    fn base(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    fn station_for(&self, slug: &str) -> Result<String, ControllerError> {
        self.zone_to_station
            .get(slug)
            .cloned()
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    /// Reverse-map a board station id back to a LocalSky zone slug for
    /// status/discovery readback. Falls back to a synthetic slug.
    fn slug_for_station(&self, station: &str) -> String {
        self.zone_to_station
            .iter()
            .find_map(|(slug, sid)| (sid == station).then(|| slug.clone()))
            .unwrap_or_else(|| format!("station_{station}"))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, ControllerError> {
        let url = format!("{}{}", self.base(), path);
        let (client, safe_url) = crate::net::safe_fetch::build_safe_client(&url, HTTP_TIMEOUT)
            .await
            .map_err(|e| ControllerError::Init(e.to_string()))?;
        let mut req = client.get(safe_url);
        if let Some(token) = &self.config.bearer_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| {
            ControllerError::Transport(crate::net::reqwest_error_category(&e).to_string())
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ControllerError::AuthFailed);
        }
        if !status.is_success() {
            // Status only: never reflect the upstream body (operator-supplied
            // base_url -> SSRF exfil channel otherwise), matching sibling adapters.
            return Err(ControllerError::Remote(format!("HTTP {status}")));
        }
        let body = resp.text().await.map_err(|e| {
            ControllerError::Transport(crate::net::reqwest_error_category(&e).to_string())
        })?;
        serde_json::from_str(&body)
            .map_err(|_| ControllerError::Remote("unexpected response shape".into()))
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<(), ControllerError> {
        let url = format!("{}{}", self.base(), path);
        let (client, safe_url) = crate::net::safe_fetch::build_safe_client(&url, HTTP_TIMEOUT)
            .await
            .map_err(|e| ControllerError::Init(e.to_string()))?;
        let mut req = client.post(safe_url).json(&body);
        if let Some(token) = &self.config.bearer_token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| {
            ControllerError::Transport(crate::net::reqwest_error_category(&e).to_string())
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ControllerError::AuthFailed);
        }
        if !status.is_success() {
            return Err(ControllerError::Remote(format!("HTTP {status}")));
        }
        Ok(())
    }
}

#[async_trait]
impl IrrigationController for HttpGeneric {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            // The contract can carry flow + rain; whether a given board
            // actually reports them is a per-device fact surfaced in status().
            flow_meter: true,
            rain_sensor: true,
            master_valve: false,
            // Independent HTTP calls per zone; the board decides if it can run
            // zones in parallel. Assume yes (it can serialize internally).
            multi_zone_parallel: true,
            history_query: false,
            remote_program_upload: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let station = self.station_for(slug)?;
        // Defensive cap, matching the API's RUN_SECONDS_MAX (7200s = 2h). A DIY
        // board should also enforce its own max-runtime watchdog, but never
        // trust the caller to do so: a stuck-open valve is the worst case.
        let clamped = duration_s.min(MAX_RUN_SECONDS).max(1);
        if clamped != duration_s {
            warn!(
                controller = %self.id,
                slug = %slug,
                requested = duration_s,
                clamped = clamped,
                "http_generic clamping run duration to 7200s"
            );
        }
        self.post(
            &format!("/zone/{}/run", encode_segment(&station)),
            json!({ "seconds": clamped }),
        )
        .await?;
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: now_epoch(),
            planned_duration_s: clamped,
            provider_ref: Some(station),
        })
    }

    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        let station = self.station_for(slug)?;
        self.post(&format!("/zone/{}/stop", encode_segment(&station)), json!({}))
            .await
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        // Prefer the single stop_all endpoint. If the board doesn't implement it
        // (or it errors), fall back to stopping each known zone individually so a
        // missing/failing endpoint never leaves valves open (mirrors mqtt_command).
        let primary = self.post("/stop_all", json!({})).await;
        if primary.is_ok() {
            return Ok(());
        }
        // Nothing to fall back to (e.g. the wizard test controller has no zone
        // map): surface the original error rather than a false success.
        if self.zone_to_station.is_empty() {
            return primary;
        }
        let mut last_err: Option<ControllerError> = None;
        for station in self.zone_to_station.values() {
            if let Err(e) = self
                .post(&format!("/zone/{}/stop", encode_segment(station)), json!({}))
                .await
            {
                warn!(
                    controller = %self.id,
                    station = %station,
                    error = %e,
                    "http_generic stop_all fallback: per-zone stop failed"
                );
                last_err = Some(e);
            }
        }
        match last_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let r: StatusResponse = self.get_json("/status").await?;
        let zone_states = r
            .zones
            .into_iter()
            .map(|z| ZoneRuntimeStatus {
                slug: self.slug_for_station(&z.id),
                running: z.running,
                remaining_s: z.remaining_s.filter(|_| z.running),
                last_run_epoch: None,
            })
            .collect();
        Ok(ControllerStatus {
            reachable: true,
            master_enabled: None,
            water_level_pct: None,
            rain_sensor_tripped: r.rain,
            current_program: None,
            zone_states,
            flow_gpm: r.flow_gpm,
            // A board that reports a flow_gpm field at all is declaring a flow
            // sensor is wired in (mirrors OpenSprinkler's flow_connected).
            flow_connected: r.flow_gpm.is_some(),
            firmware: r.firmware,
        })
    }

    async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        // The contract has no history endpoint; the runs store holds the
        // local log.
        Ok(Vec::new())
    }

    async fn discover_zones(&self) -> ControllerResult<Vec<DiscoveredZone>> {
        let r: ZonesResponse = self.get_json("/zones").await?;
        Ok(r.zones
            .into_iter()
            .map(|z| DiscoveredZone {
                name: z
                    .name
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| format!("Zone {}", z.id)),
                station_id: z.id,
            })
            .collect())
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HttpGenericConfig {
        HttpGenericConfig {
            base_url: "http://192.0.2.50".into(),
            bearer_token: None,
            poll_interval_s: 10,
        }
    }

    fn ctl() -> HttpGeneric {
        let mut map = HashMap::new();
        map.insert("back_yard".to_string(), "1".to_string());
        map.insert("front_yard".to_string(), "2".to_string());
        HttpGeneric::new("diy", cfg(), map).unwrap()
    }

    #[test]
    fn base_url_trims_trailing_slash() {
        let mut c = cfg();
        c.base_url = "http://board.local:8080/".into();
        let h = HttpGeneric::new("diy", c, HashMap::new()).unwrap();
        assert_eq!(h.base(), "http://board.local:8080");
    }

    #[test]
    fn station_resolution() {
        let h = ctl();
        assert_eq!(h.station_for("back_yard").unwrap(), "1");
        assert!(matches!(
            h.station_for("nope").unwrap_err(),
            ControllerError::ZoneUnknown(_)
        ));
    }

    #[test]
    fn encode_segment_passes_unreserved_and_escapes_the_rest() {
        assert_eq!(encode_segment("back_yard"), "back_yard");
        assert_eq!(encode_segment("1"), "1");
        assert_eq!(encode_segment("a.b-c~d"), "a.b-c~d");
        // Path/query-structural chars are escaped so they can't inject.
        assert_eq!(encode_segment("../run"), "..%2Frun");
        assert_eq!(encode_segment("1?x=2"), "1%3Fx%3D2");
        assert_eq!(encode_segment("a b"), "a%20b");
    }

    #[test]
    fn reverse_slug_lookup_falls_back() {
        let h = ctl();
        assert_eq!(h.slug_for_station("1"), "back_yard");
        assert_eq!(h.slug_for_station("9"), "station_9");
    }

    #[test]
    fn caps_advertise_flow_and_rain() {
        let h = ctl();
        let caps = h.supports();
        assert!(caps.flow_meter);
        assert!(caps.rain_sensor);
        assert!(!caps.master_valve);
    }

    #[test]
    fn status_response_parses_contract_shape() {
        let body = serde_json::json!({
            "firmware": "1.2.0",
            "zones": [
                {"id": "1", "running": true, "remaining_s": 120},
                {"id": "2", "running": false}
            ],
            "flow_gpm": 3.5,
            "rain": false
        });
        let r: StatusResponse = serde_json::from_value(body).unwrap();
        assert_eq!(r.firmware.as_deref(), Some("1.2.0"));
        assert_eq!(r.zones.len(), 2);
        assert!(r.zones[0].running);
        assert_eq!(r.zones[0].remaining_s, Some(120));
        assert_eq!(r.flow_gpm, Some(3.5));
        assert_eq!(r.rain, Some(false));
    }

    #[test]
    fn status_response_tolerates_minimal_body() {
        // Only zones are required; everything else is optional.
        let body = serde_json::json!({ "zones": [ {"id": "1"} ] });
        let r: StatusResponse = serde_json::from_value(body).unwrap();
        assert_eq!(r.zones.len(), 1);
        assert!(!r.zones[0].running);
        assert_eq!(r.flow_gpm, None);
        assert_eq!(r.rain, None);
    }

    #[test]
    fn zones_response_parses_for_discovery() {
        let body = serde_json::json!({
            "zones": [ {"id": "1", "name": "Back Yard"}, {"id": "2"} ]
        });
        let r: ZonesResponse = serde_json::from_value(body).unwrap();
        assert_eq!(r.zones.len(), 2);
        assert_eq!(r.zones[0].name.as_deref(), Some("Back Yard"));
        assert_eq!(r.zones[1].name, None);
    }
}
