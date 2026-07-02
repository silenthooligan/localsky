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

use crate::devices::{Device, DeviceChild, DeviceRegistry};
use crate::discovery::{discover_ecowitt, DiscoveredGateway};
use crate::persistence::sensor_history::{Reading, SensorHistoryStore};
use axum::{extract::State, response::Json, routing::get, Router};
use rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Optional sensor-history handle: None when persistence isn't mounted (then we
/// fall back to the registry's generic field children only).
type Db = Option<Arc<Mutex<Connection>>>;

pub fn router(registry: DeviceRegistry, db: Db) -> Router {
    Router::new()
        .route("/", get(list))
        .route("/discover", get(discover))
        .with_state((registry, db))
}

/// GET /api/v1/devices. Every known device, with each source's real recorded
/// soil channels attached as sensor children.
async fn list(State((registry, db)): State<(DeviceRegistry, Db)>) -> Json<Vec<Device>> {
    let mut devices = registry.all();
    if let Some(db) = db {
        let soil = SensorHistoryStore::new(db)
            .soil_channels()
            .await
            .unwrap_or_default();
        attach_soil_children(&mut devices, &soil);
    }
    Json(devices)
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
/// "Soil moisture · ch 1".
fn soil_label(key: &str) -> String {
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
}
