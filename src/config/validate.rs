// Config validation. One pure function over a parsed Config producing a
// structured report: errors block apply/save, warnings surface in the
// UI but never block. Stable `code` strings so the UI can map issues
// to fields without string-matching prose.

use serde::Serialize;

use super::schema::{Config, SourceKind};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub severity: Severity,
    /// Stable machine code, e.g. "zone_controller_missing".
    pub code: &'static str,
    /// Human sentence with the specifics interpolated.
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ValidationReport {
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
}

impl ValidationReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
    fn error(&mut self, code: &'static str, detail: String) {
        self.errors.push(Issue {
            severity: Severity::Error,
            code,
            detail,
        });
    }
    fn warn(&mut self, code: &'static str, detail: String) {
        self.warnings.push(Issue {
            severity: Severity::Warning,
            code,
            detail,
        });
    }
}

pub fn validate(cfg: &Config) -> ValidationReport {
    let mut r = ValidationReport::default();

    // Location.
    let loc = &cfg.deployment.location;
    if loc.lat == 0.0 && loc.lon == 0.0 {
        r.error(
            "location_unset",
            "location is 0,0 (null island); set your real coordinates".into(),
        );
    }
    if !(-90.0..=90.0).contains(&loc.lat) {
        r.error("lat_range", format!("latitude {} out of range", loc.lat));
    }
    if !(-180.0..=180.0).contains(&loc.lon) {
        r.error("lon_range", format!("longitude {} out of range", loc.lon));
    }

    // Timezone, when explicit, must be a real IANA name.
    if let Some(tz) = cfg.deployment.timezone.as_deref() {
        if tz.parse::<chrono_tz::Tz>().is_err() {
            r.warn(
                "timezone_invalid",
                format!("timezone '{tz}' is not a valid IANA name; it will be inferred from the location instead"),
            );
        }
    }

    // Duplicate ids.
    let mut seen = std::collections::HashSet::new();
    for s in &cfg.sources {
        if !seen.insert(s.id.clone()) {
            r.error(
                "source_id_duplicate",
                format!("duplicate source id '{}'", s.id),
            );
        }
    }
    let mut seen = std::collections::HashSet::new();
    for c in &cfg.controllers {
        if !seen.insert(c.id.clone()) {
            r.error(
                "controller_id_duplicate",
                format!("duplicate controller id '{}'", c.id),
            );
        }
    }

    // Exactly zero or one default controller.
    let defaults = cfg.controllers.iter().filter(|c| c.default).count();
    if defaults > 1 {
        r.error(
            "controller_default_multiple",
            format!("{defaults} controllers are marked default; only one can be"),
        );
    }

    // Sources present? Warning only (weather-only-from-HA setups exist).
    if cfg.sources.iter().filter(|s| s.enabled).count() == 0 {
        r.warn(
            "sources_empty",
            "no enabled weather sources; the dashboard will be empty until one is added".into(),
        );
    }

    // Zones reference real controllers + source-backed soil sensors.
    let controller_ids: std::collections::HashSet<&str> =
        cfg.controllers.iter().map(|c| c.id.as_str()).collect();
    let source_ids: std::collections::HashSet<&str> =
        cfg.sources.iter().map(|s| s.id.as_str()).collect();
    for (slug, z) in &cfg.zones {
        if !z.controller_id.is_empty() && !controller_ids.contains(z.controller_id.as_str()) {
            r.error(
                "zone_controller_missing",
                format!(
                    "zone '{slug}' references controller '{}' which does not exist",
                    z.controller_id
                ),
            );
        }
        if let Some(spec) = z.soil_sensor_id.as_deref() {
            if let Some(rest) = spec.strip_prefix("source:") {
                let src = rest.split(':').next().unwrap_or("");
                if !src.is_empty() && !source_ids.contains(src) {
                    r.warn(
                        "zone_soil_source_missing",
                        format!(
                            "zone '{slug}' soil sensor references source '{src}' which does not exist"
                        ),
                    );
                }
            }
        }
        if z.area_sqft <= 0.0 {
            r.warn(
                "zone_area_nonpositive",
                format!(
                    "zone '{slug}' has area {} sqft; budgets need a real area",
                    z.area_sqft
                ),
            );
        }
    }

    // Manual schedules reference real zones.
    for sched in &cfg.manual_schedules {
        let normalized = sched.zone_slug.replace('-', "_");
        let known = cfg.zones.keys().any(|k| k.replace('-', "_") == normalized);
        if !known {
            r.error(
                "schedule_zone_missing",
                format!(
                    "schedule '{}' references zone '{}' which does not exist",
                    sched.id, sched.zone_slug
                ),
            );
        }
    }

    // Auth policy sanity.
    if cfg.auth.session_ttl_days == 0 {
        r.warn(
            "auth_ttl_zero",
            "auth.session_ttl_days is 0; treated as 1 day".into(),
        );
    }
    for net in &cfg.auth.trusted_networks {
        if net.parse::<ipnet::IpNet>().is_err() {
            r.warn(
                "trusted_network_invalid",
                format!("auth.trusted_networks entry '{net}' is not a valid CIDR and is ignored"),
            );
        }
    }

    // Ecowitt poll sources need a gateway host.
    for s in &cfg.sources {
        if let SourceKind::EcowittGwPoll(c) = &s.source {
            if c.host.trim().is_empty() {
                r.error(
                    "ecowitt_host_empty",
                    format!("source '{}' (ecowitt_gw_poll) has an empty host", s.id),
                );
            }
        }
    }

    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::*;

    fn base() -> Config {
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 29.65;
        cfg.deployment.location.lon = -82.32;
        cfg
    }

    #[test]
    fn default_config_fails_on_location() {
        let r = validate(&Config::default());
        assert!(!r.ok());
        assert!(r.errors.iter().any(|i| i.code == "location_unset"));
    }

    #[test]
    fn clean_config_passes_with_source_warning() {
        let r = validate(&base());
        assert!(r.ok());
        assert!(r.warnings.iter().any(|i| i.code == "sources_empty"));
    }

    #[test]
    fn zone_with_ghost_controller_errors() {
        let mut cfg = base();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 800.0,
                "species": "other",
                "soil_texture": "loam",
                "sprinkler_type": "rotor",
                "controller_id": "ghost",
                "controller_station": "1",
            }))
            .unwrap(),
        );
        let r = validate(&cfg);
        assert!(r.errors.iter().any(|i| i.code == "zone_controller_missing"));
    }

    #[test]
    fn duplicate_ids_and_multi_default_error() {
        let mut cfg = base();
        let entry: ControllerEntry = serde_json::from_value(serde_json::json!({
            "id": "a", "default": true, "enabled": true,
            "kind": "dry_run", "config": {"simulate_runs": false},
        }))
        .unwrap();
        cfg.controllers.push(entry.clone());
        cfg.controllers.push(entry);
        let r = validate(&cfg);
        assert!(r.errors.iter().any(|i| i.code == "controller_id_duplicate"));
        assert!(r
            .errors
            .iter()
            .any(|i| i.code == "controller_default_multiple"));
    }
}
