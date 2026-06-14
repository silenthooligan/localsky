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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::schema::BhyveConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

const API_BASE: &str = "https://api.orbitbhyve.com/v1";

pub struct Bhyve {
    id: String,
    config: BhyveConfig,
    client: Client,
    /// Cached session token. Cleared on 401.
    session_token: Arc<Mutex<Option<String>>>,
    /// Reverse map (station -> slug) for status() decoding.
    station_to_slug: BTreeMap<u32, String>,
    last_status: Arc<Mutex<Option<ControllerStatus>>>,
}

#[derive(Debug, Deserialize)]
struct SessionResponse {
    orbit_session_token: String,
}

impl Bhyve {
    pub fn new(id: impl Into<String>, config: BhyveConfig) -> Result<Self, ControllerError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| ControllerError::Init(format!("reqwest client: {e}")))?;
        let station_to_slug = config
            .zone_station_map
            .iter()
            .map(|(slug, station)| (*station, slug.clone()))
            .collect();
        Ok(Self {
            id: id.into(),
            config,
            client,
            session_token: Arc::new(Mutex::new(None)),
            station_to_slug,
            last_status: Arc::new(Mutex::new(None)),
        })
    }

    fn station_for(&self, slug: &str) -> Result<u32, ControllerError> {
        self.config
            .zone_station_map
            .get(slug)
            .copied()
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    async fn login(&self) -> Result<String, ControllerError> {
        let url = format!("{API_BASE}/session");
        let body = json!({ "session": { "email": &self.config.email, "password": &self.config.password } });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ControllerError::Transport(format!("bhyve POST /session: {e}")))?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ControllerError::AuthFailed);
        }
        if !status.is_success() {
            return Err(ControllerError::Remote(format!("bhyve session {status}")));
        }
        let sr: SessionResponse = resp
            .json()
            .await
            .map_err(|e| ControllerError::Transport(format!("bhyve session decode: {e}")))?;
        *self.session_token.lock().await = Some(sr.orbit_session_token.clone());
        Ok(sr.orbit_session_token)
    }

    async fn current_token(&self) -> Result<String, ControllerError> {
        if let Some(t) = self.session_token.lock().await.clone() {
            return Ok(t);
        }
        self.login().await
    }

    async fn authed_request(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<Value>,
    ) -> Result<Value, ControllerError> {
        let mut token = self.current_token().await?;
        for attempt in 0..2 {
            let mut req = self
                .client
                .request(method.clone(), &url)
                .header("orbit-session-token", &token);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| ControllerError::Transport(format!("bhyve {method} {url}: {e}")))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                // Session expired; re-login and retry once.
                *self.session_token.lock().await = None;
                token = self.login().await?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                return Err(ControllerError::RateLimited);
            }
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(ControllerError::AuthFailed);
            }
            if !status.is_success() {
                return Err(ControllerError::Remote(format!("bhyve {status}")));
            }
            return resp
                .json()
                .await
                .map_err(|e| ControllerError::Transport(format!("bhyve decode: {e}")));
        }
        // Unreachable, the loop returns or errors on every iteration.
        Err(ControllerError::Remote("bhyve retry exhausted".into()))
    }
}

#[async_trait]
impl IrrigationController for Bhyve {
    fn id(&self) -> &str {
        &self.id
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
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let station = self.station_for(slug)?;
        let url = format!(
            "{API_BASE}/devices/{dev}/manual",
            dev = self.config.device_id
        );
        // B-hyve takes run_time in MINUTES, rounded up.
        let run_time = duration_s.div_ceil(60);
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
            planned_duration_s: duration_s,
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
                let zone_states: Vec<ZoneRuntimeStatus> = self
                    .station_to_slug
                    .values()
                    .map(|slug| ZoneRuntimeStatus {
                        slug: slug.clone(),
                        // We could parse `v.status.run_mode` + active
                        // station here; left as None for v1 since the
                        // engine layer already tracks scheduled state.
                        running: false,
                        remaining_s: None,
                        last_run_epoch: None,
                    })
                    .collect();
                let firmware = v
                    .get("firmware_version")
                    .and_then(|f| f.as_str())
                    .map(|s| s.to_string());
                let status = ControllerStatus {
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
        assert_eq!(b.station_for("back_yard").unwrap(), 1);
        assert!(matches!(
            b.station_for("not_a_zone").unwrap_err(),
            ControllerError::ZoneUnknown(_)
        ));
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
