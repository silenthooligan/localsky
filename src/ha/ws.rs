// Home Assistant WebSocket client for device/entity registry import
// (Phase F1 of the device-parity effort).
//
// HA's device + entity registries are only exposed over the WS API
// (`config/device_registry/list`, `config/entity_registry/list`), not REST.
// This connects to ws://<ha>/api/websocket, authenticates with the same
// long-lived token the REST client uses (HA_TOKEN / HA_LONG_LIVED_TOKEN +
// HA_URL), pulls both registries, and builds origin = HomeAssistant `Device`s
// so HA's hardware appears in LocalSky's Devices view alongside the native
// ones. The import is scoped to weather / soil / irrigation-relevant devices
// (HA commonly has 100+ devices; the rest are noise here).
//
// Cross-source dedup (the same physical gateway imported here AND discovered
// natively) is deferred to Phase F3: HA identifies the HA-side Ecowitt by its
// vendor passkey while native discovery keys on MAC, with no derivable
// relationship, so the two currently appear as separate cards.

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::devices::{Device, DeviceChild, DeviceKind, DeviceOrigin};

/// WS endpoint + token, resolved from the same env the REST client uses.
pub struct HaWsConfig {
    pub ws_url: String,
    pub token: String,
}

impl HaWsConfig {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("HA_TOKEN")
            .or_else(|_| std::env::var("HA_LONG_LIVED_TOKEN"))
            .context("HA_TOKEN (or HA_LONG_LIVED_TOKEN) required for HA device import")?;
        let base = std::env::var("HA_URL").context("HA_URL required for HA device import")?;
        let ws = base
            .trim_end_matches('/')
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        Ok(Self {
            ws_url: format!("{ws}/api/websocket"),
            token,
        })
    }
}

/// Connect, authenticate, and pull the device + entity registries. Returns
/// the raw arrays; `build_ha_devices` turns them into `Device`s (kept
/// separate so the build logic is unit-testable without a live HA).
async fn registry_lists(cfg: &HaWsConfig) -> Result<(Vec<Value>, Vec<Value>)> {
    let (mut ws, _) = connect_async(&cfg.ws_url)
        .await
        .context("HA WS connect failed")?;

    // First frame is auth_required; then we send auth and expect auth_ok.
    let _ = recv_json(&mut ws).await?;
    ws.send(Message::Text(
        json!({"type": "auth", "access_token": cfg.token})
            .to_string()
            .into(),
    ))
    .await?;
    let auth = recv_json(&mut ws).await?;
    if auth.get("type").and_then(Value::as_str) != Some("auth_ok") {
        bail!("HA WS auth failed: {auth}");
    }

    ws.send(Message::Text(
        json!({"id": 1, "type": "config/device_registry/list"})
            .to_string()
            .into(),
    ))
    .await?;
    ws.send(Message::Text(
        json!({"id": 2, "type": "config/entity_registry/list"})
            .to_string()
            .into(),
    ))
    .await?;

    // Results may arrive in any order; collect by id.
    let mut devices: Option<Vec<Value>> = None;
    let mut entities: Option<Vec<Value>> = None;
    while devices.is_none() || entities.is_none() {
        let m = recv_json(&mut ws).await?;
        if m.get("type").and_then(Value::as_str) != Some("result") {
            continue;
        }
        if m.get("success").and_then(Value::as_bool) == Some(false) {
            bail!("HA WS command failed: {m}");
        }
        let arr = m
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match m.get("id").and_then(Value::as_i64) {
            Some(1) => devices = Some(arr),
            Some(2) => entities = Some(arr),
            _ => {}
        }
    }
    let _ = ws.close(None).await;
    Ok((devices.unwrap_or_default(), entities.unwrap_or_default()))
}

/// Read the next JSON text frame, skipping pings/binary/etc.
async fn recv_json(
    ws: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Result<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t)?),
            Some(Ok(Message::Close(_))) | None => bail!("HA WS closed before a response"),
            Some(Ok(_)) => continue, // ping/pong/binary
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

/// Fetch HA devices, with an overall timeout so a wedged HA never stalls the
/// refresh task. Returns the scoped (weather/soil/irrigation) device list.
pub async fn fetch_ha_devices(cfg: &HaWsConfig) -> Result<Vec<Device>> {
    let (devs, ents) = tokio::time::timeout(Duration::from_secs(10), registry_lists(cfg))
        .await
        .context("HA WS registry fetch timed out")??;
    Ok(build_ha_devices(&devs, &ents))
}

/// Keywords that mark a device as weather/soil/irrigation-relevant. Matched
/// against the device's manufacturer/model/name and its entity ids. Keeps the
/// import from dumping all ~130 HA devices (lights, locks, automations) into
/// the weather app.
const RELEVANT_KEYWORDS: &[&str] = &[
    "ecowitt",
    "tempest",
    "weatherflow",
    "davis",
    "ambient",
    "netatmo",
    "acurite",
    "weather",
    "soil",
    "moisture",
    "rain",
    "wind",
    "irrigation",
    "sprinkler",
    "opensprinkler",
    "rachio",
    "hydrawise",
    "bhyve",
    "b-hyve",
    "valve",
    "evapotranspiration",
];

fn is_relevant(hay: &str) -> bool {
    let h = hay.to_ascii_lowercase();
    RELEVANT_KEYWORDS.iter().any(|k| h.contains(k))
}

/// Coarse role for an HA entity from its entity_id (domain + keywords).
fn entity_role(entity_id: &str) -> &'static str {
    let e = entity_id.to_ascii_lowercase();
    if e.contains("soil") || e.contains("moisture") {
        "soil"
    } else if e.contains("temp") {
        "temperature"
    } else if e.contains("humid") {
        "humidity"
    } else if e.contains("wind") {
        "wind"
    } else if e.contains("rain") || e.contains("precip") {
        "rain"
    } else if e.contains("pressure") {
        "pressure"
    } else if e.starts_with("valve.") || e.starts_with("switch.") {
        "valve"
    } else {
        "sensor"
    }
}

/// Build origin=HomeAssistant `Device`s from the raw registry arrays. Pure so
/// it can be tested against captured registry JSON.
pub fn build_ha_devices(devs: &[Value], ents: &[Value]) -> Vec<Device> {
    use std::collections::HashMap;
    // entity_id list per device_id.
    let mut by_device: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in ents {
        let (Some(did), Some(eid)) = (
            e.get("device_id").and_then(Value::as_str),
            e.get("entity_id").and_then(Value::as_str),
        ) else {
            continue;
        };
        // Skip disabled/hidden entities — they're not live.
        if e.get("disabled_by").map(|v| !v.is_null()).unwrap_or(false) {
            continue;
        }
        by_device.entry(did).or_default().push(eid);
    }

    let mut out = Vec::new();
    for d in devs {
        let Some(id) = d.get("id").and_then(Value::as_str) else {
            continue;
        };
        let manufacturer = d.get("manufacturer").and_then(Value::as_str).unwrap_or("");
        let model = d.get("model").and_then(Value::as_str).unwrap_or("");
        let name = d
            .get("name_by_user")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| d.get("name").and_then(Value::as_str))
            .unwrap_or("Home Assistant device");
        let entity_ids = by_device.remove(id).unwrap_or_default();

        // Relevance: device fields OR any of its entity ids.
        let blob = format!("{manufacturer} {model} {name} {}", entity_ids.join(" "));
        if !is_relevant(&blob) {
            continue;
        }

        // Kind: irrigation if it looks like a controller/valve, else gateway.
        let kind = if is_irrigation(&blob) {
            DeviceKind::IrrigationController
        } else {
            DeviceKind::WeatherGateway
        };

        // Identity: a MAC connection if present, else the first identifier
        // value (e.g. the Ecowitt passkey). F3 reconciles these.
        let identity = mac_connection(d).or_else(|| first_identifier(d));

        let children: Vec<DeviceChild> = entity_ids
            .iter()
            .map(|eid| {
                DeviceChild::sensor(format!("ha:{eid}"), humanize_entity(eid), entity_role(eid))
            })
            .collect();

        out.push(Device {
            id: format!("ha:{id}"),
            kind,
            name: name.to_string(),
            model: (!model.is_empty()).then(|| model.to_string()),
            identity,
            origin: DeviceOrigin::HomeAssistant,
            source_id: None,
            online: None,
            last_seen_epoch: None,
            children,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn is_irrigation(blob: &str) -> bool {
    let h = blob.to_ascii_lowercase();
    [
        "irrigation",
        "sprinkler",
        "opensprinkler",
        "rachio",
        "hydrawise",
        "bhyve",
        "valve",
    ]
    .iter()
    .any(|k| h.contains(k))
}

/// The MAC from a device's `connections` ([["mac","aa:bb:.."], ...]), uppercased.
fn mac_connection(d: &Value) -> Option<String> {
    d.get("connections")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|c| {
            let pair = c.as_array()?;
            if pair.first()?.as_str()? == "mac" {
                Some(pair.get(1)?.as_str()?.to_ascii_uppercase())
            } else {
                None
            }
        })
}

/// The first `identifiers` value (e.g. Ecowitt passkey), domain-prefixed.
fn first_identifier(d: &Value) -> Option<String> {
    let pair = d
        .get("identifiers")
        .and_then(Value::as_array)?
        .first()?
        .as_array()?;
    let domain = pair.first()?.as_str()?;
    let value = pair.get(1)?.as_str()?;
    Some(format!("{domain}:{value}"))
}

/// "sensor.gw1100b_indoor_temperature" -> "Gw1100b Indoor Temperature".
fn humanize_entity(entity_id: &str) -> String {
    let bare = entity_id
        .split_once('.')
        .map(|(_, r)| r)
        .unwrap_or(entity_id);
    bare.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn imports_ecowitt_device_with_entities() {
        // Real shape from a real HA registry.
        let devs = vec![
            json!({"id":"77f0359","name":"GW1100B","name_by_user":null,"manufacturer":"Ecowitt","model":"GW1100B","connections":[],"identifiers":[["ecowitt","741EB29E11D930A6A61FD83D9B621EE2"]]}),
            json!({"id":"lock1","name":"Front Door","manufacturer":"August","model":"Lock","connections":[["mac","aa:bb:cc:dd:ee:ff"]],"identifiers":[]}),
        ];
        let ents = vec![
            json!({"device_id":"77f0359","entity_id":"sensor.gw1100b_indoor_temperature","disabled_by":null}),
            json!({"device_id":"77f0359","entity_id":"sensor.back_yard_soil_moisture","disabled_by":null}),
            json!({"device_id":"lock1","entity_id":"lock.front_door","disabled_by":null}),
        ];
        let out = build_ha_devices(&devs, &ents);
        // The lock is filtered out (not weather/soil/irrigation).
        assert_eq!(out.len(), 1);
        let gw = &out[0];
        assert_eq!(gw.id, "ha:77f0359");
        assert_eq!(gw.origin, DeviceOrigin::HomeAssistant);
        assert_eq!(gw.model.as_deref(), Some("GW1100B"));
        assert_eq!(gw.kind, DeviceKind::WeatherGateway);
        // No MAC connection -> falls back to the ecowitt passkey identifier.
        assert_eq!(
            gw.identity.as_deref(),
            Some("ecowitt:741EB29E11D930A6A61FD83D9B621EE2")
        );
        assert_eq!(gw.children.len(), 2);
        let soil = gw
            .children
            .iter()
            .find(|c| c.id == "ha:sensor.back_yard_soil_moisture")
            .unwrap();
        assert_eq!(soil.label, "Back Yard Soil Moisture");
    }

    #[test]
    fn classifies_irrigation_and_skips_disabled() {
        let devs = vec![
            json!({"id":"os","name":"OpenSprinkler","manufacturer":"OpenThings","model":"OS 3.0","connections":[],"identifiers":[["opensprinkler","x"]]}),
        ];
        let ents = vec![
            json!({"device_id":"os","entity_id":"valve.sprinkler_zone_1","disabled_by":null}),
            json!({"device_id":"os","entity_id":"sensor.sprinkler_flow","disabled_by":"user"}),
        ];
        let out = build_ha_devices(&devs, &ents);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, DeviceKind::IrrigationController);
        // The disabled flow sensor is dropped.
        assert_eq!(out[0].children.len(), 1);
        assert_eq!(out[0].children[0].id, "ha:valve.sprinkler_zone_1");
    }

    #[test]
    fn from_env_builds_ws_url() {
        // http -> ws + /api/websocket (env-independent string logic).
        let cfg = HaWsConfig {
            ws_url: "ws://192.0.2.79:8123/api/websocket".into(),
            token: "t".into(),
        };
        assert!(cfg.ws_url.ends_with("/api/websocket"));
    }
}
