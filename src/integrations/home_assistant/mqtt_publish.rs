// Outbound MQTT publisher for Home Assistant. Implements the HA discovery
// protocol so users with HA get auto-created sensor.localsky_* entities
// without LocalSky reading HA. This is the HA-optional bridge: with MQTT
// configured, HA users get value for free; without it, the app runs
// fully standalone.
//
// Discovery topic shape (per HA's MQTT integration spec):
//   <discovery_prefix>/<component>/<node_id>/<object_id>/config
//   <discovery_prefix>/<component>/<node_id>/<object_id>/state
//   <discovery_prefix>/<node_id>/availability
//
// We use the deployment display_name (slugified) as <node_id> so the
// same broker can serve multiple LocalSky deployments without collision.
//
// Every state publish is retained, which is what makes a stopped LocalSky
// dangerous to read: the broker keeps handing HA the last value as a current
// measurement. The availability topic is the one thing that can say
// otherwise. It is retained, it is this client's last will, and every
// discovery payload points at it, so a crash, an OOM kill, a redeploy and a
// dropped broker link all land on "offline" and HA marks the entities
// unavailable instead of freezing them. A missing flow reading is published
// as unknown even while LocalSky itself remains online.

use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, ClientError, Event, LastWill, MqttOptions, Packet, QoS};
use serde::Serialize;
use thiserror::Error;
use tracing::{info, warn};

use crate::config::schema::MqttConfig;
use crate::model::IrrigationSnapshot;

#[derive(Debug, Error)]
pub enum MqttPublishError {
    #[error("mqtt client error: {0}")]
    Client(#[source] Box<crate::failure::Failure>),
}

impl From<ClientError> for MqttPublishError {
    fn from(e: ClientError) -> Self {
        Self::Client(Box::new(crate::diagnostics::from_error(
            &e,
            "HA MQTT publish queue",
        )))
    }
}

/// Retained payloads on the availability topic. These are exactly HA's
/// defaults for `payload_available` / `payload_not_available`, so a discovery
/// payload carries the topic and nothing else.
const PAYLOAD_ONLINE: &str = "online";
const PAYLOAD_OFFLINE: &str = "offline";

/// Worst case publishes ONE zone contributes to the burst below. Discovery
/// and state are built from the same snapshot, so the two shapes are: with a
/// bucket, three configs (planned, bucket, running) and three state values;
/// without one, two retained clears plus two configs and two state values.
/// Both cost six.
const PUBLISHES_PER_ZONE: usize = 6;

/// The yard-wide part of that burst: the retained "online", the verdict and
/// flow configs, and the verdict and flow state values.
const PUBLISHES_YARD_WIDE: usize = 5;

/// The yard the sizing below is derived from. Every irrigation controller
/// LocalSky drives tops out well under this (OpenSprinkler reaches 72 stations
/// fully expanded, Hydrawise 54, Rachio 16). `spawn` logs when a snapshot's own
/// burst would not fit, so a yard past the sizing is a line in the log rather
/// than a connection that mysteriously went quiet.
const MAX_BURST_ZONES: usize = 128;

/// Room above ONE worst-case burst.
///
/// Bursts overlap. `poll()` moves one request per call and stops taking any
/// while 100 QoS-1 publishes are unacked, so a snapshot that lands while the
/// ConnAck burst is still draining enqueues on top of what is left of it --
/// and with both `select!` arms ready, tokio picks between them at random, so
/// that overlap is ordinary rather than exotic. Sizing the channel to exactly
/// one burst would make the second one park.
const BURST_HEADROOM: usize = 512;

/// Capacity of rumqttc's request channel. Not a tuning knob: a correctness
/// bound.
///
/// The channel is drained only by `EventLoop::poll()`, and the ConnAck arm of
/// `spawn`'s `select!` enqueues its whole burst -- "online", every discovery
/// config, every state value -- with the eventloop not polled again until the
/// arm returns. A publish that does not fit parks forever; the eventloop is
/// then never polled, so NOTHING reaches the wire, not even the "online"
/// enqueued first. HA would hold the previous session's retained configs, read
/// the retained "offline" the last will left, and show every
/// `sensor.localsky_*` unavailable while LocalSky ran and watered normally:
/// worse than the frozen values this availability topic exists to prevent.
///
/// rumqttc's examples pass 10, which is roughly one two-zone yard with nothing
/// optional configured. The bound has to be the whole burst instead. flume
/// does not preallocate a bounded channel, so unused headroom costs nothing.
/// Never 0: flume reads that as a rendezvous channel, and the first publish
/// would deadlock against an eventloop nobody is polling.
const REQUEST_CHANNEL_CAP: usize =
    PUBLISHES_PER_ZONE * MAX_BURST_ZONES + PUBLISHES_YARD_WIDE + BURST_HEADROOM;

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
    /// Retained topic carrying "online" / "offline" for the LocalSky process
    /// itself. Not optional: state is published retained, so an entity that
    /// names no availability topic goes on serving the last value a dead
    /// process left on the broker, with nothing to tell HA otherwise.
    pub availability_topic: String,
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
    /// Computed once at connect so the last will, the online publish and
    /// every discovery payload can never name different topics.
    availability_topic: String,
}

impl HaMqttPublisher {
    /// Connect to the configured broker. Returns the publisher plus an
    /// EventLoop future the runtime drives in a spawned task.
    ///
    /// The last will is armed here, before the socket exists, because it is
    /// the only announcement a crashed or killed process ever gets: the
    /// broker publishes it on any ungraceful close, so the retained
    /// "offline" reaches HA without LocalSky running another line of code.
    pub async fn connect(
        cfg: &MqttConfig,
        deployment_display_name: &str,
        client_id: &str,
    ) -> Result<(Self, rumqttc::EventLoop), MqttPublishError> {
        let node_id = slugify(deployment_display_name);
        // Normalized once, here, so the will, the online publish and all three
        // topic builders can never disagree about the prefix.
        let discovery_prefix = normalize_discovery_prefix(&cfg.discovery_prefix);
        let availability_topic = availability_topic(&discovery_prefix, &node_id);
        let mut opts = MqttOptions::new(client_id, &cfg.host, cfg.port);
        opts.set_keep_alive(Duration::from_secs(60));
        opts.set_last_will(LastWill::new(
            availability_topic.clone(),
            PAYLOAD_OFFLINE,
            QoS::AtLeastOnce,
            true,
        ));
        if let (Some(u), Some(p)) = (&cfg.username, &cfg.password) {
            opts.set_credentials(u, p);
        }
        let (client, eventloop) = AsyncClient::new(opts, REQUEST_CHANNEL_CAP);
        Ok((
            Self {
                client,
                discovery_prefix,
                node_id,
                deployment_name: deployment_display_name.to_string(),
                availability_topic,
            },
            eventloop,
        ))
    }

    /// Say "online" on the availability topic. Retained, so HA gets it even
    /// when it subscribes later, and re-issued on every (re)connect because
    /// whatever ended the previous connection (our own goodbye or the
    /// broker's will) left the topic reading "offline".
    pub async fn publish_online(&self) -> Result<(), MqttPublishError> {
        let topic = self.availability_topic.clone();
        self.client
            .publish(topic, QoS::AtLeastOnce, true, PAYLOAD_ONLINE)
            .await?;
        Ok(())
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
            model: "LocalSky".into(),
            sw_version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    /// The one path every discovery config takes to the broker. Serializing
    /// and publishing in a single place is what keeps the availability
    /// guarantee structural rather than a habit: a new entity type cannot
    /// reach HA except through here, `DiscoveryEntity.availability_topic` is
    /// not an Option, and the assertion below catches a payload that names
    /// some OTHER topic (which would go on serving its retained value after
    /// LocalSky dies, exactly the defect the topic exists to prevent).
    async fn publish_entity(
        &self,
        component: &str,
        object_id: &str,
        entity: &DiscoveryEntity,
    ) -> Result<(), MqttPublishError> {
        debug_assert_eq!(
            entity.availability_topic, self.availability_topic,
            "discovery payload points at an availability topic nothing publishes"
        );
        let topic = self.config_topic(component, object_id);
        let payload = serde_json::to_string(entity).map_err(|e| {
            MqttPublishError::Client(Box::new(crate::diagnostics::from_error(
                &e,
                "HA MQTT discovery serialization",
            )))
        })?;
        self.client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await?;
        Ok(())
    }

    /// The discovery entities for one zone: the planned_seconds sensor
    /// always, the bucket_mm sensor only when a model on this install
    /// actually produced a deficit, and the `running` binary_sensor only
    /// when `running_known` is true. (No per-zone ET sensor: ET0 is a single
    /// yard-wide forecast value, not per-zone, so a per-zone et_today_mm would
    /// publish the same number N times with no real per-zone producer.)
    ///
    /// `running_known=false` (fire-and-forget MQTT/DIY controllers) means we
    /// never get a trustworthy readback, so `publish_zone_state` withholds the
    /// running state forever. Publishing its discovery anyway would leave HA
    /// with a `running` binary_sensor stuck "unknown" for the life of the
    /// deployment, so we gate the discovery the same way `flow_meter` gates the
    /// flow sensor: no producer, no entity. `bucket_known` gets the same rule,
    /// value-based: the soil model's evidence replay produces a deficit for
    /// every zone with a species and a soil texture, so the bucket sensor is
    /// discovered exactly when the zone carries a bucket_mm value; a zone
    /// with no derivable bucket (a zone with no per-zone
    /// agronomy, or an evidence-starved window) still publishes no entity.
    ///
    /// Building the payloads apart from publishing them is what lets a test
    /// prove no entity can reach HA without the availability topic.
    fn zone_entities(
        &self,
        zone_slug: &str,
        running_known: bool,
        bucket_known: bool,
    ) -> Vec<(&'static str, String, DiscoveryEntity)> {
        let device = self.device();
        let mut entities: Vec<(&'static str, String, DiscoveryEntity)> = vec![(
            "sensor",
            format!("zone_{zone_slug}_planned_seconds"),
            DiscoveryEntity {
                name: format!("{zone_slug} planned seconds"),
                unique_id: format!("{}_zone_{zone_slug}_planned_s", self.node_id),
                state_topic: self
                    .state_topic("sensor", &format!("zone_{zone_slug}_planned_seconds")),
                availability_topic: self.availability_topic.clone(),
                unit_of_measurement: Some("s".into()),
                device_class: Some("duration".into()),
                state_class: Some("measurement".into()),
                icon: Some("mdi:timer".into()),
                device: device.clone(),
                attribution: "LocalSky".into(),
            },
        )];
        if bucket_known {
            entities.push((
                "sensor",
                format!("zone_{zone_slug}_bucket_mm"),
                DiscoveryEntity {
                    name: format!("{zone_slug} bucket"),
                    unique_id: format!("{}_zone_{zone_slug}_bucket_mm", self.node_id),
                    state_topic: self.state_topic("sensor", &format!("zone_{zone_slug}_bucket_mm")),
                    availability_topic: self.availability_topic.clone(),
                    unit_of_measurement: Some("mm".into()),
                    device_class: None,
                    state_class: Some("measurement".into()),
                    icon: Some("mdi:water".into()),
                    device: device.clone(),
                    attribution: "LocalSky".into(),
                },
            ));
        }
        if running_known {
            entities.push((
                "binary_sensor",
                format!("zone_{zone_slug}_running"),
                DiscoveryEntity {
                    name: format!("{zone_slug} running"),
                    unique_id: format!("{}_zone_{zone_slug}_running", self.node_id),
                    state_topic: self
                        .state_topic("binary_sensor", &format!("zone_{zone_slug}_running")),
                    availability_topic: self.availability_topic.clone(),
                    unit_of_measurement: None,
                    device_class: Some("running".into()),
                    state_class: None,
                    icon: Some("mdi:sprinkler-variant".into()),
                    device: device.clone(),
                    attribution: "LocalSky".into(),
                },
            ));
        }
        entities
    }

    /// Publish discovery for one zone: whatever `zone_entities` says the zone
    /// has, plus, when the bucket is absent, a CLEAR of its retained config
    /// and state topics.
    pub async fn publish_zone_discovery(
        &self,
        zone_slug: &str,
        running_known: bool,
        bucket_known: bool,
    ) -> Result<(), MqttPublishError> {
        if !bucket_known {
            // Versions before 0.7.22 published this sensor unconditionally,
            // retained, holding a fabricated 0.00. Simply not publishing it
            // leaves both retained topics on the broker, so Home Assistant
            // keeps the entity registered and pinned at that 0.00 forever:
            // the exact number this release removes, still on screen. An
            // empty retained payload on the config topic is HA's documented
            // "remove this entity", and one on the state topic drops the
            // stale value. Both are idempotent, so re-issuing them on every
            // reconnect costs nothing.
            let object_id = format!("zone_{zone_slug}_bucket_mm");
            for topic in [
                self.config_topic("sensor", &object_id),
                self.state_topic("sensor", &object_id),
            ] {
                self.client
                    .publish(topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
                    .await?;
            }
        }
        let entities = self.zone_entities(zone_slug, running_known, bucket_known);
        for (component, object_id, entity) in entities {
            self.publish_entity(component, &object_id, &entity).await?;
        }
        Ok(())
    }

    /// The daily verdict sensor.
    fn verdict_entity(&self) -> DiscoveryEntity {
        DiscoveryEntity {
            name: "LocalSky verdict".into(),
            unique_id: format!("{}_verdict_today", self.node_id),
            state_topic: self.state_topic("sensor", "verdict_today"),
            availability_topic: self.availability_topic.clone(),
            unit_of_measurement: None,
            device_class: None,
            state_class: None,
            icon: Some("mdi:scale-balance".into()),
            device: self.device(),
            attribution: "LocalSky".into(),
        }
    }

    /// Publish the daily verdict sensor. One entity, not a pair: the reason
    /// string has no sensor of its own (`publish_verdict_state` takes it and
    /// ignores it), so there is nothing here to discover for it.
    pub async fn publish_verdict_discovery(&self) -> Result<(), MqttPublishError> {
        let entity_verdict = self.verdict_entity();
        self.publish_entity("sensor", "verdict_today", &entity_verdict)
            .await
    }

    /// Publish discovery for the controller's measured flow. Only call this
    /// when the active controller advertises a flow meter
    /// (`IrrigationSnapshot.flow_meter`); otherwise the entity would render
    /// "unknown" forever on non-flow setups. Object id `flow` aligns with
    /// the OpenSprinkler integration's `sensor.sprinkler_flow` naming.
    pub async fn publish_flow_discovery(&self) -> Result<(), MqttPublishError> {
        let entity_flow = self.flow_entity();
        self.publish_entity("sensor", "flow", &entity_flow).await
    }

    /// The controller's measured-flow sensor.
    fn flow_entity(&self) -> DiscoveryEntity {
        DiscoveryEntity {
            name: "LocalSky flow".into(),
            unique_id: format!("{}_flow_gpm", self.node_id),
            state_topic: self.state_topic("sensor", "flow"),
            availability_topic: self.availability_topic.clone(),
            unit_of_measurement: Some("gpm".into()),
            device_class: Some("volume_flow_rate".into()),
            state_class: Some("measurement".into()),
            icon: Some("mdi:water-pump".into()),
            device: self.device(),
            attribution: "LocalSky".into(),
        }
    }

    /// Publish the selected meter reading. HA's numeric MQTT sensor uses
    /// `None` to clear unknown evidence; withholding or sending an empty value
    /// would leave the previous flow reading falsely current.
    pub async fn publish_flow_state(&self, flow_gpm: Option<f64>) -> Result<(), MqttPublishError> {
        let value = flow_gpm
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "None".into());
        let topic = self.state_topic("sensor", "flow");
        self.client
            .publish(topic, QoS::AtLeastOnce, true, value)
            .await?;
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
            self.publish_zone_discovery(&z.slug, z.running_known, z.bucket_mm.is_some())
                .await?;
        }
        self.publish_verdict_discovery().await?;
        if snap.flow_meter || snap.flow.rate_gpm.is_some() {
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
            // than asserting a possibly-wrong OFF; the planned value is still
            // meaningful. An absent bucket publishes nothing at all.
            let running = if z.running_known {
                Some(z.running)
            } else {
                None
            };
            self.publish_zone_state(&z.slug, z.bucket_mm, Some(z.planned_run_seconds), running)
                .await?;
        }
        self.publish_verdict_state(&snap.skip_check.verdict, &snap.skip_check.reason)
            .await?;
        self.publish_flow_state(snap.flow.rate_gpm).await?;
        Ok(())
    }

    /// Graceful disconnect. The runtime calls this on shutdown.
    ///
    /// Say "offline" BEFORE the DISCONNECT: a clean disconnect tells the
    /// broker to DISCARD the last will, so a planned stop would otherwise
    /// leave the retained "online" standing while HA served the frozen
    /// readings.
    ///
    /// Both packets are enqueued, never awaited, which is why this is not an
    /// `async fn`. The caller stops polling the eventloop the moment this
    /// returns, so an awaited send would be waiting on a queue nothing drains:
    /// on a saturated channel the shutdown path would park forever instead of
    /// skipping a goodbye it cannot deliver anyway. For the same reason the
    /// DISCONNECT is requested even when the goodbye did not fit, and the
    /// error is returned rather than propagated with `?` between them.
    ///
    /// Neither packet reaching the wire is a normal outcome, not a failure:
    /// the socket drops unannounced and the broker publishes the will's
    /// identical retained "offline". Every exit ends at offline; a delivered
    /// goodbye only gets there sooner than the broker's keep-alive timeout.
    pub fn close(self) -> Result<(), MqttPublishError> {
        let goodbye = self.client.try_publish(
            self.availability_topic.clone(),
            QoS::AtLeastOnce,
            true,
            PAYLOAD_OFFLINE,
        );
        let disconnect = self.client.try_disconnect();
        goodbye?;
        disconnect?;
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
///     so queued publishes are actually flushed to the wire. The chosen arm
///     runs to completion with nothing polling the eventloop, so the request
///     channel is sized (`REQUEST_CHANNEL_CAP`) to hold an arm's whole burst:
///     a publish that had to wait for room would wait for the poll it is
///     itself preventing, and the connection would go silent for good;
///   - on any eventloop error we log, back off, and reconnect (a fresh
///     `connect`), republishing discovery + the latest state on the new
///     ConnAck so HA recovers after a broker restart;
///   - the availability topic is republished "online" on every ConnAck and
///     armed as that connection's last will, so the window where LocalSky is
///     gone or unreachable reads as unavailable in HA rather than as the last
///     retained numbers.
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
                                // Online first: whatever ended the last
                                // connection left the topic at "offline", and
                                // an entity must never appear pointing at a
                                // topic that still says we are gone.
                                if let Err(e) = publisher.publish_online().await {
                                    warn!(error = %e, "ha mqtt publisher: availability publish failed");
                                }
                                let snap = snap_rx.borrow().clone();
                                // Everything below enqueues with nothing
                                // draining the channel, so a yard whose own
                                // burst cannot fit stalls right here. Say so
                                // rather than leaving a connection that went
                                // quiet for someone to work out.
                                let burst =
                                    PUBLISHES_PER_ZONE * snap.zones.len() + PUBLISHES_YARD_WIDE;
                                if burst > REQUEST_CHANNEL_CAP {
                                    warn!(
                                        packets = burst,
                                        capacity = REQUEST_CHANNEL_CAP,
                                        zones = snap.zones.len(),
                                        "ha mqtt publisher: yard is larger than this connection can announce in one burst; discovery will stall"
                                    );
                                }
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
                                let failure = crate::net::stream_failure::mqtt(&e, "HA MQTT publisher connection");
                                warn!(%failure, "ha mqtt publisher: eventloop error; reconnecting in 5s");
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
                            if let Err(e) = publisher.close() {
                                // Not fatal and not silent: the broker still
                                // publishes the will's "offline", just after
                                // its keep-alive timeout instead of now.
                                warn!(error = %e, "ha mqtt publisher: goodbye not enqueued; HA goes unavailable on the broker's will instead");
                            }
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

/// The one retained topic that says whether this LocalSky process is alive.
/// Device-level (one per deployment, not one per entity) and deliberately
/// outside the `<component>/<node_id>/<object_id>/config` shape HA scans, so
/// a discovery subscription never mistakes it for an entity config.
pub fn availability_topic(discovery_prefix: &str, node_id: &str) -> String {
    format!("{discovery_prefix}/{node_id}/availability")
}

/// The configured `discovery_prefix` reduced to real topic levels.
///
/// A user who writes `discovery_prefix = "homeassistant/"` would otherwise get
/// `homeassistant//yard/availability`: an empty level, which HA never matches.
/// The state and config topics have always had that shape too, but this one is
/// what HA's whole view of liveness hangs on, so a stray slash would not
/// misplace one sensor, it would leave every entity permanently unavailable
/// with LocalSky running. A prefix that is nothing but slashes falls back to
/// HA's documented default rather than emitting a leading empty level; a
/// well-formed prefix is returned unchanged, so no existing install moves.
fn normalize_discovery_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() {
        "homeassistant".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The MQTT node id a display name becomes: the shared slug rule, with
/// "localsky" when the name has nothing usable (a node id cannot be
/// empty).
pub fn slugify(s: &str) -> String {
    let out = crate::text::slugify(s);
    if out.is_empty() {
        "localsky".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ZoneState;
    use rumqttc::Request;

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
            availability_topic: "a".into(),
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
            availability_topic: "homeassistant/yard/availability".into(),
            unit_of_measurement: Some("mm".into()),
            device_class: None,
            state_class: Some("measurement".into()),
            icon: Some("mdi:water".into()),
            device: DiscoveryDevice {
                identifiers: vec!["yard".into()],
                name: "Yard".into(),
                manufacturer: "LocalSky".into(),
                model: "LocalSky".into(),
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

    fn mqtt_cfg() -> MqttConfig {
        MqttConfig {
            host: "broker.local".into(),
            port: 1883,
            username: None,
            password: None,
            discovery_prefix: "homeassistant".into(),
            publish_enabled: true,
            subscribe_enabled: false,
        }
    }

    /// Everything the client has handed the eventloop and the eventloop has
    /// not sent. `EventLoop::clean()` is rumqttc's own "move the unsent
    /// requests somewhere I can look at them" hook and `pending` is public, so
    /// a test can read the exact packets a call produced with no broker.
    fn drained(eventloop: &mut rumqttc::EventLoop) -> Vec<Request> {
        eventloop.clean();
        eventloop.pending.drain(..).collect()
    }

    /// The heaviest snapshot a ConnAck burst can be handed: every zone in one
    /// of the two shapes that cost `PUBLISHES_PER_ZONE` (with a bucket; and
    /// without one, where two retained clears replace the bucket pair), plus a
    /// flow meter and a reading so both yard-wide extras are present.
    fn worst_case_snapshot(zones: usize) -> IrrigationSnapshot {
        let mut zs = Vec::with_capacity(zones);
        for i in 0..zones {
            zs.push(ZoneState {
                slug: format!("zone_{i}"),
                running: true,
                running_known: true,
                bucket_mm: if i % 2 == 0 { Some(1.25) } else { None },
                planned_run_seconds: 600,
                ..Default::default()
            });
        }
        IrrigationSnapshot {
            zones: zs,
            flow_meter: true,
            flow_gpm: Some(4.2),
            flow: crate::model::FlowReadout {
                rate_gpm: Some(4.2),
                rate_source_id: Some("controller:test".into()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// One topic per deployment, and shaped so HA's discovery scan cannot read
    /// it as an entity config. HA subscribes to `<prefix>/+/+/config` and
    /// `<prefix>/+/+/+/config` -- four and five levels. This topic is three,
    /// for every deployment name a user can type, because `slugify` can never
    /// hand back a '/'.
    #[test]
    fn availability_topic_can_never_be_read_as_an_entity_config() {
        for name in ["Back Yard", "North Lawn / Strip", "???", ""] {
            let t = availability_topic("homeassistant", &slugify(name));
            assert_eq!(
                t.split('/').count(),
                3,
                "{t} has the level count of a discovery config topic"
            );
            assert!(
                !t.ends_with("/config"),
                "{t} would be picked up as an entity config"
            );
        }
        assert_eq!(
            availability_topic("homeassistant", "back_yard"),
            "homeassistant/back_yard/availability"
        );
    }

    /// A trailing (or leading) slash on the configured prefix would produce an
    /// empty topic level, which HA never matches -- and on THIS topic that
    /// means every entity reads unavailable for the life of the deployment.
    #[test]
    fn discovery_prefix_is_reduced_to_real_topic_levels() {
        assert_eq!(normalize_discovery_prefix("homeassistant"), "homeassistant");
        assert_eq!(
            normalize_discovery_prefix("homeassistant/"),
            "homeassistant"
        );
        assert_eq!(
            normalize_discovery_prefix("/homeassistant/"),
            "homeassistant"
        );
        assert_eq!(
            normalize_discovery_prefix("  ha/discovery/  "),
            "ha/discovery"
        );
        assert_eq!(normalize_discovery_prefix("///"), "homeassistant");
        assert_eq!(normalize_discovery_prefix(""), "homeassistant");
    }

    /// The normalization has to happen once, at connect, or the will and the
    /// topics the entities name can disagree.
    #[tokio::test]
    async fn a_slashed_prefix_makes_no_empty_topic_level_anywhere() {
        let mut cfg = mqtt_cfg();
        cfg.discovery_prefix = "homeassistant/".into();
        let (publisher, eventloop) = HaMqttPublisher::connect(&cfg, "Yard", "cid").await.unwrap();
        assert_eq!(
            publisher.availability_topic,
            "homeassistant/yard/availability"
        );
        assert_eq!(
            eventloop.mqtt_options.last_will().unwrap().topic,
            "homeassistant/yard/availability"
        );
        assert_eq!(
            publisher.config_topic("sensor", "verdict_today"),
            "homeassistant/sensor/yard/verdict_today/config"
        );
        assert_eq!(
            publisher.state_topic("sensor", "verdict_today"),
            "homeassistant/sensor/yard/verdict_today/state"
        );
    }

    /// A crash, an OOM kill, a redeploy or a dropped link never runs our
    /// shutdown path, so the broker has to be the one that says LocalSky is
    /// gone. Without the will, every retained state topic keeps reading as a
    /// current measurement in HA forever.
    #[tokio::test]
    async fn connect_arms_a_retained_offline_last_will() {
        let cfg = mqtt_cfg();
        let (publisher, eventloop) = HaMqttPublisher::connect(&cfg, "Yard", "cid").await.unwrap();
        let will = eventloop
            .mqtt_options
            .last_will()
            .expect("no last will: a dead LocalSky would leave HA serving frozen readings");
        assert_eq!(will.topic, "homeassistant/yard/availability");
        let message = std::str::from_utf8(&will.message).unwrap();
        assert_eq!(message, PAYLOAD_OFFLINE);
        assert!(will.retain, "a late subscriber must still see offline");
        assert_eq!(will.qos, QoS::AtLeastOnce);
        // The will is worthless if the entities point somewhere else.
        assert_eq!(publisher.availability_topic, will.topic);
    }

    /// Every discovery payload the publisher can emit has to name the same
    /// topic the will marks offline; one that names none goes on serving its
    /// retained value after the process is gone.
    ///
    /// This enumerates today's three entity constructors, so it catches a
    /// payload that names the WRONG topic, not a fourth constructor added
    /// later. What makes omission impossible is structural and lives
    /// elsewhere: `availability_topic` is not an Option, and every config
    /// reaches the broker through `publish_entity`, which asserts the same
    /// thing on the way past.
    #[tokio::test]
    async fn every_discovery_entity_points_at_the_will_topic() {
        let cfg = mqtt_cfg();
        let (publisher, eventloop) = HaMqttPublisher::connect(&cfg, "Yard", "cid").await.unwrap();
        let will_topic = eventloop.mqtt_options.last_will().unwrap().topic;
        // A full zone (planned + bucket + running), the thinnest zone
        // (planned only), the verdict and the flow sensor: the whole set.
        let mut entities: Vec<DiscoveryEntity> = Vec::new();
        for (_, _, e) in publisher.zone_entities("front_lawn", true, true) {
            entities.push(e);
        }
        for (_, _, e) in publisher.zone_entities("strip", false, false) {
            entities.push(e);
        }
        entities.push(publisher.verdict_entity());
        entities.push(publisher.flow_entity());
        assert_eq!(entities.len(), 6);
        for entity in entities {
            let json = serde_json::to_value(&entity).unwrap();
            assert_eq!(
                json.get("availability_topic").and_then(|v| v.as_str()),
                Some(will_topic.as_str()),
                "{} keeps serving its retained value after LocalSky dies",
                entity.unique_id
            );
        }
    }

    /// Half the runtime contract: every (re)connect republishes a retained
    /// "online". Whatever ended the previous connection left the topic reading
    /// "offline", so an entity that appeared without this would point at a
    /// topic still saying we are gone.
    #[tokio::test]
    async fn publish_online_enqueues_a_retained_online_on_the_will_topic() {
        let cfg = mqtt_cfg();
        let (publisher, mut eventloop) =
            HaMqttPublisher::connect(&cfg, "Yard", "cid").await.unwrap();
        publisher.publish_online().await.unwrap();
        let sent = drained(&mut eventloop);
        assert_eq!(sent.len(), 1, "expected one packet, got {sent:?}");
        let Request::Publish(p) = &sent[0] else {
            panic!("publish_online enqueued {:?}, not a publish", sent[0]);
        };
        assert_eq!(p.topic, publisher.availability_topic);
        assert_eq!(std::str::from_utf8(&p.payload).unwrap(), PAYLOAD_ONLINE);
        assert!(
            p.retain,
            "an HA that subscribes later must still see online"
        );
        assert_eq!(p.qos, QoS::AtLeastOnce);
    }

    #[tokio::test]
    async fn flow_loss_clears_the_retained_reading_and_zero_stays_measured() {
        let (publisher, mut eventloop) = HaMqttPublisher::connect(&mqtt_cfg(), "Yard", "cid")
            .await
            .unwrap();
        for reading in [Some(4.2), None, Some(0.0), Some(f64::NAN), Some(-1.0)] {
            publisher.publish_flow_state(reading).await.unwrap();
        }
        let packets = drained(&mut eventloop);
        let payloads: Vec<&str> = packets
            .iter()
            .map(|request| {
                let Request::Publish(packet) = request else {
                    panic!("expected a flow publish")
                };
                assert_eq!(packet.topic, publisher.state_topic("sensor", "flow"));
                assert!(packet.retain);
                std::str::from_utf8(&packet.payload).unwrap()
            })
            .collect();
        assert_eq!(payloads, ["4.2", "None", "0.0", "None", "None"]);
    }

    /// The other half: every exit ends at offline, and the goodbye goes out
    /// BEFORE the DISCONNECT. A clean disconnect makes the broker discard the
    /// will, so the reverse order would leave a planned stop reading "online"
    /// over frozen numbers until someone noticed.
    #[tokio::test]
    async fn close_says_offline_before_it_disconnects() {
        let cfg = mqtt_cfg();
        let (publisher, mut eventloop) =
            HaMqttPublisher::connect(&cfg, "Yard", "cid").await.unwrap();
        let topic = publisher.availability_topic.clone();
        publisher.close().unwrap();
        let sent = drained(&mut eventloop);
        assert_eq!(
            sent.len(),
            2,
            "shutdown must say goodbye AND disconnect: {sent:?}"
        );
        let Request::Publish(p) = &sent[0] else {
            panic!("the goodbye is not the first packet: {sent:?}");
        };
        assert_eq!(p.topic, topic);
        assert_eq!(std::str::from_utf8(&p.payload).unwrap(), PAYLOAD_OFFLINE);
        assert!(
            p.retain,
            "a retained offline is what un-freezes the entities"
        );
        assert!(
            matches!(&sent[1], Request::Disconnect(_)),
            "the DISCONNECT must follow the goodbye, not replace it: {sent:?}"
        );
    }

    /// Shutdown may not park. Nothing polls the eventloop once `close` is
    /// called, so an awaited send on a full channel would wait for a drain
    /// that never comes and the publisher task would never return.
    #[tokio::test]
    async fn close_reports_a_full_channel_instead_of_waiting_on_it() {
        let cfg = mqtt_cfg();
        let (publisher, _eventloop) = HaMqttPublisher::connect(&cfg, "Yard", "cid").await.unwrap();
        let mut filled = 0usize;
        while publisher
            .client
            .try_publish("localsky/test/fill", QoS::AtLeastOnce, false, "x")
            .is_ok()
        {
            filled += 1;
        }
        assert!(
            filled >= REQUEST_CHANNEL_CAP,
            "channel took only {filled} of {REQUEST_CHANNEL_CAP}"
        );
        let closed = tokio::time::timeout(Duration::from_secs(2), async { publisher.close() })
            .await
            .expect("close parked on a full request channel; the publisher task never returns");
        assert!(
            closed.is_err(),
            "a goodbye that did not fit must be reported, not claimed as sent"
        );
    }

    /// The ConnAck arm enqueues its whole burst -- online, every discovery
    /// config, every state value -- with the eventloop not polled until the arm
    /// returns, so the request channel has to hold all of it. Sized short, the
    /// arm parks on the packet that does not fit, the eventloop is never polled
    /// again and NOTHING reaches the wire (not even the "online" enqueued
    /// first): HA shows every entity unavailable while LocalSky waters.
    ///
    /// Counting the drained packets is also what pins `PUBLISHES_PER_ZONE`,
    /// and through it the capacity, to what the code actually sends: add a
    /// seventh per-zone publish and this fails here rather than on somebody's
    /// large yard.
    #[tokio::test]
    async fn the_whole_connack_burst_fits_the_request_channel() {
        let cfg = mqtt_cfg();
        let snap = worst_case_snapshot(MAX_BURST_ZONES);
        let (publisher, mut eventloop) =
            HaMqttPublisher::connect(&cfg, "Yard", "cid").await.unwrap();
        let burst = async {
            publisher.publish_online().await.unwrap();
            publisher.publish_all_discovery(&snap).await.unwrap();
            publisher.publish_all_state(&snap).await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), burst)
            .await
            .expect("the ConnAck burst parked on a full request channel");
        let sent = drained(&mut eventloop);
        assert_eq!(
            sent.len(),
            PUBLISHES_PER_ZONE * MAX_BURST_ZONES + PUBLISHES_YARD_WIDE,
            "the burst is not the size REQUEST_CHANNEL_CAP was derived from; re-derive it"
        );
        assert!(
            sent.len() <= REQUEST_CHANNEL_CAP,
            "{} packets do not fit a channel of {REQUEST_CHANNEL_CAP}",
            sent.len()
        );
        // The availability publish is the one that must never be lost.
        let Request::Publish(first) = &sent[0] else {
            panic!(
                "the burst does not lead with the availability publish: {:?}",
                sent[0]
            );
        };
        assert_eq!(first.topic, publisher.availability_topic);
        assert_eq!(std::str::from_utf8(&first.payload).unwrap(), PAYLOAD_ONLINE);
    }

    /// `BURST_HEADROOM` is not decoration. `poll()` moves one request per call,
    /// so a snapshot landing while the ConnAck burst is still draining enqueues
    /// on top of whatever is left of it, and with both `select!` arms ready
    /// tokio chooses between them at random. Sized to exactly one burst, that
    /// second one parks and the connection goes silent for good.
    #[tokio::test]
    async fn a_state_burst_landing_on_an_undrained_connack_burst_still_fits() {
        let cfg = mqtt_cfg();
        let snap = worst_case_snapshot(MAX_BURST_ZONES);
        let (publisher, mut eventloop) =
            HaMqttPublisher::connect(&cfg, "Yard", "cid").await.unwrap();
        let overlapped = async {
            publisher.publish_online().await.unwrap();
            publisher.publish_all_discovery(&snap).await.unwrap();
            publisher.publish_all_state(&snap).await.unwrap();
            // Nothing has polled the eventloop, so the whole burst above is
            // still queued when the next snapshot arrives.
            publisher.publish_all_state(&snap).await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), overlapped)
            .await
            .expect("an overlapping state burst parked; BURST_HEADROOM is too small");
        let sent = drained(&mut eventloop);
        assert!(
            sent.len() > PUBLISHES_PER_ZONE * MAX_BURST_ZONES + PUBLISHES_YARD_WIDE,
            "the second burst never queued on top of the first: {} packets",
            sent.len()
        );
        assert!(
            sent.len() <= REQUEST_CHANNEL_CAP,
            "{} packets do not fit a channel of {REQUEST_CHANNEL_CAP}",
            sent.len()
        );
    }
}
