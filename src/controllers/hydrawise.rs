// Hunter Hydrawise cloud controller, HC v3 / HPC v6 / Pro-C upgrade
// module. Talks to app.hydrawise.com via the v1.5 "RESTful" API. Auth
// is a per-account API key (Account > Settings > API in the customer
// portal, surfaced after the user pastes the key into the wizard).
//
// Endpoints:
//   GET /api/v1/setallzones.php?period_id=999&controller_id=X&api_key=K
//      Hydrawise's "set all zones" route doubles as a stop-all (period
//      999 = "stop manually-running zones"). Per-zone start/stop go
//      through setzone.php.
//   GET /api/v1/setzone.php?action=run&period_id=999&relay_id=R&custom=S&api_key=K
//      action=run + custom=<seconds> starts a relay for that duration.
//   GET /api/v1/setzone.php?action=stop&relay_id=R&api_key=K
//      Stops a single relay immediately.
//   GET /api/v1/statusschedule.php?controller_id=X&api_key=K
//      Status: per-zone running/remaining + master valve + flow meter.
//
// Caveats:
//   - The "RESTful" API is GET-only and ignores Content-Type. Every
//     command goes in the query string.
//   - Hydrawise rate-limits to 30 calls / 5 min per api_key. Our
//     status() poll is on the engine's cadence; the engine adds its
//     own minimum interval before re-polling, so we don't add our
//     own here.
//   - relay_id values are NOT zone numbers, they're stable Hydrawise
//     relay IDs surfaced in statusschedule.php. The wizard's zone-scan
//     populates `zone_relay_map`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::schema::HydrawiseConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

const API_BASE: &str = "https://app.hydrawise.com/api/v1";

pub struct Hydrawise {
    id: String,
    config: HydrawiseConfig,
    client: Client,
    /// Reverse map (relay_id -> slug) for status() lookups.
    relay_to_slug: HashMap<i64, String>,
    last_status: Arc<Mutex<Option<ControllerStatus>>>,
}

#[derive(Debug, Deserialize)]
struct StatusScheduleResponse {
    #[serde(default)]
    relays: Vec<RelayStatus>,
    #[serde(default)]
    master: Option<i64>,
    /// Account-level live flow reading (US gallons per minute). Present
    /// only on accounts that have the HC Flow Meter accessory paired.
    #[serde(default)]
    flow_gpm: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RelayStatus {
    relay_id: i64,
    /// Seconds until next run, OR 1 if currently running.
    time: i64,
    /// Seconds remaining of the active run.
    #[serde(default)]
    run: Option<i64>,
}

impl Hydrawise {
    pub fn new(id: impl Into<String>, config: HydrawiseConfig) -> Result<Self, ControllerError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| ControllerError::Init(format!("reqwest client: {e}")))?;
        let relay_to_slug = config
            .zone_relay_map
            .iter()
            .map(|(slug, relay)| (*relay, slug.clone()))
            .collect();
        Ok(Self {
            id: id.into(),
            config,
            client,
            relay_to_slug,
            last_status: Arc::new(Mutex::new(None)),
        })
    }

    fn relay_for(&self, slug: &str) -> Result<i64, ControllerError> {
        self.config
            .zone_relay_map
            .get(slug)
            .copied()
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    async fn get_json(&self, url: String) -> Result<Value, ControllerError> {
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ControllerError::Transport(format!("hydrawise GET failed: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ControllerError::AuthFailed);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ControllerError::RateLimited);
        }
        if !status.is_success() {
            return Err(ControllerError::Remote(format!("hydrawise {status}")));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| ControllerError::Transport(format!("hydrawise decode failed: {e}")))?;
        // Hydrawise returns 200 OK with {"message":"error blah"} for
        // failures. Surface those as Remote.
        if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
            // Some success paths also include "message", only treat
            // as an error if the body lacks expected success keys.
            if v.get("relay_id").is_none()
                && v.get("relays").is_none()
                && msg.to_lowercase().contains("error")
            {
                return Err(ControllerError::Remote(msg.to_string()));
            }
        }
        Ok(v)
    }
}

#[async_trait]
impl IrrigationController for Hydrawise {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            // statusschedule.php exposes flow + master if hardware
            // present. We advertise them; status() probes per call.
            flow_meter: true,
            rain_sensor: true,
            master_valve: true,
            multi_zone_parallel: false,
            history_query: false,
            remote_program_upload: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let relay = self.relay_for(slug)?;
        let url = format!(
            "{API_BASE}/setzone.php?action=run&period_id=999&relay_id={relay}&custom={duration_s}&api_key={key}",
            key = self.config.api_key,
        );
        let _ = self.get_json(url).await?;
        debug!(controller = %self.id, zone = slug, relay, duration_s, "hydrawise run_zone OK");
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: chrono::Utc::now().timestamp(),
            planned_duration_s: duration_s,
            provider_ref: Some(relay.to_string()),
        })
    }

    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        let relay = self.relay_for(slug)?;
        let url = format!(
            "{API_BASE}/setzone.php?action=stop&relay_id={relay}&api_key={key}",
            key = self.config.api_key,
        );
        let _ = self.get_json(url).await?;
        Ok(())
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        // period_id=999 with setallzones.php = stop manually-running.
        let url = format!(
            "{API_BASE}/setallzones.php?period_id=999&controller_id={cid}&api_key={key}",
            cid = self.config.controller_id,
            key = self.config.api_key,
        );
        let _ = self.get_json(url).await?;
        Ok(())
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let url = format!(
            "{API_BASE}/statusschedule.php?controller_id={cid}&api_key={key}",
            cid = self.config.controller_id,
            key = self.config.api_key,
        );
        match self.get_json(url).await {
            Ok(v) => {
                let resp: StatusScheduleResponse = serde_json::from_value(v.clone())
                    .map_err(|e| ControllerError::Remote(format!("statusschedule decode: {e}")))?;
                let zone_states: Vec<ZoneRuntimeStatus> = resp
                    .relays
                    .iter()
                    .filter_map(|r| {
                        self.relay_to_slug.get(&r.relay_id).map(|slug| {
                            // Hydrawise convention: time=1 -> currently running.
                            let running = r.time == 1;
                            ZoneRuntimeStatus {
                                slug: slug.clone(),
                                running,
                                remaining_s: if running {
                                    r.run.map(|v| v.max(0) as u32)
                                } else {
                                    None
                                },
                                last_run_epoch: None,
                            }
                        })
                    })
                    .collect();
                let status = ControllerStatus {
                    reachable: true,
                    master_enabled: resp.master.map(|m| m != 0),
                    water_level_pct: None,
                    rain_sensor_tripped: None,
                    current_program: None,
                    zone_states,
                    flow_gpm: resp.flow_gpm,
                    firmware: None,
                };
                *self.last_status.lock().await = Some(status.clone());
                Ok(status)
            }
            Err(e) => {
                warn!(controller = %self.id, error = %e, "hydrawise status failed");
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
        // Hydrawise exposes history via reportwatering.php but the
        // payload is heavy + paginated; runs store backfill is enough.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg() -> HydrawiseConfig {
        let mut map = BTreeMap::new();
        map.insert("back_yard".to_string(), 1234);
        map.insert("front_lawn".to_string(), 5678);
        HydrawiseConfig {
            api_key: "K".into(),
            controller_id: 100,
            zone_relay_map: map,
        }
    }

    #[test]
    fn relay_lookup() {
        let h = Hydrawise::new("hd", cfg()).unwrap();
        assert_eq!(h.relay_for("back_yard").unwrap(), 1234);
        assert!(matches!(
            h.relay_for("not_a_zone").unwrap_err(),
            ControllerError::ZoneUnknown(_)
        ));
    }

    #[test]
    fn caps_advertise_flow_master() {
        let h = Hydrawise::new("hd", cfg()).unwrap();
        let caps = h.supports();
        assert!(caps.flow_meter);
        assert!(caps.master_valve);
        assert!(!caps.multi_zone_parallel);
    }
}
