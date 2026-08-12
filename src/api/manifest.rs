// /api/v1/sensors/manifest, declarative inventory of every entity
// LocalSky produces. The HACS integration consumes this so it can
// create matching HA entities WITHOUT a hardcoded sensor list, adding
// a new source/zone in LocalSky surfaces in HA automatically.
//
// Schema version is bumped when descriptor shape changes (Music-Assistant
// pattern: integration declares a min schema version; LocalSky declares
// the served version; clients warn if the gap is too wide).

use std::sync::Arc;

use axum::{extract::State, response::Json, routing::get, Router};
use serde::{Deserialize, Serialize};

use crate::config::FileConfigStore;
use crate::ha::IrrigationStore;
use crate::ports::config_store::ConfigStore;

/// Manifest schema version. SemVer-style. Bumped on shape-breaking
/// changes only; additive fields use the same major.
/// 1.3 (additive): optional `group` sub-device hint on descriptors, the
/// force_overrode_guard sensor, and capability-gated flow/leaf publishing.
pub const MANIFEST_SCHEMA_VERSION: &str = "1.3";

/// One HA entity descriptor. HACS reads `platform` + `id` + `name` +
/// `snapshot`/`path` to know where to fetch state from the coordinator,
/// and `unit`/`device_class`/`state_class`/`icon` for HA UI metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EntityDescriptor {
    /// HA platform: "sensor", "binary_sensor", "number", "valve",
    /// "weather". HACS dispatches to the matching platform setup.
    pub platform: &'static str,
    /// Stable id within LocalSky. HACS concatenates with entry_id for
    /// the HA unique_id.
    pub id: String,
    /// Friendly display name. HA's `_attr_has_entity_name` style: this
    /// is the entity-name portion that appears after the device name.
    pub name: String,
    /// Which snapshot to read state from: "tempest" | "irrigation"
    /// | "forecast". Maps to coordinator.data[snapshot].
    pub snapshot: &'static str,
    /// Dot path within the snapshot to extract the value. Each entry
    /// is a key; HACS walks dict by dict.
    pub path: Vec<String>,
    /// Native unit of measurement (HA UnitOf*). None for stateful
    /// strings (e.g. verdict, weather condition).
    pub unit: Option<&'static str>,
    /// HA device_class string (e.g. "temperature", "humidity",
    /// "wind_speed", "duration"). Drives icon + statistics.
    pub device_class: Option<&'static str>,
    /// HA state_class (e.g. "measurement", "total_increasing"). Drives
    /// long-term statistics collection.
    pub state_class: Option<&'static str>,
    /// MDI icon override when no device_class default fits.
    pub icon: Option<&'static str>,
    /// When set, HACS interprets `path` as relative to the zone object
    /// located in `snapshot.zones[]` where `zone.slug == zone_slug`.
    /// Lets a single descriptor template apply per-zone without forcing
    /// the snapshot to be a dict-keyed-by-slug map (zones[] is a list).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_slug: Option<String>,
    /// Sub-device grouping hint for the HA integration ("forecast" today).
    /// When present, the integration files the entity under this sub-device
    /// instead of inferring one from `snapshot`. Exists because the forecast
    /// scalars ride snapshot="irrigation" (their values live on the irrigation
    /// snapshot's forecast block), which made the integration file them under
    /// the Irrigation device and left the Forecast device permanently empty.
    /// Absent = integration infers from `snapshot` (pre-1.15 behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<&'static str>,
}

/// Top-level manifest. Returned by GET /api/v1/sensors/manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// SemVer of the descriptor shape. HACS compares against the
    /// version it was built for and falls back to its hardcoded
    /// sensor list if the major doesn't match.
    pub schema_version: &'static str,
    /// Flat list of entities. HACS iterates this on setup to register
    /// every entity, and again whenever the source set changes.
    pub entities: Vec<EntityDescriptor>,
}

/// Router state: the live irrigation snapshot (per-zone entities + flow
/// capability) plus the config store (which SOURCE capabilities exist, for the
/// flow/leaf publish gates). The config is loaded per fetch; this endpoint is
/// hit only at integration setup/reload and on zone-set changes, so a TOML
/// parse per call is trivial, and it always reflects the CURRENT config
/// rather than a boot-time copy.
#[derive(Clone)]
pub struct ManifestState {
    pub irrigation: Arc<IrrigationStore>,
    pub cfg: Arc<FileConfigStore>,
}

pub fn router(irrigation: Arc<IrrigationStore>, cfg: Arc<FileConfigStore>) -> Router {
    Router::new()
        .route("/sensors/manifest", get(manifest))
        .with_state(ManifestState { irrigation, cfg })
}

/// Epoch of the most recent manifest fetch. Only the Home Assistant
/// integration calls this endpoint, so it doubles as an "HA integration
/// is alive" signal surfaced by /api/v1/health's `ha` block.
pub static LAST_MANIFEST_FETCH_EPOCH: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

async fn manifest(State(state): State<ManifestState>) -> Json<Manifest> {
    LAST_MANIFEST_FETCH_EPOCH.store(
        chrono::Utc::now().timestamp(),
        std::sync::atomic::Ordering::Relaxed,
    );
    let snap = state.irrigation.snapshot();
    let mut entities = Vec::new();

    // Which reading capabilities the CONFIGURED sources provide, for the
    // flow/leaf publish gates. Fail-OPEN on a config load error (publish as
    // before) so a transient config problem never silently drops entities an
    // install genuinely has.
    let (cfg_provides_flow, cfg_provides_leaf) = match state.cfg.load().await {
        Ok(cfg) => {
            let mut flow = false;
            let mut leaf = false;
            for entry in cfg.sources.iter().filter(|s| s.enabled) {
                for f in crate::runtime::source_field_names(&cfg, entry) {
                    match f {
                        "flow_gpm" => flow = true,
                        "leaf_wetness_pct" => leaf = true,
                        _ => {}
                    }
                }
                if flow && leaf {
                    break;
                }
            }
            (flow, leaf)
        }
        Err(crate::ports::config_store::ConfigStoreError::NotFound) => (false, false),
        Err(_) => (true, true),
    };
    let has_flow = snap.flow_meter || cfg_provides_flow;
    let has_leaf = cfg_provides_leaf;

    // A live local station (Tempest serial present) gates the station-only
    // scalars (battery) so a cloud-only / Ecowitt install does not publish a
    // phantom 0% device_class=battery sensor. Configured irrigation gates the
    // irrigation entities so a weather-only install does not get phantom
    // verdict/override/threshold sliders that error on write. The irrigation
    // snapshot carries only zones (no controller list); a configured controller
    // always yields at least one zone, so non-empty zones is the presence test.
    let has_station = !snap.station_serial.is_empty();
    let has_irrigation = !snap.zones.is_empty();

    push_tempest_weather(&mut entities, has_station);
    push_irrigation_meta(&mut entities, has_irrigation);
    push_thresholds(&mut entities, has_irrigation);
    push_forecast(&mut entities);
    push_provenance_and_flow(&mut entities, has_flow, has_leaf);
    push_zone_entities(&mut entities, &snap.zones);
    push_diagnostics(&mut entities, has_irrigation);

    Json(Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        entities,
    })
}

// ─────────────────────────────────────────────────────────────────────
// Tempest weather scalars (snapshot=tempest)
// ─────────────────────────────────────────────────────────────────────
fn push_tempest_weather(out: &mut Vec<EntityDescriptor>, has_station: bool) {
    let defs: &[(
        &str,
        &str,
        &str,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
    )] = &[
        // (id, name, field, unit, device_class, state_class, icon)
        (
            "air_temp_f",
            "Air temperature",
            "air_temp_f",
            Some("°F"),
            Some("temperature"),
            Some("measurement"),
            None,
        ),
        (
            "feels_like_f",
            "Feels like",
            "feels_like_f",
            Some("°F"),
            Some("temperature"),
            Some("measurement"),
            None,
        ),
        (
            "dew_point_f",
            "Dew point",
            "dew_point_f",
            Some("°F"),
            Some("temperature"),
            Some("measurement"),
            None,
        ),
        (
            "wet_bulb_f",
            "Wet bulb",
            "wet_bulb_f",
            Some("°F"),
            Some("temperature"),
            Some("measurement"),
            None,
        ),
        (
            "rh_pct",
            "Humidity",
            "rh_pct",
            Some("%"),
            Some("humidity"),
            Some("measurement"),
            None,
        ),
        (
            "pressure_inhg",
            "Pressure",
            "pressure_inhg",
            Some("inHg"),
            Some("pressure"),
            Some("measurement"),
            None,
        ),
        (
            "wind_avg_mph",
            "Wind speed",
            "wind_avg_mph",
            Some("mph"),
            Some("wind_speed"),
            Some("measurement"),
            None,
        ),
        (
            "wind_gust_mph",
            "Wind gust",
            "wind_gust_mph",
            Some("mph"),
            Some("wind_speed"),
            Some("measurement"),
            None,
        ),
        (
            "wind_lull_mph",
            "Wind lull",
            "wind_lull_mph",
            Some("mph"),
            Some("wind_speed"),
            Some("measurement"),
            None,
        ),
        (
            "wind_dir_deg",
            "Wind direction",
            "wind_dir_deg",
            Some("°"),
            None,
            Some("measurement"),
            Some("mdi:compass"),
        ),
        (
            "solar_w_m2",
            "Solar irradiance",
            "solar_w_m2",
            Some("W/m²"),
            Some("irradiance"),
            Some("measurement"),
            None,
        ),
        (
            "uv_index",
            "UV index",
            "uv_index",
            None,
            None,
            Some("measurement"),
            Some("mdi:weather-sunny-alert"),
        ),
        (
            "illuminance_lx",
            "Illuminance",
            "illuminance_lx",
            Some("lx"),
            Some("illuminance"),
            Some("measurement"),
            None,
        ),
        (
            "rain_in_today",
            "Rain today",
            "rain_in_today",
            Some("in"),
            Some("precipitation"),
            Some("total_increasing"),
            None,
        ),
        (
            "rain_in_last_min",
            "Rain last minute",
            "rain_in_last_min",
            Some("in"),
            Some("precipitation"),
            Some("measurement"),
            None,
        ),
        (
            "rain_intensity_in_hr",
            "Rain intensity",
            "rain_intensity_in_hr",
            Some("in/h"),
            Some("precipitation_intensity"),
            Some("measurement"),
            None,
        ),
        (
            "lightning_strikes_last_hour",
            "Lightning strikes (1h)",
            "lightning_strikes_last_hour",
            None,
            None,
            Some("measurement"),
            Some("mdi:flash"),
        ),
        (
            "lightning_avg_dist_mi",
            "Lightning avg distance",
            "lightning_avg_dist_mi",
            Some("mi"),
            Some("distance"),
            Some("measurement"),
            Some("mdi:flash"),
        ),
        // The distance that persists between strikes. The average above is
        // per reporting interval and goes unknown in a quiet minute, so an
        // automation asking "how far away is the storm" wants this one.
        (
            "last_strike_distance_mi",
            "Lightning last strike distance",
            "last_strike_distance_mi",
            Some("mi"),
            Some("distance"),
            Some("measurement"),
            Some("mdi:flash"),
        ),
    ];
    for (id, name, field, unit, device_class, state_class, icon) in defs {
        out.push(EntityDescriptor {
            platform: "sensor",
            id: (*id).to_string(),
            name: (*name).to_string(),
            snapshot: "tempest",
            path: vec![(*field).to_string()],
            unit: *unit,
            device_class: *device_class,
            state_class: *state_class,
            icon: *icon,
            zone_slug: None,
            group: None,
        });
    }
    // Battery is a Tempest-specific live-station scalar. On a cloud-only or
    // Ecowitt/Davis/MQTT install there is no Tempest battery: publishing it
    // surfaced a phantom device_class=battery sensor reading 0% in HA (a fake
    // "dead battery"). Gate it on a live local station actually being present
    // (non-empty Tempest serial). When present, label it source-neutrally as
    // "Station battery" rather than hardcoding "Tempest".
    if has_station {
        out.push(EntityDescriptor {
            platform: "sensor",
            id: "battery_pct".to_string(),
            name: "Station battery".to_string(),
            snapshot: "tempest",
            path: vec!["battery_pct".to_string()],
            unit: Some("%"),
            device_class: Some("battery"),
            state_class: Some("measurement"),
            icon: None,
            zone_slug: None,
            group: None,
        });
    }
}

// ─────────────────────────────────────────────────────────────────────
// Irrigation top-level (snapshot=irrigation)
// ─────────────────────────────────────────────────────────────────────
fn push_irrigation_meta(out: &mut Vec<EntityDescriptor>, has_irrigation: bool) {
    // A weather-only install (no controllers, no zones) has no irrigation to
    // verdict, override, or threshold. Publishing these surfaced phantom
    // verdict/override sensors and number sliders in HA that error on write
    // (nothing to actuate). Gate them on irrigation actually being configured.
    if !has_irrigation {
        return;
    }
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "irrigation_verdict".into(),
        name: "Irrigation verdict".into(),
        snapshot: "irrigation",
        path: vec!["skip_check".into(), "verdict".into()],
        unit: None,
        device_class: None,
        state_class: None,
        icon: Some("mdi:water-check"),
        zone_slug: None,
        group: None,
    });
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "irrigation_reason".into(),
        name: "Irrigation reason".into(),
        snapshot: "irrigation",
        path: vec!["skip_check".into(), "reason".into()],
        unit: None,
        device_class: None,
        state_class: None,
        icon: Some("mdi:tooltip-text"),
        zone_slug: None,
        group: None,
    });
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "heat_multiplier".into(),
        name: "Heat multiplier".into(),
        snapshot: "irrigation",
        path: vec!["forecast".into(), "heat_multiplier".into()],
        unit: None,
        device_class: None,
        state_class: Some("measurement"),
        icon: Some("mdi:thermometer-alert"),
        zone_slug: None,
        group: None,
    });
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "water_level_pct".into(),
        name: "Water level".into(),
        snapshot: "irrigation",
        path: vec!["water_level_pct".into()],
        unit: Some("%"),
        device_class: None,
        state_class: Some("measurement"),
        icon: Some("mdi:water-percent"),
        zone_slug: None,
        group: None,
    });
    // Sticky global override (auto/skip/run), read-only in HA. Set it from the
    // LocalSky UI; exposed here so HA automations can react ("notify when
    // irrigation is force-skipped"). Per-zone overrides stay UI-only.
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "global_override".into(),
        name: "Override".into(),
        snapshot: "irrigation",
        path: vec!["global_override".into()],
        unit: None,
        device_class: None,
        state_class: None,
        icon: Some("mdi:tune"),
        zone_slug: None,
        group: None,
    });
}

// ─────────────────────────────────────────────────────────────────────
// User-tunable thresholds (number entities, action: set_threshold)
// ─────────────────────────────────────────────────────────────────────
fn push_thresholds(out: &mut Vec<EntityDescriptor>, has_irrigation: bool) {
    // Threshold number entities (max wind / min temp / rain skip) only mean
    // something when there is irrigation to skip. A weather-only install gets
    // none, so HA does not render sliders that write to a no-op skip-check.
    if !has_irrigation {
        return;
    }
    let defs: &[(&str, &str, &str, Option<&'static str>, Option<&'static str>)] = &[
        (
            "max_wind_mph",
            "Max wind",
            "max_wind_mph",
            Some("mph"),
            Some("mdi:weather-windy"),
        ),
        (
            "min_temp_f",
            "Min temp",
            "min_temp_f",
            Some("°F"),
            Some("mdi:thermometer-low"),
        ),
        (
            "rain_skip_in",
            "Rain skip",
            "rain_skip_in",
            Some("in"),
            Some("mdi:weather-pouring"),
        ),
    ];
    for (id, name, field, unit, icon) in defs {
        out.push(EntityDescriptor {
            zone_slug: None,
            group: None,
            platform: "number",
            id: (*id).to_string(),
            name: (*name).to_string(),
            snapshot: "irrigation",
            path: vec!["skip_check".into(), (*field).to_string()],
            unit: *unit,
            device_class: None,
            state_class: None,
            icon: *icon,
        });
    }
}

// ─────────────────────────────────────────────────────────────────────
// Forecast scalars (snapshot=forecast)
// ─────────────────────────────────────────────────────────────────────
fn push_forecast(out: &mut Vec<EntityDescriptor>) {
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "eto_today_mm".into(),
        name: "ET₀ today".into(),
        snapshot: "irrigation",
        path: vec!["forecast".into(), "eto_today_mm".into()],
        unit: Some("mm"),
        device_class: None,
        state_class: Some("measurement"),
        icon: Some("mdi:water-sync"),
        zone_slug: None,
        group: Some("forecast"),
    });
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "days_since_significant_rain".into(),
        name: "Days since rain".into(),
        snapshot: "irrigation",
        path: vec!["forecast".into(), "days_since_significant_rain".into()],
        unit: Some("d"),
        device_class: None,
        state_class: Some("measurement"),
        icon: Some("mdi:weather-sunny"),
        zone_slug: None,
        group: Some("forecast"),
    });
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "rain_tomorrow_prob_pct".into(),
        name: "Rain tomorrow probability".into(),
        snapshot: "irrigation",
        path: vec!["forecast".into(), "rain_tomorrow_prob_pct".into()],
        unit: Some("%"),
        device_class: None,
        state_class: Some("measurement"),
        icon: Some("mdi:weather-rainy"),
        zone_slug: None,
        group: Some("forecast"),
    });
    // Forecast peak wind gust today (Open-Meteo). The Tempest is wind-shadowed
    // and under-reads gusts, so the high-wind alert keys on this instead.
    // Consumed by HA's high_wind_alert (fires >35 mph, 5-min debounce).
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "wind_gust_forecast".into(),
        name: "Wind gust forecast".into(),
        snapshot: "irrigation",
        path: vec!["forecast".into(), "wind_gust_today_mph".into()],
        unit: Some("mph"),
        device_class: Some("wind_speed"),
        state_class: Some("measurement"),
        icon: Some("mdi:weather-windy"),
        zone_slug: None,
        group: Some("forecast"),
    });
    // Current probability of precipitation, merged from whichever forecast
    // source owns WeatherField::Pop (NWS, Pirate Weather, ...). It rides the
    // CONDITIONS snapshot rather than the forecast block because the merge bus
    // writes it into `Snapshot.pop_pct` alongside the live readings, but it is
    // a forecast quantity so it is grouped with the forecast device.
    //
    // Distinct from `rain_tomorrow_prob_pct`, which is tomorrow's daily
    // figure: this is the chance right now. Useful on a dashboard next to the
    // conditions, and as a cheap gate for automations that only need "is rain
    // likely" without pulling the whole hourly forecast.
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "pop_pct".into(),
        name: "Precipitation probability".into(),
        snapshot: "tempest",
        path: vec!["pop_pct".into()],
        unit: Some("%"),
        device_class: None,
        state_class: Some("measurement"),
        icon: Some("mdi:weather-rainy"),
        zone_slug: None,
        group: Some("forecast"),
    });
}

// ─────────────────────────────────────────────────────────────────────
// Source provenance + generalized flow/leaf readings (Phase D alignment).
// These ride the existing snapshots, so the manifest-driven HACS integration
// surfaces them with no Python change. Provenance answers "which source drives
// my conditions/forecast"; flow + leaf-wetness expose the generalized readings
// that any source can now provide.
// ─────────────────────────────────────────────────────────────────────
fn push_provenance_and_flow(out: &mut Vec<EntityDescriptor>, has_flow: bool, has_leaf: bool) {
    // Which source currently drives current conditions (a string sensor).
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "conditions_source".into(),
        name: "Conditions source".into(),
        snapshot: "tempest",
        path: vec!["source_label".into()],
        unit: None,
        device_class: None,
        state_class: None,
        icon: Some("mdi:transit-connection-variant"),
        zone_slug: None,
        group: None,
    });
    // Which source currently drives the forecast.
    out.push(EntityDescriptor {
        platform: "sensor",
        id: "forecast_source".into(),
        name: "Forecast source".into(),
        snapshot: "irrigation",
        path: vec!["forecast".into(), "forecast_source_label".into()],
        unit: None,
        device_class: None,
        state_class: None,
        icon: Some("mdi:weather-partly-cloudy"),
        zone_slug: None,
        group: Some("forecast"),
    });
    // Flow rate + cumulative flow today (a flow meter on a controller or a
    // standalone pulse meter). Gated on a flow CAPABILITY actually being
    // present (a controller reporting a meter, or a configured source that
    // provides flow_gpm): unconditional publishing gave every meterless
    // install two phantom always-0 water sensors.
    if has_flow {
        out.push(EntityDescriptor {
            platform: "sensor",
            id: "flow_gpm".into(),
            name: "Flow rate".into(),
            snapshot: "tempest",
            path: vec!["flow_gpm".into()],
            unit: Some("gal/min"),
            device_class: Some("volume_flow_rate"),
            state_class: Some("measurement"),
            icon: Some("mdi:water-pump"),
            zone_slug: None,
            group: None,
        });
        out.push(EntityDescriptor {
            platform: "sensor",
            id: "flow_total_gal_today".into(),
            name: "Flow total today".into(),
            snapshot: "tempest",
            path: vec!["flow_total_gal_today".into()],
            unit: Some("gal"),
            device_class: Some("water"),
            state_class: Some("total_increasing"),
            icon: Some("mdi:water"),
            zone_slug: None,
            group: None,
        });
    }
    // Leaf wetness (Davis WLL soil/leaf, Ecowitt WH35, agronomic probes).
    // Same phantom-sensor rule: only when a configured source provides it.
    if has_leaf {
        out.push(EntityDescriptor {
            platform: "sensor",
            id: "leaf_wetness_pct".into(),
            name: "Leaf wetness".into(),
            snapshot: "tempest",
            path: vec!["leaf_wetness_pct".into()],
            unit: Some("%"),
            device_class: None,
            state_class: Some("measurement"),
            icon: Some("mdi:leaf"),
            zone_slug: None,
            group: None,
        });
    }
}

// ─────────────────────────────────────────────────────────────────────
// Per-zone entities (one set per zone, dynamic from current snapshot)
// ─────────────────────────────────────────────────────────────────────
fn push_zone_entities(out: &mut Vec<EntityDescriptor>, zones: &[crate::ha::snapshot::ZoneState]) {
    for zone in zones {
        let slug = &zone.slug;
        let pretty = if zone.name.is_empty() {
            slug.clone()
        } else {
            zone.name.clone()
        };

        // Per-zone entities use zone_slug + path-relative-to-zone-object.
        // HACS finds zones[].slug == zone_slug, then walks `path` inside
        // that object. Avoids the snapshot being a list-of-zones blocking
        // direct path traversal.

        // Valve entity, open/close maps to run/stop irrigation action.
        out.push(EntityDescriptor {
            platform: "valve",
            id: slug.to_string(),
            name: pretty.clone(),
            snapshot: "irrigation",
            path: vec!["running".into()],
            device_class: Some("water"),
            icon: Some("mdi:sprinkler-variant"),
            zone_slug: Some(slug.clone()),
            group: None,
            ..Default::default()
        });

        // Soil bucket (LocalSky engine state in mm)
        out.push(EntityDescriptor {
            platform: "sensor",
            id: format!("{slug}_soil_bucket"),
            name: format!("{pretty} soil bucket"),
            snapshot: "irrigation",
            path: vec!["bucket_mm".into()],
            unit: Some("mm"),
            state_class: Some("measurement"),
            icon: Some("mdi:water-percent"),
            zone_slug: Some(slug.clone()),
            group: None,
            ..Default::default()
        });

        // Soil moisture %, the live calibrated probe reading the engine
        // decides on (native Ecowitt poll or HA bridge). Lives in
        // skip_check.soil_<slug>_pct (top-level path, not zone-relative),
        // so no zone_slug. `null` when the probe is offline → HA shows the
        // sensor unavailable, which is correct.
        out.push(EntityDescriptor {
            platform: "sensor",
            id: format!("{slug}_soil_moisture"),
            name: format!("{pretty} soil moisture"),
            snapshot: "irrigation",
            path: vec!["skip_check".into(), format!("soil_{slug}_pct")],
            unit: Some("%"),
            device_class: Some("moisture"),
            state_class: Some("measurement"),
            ..Default::default()
        });

        // Native soil temperature (°F), LocalSky polls the gateway directly,
        // so HA no longer needs the ecowitt2mqtt MQTT entity. zone_slug +
        // path-into-zone reads zones[].soil_temp_f.
        out.push(EntityDescriptor {
            platform: "sensor",
            id: format!("{slug}_soil_temperature"),
            name: format!("{pretty} soil temperature"),
            snapshot: "irrigation",
            path: vec!["soil_temp_f".into()],
            unit: Some("°F"),
            device_class: Some("temperature"),
            state_class: Some("measurement"),
            zone_slug: Some(slug.clone()),
            group: None,
            ..Default::default()
        });

        // Native soil EC (µS/cm), salinity / fertilizer drift. Display-only.
        out.push(EntityDescriptor {
            platform: "sensor",
            id: format!("{slug}_soil_ec"),
            name: format!("{pretty} soil EC"),
            snapshot: "irrigation",
            path: vec!["soil_ec".into()],
            unit: Some("µS/cm"),
            state_class: Some("measurement"),
            icon: Some("mdi:flash-outline"),
            zone_slug: Some(slug.clone()),
            group: None,
            ..Default::default()
        });

        // Probe battery (%, from the Ecowitt 0-5 level scaled ×20).
        out.push(EntityDescriptor {
            platform: "sensor",
            id: format!("{slug}_soil_battery"),
            name: format!("{pretty} soil battery"),
            snapshot: "irrigation",
            path: vec!["soil_battery_pct".into()],
            unit: Some("%"),
            device_class: Some("battery"),
            state_class: Some("measurement"),
            zone_slug: Some(slug.clone()),
            group: None,
            ..Default::default()
        });

        // Planned next run duration
        out.push(EntityDescriptor {
            platform: "sensor",
            id: format!("{slug}_planned_run"),
            name: format!("{pretty} planned run"),
            snapshot: "irrigation",
            path: vec!["planned_run_seconds".into()],
            unit: Some("s"),
            device_class: Some("duration"),
            state_class: Some("measurement"),
            zone_slug: Some(slug.clone()),
            group: None,
            ..Default::default()
        });

        // Today's accumulated run minutes
        out.push(EntityDescriptor {
            platform: "sensor",
            id: format!("{slug}_run_today"),
            name: format!("{pretty} run today"),
            snapshot: "irrigation",
            path: vec!["today_run_minutes".into()],
            unit: Some("min"),
            device_class: Some("duration"),
            state_class: Some("total_increasing"),
            zone_slug: Some(slug.clone()),
            group: None,
            ..Default::default()
        });

        // Running binary_sensor
        out.push(EntityDescriptor {
            platform: "binary_sensor",
            id: format!("{slug}_running"),
            name: format!("{pretty} running"),
            snapshot: "irrigation",
            path: vec!["running".into()],
            device_class: Some("running"),
            zone_slug: Some(slug.clone()),
            group: None,
            ..Default::default()
        });
    }
}

// ─────────────────────────────────────────────────────────────────────
// Diagnostic / connectivity binary sensors
// ─────────────────────────────────────────────────────────────────────
fn push_diagnostics(out: &mut Vec<EntityDescriptor>, has_irrigation: bool) {
    // HA connectivity is relevant to any HA-integrated install (weather-only
    // included), so it is always published.
    out.push(EntityDescriptor {
        platform: "binary_sensor",
        id: "ha_reachable".into(),
        name: "HA reachable".into(),
        snapshot: "irrigation",
        path: vec!["ha_reachable".into()],
        unit: None,
        device_class: Some("connectivity"),
        state_class: None,
        icon: None,
        zone_slug: None,
        group: None,
    });
    // "Irrigation suspended" tracks the IU/skip-check suspension state, which
    // only exists when irrigation is configured. On a weather-only install it
    // was a permanently-OFF device_class=problem sensor (a phantom that can
    // never trip). Gate it on irrigation actually being present.
    if has_irrigation {
        out.push(EntityDescriptor {
            platform: "binary_sensor",
            id: "iu_suspended".into(),
            name: "Irrigation suspended".into(),
            snapshot: "irrigation",
            path: vec!["iu_suspended".into()],
            unit: None,
            device_class: Some("problem"),
            state_class: None,
            icon: None,
            zone_slug: None,
            group: None,
        });
        // The forced-run safety signal: when a sticky Force override ran past a
        // hard guard (freeze / wind / restriction), this names the guard it
        // overrode; null otherwise. Published so an HA automation can alert
        // the moment a Force override suppresses a real protection, which is
        // exactly the state a household wants to know about.
        out.push(EntityDescriptor {
            platform: "sensor",
            id: "force_overrode_guard".into(),
            name: "Force override guard".into(),
            snapshot: "irrigation",
            path: vec!["force_overrode_guard".into()],
            unit: None,
            device_class: None,
            state_class: None,
            icon: Some("mdi:shield-alert"),
            zone_slug: None,
            group: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_is_semver() {
        let parts: Vec<&str> = MANIFEST_SCHEMA_VERSION.split('.').collect();
        assert_eq!(parts.len(), 2, "expected MAJOR.MINOR for schema_version");
    }

    #[test]
    fn weather_entities_present() {
        let mut out = Vec::new();
        push_tempest_weather(&mut out, true);
        // Minimum set HACS needs to render a weather entity
        let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
        for required in ["air_temp_f", "rh_pct", "wind_avg_mph", "pressure_inhg"] {
            assert!(ids.contains(&required), "missing weather field: {required}");
        }
    }

    #[test]
    fn battery_gated_on_station_presence() {
        // A live station present -> the (source-neutral) battery sensor is
        // published; a cloud-only / Ecowitt install (no Tempest serial) omits
        // it so HA never shows a phantom 0% battery.
        let mut with_station = Vec::new();
        push_tempest_weather(&mut with_station, true);
        assert!(with_station.iter().any(|e| e.id == "battery_pct"));

        let mut cloud_only = Vec::new();
        push_tempest_weather(&mut cloud_only, false);
        assert!(!cloud_only.iter().any(|e| e.id == "battery_pct"));
    }

    #[test]
    fn irrigation_entities_gated_on_irrigation_present() {
        // A weather-only install (no zones/controllers) must not publish the
        // irrigation verdict/override sensors, threshold sliders, or the
        // IU-suspended problem sensor.
        let mut weather_only = Vec::new();
        push_irrigation_meta(&mut weather_only, false);
        push_thresholds(&mut weather_only, false);
        push_diagnostics(&mut weather_only, false);
        let ids: Vec<&str> = weather_only.iter().map(|e| e.id.as_str()).collect();
        assert!(!ids.contains(&"irrigation_verdict"));
        assert!(!ids.contains(&"global_override"));
        assert!(!ids.contains(&"max_wind_mph"));
        assert!(!ids.contains(&"iu_suspended"));
        // HA connectivity is always published, irrigation or not.
        assert!(ids.contains(&"ha_reachable"));

        // With irrigation configured they all return.
        let mut with_irrigation = Vec::new();
        push_irrigation_meta(&mut with_irrigation, true);
        push_thresholds(&mut with_irrigation, true);
        push_diagnostics(&mut with_irrigation, true);
        let ids: Vec<&str> = with_irrigation.iter().map(|e| e.id.as_str()).collect();
        for required in ["irrigation_verdict", "max_wind_mph", "iu_suspended"] {
            assert!(
                ids.contains(&required),
                "missing irrigation entity: {required}"
            );
        }
    }

    #[test]
    fn flow_and_leaf_are_capability_gated_and_forecast_carries_group() {
        // No flow capability + no leaf provider: neither phantom sensor.
        let mut none = Vec::new();
        push_provenance_and_flow(&mut none, false, false);
        let ids: Vec<&str> = none.iter().map(|e| e.id.as_str()).collect();
        assert!(!ids.contains(&"flow_gpm"));
        assert!(!ids.contains(&"flow_total_gal_today"));
        assert!(!ids.contains(&"leaf_wetness_pct"));
        // Provenance strings always publish.
        assert!(ids.contains(&"conditions_source"));
        assert!(ids.contains(&"forecast_source"));

        // With the capabilities present they all publish.
        let mut both = Vec::new();
        push_provenance_and_flow(&mut both, true, true);
        let ids: Vec<&str> = both.iter().map(|e| e.id.as_str()).collect();
        for required in ["flow_gpm", "flow_total_gal_today", "leaf_wetness_pct"] {
            assert!(ids.contains(&required), "missing gated entity: {required}");
        }

        // The forecast-block scalars carry the "forecast" sub-device hint so
        // the integration files them under the Forecast device. Most ride
        // snapshot="irrigation" for path reasons; pop_pct rides the conditions
        // snapshot because the merge bus writes it there, which is exactly why
        // the group hint is what decides the device rather than the snapshot.
        let mut fc = Vec::new();
        push_forecast(&mut fc);
        assert!(!fc.is_empty());
        for e in &fc {
            assert_eq!(
                e.group,
                Some("forecast"),
                "forecast entity {} lacks group",
                e.id
            );
        }
        // forecast_source (in the provenance push) carries it too.
        assert_eq!(
            both.iter()
                .find(|e| e.id == "forecast_source")
                .unwrap()
                .group,
            Some("forecast")
        );
    }

    #[test]
    fn diagnostics_are_binary_sensors() {
        let mut out = Vec::new();
        push_diagnostics(&mut out, true);
        for e in &out {
            // force_overrode_guard is deliberately a STRING sensor: its value
            // NAMES the hard guard a Force override suppressed (freeze / wind /
            // restriction), null when nothing is overridden. Every other
            // diagnostic is an on/off state and stays a binary_sensor.
            if e.id == "force_overrode_guard" {
                assert_eq!(e.platform, "sensor");
                continue;
            }
            assert_eq!(e.platform, "binary_sensor");
        }
        // The forced-run safety signal is present on irrigation installs...
        assert!(out.iter().any(|e| e.id == "force_overrode_guard"));
        // ...and absent on weather-only installs (no force override exists).
        let mut weather_only = Vec::new();
        push_diagnostics(&mut weather_only, false);
        assert!(!weather_only.iter().any(|e| e.id == "force_overrode_guard"));
    }
}
