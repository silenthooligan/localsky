// Rachio cloud controller — Gen 2 / Gen 3 / Smart Hose Timer.
//
// Talks to api.rach.io/1/public via Bearer-token auth. Each LocalSky
// zone slug is mapped to a Rachio zone UUID (the controller exposes
// stable UUIDs; the wizard's zone-scan can fetch them via
// GET /device/{deviceId}).
//
// Endpoints used (public v1):
//   PUT /zone/start             body {id, duration}        run one zone
//   PUT /zone/start_multiple    body {zones: [...]}        run a sequence
//   PUT /device/stop_water      body {id}                  stop everything
//   GET /device/{deviceId}                                  device + zone state
//   GET /device/{deviceId}/event?startTime=&endTime=        history
//
// Caveats:
//   - The public API has NO per-zone stop. The only stop op is
//     `device/stop_water` (stops the whole controller). `stop_zone`
//     here calls stop_water and emits a tracing::warn so operators
//     know other concurrent zones (rare on Rachio — typically single
//     active) will also stop.
//   - Rate limit: ~1700 req/day per token. The status() poll honors
//     the configured poll_interval_s (the schema doesn't expose it
//     for Rachio yet — falls back to 60s here. Future: add field).
//   - Auth failures (401/403) map to ControllerError::AuthFailed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::schema::RachioConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

const API_BASE: &str = "https://api.rach.io/1/public";

pub struct Rachio {
    id: String,
    config: RachioConfig,
    client: Client,
    /// Reverse map (zone uuid -> slug) computed from config at construction
    /// for fast lookup during status() — Rachio returns zones by uuid.
    uuid_to_slug: HashMap<String, String>,
    /// Last successful status snapshot, used as a fallback when the
    /// device endpoint times out (Rachio's API can be flaky during
    /// firmware updates). Kept in an Arc<Mutex<>> so status() can
    /// read+write concurrently.
    last_status: Arc<Mutex<Option<ControllerStatus>>>,
}

impl Rachio {
    pub fn new(id: impl Into<String>, config: RachioConfig) -> Result<Self, ControllerError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| ControllerError::Init(format!("reqwest client: {e}")))?;
        let uuid_to_slug = config
            .zone_uuid_map
            .iter()
            .map(|(slug, uuid)| (uuid.clone(), slug.clone()))
            .collect();
        Ok(Self {
            id: id.into(),
            config,
            client,
            uuid_to_slug,
            last_status: Arc::new(Mutex::new(None)),
        })
    }

    fn uuid_for(&self, slug: &str) -> Result<String, ControllerError> {
        self.config
            .zone_uuid_map
            .get(slug)
            .cloned()
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    async fn put_json(
        &self,
        path: &str,
        body: Value,
    ) -> Result<reqwest::Response, ControllerError> {
        let url = format!("{API_BASE}{path}");
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.config.api_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| ControllerError::Transport(e.to_string()))?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ControllerError::AuthFailed);
        }
        if status.as_u16() == 429 {
            return Err(ControllerError::RateLimited);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ControllerError::Remote(format!("HTTP {status}: {body}")));
        }
        Ok(resp)
    }

    async fn get_json(&self, path: &str) -> Result<Value, ControllerError> {
        let url = format!("{API_BASE}{path}");
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.config.api_token)
            .send()
            .await
            .map_err(|e| ControllerError::Transport(e.to_string()))?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ControllerError::AuthFailed);
        }
        if status.as_u16() == 429 {
            return Err(ControllerError::RateLimited);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ControllerError::Remote(format!("HTTP {status}: {body}")));
        }
        resp.json()
            .await
            .map_err(|e| ControllerError::Remote(format!("invalid json: {e}")))
    }
}

#[async_trait]
impl IrrigationController for Rachio {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            // Most Rachio Gen 3 controllers report a rain sensor input.
            // Flow meter is supported on some Gen 3 hardware via the
            // accessory port but the public API exposes it as a normal
            // sensor field; treat as available.
            flow_meter: true,
            rain_sensor: true,
            master_valve: true,
            // Rachio fires zones sequentially by default but the
            // `start_multiple` endpoint takes a sortOrder — the engine
            // treats this as "sequential queue" not "parallel".
            multi_zone_parallel: false,
            history_query: true,
            // /event accepts arbitrary JSON; no native "upload my schedule"
            // API for community-built programs.
            remote_program_upload: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let zone_uuid = self.uuid_for(slug)?;
        // Rachio API caps a single zone start at 3 hours (10800s).
        let clamped = duration_s.min(10_800).max(1);
        if clamped != duration_s {
            warn!(
                slug = %slug,
                requested = duration_s,
                clamped = clamped,
                "Rachio caps zone duration at 10800s; clamping"
            );
        }
        self.put_json(
            "/zone/start",
            json!({ "id": zone_uuid, "duration": clamped }),
        )
        .await?;
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: chrono::Utc::now().timestamp(),
            planned_duration_s: clamped,
            provider_ref: Some(zone_uuid),
        })
    }

    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        // Rachio's public API has no per-zone stop; only device-wide
        // stop_water. Single-zone irrigation is the common case so this
        // is effectively equivalent, but we warn for visibility.
        warn!(
            slug = %slug,
            "Rachio public API has no per-zone stop; calling device/stop_water (affects all running zones on this device)"
        );
        // Validate slug is mapped — fail fast on typos rather than
        // surprise-stop the whole controller for an unknown zone.
        self.uuid_for(slug)?;
        self.put_json("/device/stop_water", json!({ "id": self.config.device_id }))
            .await?;
        Ok(())
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        self.put_json("/device/stop_water", json!({ "id": self.config.device_id }))
            .await?;
        Ok(())
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let path = format!("/device/{}", self.config.device_id);
        let device = match self.get_json(&path).await {
            Ok(v) => v,
            Err(e) => {
                // Fall back to last known-good snapshot if the API is
                // momentarily unreachable. Mark reachable=false so the
                // dashboard surfaces the degraded state.
                debug!("rachio status fetch failed, falling back to last_status: {e}");
                let last = self.last_status.lock().await;
                if let Some(s) = last.as_ref() {
                    let mut stale = s.clone();
                    stale.reachable = false;
                    return Ok(stale);
                }
                drop(last);
                return Err(e);
            }
        };

        // Parse Rachio's device shape — zones[].id (uuid), .zoneNumber,
        // .name, .enabled, .currentZone (currently running uuid at
        // device level).
        let current_zone_uuid = device
            .get("currentZone")
            .and_then(|c| c.get("zoneId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let zones_arr = device
            .get("zones")
            .and_then(|z| z.as_array())
            .cloned()
            .unwrap_or_default();
        let mut zone_states = Vec::new();
        for z in &zones_arr {
            let uuid = match z.get("id").and_then(|v| v.as_str()) {
                Some(u) => u.to_string(),
                None => continue,
            };
            let slug = match self.uuid_to_slug.get(&uuid) {
                Some(s) => s.clone(),
                None => continue, // zone not mapped — ignore
            };
            let running = current_zone_uuid.as_deref() == Some(&uuid);
            let remaining_s = z
                .get("runtime")
                .and_then(|v| v.as_u64())
                .filter(|_| running)
                .map(|v| v as u32);
            zone_states.push(ZoneRuntimeStatus {
                slug,
                running,
                remaining_s,
                last_run_epoch: z
                    .get("lastWateredDate")
                    .and_then(|v| v.as_i64())
                    // Rachio returns ms — convert.
                    .map(|ms| ms / 1000),
            });
        }

        let status = ControllerStatus {
            reachable: true,
            master_enabled: device
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s == "ONLINE"),
            water_level_pct: None, // Rachio doesn't expose this directly
            rain_sensor_tripped: device.get("rainSensorTripped").and_then(|v| v.as_bool()),
            current_program: device
                .get("scheduleRules")
                .and_then(|r| r.as_array())
                .and_then(|arr| arr.first())
                .and_then(|r| r.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            zone_states,
            flow_gpm: None,
            firmware: device
                .get("firmwareVersion")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        let mut last = self.last_status.lock().await;
        *last = Some(status.clone());
        Ok(status)
    }

    async fn run_history(&self, since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let since_ms = since_epoch * 1000;
        let path = format!(
            "/device/{}/event?startTime={}&endTime={}",
            self.config.device_id, since_ms, now_ms
        );
        let events = self.get_json(&path).await?;
        let arr = events.as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        for evt in &arr {
            // Rachio history events with type=ZONE_STATUS and
            // subType=ZONE_STARTED / ZONE_COMPLETED. We pair starts
            // with their matching completes when available; for the
            // initial pass we just emit a record per ZONE_COMPLETED
            // with start_epoch=startMs and end_epoch=eventMs.
            let evt_type = evt.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let evt_sub = evt.get("subType").and_then(|v| v.as_str()).unwrap_or("");
            if evt_type != "ZONE_STATUS" || evt_sub != "ZONE_COMPLETED" {
                continue;
            }
            let zone_uuid = evt
                .get("eventDatas")
                .and_then(|d| d.as_array())
                .and_then(|arr| {
                    arr.iter().find_map(|d| {
                        d.get("key")
                            .and_then(|k| k.as_str())
                            .filter(|k| *k == "zoneId")
                            .and_then(|_| d.get("value"))
                            .and_then(|v| v.as_str())
                    })
                })
                .map(|s| s.to_string());
            let zone_uuid = match zone_uuid {
                Some(u) => u,
                None => continue,
            };
            let slug = match self.uuid_to_slug.get(&zone_uuid) {
                Some(s) => s.clone(),
                None => continue,
            };
            let end_ms = evt
                .get("eventDate")
                .and_then(|v| v.as_i64())
                .unwrap_or(now_ms);
            let dur_s = evt
                .get("eventDatas")
                .and_then(|d| d.as_array())
                .and_then(|arr| {
                    arr.iter().find_map(|d| {
                        d.get("key")
                            .and_then(|k| k.as_str())
                            .filter(|k| *k == "duration")
                            .and_then(|_| d.get("value"))
                            .and_then(|v| v.as_u64())
                    })
                })
                .map(|v| v as u32);
            let start_ms = end_ms - dur_s.map(|s| (s as i64) * 1000).unwrap_or(0);
            out.push(RunRecord {
                zone_slug: slug,
                start_epoch: start_ms / 1000,
                end_epoch: Some(end_ms / 1000),
                duration_s: dur_s,
                source: "rachio".to_string(),
            });
        }
        Ok(out)
    }

    async fn discover_zones(
        &self,
    ) -> ControllerResult<Vec<crate::ports::irrigation_controller::DiscoveredZone>> {
        use crate::ports::irrigation_controller::DiscoveredZone;
        let device = self
            .get_json(&format!("/device/{}", self.config.device_id))
            .await?;
        let zones = device
            .get("zones")
            .and_then(|z| z.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for z in &zones {
            let Some(uuid) = z.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            // Skip disabled zones — they can't be watered.
            if z.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                continue;
            }
            let name = z
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    let n = z.get("zoneNumber").and_then(|v| v.as_u64()).unwrap_or(0);
                    format!("Zone {n}")
                });
            out.push(DiscoveredZone {
                station_id: uuid.to_string(),
                name,
            });
        }
        Ok(out)
    }
}

#[allow(dead_code)] // serde response type, kept to mirror the API shape
#[derive(Debug, Deserialize, Serialize)]
struct RachioPersonInfo {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cfg() -> RachioConfig {
        RachioConfig {
            api_token: "test".into(),
            device_id: "dev-1".into(),
            zone_uuid_map: {
                let mut m = BTreeMap::new();
                m.insert("front".into(), "uuid-front".into());
                m.insert("back".into(), "uuid-back".into());
                m
            },
        }
    }

    #[test]
    fn uuid_for_known_slug() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        assert_eq!(r.uuid_for("front").unwrap(), "uuid-front");
    }

    #[test]
    fn uuid_for_unknown_slug() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        assert!(matches!(
            r.uuid_for("side"),
            Err(ControllerError::ZoneUnknown(_))
        ));
    }

    #[test]
    fn reverse_map_populated() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        assert_eq!(r.uuid_to_slug.get("uuid-front"), Some(&"front".to_string()));
        assert_eq!(r.uuid_to_slug.get("uuid-back"), Some(&"back".to_string()));
    }

    #[test]
    fn supports_caps() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        let caps = r.supports();
        assert!(caps.rain_sensor);
        assert!(caps.master_valve);
        assert!(caps.history_query);
        // Rachio runs zones sequentially, not in parallel.
        assert!(!caps.multi_zone_parallel);
    }
}
