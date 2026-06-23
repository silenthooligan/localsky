// Sensor discovery. The point: nothing LocalSky could use should be
// invisible, and it shouldn't matter where a device was onboarded. Most
// people add devices to Home Assistant first, so we discover straight from
// HA by device_class (plus any local-POST channels in sensor_history).
//
//   GET /api/v1/sensors/soil      , soil-moisture channels (zone picker)
//   GET /api/v1/sensors/discovered, every relevant HA entity, grouped by
//                                    role, so the Sensors hub can show the
//                                    full picture.
//   GET /api/v1/sensors/inventory , unified Sensors-UI view: soil probes
//                                    (from data sources) + flow meters
//                                    (from controllers), enriched with
//                                    origin, live reading, battery, and the
//                                    zone each is bound to.
//
// Each entry's `id` is the canonical address the engine resolves:
// `ha:<entity_id>` for HA entities, `source:<src>:<key>` for local POSTs.

use std::sync::Arc;

use axum::{response::Json, routing::get, Router};
use rusqlite::Connection;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::config::FileConfigStore;
use crate::controllers::registry::ControllerRegistry;
use crate::persistence::sensor_history::SensorHistoryStore;
use crate::ports::config_store::ConfigStore;

/// Boot-time handles for GET /sensors/inventory: the config store (zone
/// bindings + source labels) and the controller registry (flow-meter
/// capability + live GPM). Set once from main.rs. Unset (demo, or boot
/// before wiring) makes the inventory fall back to soil-only with no zone
/// bindings and no flow entries, rather than failing.
struct InventoryHandles {
    cfg_store: Arc<FileConfigStore>,
    controllers: ControllerRegistry,
}

static INVENTORY: std::sync::OnceLock<InventoryHandles> = std::sync::OnceLock::new();

/// Register the config store + controller registry used by the unified
/// sensor inventory (called from main at boot).
pub fn set_inventory_handles(cfg_store: Arc<FileConfigStore>, controllers: ControllerRegistry) {
    let _ = INVENTORY.set(InventoryHandles {
        cfg_store,
        controllers,
    });
}

pub fn router(db: Arc<Mutex<Connection>>) -> Router {
    Router::new()
        .route("/soil", get(soil))
        .route("/discovered", get(discovered))
        .route("/inventory", get(inventory))
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
    // `binary_sensor.*` with device_class=moisture, explicitly NOT soil.
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
    if let Some(rest) = key.strip_prefix("soilmoisture") {
        // Zone-bound MQTT soil channels are keyed `soilmoisture_<zone_slug>`;
        // numbered native channels are `soilmoisture<N>`. Render the slug form
        // as the zone name instead of "ch_back_yard".
        if let Some(zone) = rest.strip_prefix('_') {
            return format!("Soil ({})", zone.replace('_', " "));
        }
        return format!("Soil ch{rest}");
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

/// Soil channels only, powers the zone soil-sensor picker.
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

/// Everything LocalSky could use, grouped by role, the hub's full picture.
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

// ----- Unified sensor inventory (Sensors UI) -----
//
// One call that the Sensors page renders directly: every soil-moisture
// channel (native `source:` channels + any HA/MQTT soil the soil endpoint
// knows) enriched with origin + live reading + battery/temp/EC + the zone it
// is bound to, every source that hosts a soil channel (group helper), and
// every controller that advertises a flow meter with its live GPM.

#[derive(Serialize, Clone)]
struct InventoryResponse {
    gateways: Vec<GatewayEntry>,
    soil: Vec<SoilSensor>,
    flow: Vec<FlowSensor>,
}

#[derive(Serialize, Clone)]
struct GatewayEntry {
    /// Source id that hosts the soil channel(s).
    source_id: String,
    /// Friendly label (the source's configured id; no separate display name
    /// exists in the schema). `"home_assistant"` for HA-hosted soil.
    label: String,
    /// Source kind tag (`ecowitt_gw_poll`, `mqtt`, ...), or
    /// `home_assistant` for HA-discovered soil.
    kind: String,
    /// Source freshness: false only when the config + history say this
    /// source has produced nothing recently; true when unknown.
    online: bool,
    /// How many soil-moisture channels this source provides.
    soil_count: usize,
}

#[derive(Serialize, Clone)]
struct SoilSensor {
    /// Canonical engine address; byte-identical to the soil endpoint / zone
    /// binding so a `bound_zone_slug` round-trips.
    id: String,
    /// Friendly channel label (e.g. "Soil ch1").
    channel_label: String,
    source_id: String,
    source_label: String,
    source_kind: String,
    /// Live moisture %, if available.
    moisture_pct: Option<f64>,
    /// Seconds since the live reading (0 for HA live reads).
    age_s: i64,
    /// Optional sibling channels, native `source:` only; null for HA.
    battery_pct: Option<f64>,
    temp_f: Option<f64>,
    ec: Option<f64>,
    /// Zone this sensor is bound to (scanning ZoneConfig.soil_sensor_id),
    /// or null when unbound.
    bound_zone_slug: Option<String>,
    bound_zone_name: Option<String>,
}

#[derive(Serialize, Clone)]
struct FlowSensor {
    /// Stable id: `controller:<controller_id>`.
    id: String,
    controller_id: String,
    controller_label: String,
    controller_kind: String,
    /// CAPABLE: the controller type supports flow metering
    /// (`ControllerCaps.flow_meter`). True for every OpenSprinkler whether or
    /// not a sensor is wired in.
    supported: bool,
    /// CONNECTED: a flow sensor is actually wired to this specific device,
    /// read from the controller's own configuration
    /// (`ControllerStatus.flow_connected`). False when nothing is connected,
    /// so a user with no meter never sees a phantom sensor. Replaces the old
    /// `detected` field, which conflated capability with presence.
    connected: bool,
    /// LIVE: latest measured flow, null when none/idle.
    gpm: Option<f64>,
    /// Seconds since the reading, when known (live controller read = 0).
    age_s: Option<i64>,
}

/// Source kind tag for the inventory (mirrors health's source_kind_label
/// but local so the modules stay decoupled).
fn source_kind_tag(kind: &crate::config::schema::SourceKind) -> &'static str {
    use crate::config::schema::SourceKind::*;
    match kind {
        TempestUdp(_) => "tempest_udp",
        TempestWs(_) => "tempest_ws",
        OpenMeteo(_) => "open_meteo",
        EcowittLocal(_) => "ecowitt_local",
        EcowittGwPoll(_) => "ecowitt_gw_poll",
        Nws(_) => "nws",
        OpenWeather(_) => "openweather",
        PirateWeather(_) => "pirate_weather",
        MetNorway(_) => "met_norway",
        AmbientWeather(_) => "ambient_weather",
        Netatmo(_) => "netatmo",
        Yolink(_) => "yolink",
        Lacrosse(_) => "lacrosse",
        TuyaCloud(_) => "tuya_cloud",
        DavisWll(_) => "davis_wll",
        HaPassthrough(_) => "ha_passthrough",
        Mqtt(_) => "mqtt",
        HttpWebhook(_) => "http_webhook",
        Blitzortung(_) => "blitzortung",
        DemoReplay(_) => "demo_replay",
    }
}

fn controller_kind_tag(kind: &crate::config::schema::ControllerKind) -> &'static str {
    use crate::config::schema::ControllerKind::*;
    match kind {
        OpensprinklerDirect(_) => "opensprinkler_direct",
        HaServiceCall(_) => "ha_service_call",
        EsphomeNative(_) => "esphome_native",
        Rachio(_) => "rachio",
        Hydrawise(_) => "hydrawise",
        Bhyve(_) => "bhyve",
        Rainbird(_) => "rainbird",
        MqttCommand(_) => "mqtt_command",
        HttpGeneric(_) => "http_generic",
        DryRun(_) => "dry_run",
    }
}

/// Map a soil-channel `key` to a friendly channel label.
fn channel_label(key: &str) -> String {
    if let Some(rest) = key.strip_prefix("soilmoisture") {
        // `soilmoisture_<zone_slug>` (zone-bound MQTT) vs `soilmoisture<N>`.
        if let Some(zone) = rest.strip_prefix('_') {
            return zone.replace('_', " ");
        }
        return format!("Channel {rest}");
    }
    pretty_key(key)
}

/// Swap a `soilmoisture<N>` key for a sibling channel (battery/temp/EC).
/// Returns None for non-numbered soil keys (e.g. HA `*_soil_moisture`).
fn sibling_key(key: &str, suffix_fmt: impl Fn(&str) -> String) -> Option<String> {
    key.strip_prefix("soilmoisture").map(suffix_fmt)
}

/// Unified Sensors-UI inventory. Soil from the same discovery the soil
/// endpoint uses (HA soil + native channels), enriched with battery/temp/EC
/// + zone binding; flow from controllers advertising a flow meter.
async fn inventory(
    axum::extract::State(db): axum::extract::State<Arc<Mutex<Connection>>>,
) -> Json<InventoryResponse> {
    let cfg = match INVENTORY.get() {
        Some(h) => h.cfg_store.load().await.ok(),
        None => None,
    };
    let controllers = INVENTORY.get().map(|h| h.controllers.clone());
    Json(build_inventory(SensorHistoryStore::new(db), cfg, controllers.as_ref()).await)
}

/// Core inventory builder, parameterized for testability (the handler
/// passes globals; tests pass explicit fixtures). Soil from sensor_history
/// + HA discovery, gateways grouped per soil-hosting source, flow from
/// controllers advertising a meter.
async fn build_inventory(
    store: SensorHistoryStore,
    cfg: Option<crate::config::Config>,
    controllers: Option<&ControllerRegistry>,
) -> InventoryResponse {
    let now = chrono::Utc::now().timestamp();

    // ----- Soil channels -----
    // Native channels straight from sensor_history (id-identical to the soil
    // endpoint), plus HA soil entities classified by the shared discovery.
    let mut soil: Vec<SoilSensor> = Vec::new();

    // Native `source:` channels.
    let native = store.soil_channels().await.unwrap_or_default();
    for r in &native {
        let id = format!("source:{}:{}", r.source_id, r.key);
        // Battery/temp/EC siblings live in the same source under a swapped
        // key suffix; reachable only for numbered `soilmoisture<N>` channels.
        let battery_pct = match sibling_key(&r.key, |n| format!("soilbatt{n}")) {
            Some(k) => store
                .last_value(r.source_id.clone(), k)
                .await
                .ok()
                .flatten()
                .map(|x| x.value),
            None => None,
        };
        let temp_f = match sibling_key(&r.key, |n| format!("soiltemp{n}f")) {
            Some(k) => store
                .last_value(r.source_id.clone(), k)
                .await
                .ok()
                .flatten()
                .map(|x| x.value),
            None => None,
        };
        let ec = match sibling_key(&r.key, |n| format!("soilec{n}")) {
            Some(k) => store
                .last_value(r.source_id.clone(), k)
                .await
                .ok()
                .flatten()
                .map(|x| x.value),
            None => None,
        };
        let (source_label, source_kind) = source_meta(cfg.as_ref(), &r.source_id);
        soil.push(SoilSensor {
            id,
            channel_label: channel_label(&r.key),
            source_id: r.source_id.clone(),
            source_label,
            source_kind,
            moisture_pct: Some(r.value),
            age_s: (now - r.epoch).max(0),
            battery_pct,
            temp_f,
            ec,
            bound_zone_slug: None,
            bound_zone_name: None,
        });
    }

    // HA soil entities (no native siblings; `ha:` ids).
    for d in discover_ha().await.into_iter().filter(|d| d.role == "soil") {
        soil.push(SoilSensor {
            id: d.id,
            channel_label: d.label.clone(),
            source_id: "home_assistant".into(),
            source_label: "Home Assistant".into(),
            source_kind: "home_assistant".into(),
            moisture_pct: d.current_pct,
            age_s: d.age_s,
            battery_pct: None,
            temp_f: None,
            ec: None,
            bound_zone_slug: None,
            bound_zone_name: None,
        });
    }

    // Resolve zone bindings: a zone references a sensor by an id string in
    // ZoneConfig.soil_sensor_id; match it byte-for-byte against each
    // sensor's id so the binding round-trips.
    if let Some(cfg) = cfg.as_ref() {
        for s in soil.iter_mut() {
            for (slug, z) in &cfg.zones {
                if z.soil_sensor_id.as_deref() == Some(s.id.as_str()) {
                    s.bound_zone_slug = Some(slug.clone());
                    s.bound_zone_name = Some(z.display_name.clone());
                    break;
                }
            }
        }
    }

    // ----- Gateways: one per source hosting >=1 soil channel -----
    let mut gateways: Vec<GatewayEntry> = Vec::new();
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for s in &soil {
        *counts.entry(s.source_id.clone()).or_default() += 1;
    }
    // Source freshness (online) for non-HA sources, keyed by id.
    let last_seen = if let Some(cfg) = cfg.as_ref() {
        let ids: Vec<String> = cfg.sources.iter().map(|s| s.id.clone()).collect();
        store.last_seen_per_source(ids).await.unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    for (source_id, soil_count) in counts {
        let (label, kind) = source_meta(cfg.as_ref(), &source_id);
        // online: false only when we positively know it is stale (>1h since
        // last reading); true when fresh or unknown (HA, no history yet).
        let online = if source_id == "home_assistant" {
            true
        } else {
            match last_seen.get(&source_id) {
                Some(epoch) => (now - epoch) < 3600,
                None => true,
            }
        };
        gateways.push(GatewayEntry {
            source_id,
            label,
            kind,
            online,
            soil_count,
        });
    }

    // ----- Flow: one per controller advertising a flow meter -----
    let mut flow: Vec<FlowSensor> = Vec::new();
    if let Some(registry) = controllers {
        for id in registry.ids() {
            let Some(c) = registry.get(&id) else {
                continue;
            };
            // supported (CAPABLE) is the type-level capability.
            if !c.supports().flow_meter {
                continue;
            }
            // One best-effort status read yields both the presence signal
            // (CONNECTED) and the live reading (LIVE GPM). A flaky controller
            // contributes connected=false, gpm=null rather than erroring, so a
            // supported-but-unreachable controller reads as "supported, none
            // connected" instead of a phantom sensor.
            let st = c.status().await.ok();
            let connected = st.as_ref().map(|s| s.flow_connected).unwrap_or(false);
            let gpm = st.and_then(|s| s.flow_gpm);
            let kind = cfg
                .as_ref()
                .and_then(|cfg| cfg.controllers.iter().find(|e| e.id == id))
                .map(|e| controller_kind_tag(&e.controller).to_string())
                .unwrap_or_else(|| "unknown".to_string());
            flow.push(FlowSensor {
                id: format!("controller:{id}"),
                controller_id: id.clone(),
                controller_label: id.clone(),
                controller_kind: kind,
                supported: true,
                connected,
                gpm,
                // Live read: the reading is "now". Controllers expose no
                // separate measurement timestamp, so 0 when a value is
                // present, null when there is none.
                age_s: gpm.map(|_| 0),
            });
        }
    }

    InventoryResponse {
        gateways,
        soil,
        flow,
    }
}

/// Resolve a source id to its (display label, kind tag). Falls back to the
/// id as the label and "unknown" as the kind when the source is not in the
/// loaded config (e.g. a channel recorded by a since-removed source).
fn source_meta(cfg: Option<&crate::config::Config>, source_id: &str) -> (String, String) {
    if source_id == "home_assistant" {
        return ("Home Assistant".into(), "home_assistant".into());
    }
    match cfg.and_then(|c| c.sources.iter().find(|s| s.id == source_id)) {
        Some(e) => (e.id.clone(), source_kind_tag(&e.source).to_string()),
        None => (source_id.to_string(), "unknown".to_string()),
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    use crate::config::schema::{
        Config, ControllerEntry, ControllerKind, EcowittGwPollConfig, GrassSpecies,
        OpenSprinklerDirectConfig, SoilTexture, SourceEntry, SourceKind, SprinklerType, ZoneConfig,
    };
    use crate::persistence::runner;
    use crate::persistence::sensor_history::Reading;
    use crate::ports::irrigation_controller::{
        ControllerCaps, ControllerResult, ControllerStatus, IrrigationController, RunHandle,
        RunRecord,
    };
    use async_trait::async_trait;

    /// Minimal flow-metered controller fixture: advertises flow_meter
    /// (CAPABLE), reports whether a sensor is wired in (`connected`), and a
    /// fixed live GPM, with no network. Stands in for an OpenSprinkler at the
    /// registry layer so the test stays hermetic; the controller_kind tag still
    /// resolves to opensprinkler_direct from the config entry.
    struct FlowFake {
        id: String,
        connected: bool,
        gpm: Option<f64>,
    }

    #[async_trait]
    impl IrrigationController for FlowFake {
        fn id(&self) -> &str {
            &self.id
        }
        fn supports(&self) -> ControllerCaps {
            ControllerCaps {
                flow_meter: true,
                rain_sensor: false,
                master_valve: false,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
            }
        }
        async fn run_zone(&self, _slug: &str, _d: u32) -> ControllerResult<RunHandle> {
            unimplemented!()
        }
        async fn stop_zone(&self, _slug: &str) -> ControllerResult<()> {
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            Ok(())
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            Ok(ControllerStatus {
                reachable: true,
                master_enabled: Some(true),
                water_level_pct: Some(100.0),
                rain_sensor_tripped: Some(false),
                current_program: None,
                zone_states: Vec::new(),
                flow_gpm: self.gpm,
                flow_connected: self.connected,
                firmware: None,
            })
        }
        async fn run_history(&self, _since: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(Vec::new())
        }
    }

    async fn fresh_store() -> SensorHistoryStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        SensorHistoryStore::new(Arc::new(Mutex::new(c)))
    }

    fn zone(name: &str, soil_sensor_id: Option<&str>) -> ZoneConfig {
        ZoneConfig {
            display_name: name.to_string(),
            area_sqft: 1000.0,
            species: GrassSpecies::StAugustine,
            soil_texture: SoilTexture::SandyLoam,
            slope_pct: 0.0,
            sun_exposure: Default::default(),
            sprinkler_type: SprinklerType::Rotor,
            precip_rate_mm_hr: None,
            precip_rate_source: Default::default(),
            root_depth_mm: None,
            mad_pct_override: None,
            controller_id: "opensprinkler".to_string(),
            controller_station: "1".to_string(),
            soil_sensor_id: soil_sensor_id.map(|s| s.to_string()),
            target_min_pct_soil: 30.0,
            saturation_pct_soil: 70.0,
            photo_url: None,
            weekly_budget_in: None,
            sessions_per_week: None,
        }
    }

    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.sources.push(SourceEntry {
            id: "ecowitt_gw".to_string(),
            priority: 100,
            enabled: true,
            max_age_s: None,
            source: SourceKind::EcowittGwPoll(EcowittGwPollConfig {
                host: "192.0.2.10".to_string(),
                poll_interval_s: 30,
                soil_calibration: Default::default(),
            }),
        });
        cfg.controllers.push(ControllerEntry {
            id: "opensprinkler".to_string(),
            default: true,
            enabled: true,
            controller: ControllerKind::OpensprinklerDirect(OpenSprinklerDirectConfig {
                host: "192.0.2.11".to_string(),
                port: 80,
                password_md5: "x".to_string(),
                poll_interval_s: 10,
            }),
        });
        // Bind back_yard to soil channel 1; leave front_yard unbound.
        cfg.zones.insert(
            "back_yard".to_string(),
            zone("Back Yard", Some("source:ecowitt_gw:soilmoisture1")),
        );
        cfg.zones
            .insert("front_yard".to_string(), zone("Front Yard", None));
        cfg
    }

    #[tokio::test]
    async fn inventory_unifies_soil_flow_and_resolves_binding() {
        let store = fresh_store().await;
        let now = chrono::Utc::now().timestamp();
        // Two Ecowitt soil channels with battery/temp/EC siblings on ch1.
        for r in [
            Reading {
                epoch: now,
                source_id: "ecowitt_gw".into(),
                key: "soilmoisture1".into(),
                value: 42.0,
            },
            Reading {
                epoch: now,
                source_id: "ecowitt_gw".into(),
                key: "soilmoisture2".into(),
                value: 55.0,
            },
            Reading {
                epoch: now,
                source_id: "ecowitt_gw".into(),
                key: "soilbatt1".into(),
                value: 90.0,
            },
            Reading {
                epoch: now,
                source_id: "ecowitt_gw".into(),
                key: "soiltemp1f".into(),
                value: 71.2,
            },
            Reading {
                epoch: now,
                source_id: "ecowitt_gw".into(),
                key: "soilec1".into(),
                value: 120.0,
            },
            // A non-soil key on the same source must not become a soil entry.
            Reading {
                epoch: now,
                source_id: "ecowitt_gw".into(),
                key: "tempf".into(),
                value: 70.0,
            },
        ] {
            store.insert(r).await.unwrap();
        }

        let registry = ControllerRegistry::new();
        registry.set(vec![(
            Arc::new(FlowFake {
                id: "opensprinkler".into(),
                connected: true,
                gpm: Some(8.4),
            }) as Arc<dyn IrrigationController>,
            true,
        )]);

        let inv = build_inventory(store, Some(test_config()), Some(&registry)).await;

        // ----- soil -----
        assert_eq!(inv.soil.len(), 2, "two soil channels, no non-soil keys");
        let ch1 = inv
            .soil
            .iter()
            .find(|s| s.id == "source:ecowitt_gw:soilmoisture1")
            .expect("ch1 present with canonical id");
        assert_eq!(ch1.channel_label, "Channel 1");
        assert_eq!(ch1.source_id, "ecowitt_gw");
        assert_eq!(ch1.source_kind, "ecowitt_gw_poll");
        assert_eq!(ch1.moisture_pct, Some(42.0));
        assert_eq!(ch1.battery_pct, Some(90.0));
        assert_eq!(ch1.temp_f, Some(71.2));
        assert_eq!(ch1.ec, Some(120.0));
        assert!(ch1.age_s >= 0);
        // Binding resolves by id match against ZoneConfig.soil_sensor_id.
        assert_eq!(ch1.bound_zone_slug.as_deref(), Some("back_yard"));
        assert_eq!(ch1.bound_zone_name.as_deref(), Some("Back Yard"));

        let ch2 = inv
            .soil
            .iter()
            .find(|s| s.id == "source:ecowitt_gw:soilmoisture2")
            .expect("ch2 present");
        // ch2 has no siblings and no zone binding.
        assert_eq!(ch2.battery_pct, None);
        assert_eq!(ch2.temp_f, None);
        assert_eq!(ch2.ec, None);
        assert_eq!(ch2.bound_zone_slug, None);
        assert_eq!(ch2.bound_zone_name, None);

        // ----- gateways -----
        assert_eq!(inv.gateways.len(), 1, "one source hosts soil");
        let gw = &inv.gateways[0];
        assert_eq!(gw.source_id, "ecowitt_gw");
        assert_eq!(gw.kind, "ecowitt_gw_poll");
        assert_eq!(gw.soil_count, 2);
        assert!(gw.online, "fresh reading -> online");

        // ----- flow -----
        // A controller that is CAPABLE (supported), has a sensor CONNECTED,
        // and is reading LIVE flow.
        assert_eq!(inv.flow.len(), 1, "one flow-metered controller");
        let f = &inv.flow[0];
        assert_eq!(f.id, "controller:opensprinkler");
        assert_eq!(f.controller_id, "opensprinkler");
        assert_eq!(f.controller_kind, "opensprinkler_direct");
        assert!(f.supported, "OpenSprinkler type supports flow");
        assert!(f.connected, "fixture reports a flow sensor wired in");
        assert_eq!(f.gpm, Some(8.4));
        assert_eq!(f.age_s, Some(0));
    }

    #[tokio::test]
    async fn inventory_flow_supported_but_not_connected() {
        // The bug case: a controller that SUPPORTS flow but has no sensor
        // wired in must read supported:true, connected:false, gpm:null, so the
        // UI shows "supported, none connected" instead of a phantom idle meter.
        let store = fresh_store().await;
        let registry = ControllerRegistry::new();
        registry.set(vec![(
            Arc::new(FlowFake {
                id: "opensprinkler".into(),
                connected: false,
                gpm: None,
            }) as Arc<dyn IrrigationController>,
            true,
        )]);

        let inv = build_inventory(store, Some(test_config()), Some(&registry)).await;

        assert_eq!(inv.flow.len(), 1, "still listed: the type is capable");
        let f = &inv.flow[0];
        assert!(f.supported, "type supports flow");
        assert!(!f.connected, "no sensor wired -> not connected");
        assert_eq!(f.gpm, None, "no live reading");
        assert_eq!(f.age_s, None, "no reading -> no age");
    }

    #[tokio::test]
    async fn inventory_empty_without_handles() {
        // No config, no controllers, no readings -> all-empty, never panics.
        let store = fresh_store().await;
        let inv = build_inventory(store, None, None).await;
        assert!(inv.soil.is_empty());
        assert!(inv.gateways.is_empty());
        assert!(inv.flow.is_empty());
    }
}
