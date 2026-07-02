// Config validation. One pure function over a parsed Config producing a
// structured report: errors block apply/save, warnings surface in the
// UI but never block. Stable `code` strings so the UI can map issues
// to fields without string-matching prose.

use serde::Serialize;

use super::schema::{BlitzortungTransport, Config, SourceKind};

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

    // Exactly one default controller when two or more exist. The save gate
    // (loader::validate) hard-rejects a zero-default fleet, so Review must
    // surface the same field-level error instead of letting "Save and finish"
    // 422 with an unstructured message. A SINGLE controller with no default is
    // deliberately NOT an error here: the apply path (finalize_for_apply ->
    // loader::auto_default_controller) marks the sole controller default
    // before save, so flagging it would block the happy-path single-controller
    // wizard. With two or more, the choice is ambiguous, so the operator must
    // pick one.
    let defaults = cfg.controllers.iter().filter(|c| c.default).count();
    if cfg.controllers.len() >= 2 && defaults == 0 {
        r.error(
            "controller_default_missing",
            "no controller is marked default; with more than one controller you must pick which \
             one zones use by default"
                .into(),
        );
    }
    if defaults > 1 {
        r.error(
            "controller_default_multiple",
            format!("{defaults} controllers are marked default; only one can be"),
        );
    }

    // Controller ids must be non-empty and free of whitespace/slashes (the
    // loader save gate enforces the same; promoted here so Review catches it
    // as a coded field error instead of a save-time 422).
    for c in &cfg.controllers {
        if c.id.is_empty() {
            r.error(
                "controller_id_empty",
                "a controller has an empty id; give each controller a snake_case id".into(),
            );
        } else if c.id.contains(char::is_whitespace) || c.id.contains('/') {
            r.error(
                "controller_id_invalid",
                format!(
                    "controller id '{}' contains whitespace or a slash; use snake_case",
                    c.id
                ),
            );
        }
    }

    // Source ids must be non-empty and free of whitespace/slashes (loader
    // save-gate parity).
    for s in &cfg.sources {
        if s.id.is_empty() {
            r.error(
                "source_id_empty",
                "a source has an empty id; give each source a snake_case id".into(),
            );
        } else if s.id.contains(char::is_whitespace) || s.id.contains('/') {
            r.error(
                "source_id_invalid",
                format!(
                    "source id '{}' contains whitespace or a slash; use snake_case",
                    s.id
                ),
            );
        }
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
        // Area must be positive: the engine divides by it, so zero/negative
        // is catastrophic, not cosmetic. This is an ERROR to match the loader
        // save gate (was a warning here, which let Review pass a config the
        // save step then 422'd).
        if z.area_sqft <= 0.0 {
            r.error(
                "zone_area_nonpositive",
                format!(
                    "zone '{slug}' has area {} sqft; area must be greater than 0 (the engine \
                     divides budgets by it)",
                    z.area_sqft
                ),
            );
        }
        // Moisture band must be ordered: target_min below saturation, or the
        // saturation gate has an inverted band the engine can't reason about.
        if z.target_min_pct_soil >= z.saturation_pct_soil {
            r.error(
                "zone_moisture_band_inverted",
                format!(
                    "zone '{slug}': target_min ({}%) must be below saturation ({}%)",
                    z.target_min_pct_soil, z.saturation_pct_soil
                ),
            );
        }
        // Slope is read as an absolute value by the engine; a negative entry
        // is operator confusion, so reject it rather than silently abs() it.
        if z.slope_pct < 0.0 {
            r.error(
                "zone_slope_negative",
                format!("zone '{slug}': slope {}% must be non-negative", z.slope_pct),
            );
        }
        // A measured precip rate must be physically plausible; the catalog
        // default (None) is always fine.
        if let Some(pr) = z.precip_rate_mm_hr {
            if pr <= 0.0 || pr > 200.0 {
                r.error(
                    "zone_precip_rate_range",
                    format!(
                        "zone '{slug}': precip rate {pr} mm/hr is out of the plausible range \
                         (0 < rate <= 200)"
                    ),
                );
            }
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
    for net in &cfg.auth.trusted_proxies {
        if net.parse::<ipnet::IpNet>().is_err() {
            r.warn(
                "trusted_proxy_invalid",
                format!("auth.trusted_proxies entry '{net}' is not a valid CIDR and is ignored"),
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

    // Required string credentials must be non-empty at config time. Without
    // this, an empty api_key / user_agent / access_token sails through here
    // and only surfaces as a runtime 401/400 on the first poll, long after
    // the wizard said "valid". Catch the single-credential kinds (the ones
    // whose one required string is the credential) up front. Multi-field
    // cloud kinds (OAuth client_id/secret pairs, MQTT broker creds, etc.)
    // are left to their adapters since "which subset is required" is
    // kind-specific; these are the unambiguous single-secret sources.
    for s in &cfg.sources {
        if !s.enabled {
            continue;
        }
        let empty: Option<&'static str> = match &s.source {
            SourceKind::TempestWs(c) if c.access_token.trim().is_empty() => Some("access_token"),
            SourceKind::OpenWeather(c) if c.api_key.trim().is_empty() => Some("api_key"),
            SourceKind::PirateWeather(c) if c.api_key.trim().is_empty() => Some("api_key"),
            SourceKind::Nws(c) if c.user_agent.trim().is_empty() => Some("user_agent"),
            SourceKind::MetNorway(c) if c.user_agent.trim().is_empty() => Some("user_agent"),
            SourceKind::Synoptic(c) if c.token.trim().is_empty() => Some("token"),
            SourceKind::HaPassthrough(c) if c.bearer_token.trim().is_empty() => {
                Some("bearer_token")
            }
            _ => None,
        };
        if let Some(field) = empty {
            r.error(
                "source_credential_empty",
                format!(
                    "source '{}' has an empty required {field}; fill it in or the source will \
                     fail to authenticate at runtime",
                    s.id
                ),
            );
        }
    }

    // WeatherKit is a multi-field credential, so it is NOT covered by the
    // single-secret block above. Its JWT is signed from FOUR pieces (key_id ->
    // `kid`, team_id -> `iss`, service_id -> `sub`, and the .p8 private key);
    // any empty id makes Apple return 401 on the first poll. The cloud-weather
    // one-click flow only captures the .p8, so guard the gap server-side: an
    // ENABLED WeatherKit missing any id is a coded field error here (failing
    // loudly at save time), so a dead WeatherKit can never be saved-as-enabled
    // by ANY path (one-click, raw TOML, API) and 401 silently at runtime.
    for s in &cfg.sources {
        if !s.enabled {
            continue;
        }
        if let SourceKind::WeatherKit(c) = &s.source {
            // Report each empty id by name so the UI can map the error to the
            // exact field the operator still has to fill in.
            let missing: Vec<&'static str> = [
                ("key_id", c.key_id.trim().is_empty()),
                ("team_id", c.team_id.trim().is_empty()),
                ("service_id", c.service_id.trim().is_empty()),
            ]
            .into_iter()
            .filter_map(|(name, empty)| empty.then_some(name))
            .collect();
            if !missing.is_empty() {
                r.error(
                    "weatherkit_ids_incomplete",
                    format!(
                        "source '{}' (weatherkit) is enabled but missing {}; WeatherKit signs its \
                         JWT from the key id, team id, and service id, so an empty one 401s at \
                         Apple. Add all of them (the Apple Developer portal lists each) before \
                         enabling it.",
                        s.id,
                        missing.join(", ")
                    ),
                );
            }
        }
    }

    // Blitzortung community lightning: surface the licensing boundary
    // at config time so the opt-in is informed. Warning, not error,
    // because enabling it is a legitimate operator choice; the codes
    // below also catch a config that can never match or connect.
    for s in &cfg.sources {
        if let SourceKind::Blitzortung(c) = &s.source {
            if s.enabled && c.enabled {
                r.warn(
                    "blitzortung_terms",
                    format!(
                        "source '{}' enables Blitzortung.org community lightning: data is \
                         CC BY-SA 4.0 from a volunteer network, for private non-commercial \
                         use with visible attribution; it is a display layer only and must \
                         never be used for storm warnings or automation",
                        s.id
                    ),
                );
            }
            if c.radius_mi <= 0.0 {
                r.error(
                    "blitzortung_radius_nonpositive",
                    format!(
                        "source '{}' (blitzortung) has radius_mi {}; no strike could ever match",
                        s.id, c.radius_mi
                    ),
                );
            }
            match c.transport {
                BlitzortungTransport::WebSocket => {
                    for h in &c.hosts {
                        if !(h.starts_with("ws://") || h.starts_with("wss://")) {
                            r.warn(
                                "blitzortung_host_invalid",
                                format!(
                                    "source '{}' (blitzortung) host '{h}' is not a ws:// or \
                                     wss:// URL and will fail to connect",
                                    s.id
                                ),
                            );
                        }
                    }
                }
                BlitzortungTransport::Mqtt => {
                    if c.mqtt.topic.trim().is_empty() {
                        r.error(
                            "blitzortung_mqtt_topic_empty",
                            format!(
                                "source '{}' (blitzortung) uses the mqtt transport but its \
                                 topic is empty; there is nothing to subscribe to",
                                s.id
                            ),
                        );
                    }
                    if c.mqtt.host.trim().is_empty() {
                        r.error(
                            "blitzortung_mqtt_host_empty",
                            format!(
                                "source '{}' (blitzortung) mqtt transport has an empty host",
                                s.id
                            ),
                        );
                    }
                    if s.enabled && c.enabled && c.mqtt.username.trim().is_empty() {
                        r.warn(
                            "blitzortung_mqtt_no_credentials",
                            format!(
                                "source '{}' (blitzortung) mqtt transport has no username; the \
                                 Blitzortung broker requires the credential they issue, so an \
                                 anonymous connection will be rejected",
                                s.id
                            ),
                        );
                    }
                }
            }
        }
    }

    // Open-Meteo model ids must come from the forecast model catalog.
    // The refresher appends `&models=<id>` verbatim, and an unknown id
    // makes upstream return HTTP 400 on every refresh, so warn loudly;
    // a typo should not block saving the rest of the config.
    for s in &cfg.sources {
        if let SourceKind::OpenMeteo(c) = &s.source {
            if crate::forecast::model_catalog::model_by_id(&c.model).is_none() {
                let valid = crate::forecast::model_catalog::models()
                    .iter()
                    .map(|m| m.id)
                    .collect::<Vec<_>>()
                    .join(", ");
                r.warn(
                    "open_meteo_model_unknown",
                    format!(
                        "source '{}' open_meteo model '{}' is not a known model id \
                         (valid: {valid}); the forecast fetch will fail upstream",
                        s.id, c.model
                    ),
                );
            }
        }
    }

    // Radar layer + provider ids must come from the radar catalog
    // (legacy pre-catalog ids normalize and pass; the retired satellite
    // IR layer does not). The frontend silently ignores unknown ids, so
    // warn rather than block.
    for id in &cfg.ui.radar.default_layers {
        if crate::radar_catalog::canonical_layer_id(id).is_none() {
            r.warn(
                "radar_layer_unknown",
                format!(
                    "ui.radar.default_layers entry '{id}' is not a known radar provider or \
                     feature id and is ignored"
                ),
            );
        }
    }
    for id in &cfg.ui.radar.providers {
        if crate::radar_catalog::provider_by_id(id).is_none() {
            let valid = crate::radar_catalog::providers()
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>()
                .join(", ");
            r.warn(
                "radar_provider_unknown",
                format!(
                    "ui.radar.providers entry '{id}' is not a catalog provider id \
                     (valid: {valid}) and is ignored"
                ),
            );
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
    fn unknown_radar_layer_warns_not_errors() {
        let mut cfg = base();
        cfg.ui.radar.default_layers.push("sharknado".into());
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r.warnings.iter().any(|i| i.code == "radar_layer_unknown"));
    }

    #[test]
    fn known_radar_layers_pass_clean() {
        // Catalog provider ids, feature ids, and the legacy
        // pre-catalog trio all pass without a warning.
        let mut cfg = base();
        cfg.ui.radar.default_layers = vec![
            "rainviewer".into(),
            "warnings_us".into(),
            "precip".into(),
            "nexrad".into(),
            "lightning".into(),
        ];
        let r = validate(&cfg);
        assert!(!r.warnings.iter().any(|i| i.code == "radar_layer_unknown"));
    }

    #[test]
    fn retired_satellite_layer_warns() {
        // RainViewer no longer serves the key-free IR frames, so the
        // old `satellite` id has no catalog successor.
        let mut cfg = base();
        cfg.ui.radar.default_layers = vec!["satellite".into()];
        let r = validate(&cfg);
        assert!(r.warnings.iter().any(|i| i.code == "radar_layer_unknown"));
    }

    #[test]
    fn unknown_radar_provider_warns_not_errors() {
        let mut cfg = base();
        // A feature id is not a provider id either.
        cfg.ui.radar.providers = vec!["rainviewer".into(), "warnings_us".into()];
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "radar_provider_unknown"));
    }

    #[test]
    fn known_radar_providers_pass_clean() {
        let mut cfg = base();
        cfg.ui.radar.providers = vec!["geomet_ca".into(), "nexrad_iem".into()];
        let r = validate(&cfg);
        assert!(!r
            .warnings
            .iter()
            .any(|i| i.code == "radar_provider_unknown"));
    }

    fn open_meteo_source(model: &str) -> SourceEntry {
        serde_json::from_value(serde_json::json!({
            "id": "open_meteo",
            "kind": "open_meteo",
            "config": { "model": model },
        }))
        .unwrap()
    }

    #[test]
    fn unknown_open_meteo_model_warns_not_errors() {
        let mut cfg = base();
        cfg.sources.push(open_meteo_source("ecmwf_seamless"));
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "open_meteo_model_unknown"));
    }

    #[test]
    fn known_open_meteo_models_pass_clean() {
        for model in ["best_match", "icon_seamless", "ecmwf_ifs025"] {
            let mut cfg = base();
            cfg.sources.push(open_meteo_source(model));
            let r = validate(&cfg);
            assert!(
                !r.warnings
                    .iter()
                    .any(|i| i.code == "open_meteo_model_unknown"),
                "model '{model}' should validate clean"
            );
        }
    }

    fn blitzortung_source(config: serde_json::Value) -> SourceEntry {
        serde_json::from_value(serde_json::json!({
            "id": "blitz",
            "kind": "blitzortung",
            "config": config,
        }))
        .unwrap()
    }

    #[test]
    fn enabled_blitzortung_warns_about_terms() {
        let mut cfg = base();
        cfg.sources
            .push(blitzortung_source(serde_json::json!({"enabled": true})));
        let r = validate(&cfg);
        assert!(r.ok(), "terms reminder must not block saving");
        assert!(r.warnings.iter().any(|i| i.code == "blitzortung_terms"));
    }

    #[test]
    fn opted_out_blitzortung_stays_quiet() {
        // Default config (enabled=false) is the parked state; no nag.
        let mut cfg = base();
        cfg.sources.push(blitzortung_source(serde_json::json!({})));
        let r = validate(&cfg);
        assert!(!r.warnings.iter().any(|i| i.code == "blitzortung_terms"));
    }

    #[test]
    fn blitzortung_field_hygiene() {
        let mut cfg = base();
        cfg.sources.push(blitzortung_source(serde_json::json!({
            "enabled": true,
            "radius_mi": 0.0,
            "hosts": ["https://not-a-websocket.example"],
        })));
        let r = validate(&cfg);
        assert!(r
            .errors
            .iter()
            .any(|i| i.code == "blitzortung_radius_nonpositive"));
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "blitzortung_host_invalid"));
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

    fn dry_run_controller(id: &str, default: bool) -> ControllerEntry {
        serde_json::from_value(serde_json::json!({
            "id": id, "default": default, "enabled": true,
            "kind": "dry_run", "config": {"simulate_runs": false},
        }))
        .unwrap()
    }

    #[test]
    fn single_controller_no_default_is_not_an_error() {
        // Bug #9: a lone controller with no default is auto-fixed at apply, so
        // Review must NOT flag it (flagging would block the happy path).
        let mut cfg = base();
        cfg.controllers.push(dry_run_controller("os", false));
        let r = validate(&cfg);
        assert!(
            !r.errors
                .iter()
                .any(|i| i.code == "controller_default_missing"),
            "a single zero-default controller is auto-markable, not a Review error"
        );
    }

    #[test]
    fn two_controllers_no_default_errors() {
        // With two controllers the choice is ambiguous: Review surfaces the
        // same field error the save gate would, so "Save and finish" can't 422
        // out of nowhere.
        let mut cfg = base();
        cfg.controllers.push(dry_run_controller("a", false));
        cfg.controllers.push(dry_run_controller("b", false));
        let r = validate(&cfg);
        assert!(r
            .errors
            .iter()
            .any(|i| i.code == "controller_default_missing"));
    }

    fn zone_value(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "display_name": "Z",
            "area_sqft": 800.0,
            "species": "other",
            "soil_texture": "loam",
            "sprinkler_type": "rotor",
            "controller_id": "",
            "controller_station": "1",
        });
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    #[test]
    fn inverted_moisture_band_errors() {
        // Promoted loader rule: target_min >= saturation is now a field error
        // in Review, not a save-time 422.
        let mut cfg = base();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(zone_value(serde_json::json!({
                "target_min_pct_soil": 80.0,
                "saturation_pct_soil": 60.0,
            })))
            .unwrap(),
        );
        let r = validate(&cfg);
        assert!(r
            .errors
            .iter()
            .any(|i| i.code == "zone_moisture_band_inverted"));
    }

    #[test]
    fn nonpositive_area_is_now_an_error_not_warning() {
        // Was a warning here (so Review passed a config the save 422'd); now an
        // error, matching the loader save gate.
        let mut cfg = base();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(zone_value(serde_json::json!({ "area_sqft": 0.0 }))).unwrap(),
        );
        let r = validate(&cfg);
        assert!(!r.ok());
        assert!(r.errors.iter().any(|i| i.code == "zone_area_nonpositive"));
    }

    #[test]
    fn empty_required_credential_errors() {
        // A polled cloud source with an empty api_key fails at config time
        // instead of deferring to a runtime 401.
        let mut cfg = base();
        cfg.sources.push(
            serde_json::from_value(serde_json::json!({
                "id": "owm",
                "kind": "openweather",
                "config": { "api_key": "" },
            }))
            .unwrap(),
        );
        let r = validate(&cfg);
        assert!(r.errors.iter().any(|i| i.code == "source_credential_empty"));
    }

    #[test]
    fn enabled_weatherkit_with_empty_id_is_rejected() {
        // WeatherKit signs its JWT from key_id/team_id/service_id; an enabled
        // entry with any empty id 401s at Apple. Validate must reject it at save
        // time (the cloud-weather one-click only captures the .p8, so this is the
        // server-side net that stops a dead WeatherKit from saving as enabled).
        let mut cfg = base();
        cfg.sources.push(
            serde_json::from_value(serde_json::json!({
                "id": "wk",
                "kind": "weatherkit",
                "enabled": true,
                "config": {
                    "key_id": "",
                    "team_id": "",
                    "service_id": "",
                    "private_key_pem": "-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----",
                },
            }))
            .unwrap(),
        );
        let r = validate(&cfg);
        assert!(
            !r.ok(),
            "an enabled WeatherKit with empty ids must not save"
        );
        assert!(r
            .errors
            .iter()
            .any(|i| i.code == "weatherkit_ids_incomplete"));
    }

    #[test]
    fn disabled_weatherkit_with_empty_id_is_allowed() {
        // A DISABLED WeatherKit (the stored-but-off state) never authenticates,
        // so empty ids are not yet an error: only enabling it is gated.
        let mut cfg = base();
        cfg.sources.push(
            serde_json::from_value(serde_json::json!({
                "id": "wk",
                "kind": "weatherkit",
                "enabled": false,
                "config": {
                    "key_id": "",
                    "team_id": "",
                    "service_id": "",
                    "private_key_pem": "",
                },
            }))
            .unwrap(),
        );
        let r = validate(&cfg);
        assert!(!r
            .errors
            .iter()
            .any(|i| i.code == "weatherkit_ids_incomplete"));
    }

    #[test]
    fn fully_configured_enabled_weatherkit_passes() {
        // All four pieces present: no incompleteness error.
        let mut cfg = base();
        cfg.sources.push(
            serde_json::from_value(serde_json::json!({
                "id": "wk",
                "kind": "weatherkit",
                "enabled": true,
                "config": {
                    "key_id": "ABC123",
                    "team_id": "TEAM456",
                    "service_id": "com.example.localsky",
                    "private_key_pem": "-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----",
                },
            }))
            .unwrap(),
        );
        let r = validate(&cfg);
        assert!(!r
            .errors
            .iter()
            .any(|i| i.code == "weatherkit_ids_incomplete"));
    }

    #[test]
    fn empty_controller_id_zone_is_allowed() {
        // A weather-only / no-irrigation-hardware zone (empty controller_id)
        // is a first-class setup: no zone_controller_missing error.
        let mut cfg = base();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(zone_value(serde_json::json!({}))).unwrap(),
        );
        let r = validate(&cfg);
        assert!(!r.errors.iter().any(|i| i.code == "zone_controller_missing"));
    }
}
