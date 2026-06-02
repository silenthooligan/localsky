// Sensor discovery. The point: nothing LocalSky could use should be
// invisible, and it shouldn't matter where a device was onboarded. Most
// people add devices to Home Assistant first, so we discover straight from
// HA by device_class (plus any local-POST channels in sensor_history).
//
//   GET /api/v1/sensors/soil       — soil-moisture channels (zone picker)
//   GET /api/v1/sensors/discovered — every relevant HA entity, grouped by
//                                    role, so the Sensors hub can show the
//                                    full picture.
//
// Each entry's `id` is the canonical address the engine resolves:
// `ha:<entity_id>` for HA entities, `source:<src>:<key>` for local POSTs.

use std::sync::Arc;

use axum::{response::Json, routing::get, Router};
use rusqlite::Connection;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::persistence::sensor_history::SensorHistoryStore;

pub fn router(db: Arc<Mutex<Connection>>) -> Router {
    Router::new()
        .route("/soil", get(soil))
        .route("/discovered", get(discovered))
        .with_state(db)
}

#[derive(Serialize, Clone)]
struct DiscoveredSensor {
    /// Canonical address the engine resolves (`ha:<entity>` or `source:..`).
    id: String,
    /// Friendly label for pickers / lists.
    label: String,
    /// Role bucket: soil | temperature | humidity | rain | wind | pressure | light.
    role: String,
    /// Where it came from: a source id, or "home_assistant".
    source: String,
    /// Latest value, if numeric and available.
    current_pct: Option<f64>,
    /// Unit of measurement, if known.
    unit: String,
    /// Seconds since the latest reading (0 for HA live reads).
    age_s: i64,
}

/// Classify an HA entity into a LocalSky role by device_class, falling back
/// to the entity id. Returns None for entities LocalSky has no use for, so
/// the hub shows weather/soil-relevant sensors rather than every battery %.
fn classify(entity_id: &str, device_class: &str) -> Option<&'static str> {
    // Only numeric `sensor.*` entities. Leak detectors are
    // `binary_sensor.*` with device_class=moisture — explicitly NOT soil.
    if !entity_id.starts_with("sensor.") {
        return None;
    }
    let by_class = match device_class {
        "moisture" => Some("soil"),
        "temperature" => Some("temperature"),
        "humidity" => Some("humidity"),
        "precipitation" | "precipitation_intensity" => Some("rain"),
        "wind_speed" => Some("wind"),
        "pressure" | "atmospheric_pressure" => Some("pressure"),
        "illuminance" | "irradiance" => Some("light"),
        _ => None,
    };
    if by_class.is_some() {
        return by_class;
    }
    // Name-based fallback for sensors that don't set a device_class.
    let e = entity_id;
    if e.contains("soil") && e.contains("moist") {
        Some("soil")
    } else if e.contains("rain") || e.contains("precip") {
        Some("rain")
    } else if e.contains("wind") && e.contains("speed") {
        Some("wind")
    } else {
        None
    }
}

/// Friendly label for an Ecowitt/raw local channel key.
fn pretty_key(key: &str) -> String {
    if let Some(n) = key.strip_prefix("soilmoisture") {
        return format!("Soil ch{n}");
    }
    key.replace('_', " ")
}

/// Query HA once and return every relevant entity, classified by role.
/// Excludes LocalSky's own `localsky_*` outputs and `*_raw_pct` debug
/// entities. Empty when HA isn't configured/reachable.
async fn discover_ha() -> Vec<DiscoveredSensor> {
    let Ok(client) = crate::ha::rest::HaClient::from_env() else {
        return Vec::new();
    };
    let Ok(states) = client.states().await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for s in states {
        let eid = s.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
        if eid.starts_with("sensor.localsky_") || eid.ends_with("_raw_pct") {
            continue;
        }
        let dc = s
            .get("attributes")
            .and_then(|a| a.get("device_class"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some(role) = classify(eid, dc) else {
            continue;
        };
        let attrs = s.get("attributes");
        let friendly = attrs
            .and_then(|a| a.get("friendly_name"))
            .and_then(|v| v.as_str())
            .unwrap_or(eid)
            .to_string();
        let unit = attrs
            .and_then(|a| a.get("unit_of_measurement"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let current_pct = s
            .get("state")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse::<f64>().ok());
        out.push(DiscoveredSensor {
            id: format!("ha:{eid}"),
            label: format!("{friendly} (HA)"),
            role: role.to_string(),
            source: "home_assistant".into(),
            current_pct,
            unit,
            age_s: 0,
        });
    }
    out
}

/// Local soil channels recorded in sensor_history (e.g. an Ecowitt gateway
/// posting straight to /ingest/ecowitt, bypassing HA).
async fn discover_local_soil(db: Arc<Mutex<Connection>>) -> Vec<DiscoveredSensor> {
    let store = SensorHistoryStore::new(db);
    let now = chrono::Utc::now().timestamp();
    store
        .soil_channels()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| DiscoveredSensor {
            id: format!("source:{}:{}", r.source_id, r.key),
            label: format!("{} · {}", r.source_id, pretty_key(&r.key)),
            role: "soil".into(),
            source: r.source_id,
            current_pct: Some(r.value),
            unit: "%".into(),
            age_s: (now - r.epoch).max(0),
        })
        .collect()
}

/// Soil channels only — powers the zone soil-sensor picker.
async fn soil(
    axum::extract::State(db): axum::extract::State<Arc<Mutex<Connection>>>,
) -> Json<Vec<DiscoveredSensor>> {
    let mut out: Vec<DiscoveredSensor> = discover_ha()
        .await
        .into_iter()
        .filter(|d| d.role == "soil")
        .collect();
    out.extend(discover_local_soil(db).await);
    Json(out)
}

/// Everything LocalSky could use, grouped by role — the hub's full picture.
async fn discovered(
    axum::extract::State(db): axum::extract::State<Arc<Mutex<Connection>>>,
) -> Json<std::collections::BTreeMap<String, Vec<DiscoveredSensor>>> {
    let mut all = discover_ha().await;
    all.extend(discover_local_soil(db).await);
    let mut grouped: std::collections::BTreeMap<String, Vec<DiscoveredSensor>> =
        std::collections::BTreeMap::new();
    for d in all {
        grouped.entry(d.role.clone()).or_default().push(d);
    }
    Json(grouped)
}
