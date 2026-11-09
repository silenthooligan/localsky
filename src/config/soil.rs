//! Soil probe identity, compatibility and shared binding operations.
use super::schema::{Config, SoilProbeConfig, SourceKind, ZoneConfig};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

pub const DEFAULT_STALE_AFTER_S: u64 = 24 * 3600;

pub fn default_stale_after_s() -> u64 {
    DEFAULT_STALE_AFTER_S
}

impl SoilProbeConfig {
    pub fn new(id: impl AsRef<str>) -> Self {
        Self {
            sensor_id: id.as_ref().to_string(),
            excluded_from_decisions: false,
            stale_after_s: DEFAULT_STALE_AFTER_S,
        }
    }
}

/// Lossless, source-scoped channel identity. Mapping order and zone names are
/// deliberately absent. Length-delimited JSON prevents selector collisions.
pub fn probe_key(kind: &str, selectors: &[&str]) -> String {
    let identity = serde_json::to_vec(&(kind, selectors)).expect("string tuple serializes");
    format!("soilmoisture_probe_{}", URL_SAFE_NO_PAD.encode(identity))
}

pub fn probe_label(key: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(key.strip_prefix("soilmoisture_probe_")?)
        .ok()?;
    let (_, selectors): (String, Vec<String>) = serde_json::from_slice(&bytes).ok()?;
    Some(
        selectors
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" / "),
    )
}

/// The same selectors are used by migration and the source adapters.
pub fn mapped_probes(source: &SourceKind) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut add = |zone: Option<&str>, kind: &str, selectors: &[&str]| {
        if let Some(zone) = zone.map(str::trim).filter(|z| !z.is_empty()) {
            out.push((zone.to_string(), probe_key(kind, selectors)));
        }
    };
    match source {
        SourceKind::Mqtt(c) => {
            for m in &c.subscriptions {
                add(
                    m.zone_slug.as_deref(),
                    "mqtt",
                    &[&m.topic, m.json_path.as_deref().unwrap_or("")],
                );
            }
        }
        SourceKind::HaPassthrough(c) => {
            for (entity, zone) in &c.soil_zone_map {
                add(Some(zone), "ha", &[entity]);
            }
        }
        SourceKind::HttpWebhook(c) => {
            for m in &c.fields {
                add(
                    m.zone_slug.as_deref(),
                    "http",
                    &[m.json_path.as_deref().unwrap_or("")],
                );
            }
        }
        SourceKind::RestPoll(c) => {
            for m in &c.fields {
                add(
                    m.zone_slug.as_deref(),
                    "http",
                    &[m.json_path.as_deref().unwrap_or("")],
                );
            }
        }
        SourceKind::InfluxDb(c) => {
            for m in &c.queries {
                add(m.zone_slug.as_deref(), "influx", &[&m.query]);
            }
        }
        SourceKind::Prometheus(c) => {
            for m in &c.queries {
                add(m.zone_slug.as_deref(), "prometheus", &[&m.query]);
            }
        }
        SourceKind::TuyaCloud(c) => {
            for m in &c.device_field_map {
                add(
                    m.zone_slug.as_deref(),
                    "tuya",
                    &[&m.device_id, &m.status_code],
                );
            }
        }
        SourceKind::Yolink(c) => {
            for m in &c.device_field_map {
                add(
                    m.zone_slug.as_deref(),
                    "yolink",
                    &[
                        &m.device_id,
                        &m.state_path
                            .split('.')
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join("."),
                    ],
                );
            }
        }
        _ => {}
    }
    out
}

impl ZoneConfig {
    pub fn soil_bindings(&self) -> Vec<SoilProbeConfig> {
        bindings(&self.soil_probes, self.soil_sensor_id.as_deref())
    }

    pub fn has_soil_probe(&self) -> bool {
        !self.soil_bindings().is_empty()
    }
}

pub fn bindings(probes: &[SoilProbeConfig], legacy: Option<&str>) -> Vec<SoilProbeConfig> {
    if !probes.is_empty() {
        return probes.to_vec();
    }
    legacy
        .filter(|s| !s.trim().is_empty())
        .map(|s| vec![SoilProbeConfig::new(s)])
        .unwrap_or_default()
}

/// Expand old virtual zone channels into independent probe bindings. Never
/// copy mixed historical samples into an individual probe's history. Wait for
/// that probe's first new sample instead. Native and direct HA IDs still work.
pub fn migrate_bindings(cfg: &mut Config) {
    for zone in cfg.zones.values_mut() {
        let mut expanded = Vec::new();
        let mut changed = false;
        for probe in zone.soil_bindings() {
            let keys = probe
                .sensor_id
                .strip_prefix("source:")
                .and_then(|s| s.split_once(':'))
                .and_then(|(source_id, key)| {
                    cfg.sources
                        .iter()
                        .find(|s| s.id == source_id)
                        .map(|source| {
                            mapped_probes(&source.source)
                                .into_iter()
                                .filter(|(slug, _)| key == format!("soilmoisture_{slug}"))
                                .map(|(_, channel)| format!("source:{source_id}:{channel}"))
                                .collect::<Vec<_>>()
                        })
                })
                .unwrap_or_default();
            if keys.is_empty() {
                expanded.push(probe);
            } else {
                changed = true;
                for sensor_id in keys {
                    expanded.push(SoilProbeConfig {
                        sensor_id,
                        ..probe.clone()
                    });
                }
            }
        }
        if changed {
            expanded.sort_by(|a, b| a.sensor_id.cmp(&b.sensor_id));
            expanded.dedup_by(|a, b| a.sensor_id == b.sensor_id);
            zone.soil_probes = expanded;
            zone.soil_sensor_id = None;
        }
    }
}

pub fn validation_errors(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    for (slug, zone) in &cfg.zones {
        let mut seen = std::collections::HashSet::new();
        for probe in zone.soil_bindings() {
            if probe.sensor_id.trim().is_empty() || !seen.insert(probe.sensor_id.clone()) {
                errors.push(format!(
                    "zone '{slug}': soil probe IDs must be nonempty and unique"
                ));
            }
            if !(1..=604800).contains(&probe.stale_after_s) {
                errors.push(format!(
                    "zone '{slug}': probe timeout must be 1 to 604800 seconds"
                ));
            }
        }
    }
    for source in &cfg.sources {
        if let SourceKind::Mqtt(c) = &source.source {
            for m in &c.subscriptions {
                if m.zone_slug.as_deref().is_some_and(|z| !z.trim().is_empty())
                    && (m.topic.contains('+') || m.topic.contains('#'))
                {
                    errors.push(format!("source '{}': soil probes need exact MQTT topics; add one mapping per probe", source.id));
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for (_, key) in mapped_probes(&source.source) {
            if !seen.insert(key) {
                errors.push(format!(
                    "source '{}': duplicate soil probe mapping",
                    source.id
                ));
            }
        }
    }
    errors
}

pub fn legacy_mapped_key(cfg: &Config, source_id: &str, key: &str) -> bool {
    cfg.sources
        .iter()
        .find(|s| s.id == source_id)
        .is_some_and(|s| {
            mapped_probes(&s.source)
                .iter()
                .any(|(zone, _)| key == format!("soilmoisture_{zone}"))
        })
}

pub fn json_bindings(zone: &serde_json::Value) -> Vec<SoilProbeConfig> {
    let probes = zone
        .get("soil_probes")
        .cloned()
        .and_then(|v| serde_json::from_value::<Vec<SoilProbeConfig>>(v).ok())
        .unwrap_or_default();
    bindings(&probes, zone.get("soil_sensor_id").and_then(|v| v.as_str()))
}

fn replace_json_bindings(zone: &mut serde_json::Value, probes: &[SoilProbeConfig]) {
    if let Some(obj) = zone.as_object_mut() {
        obj.insert("soil_probes".into(), serde_json::json!(probes));
        obj.insert("soil_sensor_id".into(), serde_json::Value::Null);
    }
}

/// Assign one probe without replacing any other probe. Moving it preserves
/// the timeout and exclusion setting. Explicit unbinding removes it.
pub fn bind_json(zones: &mut serde_json::Map<String, serde_json::Value>, id: &str, target: &str) {
    let binding = zones
        .values()
        .flat_map(json_bindings)
        .find(|p| p.sensor_id == id)
        .unwrap_or_else(|| SoilProbeConfig::new(id));
    for (slug, zone) in zones {
        let mut probes = json_bindings(zone);
        probes.retain(|p| p.sensor_id != id);
        if slug == target {
            probes.push(binding.clone());
        }
        replace_json_bindings(zone, &probes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SoilAggregation, SourceEntry};

    fn fixture() -> Config {
        let mut cfg = Config::default();
        cfg.sources.push(
            serde_json::from_value::<SourceEntry>(serde_json::json!({
                "id": "mqtt", "kind": "mqtt", "config": {
                    "broker_host": "localhost", "subscriptions": [
                        {"topic": "soil/1", "field": "rh_pct", "zone_slug": "bed"},
                        {"topic": "soil/2", "field": "rh_pct", "zone_slug": "bed"}
                    ]
                }
            }))
            .unwrap(),
        );
        cfg.zones.insert(
            "bed".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Bed", "area_sqft": 100, "species": "other", "soil_texture": "loam",
                "slope_pct": 0, "sun_exposure": "full", "sprinkler_type": "spray",
                "controller_id": "", "controller_station": "",
                "soil_sensor_id": "source:mqtt:soilmoisture_bed"
            }))
            .unwrap(),
        );
        cfg
    }

    #[test]
    fn identity_is_unambiguous_and_label_preserves_selector() {
        assert_ne!(
            probe_key("mqtt", &["a/b", "c"]),
            probe_key("mqtt", &["a", "b/c"])
        );
        assert_eq!(
            probe_label(&probe_key("mqtt", &["soil/1", "moisture"])).as_deref(),
            Some("soil/1 / moisture")
        );
    }

    #[test]
    fn migration_expands_all_probes_and_is_idempotent() {
        let mut cfg = fixture();
        migrate_bindings(&mut cfg);
        let zone = &cfg.zones["bed"];
        assert!(zone.soil_sensor_id.is_none());
        assert_eq!(zone.soil_probes.len(), 2);
        assert_ne!(zone.soil_probes[0].sensor_id, zone.soil_probes[1].sensor_id);
        assert!(zone.soil_probes.iter().all(|p| p.stale_after_s == 86400));
        let first = zone.soil_probes.clone();
        migrate_bindings(&mut cfg);
        assert_eq!(cfg.zones["bed"].soil_probes, first);
        assert!(validation_errors(&cfg).is_empty());
    }

    #[test]
    fn single_probe_migration_and_explicit_exclusion_survive_reload() {
        let mut cfg = fixture();
        if let SourceKind::Mqtt(c) = &mut cfg.sources[0].source {
            c.subscriptions.truncate(1);
        }
        migrate_bindings(&mut cfg);
        let zone = cfg.zones.get_mut("bed").unwrap();
        assert_eq!(zone.soil_probes.len(), 1);
        zone.soil_probes[0].excluded_from_decisions = true;
        zone.soil_probes[0].stale_after_s = 900;
        migrate_bindings(&mut cfg);
        assert!(cfg.zones["bed"].soil_probes[0].excluded_from_decisions);
        assert_eq!(cfg.zones["bed"].soil_probes[0].stale_after_s, 900);
        assert!(cfg.zones["bed"].has_soil_probe());
    }

    #[test]
    fn binding_one_probe_preserves_peers_and_moving_preserves_settings() {
        let mut zones =
            serde_json::json!({"one": {"soil_sensor_id": "ha:sensor.first"}, "two": {}})
                .as_object()
                .unwrap()
                .clone();
        bind_json(&mut zones, "ha:sensor.second", "one");
        assert_eq!(json_bindings(&zones["one"]).len(), 2);
        zones.get_mut("one").unwrap()["soil_probes"][1]["excluded_from_decisions"] = true.into();
        bind_json(&mut zones, "ha:sensor.second", "two");
        assert_eq!(json_bindings(&zones["one"])[0].sensor_id, "ha:sensor.first");
        assert!(json_bindings(&zones["two"])[0].excluded_from_decisions);
    }

    #[test]
    fn aggregation_policies_are_explicit() {
        let values = vec![74.0, 40.0, 36.0, 29.0];
        assert_eq!(SoilAggregation::Median.combine(values.clone()), Some(38.0));
        assert_eq!(
            SoilAggregation::Average.combine(values.clone()),
            Some(44.75)
        );
        assert_eq!(SoilAggregation::Minimum.combine(values), Some(29.0));
        assert_eq!(SoilAggregation::Median.combine(vec![36.0]), Some(36.0));
        assert_eq!(SoilAggregation::Median.combine(vec![]), None);
    }
}
