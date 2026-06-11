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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::schema::RainbirdConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

pub struct Rainbird {
    id: String,
    config: RainbirdConfig,
    client: Client,
    access_token: Arc<Mutex<Option<String>>>,
    station_to_slug: BTreeMap<u32, String>,
    last_status: Arc<Mutex<Option<ControllerStatus>>>,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
}

impl Rainbird {
    pub fn new(id: impl Into<String>, config: RainbirdConfig) -> Result<Self, ControllerError> {
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
            access_token: Arc::new(Mutex::new(None)),
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
        let url = format!(
            "{}/v1/userLogin",
            self.config.base_url.trim_end_matches('/')
        );
        let resp = self
            .client
            .post(&url)
            .json(&json!({
                "email": &self.config.email,
                "password": &self.config.password,
            }))
            .send()
            .await
            .map_err(|e| ControllerError::Transport(format!("rainbird login: {e}")))?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(ControllerError::AuthFailed);
        }
        if !status.is_success() {
            return Err(ControllerError::Remote(format!("rainbird login {status}")));
        }
        let lr: LoginResponse = resp
            .json()
            .await
            .map_err(|e| ControllerError::Transport(format!("rainbird login decode: {e}")))?;
        *self.access_token.lock().await = Some(lr.access_token.clone());
        Ok(lr.access_token)
    }

    async fn current_token(&self) -> Result<String, ControllerError> {
        if let Some(t) = self.access_token.lock().await.clone() {
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
                .bearer_auth(&token);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| ControllerError::Transport(format!("rainbird {method} {url}: {e}")))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                *self.access_token.lock().await = None;
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
                return Err(ControllerError::Remote(format!("rainbird {status}")));
            }
            return resp
                .json()
                .await
                .map_err(|e| ControllerError::Transport(format!("rainbird decode: {e}")));
        }
        Err(ControllerError::Remote("rainbird retry exhausted".into()))
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

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            flow_meter: false,
            rain_sensor: true,
            master_valve: true,
            multi_zone_parallel: false,
            history_query: false,
            remote_program_upload: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let station = self.station_for(slug)?;
        // Rain Bird's cloud takes duration in MINUTES (round up).
        let duration_min = duration_s.div_ceil(60);
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
            planned_duration_s: duration_s,
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
                // map back to slugs via station_to_slug.
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
                let zone_states: Vec<ZoneRuntimeStatus> = self
                    .station_to_slug
                    .iter()
                    .map(|(station, slug)| {
                        let remaining = running_map.get(station).copied();
                        ZoneRuntimeStatus {
                            slug: slug.clone(),
                            running: remaining.is_some(),
                            remaining_s: remaining,
                            last_run_epoch: None,
                        }
                    })
                    .collect();
                let rain_sensor_tripped = v.get("rain_sensor_tripped").and_then(|x| x.as_bool());
                let firmware = v
                    .get("firmware_version")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
                let status = ControllerStatus {
                    reachable: true,
                    master_enabled: None,
                    water_level_pct: None,
                    rain_sensor_tripped,
                    current_program: None,
                    zone_states,
                    flow_gpm: None,
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
        assert_eq!(r.station_for("back_yard").unwrap(), 1);
        assert!(matches!(
            r.station_for("not_a_zone").unwrap_err(),
            ControllerError::ZoneUnknown(_)
        ));
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
