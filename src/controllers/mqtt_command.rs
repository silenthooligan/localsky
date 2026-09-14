// Generic MQTT controller.
//
// Publishes a configured on/off payload to a per-zone topic when LocalSky
// runs/stops a zone. Connects once at construction and maintains a background
// event-loop task that keeps the rumqttc client alive + reconnecting; the
// run_zone / stop_zone calls just enqueue a publish.
//
// This is the "anything that listens to MQTT" controller. Targets:
//   - ESPHome with `switch.mqtt:` blocks
//   - Tasmota POWER1/POWER2 (cmnd/<topic>/POWER on/off)
//   - Sonoff / Shelly devices in MQTT mode
//   - Zigbee2MQTT relay devices (zigbee2mqtt/<friendly_name>/set body)
//   - DIY relay boards (ESP32, Raspberry Pi GPIO bridges)
//   - OpenSprinkler's MQTT plug-in
//
// OPTIONAL state readback: when a zone has a `state_topic` (and/or the
// controller has an `availability_topic` / `flow_topic`), the adapter
// subscribes and the board's reported state flows back into status(). This is
// the HA-native MQTT convention (state + availability + LWT) that ESPHome,
// Tasmota and Z2M already speak. Without a state_topic the adapter is
// fire-and-forget: LocalSky owns the shutoff timer and reports running state
// from its own run log.
//
// run_zone fires the on-publish, spawns a shutoff timer that publishes the
// matching off payload after duration_s, and immediately returns a RunHandle.
// stop_zone / stop_all publish off right away; the timer's later off-publish is
// idempotent so an early stop is harmless.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::schema::MqttCommandConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

use crate::controllers::guard::RUN_SECONDS_MAX;

// ----- pure payload classifiers (unit-tested; no I/O) -----

/// Whether a state-topic payload means running, stopped, or unknown.
/// Whole-payload, trimmed, case-insensitive compares. (JSON state bodies
/// like Tasmota's `{"POWER":"ON"}` are not destructured yet; see the DIY docs.)
fn classify_running(payload: &str, on_match: &str, off_match: &str) -> Option<bool> {
    let payload = payload.trim();
    if payload.eq_ignore_ascii_case(on_match) {
        Some(true)
    } else if payload.eq_ignore_ascii_case(off_match) {
        Some(false)
    } else {
        None
    }
}

/// Parse a flow-topic payload as gallons/min. Whole-payload, trimmed.
fn parse_flow(payload: &str) -> Option<f64> {
    payload
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// Classify an availability-topic payload. `Some(true)` = online, `Some(false)`
/// = offline, `None` = neither (indeterminate; the caller leaves the last known
/// value unchanged rather than guessing).
fn classify_availability(payload: &str, online: &str, offline: &str) -> Option<bool> {
    let p = payload.trim();
    if p.eq_ignore_ascii_case(online) {
        Some(true)
    } else if p.eq_ignore_ascii_case(offline) {
        Some(false)
    } else {
        None
    }
}

/// Topics the adapter subscribes to and how to interpret them. Built once
/// from config and shared (read-only) with the event-loop task.
struct SubMeta {
    /// state_topic -> list of (zone_slug, running payload, stopped payload). A Vec so
    /// several zones can legitimately share one state topic (a multi-relay
    /// board that reports all zones on one topic, or two slugs mapped to the
    /// same physical relay); every mapped zone updates on a message.
    state: HashMap<String, Vec<(String, String, String)>>,
    availability_topic: Option<String>,
    /// payload meaning "online" on availability_topic.
    payload_available: String,
    /// payload meaning "offline" on availability_topic.
    payload_not_available: String,
    flow_topic: Option<String>,
}

impl SubMeta {
    /// Every topic to subscribe to on (re)connect.
    fn topics(&self) -> Vec<String> {
        let mut t: Vec<String> = self.state.keys().cloned().collect();
        if let Some(a) = &self.availability_topic {
            t.push(a.clone());
        }
        if let Some(f) = &self.flow_topic {
            t.push(f.clone());
        }
        t
    }
}

/// State learned from subscribed topics. `None`/absent means "not reported".
#[derive(Default)]
struct SharedState {
    /// zone_slug -> running. Only populated for zones with a state_topic.
    running: Mutex<HashMap<String, ReportedRunning>>,
    /// Last availability value; None when no availability_topic is configured.
    available: Mutex<Option<bool>>,
    flow_gpm: Mutex<Option<f64>>,
}

#[derive(Clone, Copy, Default)]
struct ReportedRunning {
    running: bool,
    known: bool,
}

impl ReportedRunning {
    fn observe(&mut self, payload: &str, on: &str, off: &str) {
        let reported = classify_running(payload, on, off);
        if let Some(running) = reported {
            self.running = running;
        }
        // An unrecognized message is evidence that we cannot interpret the
        // board's state. Preserve the last value, but never certify it idle.
        self.known = reported.is_some();
    }
}

pub struct MqttCommand {
    id: String,
    config: MqttCommandConfig,
    client: AsyncClient,
    /// Background task driving the EventLoop. Held so we don't drop the
    /// event loop while the controller is alive.
    _eventloop_handle: JoinHandle<()>,
    /// Tracks broker connection state for status().reachable.
    connected: Arc<Mutex<bool>>,
    /// Reported state from subscribed topics (empty when none configured).
    shared: Arc<SharedState>,
}

impl MqttCommand {
    pub fn new(id: impl Into<String>, config: MqttCommandConfig) -> Self {
        let id = id.into();
        let client_id = config
            .client_id
            .clone()
            .unwrap_or_else(|| format!("localsky-controller-{}", id));
        let mut opts = MqttOptions::new(&client_id, &config.broker_host, config.broker_port);
        opts.set_keep_alive(Duration::from_secs(30));
        if let (Some(u), Some(p)) = (&config.username, &config.password) {
            opts.set_credentials(u, p);
        }
        let (client, eventloop) = AsyncClient::new(opts, 32);
        let connected = Arc::new(Mutex::new(false));
        let shared = Arc::new(SharedState::default());

        // Build the subscription map from config. A zone's "running" payload
        // is its state_on_payload, falling back to its command on_payload.
        // Multiple zones may map to the same state topic, so accumulate.
        let mut state: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
        for (slug, cmd) in &config.zone_command_map {
            if let Some(topic) = &cmd.state_topic {
                let on_match = cmd
                    .state_on_payload
                    .clone()
                    .unwrap_or_else(|| cmd.on_payload.clone());
                state.entry(topic.clone()).or_default().push((
                    slug.clone(),
                    on_match,
                    cmd.off_payload.clone(),
                ));
            }
        }
        let sub_meta = Arc::new(SubMeta {
            state,
            availability_topic: config.availability_topic.clone(),
            payload_available: config.payload_available.clone(),
            payload_not_available: config.payload_not_available.clone(),
            flow_topic: config.flow_topic.clone(),
        });

        let _eventloop_handle = tokio::spawn(drive_eventloop(
            id.clone(),
            eventloop,
            client.clone(),
            connected.clone(),
            sub_meta,
            shared.clone(),
        ));
        Self {
            id,
            config,
            client,
            _eventloop_handle,
            connected,
            shared,
        }
    }

    fn zone_command(
        &self,
        slug: &str,
    ) -> Result<&crate::config::schema::MqttZoneCommand, ControllerError> {
        self.config
            .zone_command_map
            .get(slug)
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    /// Refuse to claim delivery when nothing can deliver.
    ///
    /// `AsyncClient::publish` returns Ok on enqueue into rumqttc's request
    /// channel whether or not a broker is on the other end. A stop that
    /// "succeeded" into a dead channel disarmed the shutoff deadline and
    /// left the valve open with no backstop. Disconnected from the broker,
    /// or told by the availability topic that the device itself is
    /// offline, the answer is a transport error: the row stays armed and
    /// the reaper retries every tick until the command can land.
    async fn deliverable(&self) -> ControllerResult<()> {
        if !*self.connected.lock().await {
            return Err(ControllerError::Transport(
                "mqtt broker disconnected; command not delivered".into(),
            ));
        }
        if *self.shared.available.lock().await == Some(false) {
            return Err(ControllerError::Transport(
                "device reports offline on its availability topic; command not delivered".into(),
            ));
        }
        Ok(())
    }

    async fn publish_zone(&self, slug: &str, on: bool) -> ControllerResult<()> {
        let cmd = self.zone_command(slug)?;
        self.deliverable().await?;
        let payload = if on {
            cmd.on_payload.as_bytes()
        } else {
            cmd.off_payload.as_bytes()
        };
        // OFF is always retained. A device that drops off the broker and
        // reconnects receives the last retained command, and the safe
        // command to receive is the one that closes the valve; a retained
        // ON would be the opposite. ON follows the operator's setting.
        let retain = if on { cmd.retain } else { true };
        self.client
            .publish(&cmd.topic, QoS::AtLeastOnce, retain, payload.to_vec())
            .await
            .map_err(|e| ControllerError::Transport(format!("mqtt publish failed: {e}")))?;
        debug!(
            controller = %self.id,
            zone = slug,
            topic = cmd.topic,
            on = on,
            "mqtt command published",
        );
        Ok(())
    }
}

async fn drive_eventloop(
    id: String,
    mut eventloop: EventLoop,
    client: AsyncClient,
    connected: Arc<Mutex<bool>>,
    sub_meta: Arc<SubMeta>,
    shared: Arc<SharedState>,
) {
    info!(controller = %id, "mqtt controller event loop started");
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                *connected.lock().await = true;
                info!(controller = %id, "mqtt controller connected");
                // (Re)subscribe to every readback topic on each connect so
                // subscriptions survive a reconnect. Retained state topics
                // deliver the current value immediately.
                for topic in sub_meta.topics() {
                    if let Err(e) = client.subscribe(&topic, QoS::AtLeastOnce).await {
                        warn!(controller = %id, topic = %topic, error = %e, "mqtt subscribe failed");
                    }
                }
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let payload = String::from_utf8_lossy(&p.payload);
                if let Some(subs) = sub_meta.state.get(&p.topic) {
                    // Every zone mapped to this state topic updates.
                    let mut running = shared.running.lock().await;
                    for (slug, on_match, off_match) in subs {
                        let state = running.entry(slug.clone()).or_default();
                        state.observe(&payload, on_match, off_match);
                        debug!(controller = %id, zone = %slug, running = state.running, known = state.known, "mqtt state update");
                    }
                } else if sub_meta.availability_topic.as_deref() == Some(p.topic.as_str()) {
                    // Online/offline only; an unrecognized payload is
                    // indeterminate and leaves the last known value alone.
                    if let Some(online) = classify_availability(
                        &payload,
                        &sub_meta.payload_available,
                        &sub_meta.payload_not_available,
                    ) {
                        *shared.available.lock().await = Some(online);
                    }
                } else if sub_meta.flow_topic.as_deref() == Some(p.topic.as_str()) {
                    *shared.flow_gpm.lock().await = parse_flow(&payload);
                }
            }
            Ok(Event::Incoming(Packet::Disconnect)) => {
                *connected.lock().await = false;
                clear_stale_telemetry(&shared).await;
                warn!(controller = %id, "mqtt controller broker-disconnected");
            }
            Ok(_) => {}
            Err(e) => {
                *connected.lock().await = false;
                clear_stale_telemetry(&shared).await;
                warn!(controller = %id, error = %e, "mqtt controller eventloop error; rumqttc will reconnect");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// On a broker disconnect, drop telemetry learned over the (now-dead) link so
/// the dashboard never shows a stale "live" flow or a phantom availability. A
/// reconnect re-reads retained topics and re-populates these.
async fn clear_stale_telemetry(shared: &SharedState) {
    for state in shared.running.lock().await.values_mut() {
        state.known = false;
    }
    *shared.available.lock().await = None;
    *shared.flow_gpm.lock().await = None;
}

#[async_trait]
impl IrrigationController for MqttCommand {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            // A flow_topic means the board reports flow; otherwise no meter.
            flow_meter: self.config.flow_topic.is_some(),
            rain_sensor: false,
            master_valve: false,
            // MQTT is per-topic, so concurrent zones run fine; the broker
            // handles each publish independently.
            multi_zone_parallel: true,
            history_query: false,
            remote_program_upload: false,
            water_level: false,
            per_zone_stop: true,
            duration_quantum_s: 1,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        // Defensive cap (2h) matching the API's RUN_SECONDS_MAX. On the
        // fire-and-forget path the in-process shutoff timer below is the ONLY
        // thing that closes the valve, so an unbounded duration is a
        // stuck-valve risk for exactly this (DIY) audience.
        let capped = duration_s.min(RUN_SECONDS_MAX).max(1);
        if capped != duration_s {
            warn!(
                controller = %self.id,
                zone = %slug,
                requested = duration_s,
                clamped = capped,
                "mqtt clamping run duration to 7200s"
            );
        }
        let duration_s = capped;
        self.publish_zone(slug, true).await?;
        // MQTT targets are dumb command sinks with no native run timer:
        // without a scheduled off-publish the valve stays open until
        // something else closes it. Spawn the shutoff timer here so the
        // promise in the RunHandle (planned_duration_s) is actually kept.
        // An earlier stop_zone/stop_all publishes off immediately; the
        // timer's later duplicate off is idempotent.
        if duration_s > 0 {
            if let Some(cmd) = self.config.zone_command_map.get(slug) {
                let client = self.client.clone();
                let topic = cmd.topic.clone();
                let off_payload = cmd.off_payload.clone().into_bytes();
                // The timer's OFF is retained for the same reason the
                // explicit one is: the closing command is the one a
                // reconnecting device should find waiting.
                let retain = true;
                let controller_id = self.id.clone();
                let zone = slug.to_string();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(duration_s as u64)).await;
                    match client
                        .publish(&topic, QoS::AtLeastOnce, retain, off_payload)
                        .await
                    {
                        Ok(()) => debug!(
                            controller = %controller_id,
                            zone = %zone,
                            topic = %topic,
                            "mqtt shutoff timer: off published"
                        ),
                        Err(e) => warn!(
                            controller = %controller_id,
                            zone = %zone,
                            topic = %topic,
                            error = %e,
                            "mqtt shutoff timer: off publish failed; zone may still be running"
                        ),
                    }
                });
            }
        }
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: chrono::Utc::now().timestamp(),
            planned_duration_s: duration_s,
            provider_ref: self
                .config
                .zone_command_map
                .get(slug)
                .map(|c| c.topic.clone()),
        })
    }

    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        self.publish_zone(slug, false).await
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        // Publish off to every configured zone topic. We swallow per-zone
        // errors so a single failure doesn't leave the rest running.
        let mut last_err: Option<ControllerError> = None;
        for slug in self.config.zone_command_map.keys() {
            if let Err(e) = self.publish_zone(slug, false).await {
                warn!(
                    controller = %self.id,
                    zone = slug,
                    error = %e,
                    "mqtt stop_all: publish failed for zone",
                );
                last_err = Some(e);
            }
        }
        match last_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let connected = *self.connected.lock().await;
        // If an availability topic is configured, an explicit "offline" makes
        // the controller unreachable even while the broker link is up. With no
        // availability topic, reachability == broker connection.
        let available = *self.shared.available.lock().await;
        let reachable = connected && available.unwrap_or(true);

        // Only report runtime state for zones that have a `state_topic`, those
        // are the ones the board actually reports back. Command-only
        // (fire-and-forget) zones are OMITTED here on purpose: emitting them
        // with running=false would authoritatively contradict the engine's own
        // view (the refresher would mark running_known=true,false), so a zone
        // LocalSky just started would show idle and the optimistic UI would
        // fire a false "controller didn't confirm" toast. Omitting them lets
        // the refresher fall through to running_known=false and use the run log.
        let running = self.shared.running.lock().await;
        let zone_states = self
            .config
            .zone_command_map
            .iter()
            .filter(|(_, cmd)| cmd.state_topic.is_some())
            .map(|(slug, _)| ZoneRuntimeStatus {
                slug: slug.clone(),
                running: running
                    .get(slug)
                    .map(|state| state.running)
                    .unwrap_or(false),
                remaining_s: None,
                last_run_epoch: None,
                running_known: reachable && running.get(slug).is_some_and(|state| state.known),
            })
            .collect();

        let flow_gpm = if reachable {
            *self.shared.flow_gpm.lock().await
        } else {
            None
        };
        Ok(ControllerStatus {
            observed_epoch: None,
            reachable,
            master_enabled: None,
            water_level_pct: None,
            rain_sensor_tripped: None,
            current_program: None,
            zone_states,
            // Only surface flow while the broker link is up; a disconnect
            // clears it, but gate here too so a value can never read as "live"
            // when we can't be receiving updates.
            flow_gpm,
            flow_connected: flow_gpm.is_some(),
            firmware: None,
        })
    }

    async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        // No remote history; the runs store layer holds the local log.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::MqttZoneCommand;
    use std::collections::BTreeMap;

    fn zone(topic: &str, on: &str, off: &str) -> MqttZoneCommand {
        MqttZoneCommand {
            topic: topic.into(),
            on_payload: on.into(),
            off_payload: off.into(),
            retain: false,
            state_topic: None,
            state_on_payload: None,
        }
    }

    fn cfg() -> MqttCommandConfig {
        let mut map = BTreeMap::new();
        map.insert(
            "back_yard".to_string(),
            zone("homeassistant/switch/back_yard/set", "ON", "OFF"),
        );
        map.insert(
            "front_lawn".to_string(),
            zone("tasmota/cmnd/front_lawn/POWER", "1", "0"),
        );
        MqttCommandConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: 1883,
            username: None,
            password: None,
            client_id: Some("test-controller".into()),
            availability_topic: None,
            payload_available: "online".into(),
            payload_not_available: "offline".into(),
            flow_topic: None,
            zone_command_map: map,
        }
    }

    fn cfg_stateful() -> MqttCommandConfig {
        let mut c = cfg();
        let by = c.zone_command_map.get_mut("back_yard").unwrap();
        by.state_topic = Some("homeassistant/switch/back_yard/state".into());
        c.availability_topic = Some("diy-irrig/status".into());
        c.flow_topic = Some("diy-irrig/flow".into());
        c
    }

    #[tokio::test]
    async fn id_and_caps() {
        let c = MqttCommand::new("mq", cfg());
        assert_eq!(c.id(), "mq");
        let caps = c.supports();
        assert!(caps.multi_zone_parallel);
        assert!(!caps.flow_meter);
    }

    #[tokio::test]
    async fn flow_topic_enables_flow_meter_cap() {
        let c = MqttCommand::new("mq", cfg_stateful());
        assert!(c.supports().flow_meter);
        assert!(c
            .config
            .zone_command_map
            .values()
            .any(|z| z.state_topic.is_some()));
    }

    /// With no broker on the other end, a stop is not a success: the
    /// reaper keeps the deadline row and retries. A run is refused for
    /// the same reason; a valve nobody can reach must not be "started".
    #[tokio::test]
    async fn a_stop_into_a_disconnected_broker_is_a_transport_error() {
        let c = MqttCommand::new("mq", cfg());
        let zone = c.config.zone_command_map.keys().next().unwrap().clone();
        let err = c.stop_zone(&zone).await.unwrap_err();
        assert!(
            matches!(err, ControllerError::Transport(ref m) if m.contains("disconnected")),
            "{err:?}"
        );
        let err = c.stop_all().await.unwrap_err();
        assert!(matches!(err, ControllerError::Transport(_)), "{err:?}");
        let err = c.run_zone(&zone, 60).await.unwrap_err();
        assert!(matches!(err, ControllerError::Transport(_)), "{err:?}");
    }

    /// The device saying "offline" on its availability topic is the same
    /// answer, even with the broker connected.
    #[tokio::test]
    async fn a_device_reporting_offline_refuses_the_command() {
        let c = MqttCommand::new("mq", cfg_stateful());
        let zone = c.config.zone_command_map.keys().next().unwrap().clone();
        *c.connected.lock().await = true;
        *c.shared.available.lock().await = Some(false);
        let err = c.stop_zone(&zone).await.unwrap_err();
        assert!(
            matches!(err, ControllerError::Transport(ref m) if m.contains("offline")),
            "{err:?}"
        );
        *c.shared.available.lock().await = Some(true);
        // Connected and available: the publish enqueues (no broker in
        // tests, but the channel accepts it), which is the pre-existing
        // behavior for a reachable device.
        assert!(c.stop_zone(&zone).await.is_ok());
    }

    #[tokio::test]
    async fn unknown_zone_errors() {
        let c = MqttCommand::new("mq", cfg());
        let err = c.run_zone("not_a_zone", 60).await.unwrap_err();
        assert!(matches!(err, ControllerError::ZoneUnknown(_)));
    }

    #[tokio::test]
    async fn run_zone_enqueues_on_and_schedules_off() {
        // No broker in tests: publishes enqueue into rumqttc's request
        // channel and succeed locally. This verifies the on-publish +
        // shutoff-timer spawn path doesn't error or panic.
        let c = MqttCommand::new("mq", cfg());
        // A connected broker, as far as the delivery check is concerned.
        *c.connected.lock().await = true;
        let handle = c.run_zone("back_yard", 1).await.unwrap();
        assert_eq!(handle.planned_duration_s, 1);
        assert_eq!(
            handle.provider_ref.as_deref(),
            Some("homeassistant/switch/back_yard/set")
        );
        // Let the 1s shutoff timer fire (its off-publish also enqueues).
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    }

    #[tokio::test]
    async fn status_omits_command_only_zones() {
        // cfg() has NO state_topics, so status() must not assert running state
        // for any zone (it would contradict the engine's own view). Zones are
        // omitted, letting the refresher fall through to running_known=false.
        let c = MqttCommand::new("mq", cfg());
        let s = c.status().await.unwrap();
        assert!(
            s.zone_states.is_empty(),
            "command-only zones must be omitted from status"
        );
        assert!(!s.flow_connected);
    }

    #[tokio::test]
    async fn status_reports_state_topic_zones() {
        // cfg_stateful() gives back_yard a state_topic, so it (and only it) is
        // reported; front_lawn (command-only) is omitted.
        let c = MqttCommand::new("mq", cfg_stateful());
        let s = c.status().await.unwrap();
        let slugs: Vec<_> = s.zone_states.iter().map(|z| z.slug.clone()).collect();
        assert_eq!(slugs, vec!["back_yard".to_string()]);
        assert!(!s.zone_states[0].running_known, "a topic is not a reading");
        assert!(
            !s.flow_connected,
            "a configured topic is not meter evidence"
        );
    }

    fn build_sub_meta(cfg: &MqttCommandConfig) -> SubMeta {
        // Mirror new()'s SubMeta construction without a broker.
        let mut state: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
        for (slug, cmd) in &cfg.zone_command_map {
            if let Some(topic) = &cmd.state_topic {
                let on_match = cmd
                    .state_on_payload
                    .clone()
                    .unwrap_or_else(|| cmd.on_payload.clone());
                state.entry(topic.clone()).or_default().push((
                    slug.clone(),
                    on_match,
                    cmd.off_payload.clone(),
                ));
            }
        }
        SubMeta {
            state,
            availability_topic: cfg.availability_topic.clone(),
            payload_available: cfg.payload_available.clone(),
            payload_not_available: cfg.payload_not_available.clone(),
            flow_topic: cfg.flow_topic.clone(),
        }
    }

    #[test]
    fn submeta_collects_state_topics_and_on_match() {
        let meta = build_sub_meta(&cfg_stateful());
        // back_yard has a state topic; front_lawn does not.
        let subs = meta
            .state
            .get("homeassistant/switch/back_yard/state")
            .unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].0, "back_yard");
        assert_eq!(subs[0].1, "ON"); // stored verbatim; match is case-insensitive
                                     // topics() includes state + availability + flow.
        let topics = meta.topics();
        assert!(topics.contains(&"homeassistant/switch/back_yard/state".to_string()));
        assert!(topics.contains(&"diy-irrig/status".to_string()));
        assert!(topics.contains(&"diy-irrig/flow".to_string()));
        assert_eq!(topics.len(), 3);
    }

    #[test]
    fn submeta_shares_one_state_topic_across_zones() {
        // Two zones reporting on the same state topic both register (C3).
        let mut cfg = cfg();
        let shared_topic = "diy/relays/state";
        for slug in ["back_yard", "front_lawn"] {
            let z = cfg.zone_command_map.get_mut(slug).unwrap();
            z.state_topic = Some(shared_topic.into());
        }
        let meta = build_sub_meta(&cfg);
        let subs = meta.state.get(shared_topic).unwrap();
        assert_eq!(subs.len(), 2, "both zones map to the shared state topic");
        let slugs: std::collections::BTreeSet<_> =
            subs.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(slugs.contains("back_yard") && slugs.contains("front_lawn"));
    }

    #[test]
    fn state_payloads_distinguish_idle_from_uninterpretable() {
        for payload in ["ON", " on ", "on"] {
            assert_eq!(classify_running(payload, "ON", "OFF"), Some(true));
        }
        assert_eq!(classify_running(" off ", "ON", "OFF"), Some(false));
        for payload in ["", "unknown", "{\"POWER\":\"ON\"}"] {
            assert_eq!(classify_running(payload, "ON", "OFF"), None);
        }
    }

    #[test]
    fn parse_flow_reads_plain_numbers_only() {
        assert_eq!(parse_flow(" 3.5 "), Some(3.5));
        assert_eq!(parse_flow("0"), Some(0.0));
        assert_eq!(parse_flow("nan-ish"), None);
        assert_eq!(parse_flow("{\"gpm\":3.5}"), None);
        for payload in ["NaN", "inf", "-1"] {
            assert_eq!(parse_flow(payload), None);
        }
    }

    #[tokio::test]
    async fn reported_state_requires_a_parseable_message_on_the_current_connection() {
        let c = MqttCommand::new("mq", cfg_stateful());
        c._eventloop_handle.abort();
        *c.connected.lock().await = true;
        assert!(!c.status().await.unwrap().zone_states[0].running_known);
        let mut reading = ReportedRunning::default();
        reading.observe("ON", "ON", "OFF");
        c.shared
            .running
            .lock()
            .await
            .insert("back_yard".into(), reading);
        let state = c.status().await.unwrap().zone_states.remove(0);
        assert!(state.running && state.running_known);
        reading.observe("malformed", "ON", "OFF");
        c.shared
            .running
            .lock()
            .await
            .insert("back_yard".into(), reading);
        let state = c.status().await.unwrap().zone_states.remove(0);
        assert!(state.running && !state.running_known);
        reading.observe("OFF", "ON", "OFF");
        c.shared
            .running
            .lock()
            .await
            .insert("back_yard".into(), reading);
        let state = c.status().await.unwrap().zone_states.remove(0);
        assert!(!state.running && state.running_known);
        clear_stale_telemetry(&c.shared).await;
        assert!(!c.status().await.unwrap().zone_states[0].running_known);
    }

    #[test]
    fn classify_availability_online_offline_indeterminate() {
        assert_eq!(
            classify_availability("online", "online", "offline"),
            Some(true)
        );
        assert_eq!(
            classify_availability(" OFFLINE ", "online", "offline"),
            Some(false)
        );
        // Anything else is indeterminate -> caller leaves last value alone.
        assert_eq!(classify_availability("weird", "online", "offline"), None);
    }
}
