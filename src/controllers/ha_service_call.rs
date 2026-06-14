// HA service-call controller. Wraps Home Assistant's REST `/api/services`
// endpoint to dispatch zone runs through any HA-side irrigation
// integration (OpenSprinkler component, Irrigation Unlimited, Rachio
// HACS, etc.).
//
// This is the legacy continuity path: v0.1 LocalSky deployments that
// already drive irrigation through HA can keep working under v2 by
// configuring this controller, with no other plumbing changes.
//
// The adapter reads `config.start_service` + `config.stop_service` for
// the HA service names; per-zone mapping flows through
// config.zone_entity_map (slug -> HA entity_id / station identifier).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use crate::config::schema::HaServiceCallConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord,
};

pub struct HaServiceCall {
    id: String,
    config: HaServiceCallConfig,
    client: Client,
}

impl HaServiceCall {
    pub fn new(
        id: impl Into<String>,
        config: HaServiceCallConfig,
    ) -> Result<Self, ControllerError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| ControllerError::Init(format!("reqwest client: {e}")))?;
        Ok(Self {
            id: id.into(),
            config,
            client,
        })
    }

    fn entity_for(&self, slug: &str) -> Result<String, ControllerError> {
        self.config
            .zone_entity_map
            .get(slug)
            .cloned()
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    async fn call_service(&self, service_dotted: &str, data: Value) -> Result<(), ControllerError> {
        let (domain, service) = service_dotted.split_once('.').ok_or_else(|| {
            ControllerError::Remote(format!(
                "service '{service_dotted}' must be 'domain.action'"
            ))
        })?;
        let url = format!(
            "{}/api/services/{}/{}",
            self.config.base_url.trim_end_matches('/'),
            domain,
            service
        );
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.config.bearer_token)
            .json(&data)
            .send()
            .await
            .map_err(|e| ControllerError::Transport(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ControllerError::AuthFailed);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ControllerError::Remote(format!("HTTP {status}: {body}")));
        }
        Ok(())
    }
}

#[async_trait]
impl IrrigationController for HaServiceCall {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            // We can't know what's behind HA's service call, so report
            // conservative caps. Operators with stronger backends can
            // surface flow / rain via separate HA sensors.
            flow_meter: false,
            rain_sensor: false,
            master_valve: false,
            multi_zone_parallel: false,
            history_query: false,
            remote_program_upload: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let entity = self.entity_for(slug)?;
        // The dispatch shape follows the most common HA irrigation
        // service: `entity_id` + minutes. Operators who use a different
        // service (e.g. opensprinkler.run_station expects sid + seconds)
        // can override `start_service` and the wrapper passes a normalized
        // payload that the receiving automation can transform.
        let payload = json!({
            "entity_id": entity,
            "duration_s": duration_s,
            // Provide both unit formats; the receiver picks the one it
            // understands (HA service template can switch on which is
            // present).
            "minutes": (duration_s as f64 / 60.0),
        });
        self.call_service(&self.config.start_service, payload)
            .await?;
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: now_epoch(),
            planned_duration_s: duration_s,
            provider_ref: Some(entity),
        })
    }

    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        let entity = self.entity_for(slug)?;
        let payload = json!({ "entity_id": entity });
        self.call_service(&self.config.stop_service, payload).await
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        // Call the configured stop_service without an entity_id to mean
        // "all". Receivers that don't support this should be wired with
        // a discrete stop_all_service in a future config field.
        self.call_service(&self.config.stop_service, json!({}))
            .await
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        // We can't enumerate zone state through service calls alone.
        // The reachable flag flips by hitting /api/ with a HEAD request.
        let url = format!("{}/api/", self.config.base_url.trim_end_matches('/'));
        let reachable = self
            .client
            .get(&url)
            .bearer_auth(&self.config.bearer_token)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        Ok(ControllerStatus {
            reachable,
            master_enabled: None,
            water_level_pct: None,
            rain_sensor_tripped: None,
            current_program: None,
            zone_states: Vec::new(),
            flow_gpm: None,
            flow_connected: false,
            firmware: None,
        })
    }

    async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        // HA service calls don't expose history; the scheduler relies on
        // its own runs table. Return empty so backfill is a no-op.
        Ok(Vec::new())
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

    fn cfg() -> HaServiceCallConfig {
        let mut map = std::collections::BTreeMap::new();
        map.insert("back_yard".to_string(), "switch.back_yard_zone".to_string());
        HaServiceCallConfig {
            base_url: "http://example.invalid:8123".into(),
            bearer_token: "tok".into(),
            start_service: "script.os_zone_toggle".into(),
            stop_service: "opensprinkler.stop".into(),
            zone_entity_map: map,
        }
    }

    #[test]
    fn entity_resolves_for_mapped_zone() {
        let c = HaServiceCall::new("ha1", cfg()).unwrap();
        assert_eq!(
            c.entity_for("back_yard").unwrap(),
            "switch.back_yard_zone".to_string()
        );
    }

    #[test]
    fn entity_unknown_zone_errors() {
        let c = HaServiceCall::new("ha1", cfg()).unwrap();
        assert!(matches!(
            c.entity_for("nonexistent"),
            Err(ControllerError::ZoneUnknown(_))
        ));
    }

    #[test]
    fn caps_are_conservative() {
        let c = HaServiceCall::new("ha1", cfg()).unwrap();
        let caps = c.supports();
        assert!(!caps.flow_meter);
        assert!(!caps.history_query);
    }
}
