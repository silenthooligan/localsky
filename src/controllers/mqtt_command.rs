// Generic MQTT command-sink controller.
//
// Publishes a configured on/off payload to a per-zone topic when LocalSky
// runs/stops a zone. Connects once at construction and maintains a
// background event-loop task that keeps the rumqttc client alive +
// reconnecting; the run_zone / stop_zone calls just enqueue a publish.
//
// This is the "anything that listens to MQTT" controller. Targets:
//   - ESPHome with `switch.mqtt:` blocks
//   - Tasmota POWER1/POWER2 (cmnd/<topic>/POWER on/off)
//   - Sonoff / Shelly devices in MQTT mode
//   - Zigbee2MQTT relay devices (zigbee2mqtt/<friendly_name>/set body)
//   - DIY relay boards (ESP32, Raspberry Pi GPIO bridges)
//   - OpenSprinkler's MQTT plug-in
//
// No state subscription — commands are fire-and-forget. For confirmed
// state with feedback, use ESPHome native or HaServiceCall instead.
//
// run_zone fires the on-publish and immediately returns a RunHandle.
// LocalSky's irrigation engine schedules the matching off-publish at
// start+duration; MqttCommand does not run its own timer.

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

pub struct MqttCommand {
    id: String,
    config: MqttCommandConfig,
    client: AsyncClient,
    /// Background task driving the EventLoop. Held so we don't drop the
    /// event loop while the controller is alive.
    _eventloop_handle: JoinHandle<()>,
    /// Tracks connection state for status().reachable.
    connected: Arc<Mutex<bool>>,
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
        let _eventloop_handle =
            tokio::spawn(drive_eventloop(id.clone(), eventloop, connected.clone()));
        Self {
            id,
            config,
            client,
            _eventloop_handle,
            connected,
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

    async fn publish_zone(&self, slug: &str, on: bool) -> ControllerResult<()> {
        let cmd = self.zone_command(slug)?;
        let payload = if on {
            cmd.on_payload.as_bytes()
        } else {
            cmd.off_payload.as_bytes()
        };
        self.client
            .publish(&cmd.topic, QoS::AtLeastOnce, cmd.retain, payload.to_vec())
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

async fn drive_eventloop(id: String, mut eventloop: EventLoop, connected: Arc<Mutex<bool>>) {
    info!(controller = %id, "mqtt controller event loop started");
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                *connected.lock().await = true;
                info!(controller = %id, "mqtt controller connected");
            }
            Ok(Event::Incoming(Packet::Disconnect)) => {
                *connected.lock().await = false;
                warn!(controller = %id, "mqtt controller broker-disconnected");
            }
            Ok(_) => {}
            Err(e) => {
                *connected.lock().await = false;
                warn!(controller = %id, error = %e, "mqtt controller eventloop error; rumqttc will reconnect");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

#[async_trait]
impl IrrigationController for MqttCommand {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            flow_meter: false,
            rain_sensor: false,
            master_valve: false,
            // MQTT is fire-and-forget per topic, so concurrent zones run
            // fine — the broker handles each publish independently.
            multi_zone_parallel: true,
            history_query: false,
            remote_program_upload: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        self.publish_zone(slug, true).await?;
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
        let reachable = *self.connected.lock().await;
        Ok(ControllerStatus {
            reachable,
            master_enabled: None,
            water_level_pct: None,
            rain_sensor_tripped: None,
            current_program: None,
            // No state feedback; report every mapped zone as 'unknown
            // (not running)'. The engine layers its own scheduled-state
            // view on top of run_history() in the runs store.
            zone_states: self
                .config
                .zone_command_map
                .keys()
                .map(|slug| ZoneRuntimeStatus {
                    slug: slug.clone(),
                    running: false,
                    remaining_s: None,
                    last_run_epoch: None,
                })
                .collect(),
            flow_gpm: None,
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

    fn cfg() -> MqttCommandConfig {
        let mut map = BTreeMap::new();
        map.insert(
            "back_yard".to_string(),
            MqttZoneCommand {
                topic: "homeassistant/switch/back_yard/set".into(),
                on_payload: "ON".into(),
                off_payload: "OFF".into(),
                retain: false,
            },
        );
        map.insert(
            "front_lawn".to_string(),
            MqttZoneCommand {
                topic: "tasmota/cmnd/front_lawn/POWER".into(),
                on_payload: "1".into(),
                off_payload: "0".into(),
                retain: false,
            },
        );
        MqttCommandConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: 1883,
            username: None,
            password: None,
            client_id: Some("test-controller".into()),
            zone_command_map: map,
        }
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
    async fn unknown_zone_errors() {
        let c = MqttCommand::new("mq", cfg());
        let err = c.run_zone("not_a_zone", 60).await.unwrap_err();
        assert!(matches!(err, ControllerError::ZoneUnknown(_)));
    }

    #[tokio::test]
    async fn status_lists_all_configured_zones() {
        let c = MqttCommand::new("mq", cfg());
        let s = c.status().await.unwrap();
        // Broker is unreachable in tests so reachable=false, but the
        // zone list is computed from config and must be populated.
        assert_eq!(s.zone_states.len(), 2);
        let slugs: Vec<_> = s.zone_states.iter().map(|z| z.slug.clone()).collect();
        assert!(slugs.contains(&"back_yard".to_string()));
        assert!(slugs.contains(&"front_lawn".to_string()));
    }
}
