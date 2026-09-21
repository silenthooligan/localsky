// Orbit B-hyve cloud controller, WiFi Timer / Smart Indoor / XR / XD.
//
// Talks to api.orbitbhyve.com. There's no official documentation; the
// endpoint set used here is the same one the official mobile + web
// apps use, reverse-engineered by the bhyve-iot project and exposed
// the same way by Home Assistant's b-hyve integration.
//
// Auth flow:
//   POST /v1/session  body {session: {email, password}}
//      -> response { orbit_session_token, user: {id, ...} }
//
// Commands (Authorization: <session_token> header):
//   POST /v1/devices/{device_id}/manual
//      body {action: "run", stations:[{station, run_time}]}
//   POST /v1/devices/{device_id}/manual
//      body {action: "stop"}              stop everything
//   GET  /v1/devices/{device_id}          status + stations
//
// Caveats:
//   - No per-station stop; stop is whole-device (similar to Rachio).
//   - Session token rotates; on 401 we re-login and retry once.
//   - station numbers are 1-based and stable per device.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::schema::BhyveConfig;
use crate::controllers::zone_map::ZoneMap;
use crate::sources::auth::{with_reauth, TokenCache};

/// B-hyve's cloud API takes run lengths in whole minutes and rounds
/// up. Every segment is planned in multiples of this, and the handle
/// reports the rounded figure, so the plan and the wait match the valve.
pub const DURATION_QUANTUM_S: u32 = 60;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

const API_BASE: &str = "https://api.orbitbhyve.com/v1";

pub struct Bhyve {
    id: String,
    config: BhyveConfig,
    client: Client,
    /// Cached session token; a 401 invalidates it.
    session_token: TokenCache,
    /// Zone slug <-> station number, both directions.
    zones: ZoneMap<u32>,
    last_status: Arc<Mutex<Option<ControllerStatus>>>,
}

#[derive(Debug, Deserialize)]
struct SessionResponse {
    orbit_session_token: String,
}

/// How one authenticated call fails: the session token was rejected (a
/// 401, worth exactly one re-login), or anything else, passed through
/// untouched. Only a 401 earns the retry; a 403 is refused outright.
enum CallError {
    TokenRejected,
    Other(ControllerError),
}

impl crate::sources::auth::ReauthError for CallError {
    fn diagnostic(&self) -> crate::failure::Failure {
        match self {
            Self::TokenRejected => {
                crate::failure::Failure::http(401, None, "B-hyve authenticated request")
            }
            Self::Other(error) => error.diagnostic(),
        }
    }
    fn after_rejection(self, previous: Self) -> Self {
        let convert = |attempt: Self| match attempt {
            Self::TokenRejected => ControllerError::http(401, None, "B-hyve authenticated request"),
            Self::Other(error) => error,
        };
        Self::Other(crate::sources::auth::ReauthError::after_rejection(
            convert(self),
            convert(previous),
        ))
    }
}

impl Bhyve {
    /// Infallible today (`net::client` falls back to reqwest's defaults
    /// rather than failing); the `Result` stays for the registry's
    /// uniform construction path.
    pub fn new(id: impl Into<String>, config: BhyveConfig) -> Result<Self, ControllerError> {
        let zones = ZoneMap::new(
            config
                .zone_station_map
                .iter()
                .map(|(slug, station)| (slug.clone(), *station))
                .collect(),
        );
        Ok(Self {
            id: id.into(),
            config,
            client: crate::net::client(Duration::from_secs(15)),
            session_token: TokenCache::new(),
            zones,
            last_status: Arc::new(Mutex::new(None)),
        })
    }

    /// Exchange email + password for a session token. Caching is the
    /// `TokenCache`'s job; this only talks to /session.
    async fn login(&self) -> Result<String, ControllerError> {
        let url = format!("{API_BASE}/session");
        let body = json!({ "session": { "email": &self.config.email, "password": &self.config.password } });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ControllerError::transport(crate::net::source_failure::from_reqwest(
                    &e,
                    "B-hyve authenticate",
                ))
            })?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ControllerError::http(
                status.as_u16(),
                None,
                "B-hyve authentication",
            ));
        }
        if !status.is_success() {
            return Err(ControllerError::http(
                status.as_u16(),
                None,
                "B-hyve response",
            ));
        }
        let sr: SessionResponse = resp.json().await.map_err(|e| {
            ControllerError::transport(crate::net::source_failure::from_reqwest(
                &e,
                "B-hyve decode response",
            ))
        })?;
        Ok(sr.orbit_session_token)
    }

    /// One call carrying a given session token. A 401 comes back as
    /// `TokenRejected` so `authed_request` can tell "the session expired"
    /// from "the upstream refused or is down".
    async fn send_with_token(
        &self,
        method: &reqwest::Method,
        url: &str,
        body: Option<&Value>,
        token: String,
    ) -> Result<Value, CallError> {
        let mut req = self
            .client
            .request(method.clone(), url)
            .header("orbit-session-token", token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| {
            CallError::Other(ControllerError::transport(
                crate::net::source_failure::from_reqwest(&e, "B-hyve request"),
            ))
        })?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(CallError::TokenRejected);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(CallError::Other(ControllerError::http(
                429,
                None,
                "B-hyve request",
            )));
        }
        if status == StatusCode::FORBIDDEN {
            return Err(CallError::Other(ControllerError::http(
                status.as_u16(),
                None,
                "B-hyve authorization",
            )));
        }
        if !status.is_success() {
            return Err(CallError::Other(ControllerError::http(
                status.as_u16(),
                None,
                "B-hyve response",
            )));
        }
        resp.json().await.map_err(|e| {
            CallError::Other(ControllerError::transport(
                crate::net::source_failure::from_reqwest(&e, "B-hyve decode response"),
            ))
        })
    }

    /// The call with the cached session token; on a 401, log in again
    /// and retry once.
    async fn authed_request(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, ControllerError> {
        with_reauth(
            &self.session_token,
            || async move { self.login().await.map_err(CallError::Other) },
            |e: &CallError| matches!(e, CallError::TokenRejected),
            |token| self.send_with_token(&method, &url, body.as_ref(), token),
        )
        .await
        .map_err(|e| match e {
            // Rejected again on a fresh session: the account is refused.
            CallError::TokenRejected => {
                ControllerError::http(401, None, "B-hyve reauthenticated request")
            }
            CallError::Other(e) => e,
        })
    }
}

#[async_trait]
impl IrrigationController for Bhyve {
    fn id(&self) -> &str {
        &self.id
    }

    fn mapped_zone_slugs(&self) -> Vec<String> {
        self.zones.slugs()
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            flow_meter: false,
            rain_sensor: true,
            master_valve: true,
            // Stations run sequentially in a B-hyve program; single
            // manual zone run is one-at-a-time.
            multi_zone_parallel: false,
            history_query: false,
            remote_program_upload: false,
            water_level: false,
            // stop_zone falls back to the whole-device manual stop (the
            // B-hyve API has no per-station stop), so consumers must treat
            // a zone-stop as stopping every running station on the device.
            per_zone_stop: false,
            duration_quantum_s: DURATION_QUANTUM_S,
        }
    }

    /// A cloud read: the registry serves the last status inside this
    /// window and re-reads after any command.
    fn status_poll_interval_s(&self) -> Option<u32> {
        Some(crate::controllers::guard::CLOUD_STATUS_POLL_S)
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let station = self.zones.station_for(slug)?;
        let url = format!(
            "{API_BASE}/devices/{dev}/manual",
            dev = self.config.device_id
        );
        // B-hyve takes run_time in MINUTES, rounded up.
        let run_time = duration_s.div_ceil(DURATION_QUANTUM_S);
        let body = json!({
            "action": "run",
            "stations": [{ "station": station, "run_time": run_time }],
        });
        let _ = self
            .authed_request(reqwest::Method::POST, url, Some(body))
            .await?;
        debug!(controller = %self.id, zone = slug, station, run_time, "bhyve run_zone OK");
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: chrono::Utc::now().timestamp(),
            // What was actually sent, so the executor waits for the whole
            // minute the valve will be open and not the seconds we asked.
            planned_duration_s: run_time * DURATION_QUANTUM_S,
            provider_ref: Some(station.to_string()),
        })
    }

    async fn stop_zone(&self, _slug: &str) -> ControllerResult<()> {
        // B-hyve has no per-zone stop; the manual action="stop"
        // halts the whole device. Surface a warn so operators know
        // other in-flight stations on the same device will also stop.
        warn!(controller = %self.id, "bhyve stop_zone falls back to whole-device stop");
        self.stop_all().await
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        let url = format!(
            "{API_BASE}/devices/{dev}/manual",
            dev = self.config.device_id
        );
        let body = json!({ "action": "stop" });
        let _ = self
            .authed_request(reqwest::Method::POST, url, Some(body))
            .await?;
        Ok(())
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let url = format!("{API_BASE}/devices/{dev}", dev = self.config.device_id);
        match self.authed_request(reqwest::Method::GET, url, None).await {
            Ok(v) => {
                // One entry per bound zone, in station order.
                let mut by_station: Vec<(u32, &String)> =
                    self.zones.iter().map(|(slug, st)| (*st, slug)).collect();
                by_station.sort();
                let zone_states: Vec<ZoneRuntimeStatus> = by_station
                    .into_iter()
                    .map(|(_, slug)| ZoneRuntimeStatus {
                        slug: slug.clone(),
                        // v1 does not parse live running state (a future
                        // wave could read `v.status.run_mode` + the active
                        // station), so `running: false` here is a
                        // placeholder, NOT a reading. running_known=false
                        // says so: the run-edge observer must not treat
                        // this as a confirmed idle, and the reaper's
                        // verify-before-enforce path must not take it as
                        // confirmation either.
                        running: false,
                        remaining_s: None,
                        last_run_epoch: None,
                        running_known: false,
                    })
                    .collect();
                let firmware = v
                    .get("firmware_version")
                    .and_then(|f| f.as_str())
                    .map(|s| s.to_string());
                let status = ControllerStatus {
                    observed_epoch: Some(crate::timefmt::now_epoch()),
                    reachable: true,
                    master_enabled: None,
                    water_level_pct: None,
                    rain_sensor_tripped: None,
                    current_program: None,
                    zone_states,
                    flow_gpm: None,
                    flow_connected: false,
                    firmware,
                };
                *self.last_status.lock().await = Some(status.clone());
                Ok(status)
            }
            Err(e) => {
                warn!(controller = %self.id, error = %e, "bhyve status failed");
                if let Some(prev) = self.last_status.lock().await.clone() {
                    return Ok(ControllerStatus {
                        reachable: false,
                        ..prev
                    });
                }
                Err(e)
            }
        }
    }

    async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        // /v1/devices/{id}/history exists but is heavily paginated and
        // the runs store backfill covers our needs. Left as future work.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg() -> BhyveConfig {
        let mut map = BTreeMap::new();
        map.insert("back_yard".to_string(), 1);
        map.insert("front_lawn".to_string(), 2);
        BhyveConfig {
            email: "user@example.com".into(),
            password: "pw".into(),
            device_id: "abc123".into(),
            zone_station_map: map,
        }
    }

    #[test]
    fn station_lookup() {
        let b = Bhyve::new("bh", cfg()).unwrap();
        assert_eq!(b.zones.station_for("back_yard").unwrap(), 1);
        assert!(matches!(
            b.zones.station_for("not_a_zone").unwrap_err(),
            ControllerError::ZoneUnknown(_)
        ));
        assert_eq!(b.mapped_zone_slugs(), vec!["back_yard", "front_lawn"]);
    }

    #[test]
    fn caps_advertise_rain_sensor_master() {
        let b = Bhyve::new("bh", cfg()).unwrap();
        let caps = b.supports();
        assert!(caps.rain_sensor);
        assert!(caps.master_valve);
        assert!(!caps.flow_meter);
        assert!(!caps.multi_zone_parallel);
    }
}
