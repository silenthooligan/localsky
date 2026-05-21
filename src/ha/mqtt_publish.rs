// Outbound MQTT publisher for Home Assistant. Implements the HA discovery
// protocol so users with HA get auto-created sensor.localsky_* entities
// without LocalSky reading HA. This is the HA-optional bridge: with MQTT
// configured, HA users get value for free; without it, the app runs
// fully standalone.
//
// Discovery topic shape (per HA's MQTT integration spec):
//   <discovery_prefix>/<component>/<node_id>/<object_id>/config
//   <discovery_prefix>/<component>/<node_id>/<object_id>/state
//
// We use the deployment display_name (slugified) as <node_id> so the
// same broker can serve multiple LocalSky deployments without collision.

use std::time::Duration;

use rumqttc::{AsyncClient, ClientError, MqttOptions, QoS};
use serde::Serialize;
use thiserror::Error;
use tracing::{info, warn};

use crate::config::schema::MqttConfig;

#[derive(Debug, Error)]
pub enum MqttPublishError {
    #[error("mqtt client error: {0}")]
    Client(String),
}

impl From<ClientError> for MqttPublishError {
    fn from(e: ClientError) -> Self {
        Self::Client(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryDevice {
    pub identifiers: Vec<String>,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub sw_version: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryEntity {
    pub name: String,
    pub unique_id: String,
    pub state_topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_of_measurement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub device: DiscoveryDevice,
    /// Marker attribute so any ha_passthrough source can refuse to
    /// ingest this entity back into LocalSky (avoids publish/subscribe
    /// cycles).
    pub attribution: String,
}

pub struct HaMqttPublisher {
    client: AsyncClient,
    discovery_prefix: String,
    node_id: String,
    deployment_name: String,
}

impl HaMqttPublisher {
    /// Connect to the configured broker. Returns the publisher plus an
    /// EventLoop future the runtime drives in a spawned task.
    pub async fn connect(
        cfg: &MqttConfig,
        deployment_display_name: &str,
        client_id: &str,
    ) -> Result<(Self, rumqttc::EventLoop), MqttPublishError> {
        let mut opts = MqttOptions::new(client_id, &cfg.host, cfg.port);
        opts.set_keep_alive(Duration::from_secs(60));
        if let (Some(u), Some(p)) = (&cfg.username, &cfg.password) {
            opts.set_credentials(u, p);
        }
        let (client, eventloop) = AsyncClient::new(opts, 10);
        let node_id = slugify(deployment_display_name);
        Ok((
            Self {
                client,
                discovery_prefix: cfg.discovery_prefix.clone(),
                node_id,
                deployment_name: deployment_display_name.to_string(),
            },
            eventloop,
        ))
    }

    fn state_topic(&self, component: &str, object_id: &str) -> String {
        format!(
            "{}/{}/{}/{}/state",
            self.discovery_prefix, component, self.node_id, object_id
        )
    }

    fn config_topic(&self, component: &str, object_id: &str) -> String {
        format!(
            "{}/{}/{}/{}/config",
            self.discovery_prefix, component, self.node_id, object_id
        )
    }

    fn device(&self) -> DiscoveryDevice {
        DiscoveryDevice {
            identifiers: vec![self.node_id.clone()],
            name: self.deployment_name.clone(),
            manufacturer: "LocalSky".into(),
            model: "LocalSky v2".into(),
            sw_version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    /// Publish discovery for one zone. Issues four entities per zone:
    /// bucket_mm, et_today_mm, planned_seconds, running.
    pub async fn publish_zone_discovery(
        &self,
        zone_slug: &str,
    ) -> Result<(), MqttPublishError> {
        let device = self.device();
        let entities: Vec<(&str, String, DiscoveryEntity)> = vec![
            (
                "sensor",
                format!("zone_{zone_slug}_bucket_mm"),
                DiscoveryEntity {
                    name: format!("{zone_slug} bucket"),
                    unique_id: format!("{}_zone_{zone_slug}_bucket_mm", self.node_id),
                    state_topic: self
                        .state_topic("sensor", &format!("zone_{zone_slug}_bucket_mm")),
                    unit_of_measurement: Some("mm".into()),
                    device_class: None,
                    state_class: Some("measurement".into()),
                    icon: Some("mdi:water".into()),
                    device: device.clone(),
                    attribution: "LocalSky".into(),
                },
            ),
            (
                "sensor",
                format!("zone_{zone_slug}_et_today_mm"),
                DiscoveryEntity {
                    name: format!("{zone_slug} ET today"),
                    unique_id: format!("{}_zone_{zone_slug}_et_today_mm", self.node_id),
                    state_topic: self
                        .state_topic("sensor", &format!("zone_{zone_slug}_et_today_mm")),
                    unit_of_measurement: Some("mm".into()),
                    device_class: None,
                    state_class: Some("measurement".into()),
                    icon: Some("mdi:weather-sunny".into()),
                    device: device.clone(),
                    attribution: "LocalSky".into(),
                },
            ),
            (
                "sensor",
                format!("zone_{zone_slug}_planned_seconds"),
                DiscoveryEntity {
                    name: format!("{zone_slug} planned seconds"),
                    unique_id: format!("{}_zone_{zone_slug}_planned_s", self.node_id),
                    state_topic: self
                        .state_topic("sensor", &format!("zone_{zone_slug}_planned_seconds")),
                    unit_of_measurement: Some("s".into()),
                    device_class: Some("duration".into()),
                    state_class: Some("measurement".into()),
                    icon: Some("mdi:timer".into()),
                    device: device.clone(),
                    attribution: "LocalSky".into(),
                },
            ),
            (
                "binary_sensor",
                format!("zone_{zone_slug}_running"),
                DiscoveryEntity {
                    name: format!("{zone_slug} running"),
                    unique_id: format!("{}_zone_{zone_slug}_running", self.node_id),
                    state_topic: self
                        .state_topic("binary_sensor", &format!("zone_{zone_slug}_running")),
                    unit_of_measurement: None,
                    device_class: Some("running".into()),
                    state_class: None,
                    icon: Some("mdi:sprinkler-variant".into()),
                    device: device.clone(),
                    attribution: "LocalSky".into(),
                },
            ),
        ];
        for (component, object_id, entity) in entities {
            let topic = self.config_topic(component, &object_id);
            let payload = serde_json::to_string(&entity)
                .map_err(|e| MqttPublishError::Client(format!("serialize: {e}")))?;
            self.client
                .publish(topic, QoS::AtLeastOnce, true, payload)
                .await?;
        }
        Ok(())
    }

    /// Publish the daily verdict + reason as a sensor pair.
    pub async fn publish_verdict_discovery(&self) -> Result<(), MqttPublishError> {
        let entity_verdict = DiscoveryEntity {
            name: "LocalSky verdict".into(),
            unique_id: format!("{}_verdict_today", self.node_id),
            state_topic: self.state_topic("sensor", "verdict_today"),
            unit_of_measurement: None,
            device_class: None,
            state_class: None,
            icon: Some("mdi:scale-balance".into()),
            device: self.device(),
            attribution: "LocalSky".into(),
        };
        let topic = self.config_topic("sensor", "verdict_today");
        let payload = serde_json::to_string(&entity_verdict)
            .map_err(|e| MqttPublishError::Client(format!("serialize: {e}")))?;
        self.client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await?;
        Ok(())
    }

    pub async fn publish_zone_state(
        &self,
        zone_slug: &str,
        bucket_mm: Option<f64>,
        et_today_mm: Option<f64>,
        planned_seconds: Option<u32>,
        running: Option<bool>,
    ) -> Result<(), MqttPublishError> {
        if let Some(v) = bucket_mm {
            let topic = self.state_topic("sensor", &format!("zone_{zone_slug}_bucket_mm"));
            self.client
                .publish(topic, QoS::AtLeastOnce, true, format!("{v:.2}"))
                .await?;
        }
        if let Some(v) = et_today_mm {
            let topic = self.state_topic("sensor", &format!("zone_{zone_slug}_et_today_mm"));
            self.client
                .publish(topic, QoS::AtLeastOnce, true, format!("{v:.2}"))
                .await?;
        }
        if let Some(v) = planned_seconds {
            let topic =
                self.state_topic("sensor", &format!("zone_{zone_slug}_planned_seconds"));
            self.client
                .publish(topic, QoS::AtLeastOnce, true, format!("{v}"))
                .await?;
        }
        if let Some(v) = running {
            let topic = self.state_topic("binary_sensor", &format!("zone_{zone_slug}_running"));
            self.client
                .publish(topic, QoS::AtLeastOnce, true, if v { "ON" } else { "OFF" })
                .await?;
        }
        Ok(())
    }

    pub async fn publish_verdict_state(
        &self,
        verdict: &str,
        _reason: &str,
    ) -> Result<(), MqttPublishError> {
        let topic = self.state_topic("sensor", "verdict_today");
        self.client
            .publish(topic, QoS::AtLeastOnce, true, verdict.to_string())
            .await?;
        Ok(())
    }

    /// Graceful disconnect. The runtime calls this on shutdown.
    pub async fn close(self) -> Result<(), MqttPublishError> {
        self.client.disconnect().await?;
        Ok(())
    }
}

/// Slugify a free-text display name into a safe MQTT topic segment.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('_');
            last_dash = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "localsky".to_string()
    } else {
        out
    }
}

#[allow(dead_code)]
fn surface_warning() {
    warn!("placeholder so tracing isn't optimized out in tests");
    info!("...");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Aperture Yard"), "aperture_yard");
        assert_eq!(slugify("Casa-Bonita 2!"), "casa_bonita_2");
        assert_eq!(slugify("  weird   spaces  "), "weird_spaces");
        assert_eq!(slugify(""), "localsky");
        assert_eq!(slugify("???"), "localsky");
    }

    #[test]
    fn slugify_strips_trailing_underscores() {
        assert_eq!(slugify("test  "), "test");
    }

    #[test]
    fn discovery_entity_serializes_attribution() {
        let e = DiscoveryEntity {
            name: "n".into(),
            unique_id: "u".into(),
            state_topic: "s".into(),
            unit_of_measurement: None,
            device_class: None,
            state_class: None,
            icon: None,
            device: DiscoveryDevice {
                identifiers: vec!["i".into()],
                name: "n".into(),
                manufacturer: "LocalSky".into(),
                model: "v2".into(),
                sw_version: "0.2.0".into(),
            },
            attribution: "LocalSky".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"attribution\":\"LocalSky\""));
    }
}
