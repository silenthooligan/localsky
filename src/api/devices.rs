// Devices API. Exposes the DeviceRegistry as the MA-style device view:
// every gateway / hub / controller / cloud account LocalSky knows about,
// each with the sensors or zones it provides.
//
// The registry's source children are the GENERIC fields a source kind
// advertises (builder::kind_fields). On read we additionally enrich each source
// with the CONCRETE per-channel soil sensors it is actually recording
// (sensor_history.soil_channels), so a gateway shows its real soil probes as
// children -- e.g. an Ecowitt gateway's four soil channels -- not just the
// generic weather fields. Keyed identically to the readings + zone bindings
// (`source:<id>:<key>`) so a probe can be bound to a zone later.
//
// Mounted at /api/v1/devices by api::router.

use crate::config::FileConfigStore;
use crate::devices::{Device, DeviceChild, DeviceRegistry};
use crate::discovery::{discover_ecowitt, DiscoveredGateway};
use crate::persistence::sensor_history::{Reading, SensorHistoryStore};
use crate::ports::config_store::ConfigStore;
use axum::{extract::State, response::Json, routing::get, Router};
use rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Optional sensor-history handle: None when persistence isn't mounted (then we
/// fall back to the registry's generic field children only).
type Db = Option<Arc<Mutex<Connection>>>;

/// The config the soil children are judged against: which recorded channels
/// are retired pre-1.1.0 zone readings. None when no config store is mounted.
type Cfg = Option<Arc<FileConfigStore>>;

pub fn router(registry: DeviceRegistry, db: Db, cfg_store: Cfg) -> Router {
    Router::new()
        .route("/", get(list))
        .route("/discover", get(discover))
        .with_state((registry, db, cfg_store))
}

/// GET /api/v1/devices. Every known device, with each source's real recorded
/// soil channels attached as sensor children.
async fn list(
    State((registry, db, cfg_store)): State<(DeviceRegistry, Db, Cfg)>,
) -> Json<Vec<Device>> {
    let mut devices = registry.all();
    if let Some(db) = db {
        let soil = SensorHistoryStore::new(db)
            .soil_channels()
            .await
            .unwrap_or_default();
        let cfg = match cfg_store.as_ref() {
            Some(store) => store.load().await.ok(),
            None => None,
        };
        attach_soil_children(&mut devices, &live_soil_channels(cfg.as_ref(), soil));
    }
    Json(devices)
}

/// Drop the retired zone readings. Before 1.1.0 every mapped probe bound to a
/// zone wrote one shared `soilmoisture_<zone>` channel. 1.1.0 gives each probe
/// its own channel and nothing writes the shared one again, but its history
/// stays until retention prunes it. Listed, it shows a frozen value as if it
/// were a live probe, and binding it does not stick: the config loader folds
/// that binding back into the per-probe bindings, so it reads "Not bound"
/// after every refresh. The Sensors inventory hides these with the same rule.
fn live_soil_channels(cfg: Option<&crate::config::Config>, soil: Vec<Reading>) -> Vec<Reading> {
    soil.into_iter()
        .filter(|r| {
            !cfg.is_some_and(|c| crate::config::soil::legacy_mapped_key(c, &r.source_id, &r.key))
        })
        .collect()
}

/// GET /api/v1/devices/discover, broadcast LAN discovery (Ecowitt for now)
/// and return the gateways found, each with a suggested host the UI's "Add"
/// button pre-fills into an ecowitt_gw_poll source. ~3s while it listens.
async fn discover() -> Json<Vec<DiscoveredGateway>> {
    Json(discover_ecowitt(Duration::from_secs(3)).await)
}

/// Add each recorded soil reading as a sensor child of its source device, so the
/// Devices hub shows the actual probes a source carries. Idempotent: skips a
/// channel already present (e.g. if a future builder enumerates it).
fn attach_soil_children(devices: &mut [Device], soil: &[Reading]) {
    for dev in devices.iter_mut() {
        let Some(sid) = dev.source_id.clone() else {
            continue;
        };
        if !dev.id.starts_with("source:") {
            continue;
        }
        for r in soil.iter().filter(|r| r.source_id == sid) {
            let child_id = format!("source:{sid}:{}", r.key);
            if dev.children.iter().any(|c| c.id == child_id) {
                continue;
            }
            dev.children
                .push(DeviceChild::sensor(child_id, soil_label(&r.key), "soil"));
        }
    }
}

/// "soilmoisture_back_yard" -> "Soil moisture · back yard"; "soilmoisture1" ->
/// "Soil moisture · ch 1"; a mapped probe's encoded channel -> "Soil moisture ·
/// <its topic, entity or query>", never the encoded identity itself.
fn soil_label(key: &str) -> String {
    if let Some(selector) = crate::config::soil::probe_label(key) {
        return if selector.is_empty() {
            "Soil moisture · probe".to_string()
        } else {
            format!("Soil moisture · {selector}")
        };
    }
    let rest = key
        .strip_prefix("soilmoisture_")
        .or_else(|| key.strip_prefix("soilmoisture"))
        .unwrap_or(key);
    if rest.is_empty() {
        "Soil moisture".to_string()
    } else if rest.chars().all(|c| c.is_ascii_digit()) {
        format!("Soil moisture · ch {rest}")
    } else {
        format!(
            "Soil moisture · {}",
            rest.trim_start_matches('_').replace('_', " ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{DeviceKind, DeviceOrigin};

    fn src_dev(id: &str, source_id: &str) -> Device {
        Device {
            id: id.to_string(),
            kind: DeviceKind::WeatherGateway,
            name: id.to_string(),
            model: None,
            identity: None,
            origin: DeviceOrigin::Native,
            source_id: Some(source_id.to_string()),
            online: None,
            last_seen_epoch: None,
            also_in_ha: false,
            enabled: None,
            source_kind: None,
            children: Vec::new(),
        }
    }

    fn reading(source_id: &str, key: &str) -> Reading {
        Reading {
            epoch: 0,
            source_id: source_id.to_string(),
            key: key.to_string(),
            value: 42.0,
        }
    }

    #[test]
    fn attaches_soil_to_matching_source_only() {
        let mut devices = vec![src_dev("source:gw", "gw"), src_dev("source:other", "other")];
        let soil = vec![
            reading("gw", "soilmoisture_back_yard"),
            reading("gw", "soilmoisture1"),
        ];
        attach_soil_children(&mut devices, &soil);
        assert_eq!(devices[0].children.len(), 2);
        assert_eq!(
            devices[0].children[0].id,
            "source:gw:soilmoisture_back_yard"
        );
        assert!(devices[1].children.is_empty());
    }

    #[test]
    fn attach_is_idempotent() {
        let mut devices = vec![src_dev("source:gw", "gw")];
        let soil = vec![reading("gw", "soilmoisture1")];
        attach_soil_children(&mut devices, &soil);
        attach_soil_children(&mut devices, &soil);
        assert_eq!(devices[0].children.len(), 1);
    }

    #[test]
    fn soil_label_humanizes() {
        assert_eq!(
            soil_label("soilmoisture_back_yard"),
            "Soil moisture · back yard"
        );
        assert_eq!(soil_label("soilmoisture1"), "Soil moisture · ch 1");
        assert_eq!(soil_label("soilmoisture"), "Soil moisture");
    }

    /// A 1.1.0 probe channel is keyed by an encoded identity. The label is the
    /// topic the user typed, not the encoding.
    #[test]
    fn soil_label_names_a_mapped_probe_by_its_topic() {
        let key = crate::config::soil::probe_key(
            "mqtt",
            &["devices/sensor-soil1/sensor/soil_moisture/state", ""],
        );
        assert_eq!(
            soil_label(&key),
            "Soil moisture · devices/sensor-soil1/sensor/soil_moisture/state"
        );
        let with_path = crate::config::soil::probe_key("mqtt", &["soil/1", "moisture"]);
        assert_eq!(soil_label(&with_path), "Soil moisture · soil/1 / moisture");
        let bare = crate::config::soil::probe_key("http", &[""]);
        assert_eq!(soil_label(&bare), "Soil moisture · probe");
    }

    /// The pre-1.1.0 shared zone channel is retired once its mappings have
    /// their own channels, so the Devices list must not offer it. The probes,
    /// native channels and other sources' channels stay.
    #[test]
    fn retired_zone_channels_are_not_listed() {
        let mut cfg = crate::config::Config::default();
        cfg.sources.push(
            serde_json::from_value::<crate::config::SourceEntry>(serde_json::json!({
                "id": "mqtt", "kind": "mqtt", "config": {
                    "broker_host": "localhost", "subscriptions": [{
                        "topic": "devices/sensor-soil1/sensor/soil_moisture/state",
                        "field": "rh_pct",
                        "zone_slug": "planter_boxes_tree"
                    }]
                }
            }))
            .unwrap(),
        );
        let probe = crate::config::soil::probe_key(
            "mqtt",
            &["devices/sensor-soil1/sensor/soil_moisture/state", ""],
        );
        let soil = vec![
            reading("mqtt", "soilmoisture_planter_boxes_tree"),
            reading("mqtt", &probe),
            reading("gw", "soilmoisture1"),
            reading("gw", "soilmoisture_planter_boxes_tree"),
        ];
        let kept: Vec<String> = live_soil_channels(Some(&cfg), soil.clone())
            .into_iter()
            .map(|r| format!("{}:{}", r.source_id, r.key))
            .collect();
        assert_eq!(
            kept,
            [
                format!("mqtt:{probe}"),
                "gw:soilmoisture1".to_string(),
                "gw:soilmoisture_planter_boxes_tree".to_string(),
            ]
        );
        assert_eq!(
            live_soil_channels(None, soil).len(),
            4,
            "without a config nothing can be judged retired"
        );
    }
}
