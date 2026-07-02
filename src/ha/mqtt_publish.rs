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

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, ClientError, Event, MqttOptions, Packet, QoS};
use serde::Serialize;
use thiserror::Error;
use tracing::{info, warn};

use crate::config::schema::MqttConfig;
use crate::ha::snapshot::IrrigationSnapshot;

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

    /// Publish discovery for one zone. Issues the bucket_mm + planned_seconds
    /// sensors always, and the `running` binary_sensor only when
    /// `running_known` is true. (No per-zone ET sensor: ET0 is a single
    /// yard-wide forecast value, not per-zone, so a per-zone et_today_mm would
    /// publish the same number N times with no real per-zone producer.)
    ///
    /// `running_known=false` (fire-and-forget MQTT/DIY controllers) means we
    /// never get a trustworthy readback, so `publish_zone_state` withholds the
    /// running state forever. Publishing its discovery anyway would leave HA
    /// with a `running` binary_sensor stuck "unknown" for the life of the
    /// deployment, so we gate the discovery the same way `flow_meter` gates the
    /// flow sensor: no producer, no entity.
    pub async fn publish_zone_discovery(
        &self,
        zone_slug: &str,
        running_known: bool,
    ) -> Result<(), MqttPublishError> {
        let device = self.device();
        let mut entities: Vec<(&str, String, DiscoveryEntity)> = vec![
            (
                "sensor",
                format!("zone_{zone_slug}_bucket_mm"),
                DiscoveryEntity {
                    name: format!("{zone_slug} bucket"),
                    unique_id: format!("{}_zone_{zone_slug}_bucket_mm", self.node_id),
                    state_topic: self.state_topic("sensor", &format!("zone_{zone_slug}_bucket_mm")),
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
        ];
        if running_known {
            entities.push((
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
            ));
        }
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

    /// Publish discovery for the controller's measured flow. Only call this
    /// when the active controller advertises a flow meter
    /// (`IrrigationSnapshot.flow_meter`); otherwise the entity would render
    /// "unknown" forever on non-flow setups. Object id `flow` aligns with
    /// the OpenSprinkler integration's `sensor.sprinkler_flow` naming.
    pub async fn publish_flow_discovery(&self) -> Result<(), MqttPublishError> {
        let entity_flow = DiscoveryEntity {
            name: "LocalSky flow".into(),
            unique_id: format!("{}_flow_gpm", self.node_id),
            state_topic: self.state_topic("sensor", "flow"),
            unit_of_measurement: Some("gpm".into()),
            device_class: Some("volume_flow_rate".into()),
            state_class: Some("measurement".into()),
            icon: Some("mdi:water-pump".into()),
            device: self.device(),
            attribution: "LocalSky".into(),
        };
        let topic = self.config_topic("sensor", "flow");
        let payload = serde_json::to_string(&entity_flow)
            .map_err(|e| MqttPublishError::Client(format!("serialize: {e}")))?;
        self.client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await?;
        Ok(())
    }

    /// Publish the current measured flow value (gpm). `None` (no meter /
    /// no reading) publishes nothing so the entity holds its last value
    /// rather than flapping to "unknown".
    pub async fn publish_flow_state(&self, flow_gpm: Option<f64>) -> Result<(), MqttPublishError> {
        if let Some(v) = flow_gpm {
            let topic = self.state_topic("sensor", "flow");
            self.client
                .publish(topic, QoS::AtLeastOnce, true, format!("{v:.1}"))
                .await?;
        }
        Ok(())
    }

    pub async fn publish_zone_state(
        &self,
        zone_slug: &str,
        bucket_mm: Option<f64>,
        planned_seconds: Option<u32>,
        running: Option<bool>,
    ) -> Result<(), MqttPublishError> {
        if let Some(v) = bucket_mm {
            let topic = self.state_topic("sensor", &format!("zone_{zone_slug}_bucket_mm"));
            self.client
                .publish(topic, QoS::AtLeastOnce, true, format!("{v:.2}"))
                .await?;
        }
        if let Some(v) = planned_seconds {
            let topic = self.state_topic("sensor", &format!("zone_{zone_slug}_planned_seconds"));
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

    /// Publish discovery for every zone in the snapshot + the verdict + (when
    /// the active controller has a flow meter) the flow sensor. Idempotent +
    /// retained, so it is safe to re-issue on each broker (re)connect. HA picks
    /// up the entities the moment it sees the retained config topics.
    pub async fn publish_all_discovery(
        &self,
        snap: &IrrigationSnapshot,
    ) -> Result<(), MqttPublishError> {
        for z in &snap.zones {
            self.publish_zone_discovery(&z.slug, z.running_known)
                .await?;
        }
        self.publish_verdict_discovery().await?;
        if snap.flow_meter {
            self.publish_flow_discovery().await?;
        }
        Ok(())
    }

    /// Publish the current state for every zone + the verdict + flow. Mirrors
    /// the discovery set so HA's auto-created entities carry live values.
    pub async fn publish_all_state(
        &self,
        snap: &IrrigationSnapshot,
    ) -> Result<(), MqttPublishError> {
        for z in &snap.zones {
            // `running_known=false` (fire-and-forget MQTT/DIY) means we cannot
            // trust the readback, so withhold the running state (None) rather
            // than asserting a possibly-wrong OFF; the bucket + planned values
            // are still meaningful.
            let running = if z.running_known {
                Some(z.running)
            } else {
                None
            };
            self.publish_zone_state(
                &z.slug,
                Some(z.bucket_mm),
                Some(z.planned_run_seconds),
                running,
            )
            .await?;
        }
        self.publish_verdict_state(&snap.skip_check.verdict, &snap.skip_check.reason)
            .await?;
        self.publish_flow_state(snap.flow_gpm).await?;
        Ok(())
    }

    /// Graceful disconnect. The runtime calls this on shutdown.
    pub async fn close(self) -> Result<(), MqttPublishError> {
        self.client.disconnect().await?;
        Ok(())
    }
}

/// Spawn the outbound HA-discovery publisher (boot "step 6"). Drives one
/// rumqttc connection: it publishes HA MQTT discovery configs once per
/// (re)connect and republishes live state for every `sensor.localsky_*` /
/// `binary_sensor.localsky_*` entity whenever the engine produces a new
/// irrigation snapshot. Wholly optional: the caller only calls this when
/// `cfg.notifications.mqtt` is set AND the publish toggles are on, so a
/// no-MQTT deploy is unaffected.
///
/// Resilience contract (never panics boot):
///   - construction + the whole loop run inside the spawned task, so a bad
///     broker host can never fail `main`;
///   - the eventloop is polled in the same `select!` as the snapshot watcher,
///     so queued publishes are actually flushed to the wire;
///   - on any eventloop error we log, back off, and reconnect (a fresh
///     `connect`), republishing discovery + the latest state on the new
///     ConnAck so HA recovers after a broker restart.
pub fn spawn(
    cfg: MqttConfig,
    deployment_display_name: String,
    mut snap_rx: tokio::sync::watch::Receiver<Arc<IrrigationSnapshot>>,
) {
    // Stable client id per deployment so a reconnect resumes the same MQTT
    // session identity (and the broker's retained discovery survives).
    let client_id = format!("localsky-pub-{}", slugify(&deployment_display_name));
    info!(
        broker = %cfg.host,
        port = cfg.port,
        discovery_prefix = %cfg.discovery_prefix,
        "ha mqtt publisher: starting (boot step 6)"
    );
    tokio::spawn(async move {
        // Outer loop: each pass owns one connection. On an eventloop error we
        // drop the publisher + eventloop, back off, and reconnect.
        loop {
            let (publisher, mut eventloop) =
                match HaMqttPublisher::connect(&cfg, &deployment_display_name, &client_id).await {
                    Ok(pe) => pe,
                    Err(e) => {
                        warn!(error = %e, "ha mqtt publisher: connect failed; retrying in 10s");
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue;
                    }
                };

            // Track whether we have an active session: only publish state once
            // the broker has ConnAck'd (a publish before connect just queues).
            let mut connected = false;

            // Inner loop: poll the eventloop (drives I/O + reconnect detection)
            // and react to new snapshots. We `borrow()` the watch on every
            // wake, so a snapshot that arrived before ConnAck is still picked
            // up on the first post-connect publish.
            loop {
                tokio::select! {
                    ev = eventloop.poll() => {
                        match ev {
                            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                                connected = true;
                                info!("ha mqtt publisher: connected; publishing discovery");
                                let snap = snap_rx.borrow().clone();
                                if let Err(e) = publisher.publish_all_discovery(&snap).await {
                                    warn!(error = %e, "ha mqtt publisher: discovery publish failed");
                                }
                                // Seed live state immediately so HA's freshly
                                // discovered entities are not "unknown".
                                if let Err(e) = publisher.publish_all_state(&snap).await {
                                    warn!(error = %e, "ha mqtt publisher: initial state publish failed");
                                }
                            }
                            Ok(_) => {} // PingResp, PubAck, outgoing, etc.
                            Err(e) => {
                                warn!(error = %e, "ha mqtt publisher: eventloop error; reconnecting in 5s");
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                break; // drop this connection; outer loop reconnects
                            }
                        }
                    }
                    changed = snap_rx.changed() => {
                        if changed.is_err() {
                            // The refresher dropped its sender (process is
                            // tearing down): stop the publisher cleanly.
                            info!("ha mqtt publisher: snapshot channel closed; stopping");
                            let _ = publisher.close().await;
                            return;
                        }
                        if connected {
                            let snap = snap_rx.borrow().clone();
                            if let Err(e) = publisher.publish_all_state(&snap).await {
                                warn!(error = %e, "ha mqtt publisher: state publish failed");
                            }
                        }
                    }
                }
            }
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Front Yard"), "front_yard");
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

    #[test]
    fn discovery_payload_carries_ha_required_keys() {
        // HA MQTT discovery requires (at minimum) state_topic + unique_id +
        // a device block on each entity config payload; the optional
        // unit_of_measurement / device_class / state_class are skipped when
        // None (so a string sensor like the verdict serializes cleanly).
        let e = DiscoveryEntity {
            name: "back_yard bucket".into(),
            unique_id: "yard_zone_back_yard_bucket_mm".into(),
            state_topic: "homeassistant/sensor/yard/zone_back_yard_bucket_mm/state".into(),
            unit_of_measurement: Some("mm".into()),
            device_class: None,
            state_class: Some("measurement".into()),
            icon: Some("mdi:water".into()),
            device: DiscoveryDevice {
                identifiers: vec!["yard".into()],
                name: "Yard".into(),
                manufacturer: "LocalSky".into(),
                model: "LocalSky v2".into(),
                sw_version: "0.7.0".into(),
            },
            attribution: "LocalSky".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert!(
            v.get("state_topic").is_some(),
            "discovery needs state_topic"
        );
        assert!(v.get("unique_id").is_some(), "discovery needs unique_id");
        assert!(
            v.get("device").and_then(|d| d.get("identifiers")).is_some(),
            "discovery needs device.identifiers"
        );
        // None-valued optionals are omitted (HA tolerates absence).
        assert!(
            v.get("device_class").is_none(),
            "None device_class must be skipped, not null"
        );
    }

    #[test]
    fn slugify_yields_a_safe_topic_node_id() {
        // The node id keys the discovery topic
        // (homeassistant/<component>/<node>/<object>/config), so it must be a
        // safe single topic segment (no spaces, no '/').
        let node = slugify("North Lawn / Strip");
        assert!(!node.contains(' '));
        assert!(!node.contains('/'));
        assert_eq!(node, "north_lawn_strip");
    }
}
