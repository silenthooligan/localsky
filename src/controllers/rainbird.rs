// Rain Bird LNK2 cloud controller, rdz-rest.rainbird.com.
//
// The LNK2 is the WiFi module that bolts onto ESP-Me / ARC8 / ESP-RZXe
// controllers. This adapter targets the same cloud REST endpoint the
// official Rain Bird mobile app uses. Auth is session-token: POST
// /v1/userLogin with email + password returns an access_token (rotates
// on 401, retried once).
//
// Endpoints used (best-effort; Rain Bird's cloud isn't officially
// documented but the mobile app's reverse-engineered set is stable):
//   POST /v1/userLogin                {email, password}    -> {access_token}
//   GET  /v1/userControllers          (Bearer)             -> list of controllers
//   POST /v1/controllers/{id}/command (Bearer)             -> issue command
//   GET  /v1/controllers/{id}/state   (Bearer)             -> current state
//
// LAN-direct (AES-encrypted) is deferred, requires aes + cbc + pbkdf2
// deps. HA users with a working RainBird LAN integration can route
// through ha_service_call until then.
//
// Caveats:
//   - Rain Bird has no per-zone stop in the public command set; stop is
//     whole-controller. stop_zone falls back to stop_all + warn.
//   - The cloud occasionally rotates host names; base_url is configurable
//     in the schema for that reason.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::schema::RainbirdConfig;
use crate::controllers::zone_map::ZoneMap;
use crate::sources::auth::{with_reauth, TokenCache};

/// Rain Bird's cloud API takes run lengths in whole minutes and rounds
/// up. Every segment is planned in multiples of this, and the handle
/// reports the rounded figure, so the plan and the wait match the valve.
pub const DURATION_QUANTUM_S: u32 = 60;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

/// Per-request HTTP budget for Rain Bird cloud calls. Each request
/// builds an SSRF-hardened, IP-pinned client (no redirects) via
/// net::safe_fetch because `base_url` is operator-configurable (the schema
/// exposes it so the operator can follow Rain Bird's host rotations), which
/// makes the target host user-controlled and reachable from the wizard's
/// test_controller / scan_zones probe.
const RB_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Rainbird {
    id: String,
    config: RainbirdConfig,
    /// The /v1/userLogin access_token: fetched on first use, dropped and
    /// re-fetched once when a request comes back 401.
    token: TokenCache,
    /// Zone slug -> Rain Bird station number, built from
    /// `config.zone_station_map` at construction. Commands look up the
    /// station for a slug; status() walks it the other way.
    zones: ZoneMap<u32>,
    last_status: Arc<Mutex<Option<ControllerStatus>>>,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
}

/// The outcome of one bearer-authenticated attempt. A 401 means the
/// cached session token was rejected and is the only thing that earns a
/// re-login and a retry; a 403, a 429, a transport failure and every
/// other status are final and pass through unchanged.
enum Attempt {
    TokenRejected,
    Failed(ControllerError),
}

impl From<ControllerError> for Attempt {
    fn from(e: ControllerError) -> Self {
        Attempt::Failed(e)
    }
}

fn token_rejected(e: &Attempt) -> bool {
    matches!(e, Attempt::TokenRejected)
}

impl Rainbird {
    pub fn new(id: impl Into<String>, config: RainbirdConfig) -> Result<Self, ControllerError> {
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
            token: TokenCache::new(),
            zones,
            last_status: Arc::new(Mutex::new(None)),
        })
    }

    /// SSRF-hardened, IP-pinned client for one Rain Bird URL. `base_url` is
    /// operator-configurable, so loopback/metadata/link-local/multicast are
    /// rejected, the resolved IP is pinned (anti DNS-rebinding) and
    /// redirects are disabled. A blocked/unresolvable target is an
    /// Init-class failure (the call was never made).
    async fn safe_client(&self, url: &str) -> Result<(Client, reqwest::Url), ControllerError> {
        crate::net::safe_fetch::build_safe_client(url, RB_TIMEOUT)
            .await
            .map_err(|e| ControllerError::Init(e.to_string()))
    }

    /// Exchange email + password for a fresh access_token. Caching is the
    /// `TokenCache`'s job; this only talks to /v1/userLogin.
    async fn login(&self) -> Result<String, ControllerError> {
        let url = format!(
            "{}/v1/userLogin",
            self.config.base_url.trim_end_matches('/')
        );
        let (client, safe_url) = self.safe_client(&url).await?;
        let resp = client
            .post(safe_url)
            .json(&json!({
                "email": &self.config.email,
                "password": &self.config.password,
            }))
            .send()
            .await
            .map_err(|e| {
                ControllerError::Transport(format!(
                    "rainbird login: {}",
                    crate::net::reqwest_error_category(&e)
                ))
            })?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ControllerError::AuthFailed);
        }
        if !status.is_success() {
            return Err(ControllerError::Remote(format!("rainbird login {status}")));
        }
        let lr: LoginResponse = crate::net::safe_fetch::read_json_capped(resp)
            .await
            .map_err(|e| ControllerError::Transport(format!("rainbird login decode: {e}")))?;
        Ok(lr.access_token)
    }

    /// One bearer-authenticated request with the given token. The safe
    /// client is built per attempt (host resolved + IP-pinned, redirects
    /// off, forbidden targets rejected) so the retry after a re-login
    /// re-vets the target the same way.
    async fn send_authed(
        &self,
        method: &reqwest::Method,
        url: &str,
        body: Option<&Value>,
        token: String,
    ) -> Result<Value, Attempt> {
        let (client, safe_url) = self.safe_client(url).await?;
        let mut req = client.request(method.clone(), safe_url).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| {
            ControllerError::Transport(format!(
                "rainbird {method}: {}",
                crate::net::reqwest_error_category(&e)
            ))
        })?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(Attempt::TokenRejected);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Attempt::Failed(ControllerError::RateLimited));
        }
        if status == StatusCode::FORBIDDEN {
            return Err(Attempt::Failed(ControllerError::AuthFailed));
        }
        if !status.is_success() {
            return Err(Attempt::Failed(ControllerError::Remote(format!(
                "rainbird {status}"
            ))));
        }
        crate::net::safe_fetch::read_json_capped(resp)
            .await
            .map_err(|e| {
                Attempt::Failed(ControllerError::Transport(format!("rainbird decode: {e}")))
            })
    }

    /// Send with the cached session token; on a 401, log in again and
    /// retry once. A second 401 is an auth failure.
    async fn authed_request(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, ControllerError> {
        with_reauth(
            &self.token,
            || async move { self.login().await.map_err(Attempt::Failed) },
            token_rejected,
            |token| self.send_authed(&method, &url, body.as_ref(), token),
        )
        .await
        .map_err(|e| match e {
            Attempt::TokenRejected => ControllerError::AuthFailed,
            Attempt::Failed(e) => e,
        })
    }

    fn command_url(&self) -> String {
        format!(
            "{}/v1/controllers/{}/command",
            self.config.base_url.trim_end_matches('/'),
            self.config.controller_id
        )
    }
}

#[async_trait]
impl IrrigationController for Rainbird {
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
            multi_zone_parallel: false,
            history_query: false,
            remote_program_upload: false,
            water_level: false,
            // stop_zone falls back to the whole-controller StopIrrigation
            // command (the cloud API has no per-station stop), so consumers
            // must treat a zone-stop as stopping the whole controller.
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
        // Rain Bird's cloud takes duration in MINUTES (round up).
        let duration_min = duration_s.div_ceil(DURATION_QUANTUM_S);
        let body = json!({
            "command": "WaterControllerOnce",
            "station": station,
            "duration_minutes": duration_min,
        });
        let _ = self
            .authed_request(reqwest::Method::POST, self.command_url(), Some(body))
            .await?;
        debug!(controller = %self.id, zone = slug, station, duration_min, "rainbird run_zone OK");
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: chrono::Utc::now().timestamp(),
            // What was actually sent, so the executor waits for the whole
            // minute the valve will be open and not the seconds we asked.
            planned_duration_s: duration_min * DURATION_QUANTUM_S,
            provider_ref: Some(station.to_string()),
        })
    }

    async fn stop_zone(&self, _slug: &str) -> ControllerResult<()> {
        warn!(controller = %self.id, "rainbird stop_zone falls back to whole-controller stop");
        self.stop_all().await
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        let body = json!({ "command": "StopIrrigation" });
        let _ = self
            .authed_request(reqwest::Method::POST, self.command_url(), Some(body))
            .await?;
        Ok(())
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let url = format!(
            "{}/v1/controllers/{}/state",
            self.config.base_url.trim_end_matches('/'),
            self.config.controller_id,
        );
        match self.authed_request(reqwest::Method::GET, url, None).await {
            Ok(v) => {
                // Rain Bird's state payload includes `running_stations`,
                // a list of {station, remaining_seconds}. Walk it and
                // map back to slugs via the zone map.
                let mut running_map: std::collections::HashMap<u32, u32> =
                    std::collections::HashMap::new();
                if let Some(arr) = v.get("running_stations").and_then(|a| a.as_array()) {
                    for entry in arr {
                        if let (Some(s), Some(r)) = (
                            entry.get("station").and_then(|x| x.as_u64()),
                            entry.get("remaining_seconds").and_then(|x| x.as_u64()),
                        ) {
                            running_map.insert(s as u32, r as u32);
                        }
                    }
                }
                // Report the bound zones in station order, the same order
                // the pre-ZoneMap station->slug BTreeMap gave.
                let mut bound: Vec<(u32, &String)> = self
                    .zones
                    .iter()
                    .map(|(slug, station)| (*station, slug))
                    .collect();
                bound.sort_unstable();
                let zone_states: Vec<ZoneRuntimeStatus> = bound
                    .into_iter()
                    .map(|(station, slug)| {
                        let remaining = running_map.get(&station).copied();
                        ZoneRuntimeStatus {
                            slug: slug.clone(),
                            running: remaining.is_some(),
                            remaining_s: remaining,
                            last_run_epoch: None,
                            running_known: true,
                        }
                    })
                    .collect();
                let rain_sensor_tripped = v.get("rain_sensor_tripped").and_then(|x| x.as_bool());
                let firmware = v
                    .get("firmware_version")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
                let status = ControllerStatus {
                    observed_epoch: Some(crate::timefmt::now_epoch()),
                    reachable: true,
                    master_enabled: None,
                    water_level_pct: None,
                    rain_sensor_tripped,
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
                warn!(controller = %self.id, error = %e, "rainbird status failed");
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
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg() -> RainbirdConfig {
        let mut map = BTreeMap::new();
        map.insert("back_yard".to_string(), 1);
        map.insert("front_lawn".to_string(), 2);
        RainbirdConfig {
            email: "user@example.com".into(),
            password: "pw".into(),
            controller_id: "abc123".into(),
            zone_station_map: map,
            base_url: "https://rdz-rest.rainbird.com".into(),
        }
    }

    #[test]
    fn station_lookup() {
        let r = Rainbird::new("rb", cfg()).unwrap();
        assert_eq!(r.zones.station_for("back_yard").unwrap(), 1);
        assert!(matches!(
            r.zones.station_for("not_a_zone").unwrap_err(),
            ControllerError::ZoneUnknown(_)
        ));
        assert_eq!(r.zones.slug_for(&2), Some("front_lawn"));
        assert_eq!(r.zones.slug_for(&9), None);
        assert_eq!(r.mapped_zone_slugs(), vec!["back_yard", "front_lawn"]);
    }

    #[test]
    fn caps_advertise_rain_sensor_master() {
        let r = Rainbird::new("rb", cfg()).unwrap();
        let caps = r.supports();
        assert!(caps.rain_sensor);
        assert!(caps.master_valve);
        assert!(!caps.multi_zone_parallel);
        assert!(!caps.flow_meter);
    }

    #[test]
    fn command_url_concatenates_base_and_controller() {
        let r = Rainbird::new("rb", cfg()).unwrap();
        assert_eq!(
            r.command_url(),
            "https://rdz-rest.rainbird.com/v1/controllers/abc123/command"
        );
    }
}
