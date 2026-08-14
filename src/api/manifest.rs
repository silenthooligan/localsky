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
/// 1.4 (additive): the flow/leaf capability-gate rule extended to every
/// entity whose install may lack the backing hardware/data: pop_pct (a
/// configured source must provide Pop), the station-only scalars
/// wet_bulb_f / wind_lull_mph / rain_in_last_min / illuminance_lx (a live
/// station must be present, same rule as battery_pct), the per-zone
/// soil moisture/temperature/EC/battery quartet (the zone must have a
/// soil probe configured or live-reporting), and water_level_pct (the
/// controller must report the capability).
pub const MANIFEST_SCHEMA_VERSION: &str = "1.4";

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

    // Which reading capabilities the CONFIGURED sources provide (for the
    // flow/leaf/pop/lux publish gates), plus which zones have a soil probe
    // configured (for the per-zone soil entity gate; None = config
    // unreadable, publish per-zone soil as before). Fail-OPEN on a config
    // load error (publish as before) so a transient config problem never
    // silently drops entities an install genuinely has.
    let (cfg_provides_flow, cfg_provides_leaf, cfg_provides_pop, cfg_provides_lux, cfg_soil_zones) =
        match state.cfg.load().await {
            Ok(cfg) => {
                let mut flow = false;
                let mut leaf = false;
                let mut pop = false;
                let mut lux = false;
                for entry in cfg.sources.iter().filter(|s| s.enabled) {
                    for f in crate::runtime::source_field_names(&cfg, entry) {
                        match f {
                            "flow_gpm" => flow = true,
                            "leaf_wetness_pct" => leaf = true,
                            "pop" => pop = true,
                            "illuminance" => lux = true,
                            _ => {}
                        }
                    }
                    if flow && leaf && pop && lux {
                        break;
                    }
                }
                // The Open-Meteo forecast refresher is IMPLICIT: main.rs
                // spawns it even when no OpenMeteo entry exists in sources
                // (only an explicit disabled entry opts out), and it emits
                // Pop. Mirror that backstop or the gate drops the pop_pct
                // sensor while the data plane is still feeding it.
                pop = pop
                    || !cfg.sources.iter().any(|s| {
                        matches!(s.source, crate::config::schema::SourceKind::OpenMeteo(_))
                    });
                let soil: std::collections::BTreeSet<String> = cfg
                    .zones
                    .iter()
                    .filter(|(_, z)| z.soil_sensor_id.is_some())
                    .map(|(slug, _)| slug.clone())
                    .collect();
                (flow, leaf, pop, lux, Some(soil))
            }
            Err(crate::ports::config_store::ConfigStoreError::NotFound) => {
                // Pre-config the implicit Open-Meteo backstop still runs
                // (and can fetch, via the legacy env coords), so Pop stays
                // capable; everything else needs a configured source.
                (false, false, true, false, Some(Default::default()))
            }
            Err(_) => (true, true, true, true, None),
        };
    let has_flow = snap.flow_meter || cfg_provides_flow;
    let has_leaf = cfg_provides_leaf;
    // Belt and braces with the nullable Snapshot.pop_pct: the gate drops the
    // entity when no configured source can ever provide Pop, and the null
    // keeps a gated-in sensor unavailable until the first real write.
    let has_pop = cfg_provides_pop;

    // A live local station (Tempest serial present) gates the station-only
    // scalars (battery) so a cloud-only / Ecowitt install does not publish a
    // phantom 0% device_class=battery sensor. Configured irrigation gates the
    // irrigation entities so a weather-only install does not get phantom
    // verdict/override/threshold sliders that error on write. The irrigation
    // snapshot carries only zones (no controller list); a configured controller
    // always yields at least one zone, so non-empty zones is the presence test.
    let has_station = !snap.station_serial.is_empty();
    let has_irrigation = !snap.zones.is_empty();
    // Illuminance is station-only (cloud sources provide irradiance and UV,
    // never lux), but any lux-capable configured station source counts, not
    // just one that has stamped a serial yet.
    let has_lux = has_station || cfg_provides_lux;

    // Water-level capability rides the snapshot (ControllerCaps.water_level
    // via the refresher, or a live read), mirroring the flow_meter flag.
    let has_water_level = snap.water_level_capable || snap.water_level_pct.is_some();

    push_tempest_weather(&mut entities, has_station, has_lux);
    push_irrigation_meta(&mut entities, has_irrigation, has_water_level);
    push_thresholds(&mut entities, has_irrigation);
    push_forecast(&mut entities, has_pop);
    push_provenance_and_flow(&mut entities, has_flow, has_leaf);
    let soil_zones = soil_equipped_zones(cfg_soil_zones, &snap);
    push_zone_entities(&mut entities, &snap.zones, soil_zones.as_ref());
    push_diagnostics(&mut entities, has_irrigation);

    Json(Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        entities,
    })
}

// ─────────────────────────────────────────────────────────────────────
// Tempest weather scalars (snapshot=tempest)
// ─────────────────────────────────────────────────────────────────────
fn push_tempest_weather(out: &mut Vec<EntityDescriptor>, has_station: bool, has_lux: bool) {
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
            "rain_in_today",
            "Rain today",
            "rain_in_today",
            Some("in"),
            Some("precipitation"),
            Some("total_increasing"),
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
    // Wet bulb, wind lull, and rain-last-minute are computed ONLY by the
    // Tempest UDP path (apply_obs); no WeatherField variant exists for them,
    // so no other station kind and no cloud fill can ever write them.
    // Ungated they gave every non-Tempest install a frozen 0.0 °F "Wet bulb"
    // (device_class=temperature feeding long-term statistics) plus dead
    // "Wind lull" / "Rain last minute" sensors, the same phantom class the
    // battery gate below exists for. Same gate: a live station present.
    if has_station {
        let station_only: &[(
            &str,
            &str,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
        )] = &[
            (
                "wet_bulb_f",
                "Wet bulb",
                Some("°F"),
                Some("temperature"),
                Some("measurement"),
            ),
            (
                "wind_lull_mph",
                "Wind lull",
                Some("mph"),
                Some("wind_speed"),
                Some("measurement"),
            ),
            (
                "rain_in_last_min",
                "Rain last minute",
                Some("in"),
                Some("precipitation"),
                Some("measurement"),
            ),
        ];
        for (id, name, unit, device_class, state_class) in station_only {
            out.push(EntityDescriptor {
                platform: "sensor",
                id: (*id).to_string(),
                name: (*name).to_string(),
                snapshot: "tempest",
                path: vec![(*id).to_string()],
                unit: *unit,
                device_class: *device_class,
                state_class: *state_class,
                icon: None,
                zone_slug: None,
                group: None,
            });
        }
    }
    // Illuminance is station-only lux (cloud sources provide irradiance + UV
    // but never lux; solar.rs hides the panel for cloud-only for the same
    // reason), yet it published ungated, so cloud-only installs carried a
    // 0 lx sensor forever. Gated on a live station or a configured
    // lux-capable source.
    if has_lux {
        out.push(EntityDescriptor {
            platform: "sensor",
            id: "illuminance_lx".to_string(),
            name: "Illuminance".to_string(),
            snapshot: "tempest",
            path: vec!["illuminance_lx".to_string()],
            unit: Some("lx"),
            device_class: Some("illuminance"),
            state_class: Some("measurement"),
            icon: None,
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
fn push_irrigation_meta(
    out: &mut Vec<EntityDescriptor>,
    has_irrigation: bool,
    has_water_level: bool,
) {
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
    // Water level is a controller readback only OpenSprinkler-class hardware
    // reports; ungated it registered for every irrigation install (the
    // flow_gpm defect shape). Gate on the capability; the nullable snapshot
    // field keeps a gated-in sensor unavailable until the first real read.
    if has_water_level {
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
    }
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
fn push_forecast(out: &mut Vec<EntityDescriptor>, has_pop: bool) {
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
    //
    // Capability-gated like flow/leaf: only a handful of sources emit Pop
    // into current conditions (Open-Meteo current, Pirate Weather), so an
    // install whose sources never provide it must not register a phantom
    // probability sensor (the nullable snapshot field covers the window
    // between config and the first real write).
    if has_pop {
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

/// The soil-equipped zone set for the per-zone entity gate. A zone counts
/// when the config binds a probe to it (`cfg_soil`) OR the live snapshot
/// carries soil evidence for it: probe extras on the zone, or a NON-NULL
/// `skip_check.soil_<slug>_pct` reading. The union keeps a
/// just-unconfigured-but-still-reporting probe visible while the config
/// settles. The key's PRESENCE alone is NOT evidence: `build_soil_fields`
/// emits a null-valued `soil_<slug>_pct` for EVERY zone, probe or not, so a
/// contains_key test would re-admit exactly the probe-less phantoms this
/// gate exists to drop. `None` in = config unreadable = `None` out (the
/// caller fails open and publishes soil for every zone).
fn soil_equipped_zones(
    cfg_soil: Option<std::collections::BTreeSet<String>>,
    snap: &crate::ha::snapshot::IrrigationSnapshot,
) -> Option<std::collections::BTreeSet<String>> {
    cfg_soil.map(|mut set| {
        for z in &snap.zones {
            let evidence = z.soil_temp_f.is_some()
                || z.soil_ec.is_some()
                || z.soil_battery_pct.is_some()
                || snap
                    .skip_check
                    .soil_fields
                    .get(&format!("soil_{}_pct", z.slug))
                    .is_some_and(|v| v.is_some());
            if evidence {
                set.insert(z.slug.clone());
            }
        }
        set
    })
}

fn push_zone_entities(
    out: &mut Vec<EntityDescriptor>,
    zones: &[crate::ha::snapshot::ZoneState],
    // Slugs of zones with a soil probe configured (or live soil evidence).
    // `None` = config unreadable: fail open and publish soil for every zone,
    // matching the flow/leaf gates' fail-open posture.
    soil_zones: Option<&std::collections::BTreeSet<String>>,
) {
    for zone in zones {
        let slug = &zone.slug;
        let pretty = if zone.name.is_empty() {
            slug.clone()
        } else {
            zone.name.clone()
        };
        let has_soil = soil_zones.map(|set| set.contains(slug)).unwrap_or(true);

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

        // The four probe-backed soil entities publish only for zones that
        // actually have a soil probe (configured or live-reporting). On a
        // probe-less zone the backing fields are permanently absent, so HA
        // registered four forever-unavailable phantoms per zone, including a
        // dead-looking device_class=battery sensor; the zone-detail UI
        // already gates on is_some() and the manifest now matches it.
        if has_soil {
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

            // Native soil temperature (°F), LocalSky polls the gateway
            // directly, so HA no longer needs the ecowitt2mqtt MQTT entity.
            // zone_slug + path-into-zone reads zones[].soil_temp_f.
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

            // Native soil EC (µS/cm), salinity / fertilizer drift.
            // Display-only.
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
        }

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
        push_tempest_weather(&mut out, true, true);
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
        push_tempest_weather(&mut with_station, true, true);
        assert!(with_station.iter().any(|e| e.id == "battery_pct"));

        let mut cloud_only = Vec::new();
        push_tempest_weather(&mut cloud_only, false, false);
        assert!(!cloud_only.iter().any(|e| e.id == "battery_pct"));
    }

    #[test]
    fn station_only_scalars_gated_like_battery() {
        // Wet bulb / wind lull / rain-last-minute exist only in the Tempest
        // UDP packet; without a station they were frozen 0.0 sensors in HA.
        // Illuminance is station-only lux with its own (station OR
        // lux-capable-source) gate.
        let mut cloud_only = Vec::new();
        push_tempest_weather(&mut cloud_only, false, false);
        let ids: Vec<&str> = cloud_only.iter().map(|e| e.id.as_str()).collect();
        for absent in [
            "wet_bulb_f",
            "wind_lull_mph",
            "rain_in_last_min",
            "illuminance_lx",
        ] {
            assert!(!ids.contains(&absent), "phantom station scalar: {absent}");
        }
        // The universal conditions still publish on a cloud-only install.
        assert!(ids.contains(&"air_temp_f"));
        assert!(ids.contains(&"solar_w_m2"));

        // With a station present they all return.
        let mut with_station = Vec::new();
        push_tempest_weather(&mut with_station, true, true);
        let ids: Vec<&str> = with_station.iter().map(|e| e.id.as_str()).collect();
        for required in [
            "wet_bulb_f",
            "wind_lull_mph",
            "rain_in_last_min",
            "illuminance_lx",
        ] {
            assert!(
                ids.contains(&required),
                "missing station scalar: {required}"
            );
        }

        // A lux-capable configured source (e.g. Ecowitt) publishes
        // illuminance without a stamped station serial.
        let mut lux_source = Vec::new();
        push_tempest_weather(&mut lux_source, false, true);
        let ids: Vec<&str> = lux_source.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"illuminance_lx"));
        assert!(!ids.contains(&"wet_bulb_f"), "wet bulb stays Tempest-only");
    }

    #[test]
    fn irrigation_entities_gated_on_irrigation_present() {
        // A weather-only install (no zones/controllers) must not publish the
        // irrigation verdict/override sensors, threshold sliders, or the
        // IU-suspended problem sensor.
        let mut weather_only = Vec::new();
        push_irrigation_meta(&mut weather_only, false, false);
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
        push_irrigation_meta(&mut with_irrigation, true, true);
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
        push_forecast(&mut fc, true);
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
    fn water_level_gated_on_controller_capability() {
        // A Rachio/B-hyve-class install (controller never reports a water
        // level) must not register the sensor; an OpenSprinkler-class one
        // (ControllerCaps.water_level, or a live HA-bridge read) does.
        let mut without = Vec::new();
        push_irrigation_meta(&mut without, true, false);
        let ids: Vec<&str> = without.iter().map(|e| e.id.as_str()).collect();
        assert!(!ids.contains(&"water_level_pct"));
        // The rest of the irrigation meta publishes either way.
        assert!(ids.contains(&"irrigation_verdict"));

        let mut with = Vec::new();
        push_irrigation_meta(&mut with, true, true);
        assert!(with.iter().any(|e| e.id == "water_level_pct"));
    }

    #[test]
    fn zone_soil_entities_gated_on_a_configured_probe() {
        use crate::ha::snapshot::ZoneState;
        let zones = vec![
            ZoneState {
                slug: "front".into(),
                name: "Front".into(),
                ..Default::default()
            },
            ZoneState {
                slug: "back".into(),
                name: "Back".into(),
                ..Default::default()
            },
        ];
        // Only "back" has a soil probe configured.
        let soil: std::collections::BTreeSet<String> = ["back".to_string()].into_iter().collect();
        let mut out = Vec::new();
        push_zone_entities(&mut out, &zones, Some(&soil));
        let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
        for id in [
            "front_soil_moisture",
            "front_soil_temperature",
            "front_soil_ec",
            "front_soil_battery",
        ] {
            assert!(!ids.contains(&id), "probe-less zone got soil entity {id}");
        }
        for id in [
            "back_soil_moisture",
            "back_soil_temperature",
            "back_soil_ec",
            "back_soil_battery",
        ] {
            assert!(ids.contains(&id), "soil zone missing {id}");
        }
        // Engine-state and runtime entities publish for every zone either
        // way (the bucket is engine math, not a probe reading).
        assert!(ids.contains(&"front_soil_bucket"));
        assert!(ids.contains(&"front_planned_run"));

        // Config unreadable (None): fail open, soil publishes for every zone.
        let mut open = Vec::new();
        push_zone_entities(&mut open, &zones, None);
        assert!(open.iter().any(|e| e.id == "front_soil_moisture"));
    }

    #[test]
    fn null_soil_fields_key_is_not_probe_evidence() {
        // build_soil_fields emits soil_<slug>_pct for EVERY zone (null when
        // no probe reports), so the handler's evidence union must test the
        // VALUE, not key presence: a probe-less zone whose only "evidence"
        // is the null key stays gated out, a zone with a real live reading
        // gates in even with no config binding, and a config-bound zone
        // stays in while its probe reads null (offline probe).
        use crate::ha::snapshot::{IrrigationSnapshot, ZoneState};
        let mut snap = IrrigationSnapshot {
            zones: vec![
                ZoneState {
                    slug: "front".into(),
                    ..Default::default()
                },
                ZoneState {
                    slug: "back".into(),
                    ..Default::default()
                },
                ZoneState {
                    slug: "side".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        snap.skip_check
            .soil_fields
            .insert("soil_front_pct".into(), None);
        snap.skip_check
            .soil_fields
            .insert("soil_back_pct".into(), Some(41.0));
        snap.skip_check
            .soil_fields
            .insert("soil_side_pct".into(), None);

        // Config binds a probe to "side" only.
        let cfg: std::collections::BTreeSet<String> = ["side".to_string()].into_iter().collect();
        let got = soil_equipped_zones(Some(cfg), &snap).expect("Some in, Some out");
        assert!(
            !got.contains("front"),
            "null soil_fields key must not admit a probe-less zone"
        );
        assert!(got.contains("back"), "live non-null reading is evidence");
        assert!(got.contains("side"), "config binding survives a null read");

        // Config unreadable: fail open end to end.
        assert_eq!(soil_equipped_zones(None, &snap), None);
    }

    #[test]
    fn pop_pct_is_capability_gated() {
        // No configured source provides Pop: the probability sensor is not
        // published, so HA never grows a phantom "0% chance of rain" entity.
        let mut without = Vec::new();
        push_forecast(&mut without, false);
        assert!(!without.iter().any(|e| e.id == "pop_pct"));
        // The rest of the forecast scalars publish either way.
        assert!(without.iter().any(|e| e.id == "rain_tomorrow_prob_pct"));

        // A Pop-capable source configured: the sensor returns.
        let mut with = Vec::new();
        push_forecast(&mut with, true);
        assert!(with.iter().any(|e| e.id == "pop_pct"));
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
