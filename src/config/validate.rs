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
    /// One-line summary of the blocking errors, for flat `detail` fields
    /// (curl users and any client that does not unpack `validation.errors`).
    /// The structured report stays the canonical payload.
    pub fn error_summary(&self) -> String {
        match self.errors.as_slice() {
            [] => String::new(),
            [one] => one.detail.clone(),
            [first, rest @ ..] => {
                format!("{} (and {} more)", first.detail, rest.len())
            }
        }
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

    // Rachio poll interval must respect the cloud's request budget: the
    // adapter clamps to a 60s floor at use, but a config asking for less is
    // a misunderstanding worth rejecting at Review (a 10s poll would spend
    // the ~1700 daily requests before mid-morning). The upper bound guards
    // an accidental huge value (unit confusion) that would freeze status.
    for c in &cfg.controllers {
        if let crate::config::schema::ControllerKind::Rachio(rc) = &c.controller {
            if let Some(s) = rc.poll_interval_s {
                if !(60..=3600).contains(&s) {
                    r.error(
                        "controller_poll_interval_invalid",
                        format!(
                            "controller '{}': poll_interval_s {} is outside 60..=3600 seconds \
                             (Rachio's cloud allows roughly 1700 requests per day; 120 is the \
                             default)",
                            c.id, s
                        ),
                    );
                }
            }
        }
    }

    // Cloud zone-map keys that match no configured zone (after the same
    // hyphen-to-underscore normalization dispatch uses) bind nothing: the
    // entry sits in the config while the zone it was meant for never
    // waters. Warning, not error: controllers are often added before zones
    // exist, and an extra map entry is harmless by itself.
    {
        let zone_norm: std::collections::HashSet<String> =
            cfg.zones.keys().map(|z| z.replace('-', "_")).collect();
        for c in &cfg.controllers {
            let keys: Vec<&String> = match &c.controller {
                crate::config::schema::ControllerKind::Rachio(rc) => {
                    rc.zone_uuid_map.keys().collect()
                }
                crate::config::schema::ControllerKind::Hydrawise(hc) => {
                    hc.zone_relay_map.keys().collect()
                }
                crate::config::schema::ControllerKind::Bhyve(bc) => {
                    bc.zone_station_map.keys().collect()
                }
                crate::config::schema::ControllerKind::Rainbird(rb) => {
                    rb.zone_station_map.keys().collect()
                }
                _ => continue,
            };
            for k in keys {
                if !zone_norm.contains(&k.replace('-', "_")) {
                    r.warn(
                        "controller_zone_map_key_unmatched",
                        format!(
                            "controller '{}': zone map key '{}' matches no configured zone \
                             (slugs compare with hyphens normalized to underscores); nothing \
                             dispatches through it",
                            c.id, k
                        ),
                    );
                }
            }
        }
    }

    // The other direction, and the one that actually costs a user water: a
    // zone bound to a real controller by NEITHER path. Either its station
    // field is empty, or it holds a value this controller kind cannot use
    // (a Rachio UUID left behind on a zone moved to Hydrawise), and the
    // controller's own zone map has no entry under its slug either. Every
    // dispatch answers zone_unknown and the zone silently never waters.
    // Until now such a config validated completely clean.
    //
    // Warning, not error, for the same reason the map-key check is one:
    // validate() is a strict superset of the load/save gate, so an error
    // here would make a previously-loadable config unloadable and drop the
    // install to env_compat defaults. The hard refusal already lives where
    // it belongs, in the 400 zone_unknown body at dispatch time.
    for (slug, z) in &cfg.zones {
        use crate::config::schema::ControllerKind as K;
        if z.controller_id.is_empty() {
            continue;
        }
        let Some(c) = cfg.controllers.iter().find(|c| c.id == z.controller_id) else {
            continue; // already a zone_controller_missing error
        };
        // ESPHome native has no adapter: build_controllers warn-skips the
        // whole controller, so NEITHER binding fires and testing them says
        // nothing useful. Name the real problem instead.
        if matches!(c.controller, K::EsphomeNative(_)) {
            r.warn(
                "zone_controller_not_built",
                format!(
                    "zone '{slug}' is on controller '{}', whose ESPHome native adapter is \
                     not built yet, so nothing waters this zone whatever it is bound to. \
                     Move it to the MQTT or DIY board controller kind.",
                    c.id
                ),
            );
            continue;
        }
        // A non-empty station is evidence of a binding only when the kind
        // can actually dispatch that SHAPE. `station_is_dispatchable` is the
        // same code `runtime::build_controllers` binds with, so this check
        // and dispatch can never disagree; it answers None for the kinds
        // that do not read the field at all, which is exactly MQTT (its
        // per-zone value is a command struct) and dry run (any slug runs).
        //
        // A value the kind's parser rejects is the issue #8 shape one step
        // on: it LOOKS like a binding, so nothing flagged it, and it only
        // failed at dispatch. The clearest live example is a Rachio UUID
        // left in a zone that was moved to a Hydrawise controller.
        let station = z.controller_station.trim();
        let shape = if station.is_empty() {
            None
        } else {
            crate::station_id::station_is_dispatchable(c.controller.kind_str(), station)
        };
        if shape == Some(true) {
            continue;
        }
        // The controller's own zone map still binds the zone, whatever is
        // sitting in the station field: `overlay_zone_entries` warn-skips an
        // unparseable value and leaves the mapped entry in place, so the
        // zone waters and there is nothing to report.
        if controller_zone_map_covers(&c.controller, slug) {
            continue;
        }
        if shape == Some(false) {
            let expects = crate::station_id::station_expectation(c.controller.kind_str())
                .unwrap_or("an id of its own");
            r.warn(
                "zone_station_unparseable",
                format!(
                    "zone '{slug}': Controller station is '{station}', which controller '{}' \
                     ({}) cannot use. It expects {expects}. The value is ignored at dispatch \
                     and no zone map entry covers this zone, so nothing waters it. Open the \
                     zone in Settings, then Zones, and set Controller station to one of that \
                     controller's own zones.",
                    c.id,
                    c.controller.kind_str()
                ),
            );
            continue;
        }
        let detail = if matches!(c.controller, K::MqttCommand(_)) {
            format!(
                "zone '{slug}' has no entry in controller '{}'s zone_command_map, so nothing \
                 waters this zone. An MQTT zone binds by command topic and payloads, which \
                 the Controller station field cannot carry: add it under Settings, then \
                 Devices, then Advanced.",
                c.id
            )
        } else {
            format!(
                "zone '{slug}' has no station on controller '{}' and that controller's zone \
                 map has no entry for it, so nothing waters this zone. Open the zone in \
                 Settings, then Zones, and pick the controller's zone in the Controller \
                 station field.",
                c.id
            )
        };
        r.warn("zone_unbound", detail);
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
        // Per-zone run limit stays inside a physically sane band; None (the
        // 60 minute default) is always fine. Values above 60 are allowed
        // here on purpose: the raise is confirmed in the UI at save time,
        // and a validator warning would nag on every later save.
        if let Some(m) = z.max_run_minutes {
            if !(5..=360).contains(&m) {
                r.error(
                    "zone_max_run_minutes_range",
                    format!(
                        "zone '{slug}': max run limit {m} min is out of range (5 <= minutes <= \
                         360)"
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
    // this, an empty api_key / access_token sails through here and only
    // surfaces as a runtime 401/400 on the first poll, long after the wizard
    // said "valid". Catch the single-credential kinds (the ones whose one
    // required string is the credential) up front. Multi-field cloud kinds
    // (OAuth client_id/secret pairs, MQTT broker creds, etc.) are left to
    // their adapters since "which subset is required" is kind-specific;
    // these are the unambiguous single-secret sources.
    //
    // NWS / Met.no user_agent is deliberately NOT here: it is an identity,
    // not a credential, and an empty value is VALID (the adapters derive a
    // real per-install UA at request time; see
    // sources::resolve_outbound_user_agent). Requiring it non-empty is what
    // shipped the "you@example.com" template to both agencies.
    for s in &cfg.sources {
        if !s.enabled {
            continue;
        }
        let empty: Option<&'static str> = match &s.source {
            SourceKind::TempestWs(c) if c.access_token.trim().is_empty() => Some("access_token"),
            SourceKind::OpenWeather(c) if c.api_key.trim().is_empty() => Some("api_key"),
            SourceKind::PirateWeather(c) if c.api_key.trim().is_empty() => Some("api_key"),
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

    // Per-field source overrides/chains + the forecast-provider pin must
    // reference canonical field names and configured source ids. The merge
    // layer deliberately SKIPS anything it can't resolve (runtime::
    // field_override_map / field_chain_map drop unknown keys and dead ids;
    // forecast_priority_map ignores an unresolvable pin) so a bad entry can
    // never blank a reading, but that also means a typo'd field name or a
    // stale id has ZERO runtime signal: the user believes "rain is pinned to
    // MRMS then NWS" while plain priority arbitration quietly drives skip
    // decisions. Surface them here as WARNINGS (degraded-but-runnable,
    // matching zone_soil_source_missing): blocking a save over a leftover id
    // from a deleted source would be hostile.
    //
    // Values are validated against config SOURCE IDS only: the merge resolves
    // a chain/override entry by config id (runtime::field_chain_map keys its
    // lookup table by `entry.id`) and only THEN translates a TempestUdp id to
    // its writer label, so the literal label ("Tempest") is never a valid
    // config value and needs no special case here.
    let enabled_source_ids: std::collections::HashSet<&str> = cfg
        .sources
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.id.as_str())
        .collect();
    for (field, source_id) in &cfg.field_source_overrides {
        if crate::config::field_overrides::parse_field_name(field).is_none() {
            r.warn(
                "field_override_unknown_field",
                format!(
                    "field_source_overrides key '{field}' is not a known weather field name and \
                     is ignored (valid names are the snake_case field ids, e.g. 'wind_mph', \
                     'rain_today_in')"
                ),
            );
            continue;
        }
        if !source_ids.contains(source_id.as_str()) {
            r.warn(
                "field_override_source_missing",
                format!(
                    "field_source_overrides['{field}'] references source '{source_id}' which \
                     does not exist; the override is ignored and priority arbitration applies"
                ),
            );
        } else if !enabled_source_ids.contains(source_id.as_str()) {
            r.warn(
                "field_override_source_disabled",
                format!(
                    "field_source_overrides['{field}'] references source '{source_id}' which is \
                     disabled; the override is ignored until the source is re-enabled"
                ),
            );
        }
    }
    for (field, chain) in &cfg.field_source_chains {
        if crate::config::field_overrides::parse_field_name(field).is_none() {
            r.warn(
                "field_chain_unknown_field",
                format!(
                    "field_source_chains key '{field}' is not a known weather field name and the \
                     whole chain is ignored (valid names are the snake_case field ids, e.g. \
                     'wind_mph', 'rain_today_in')"
                ),
            );
            continue;
        }
        for source_id in chain {
            if !source_ids.contains(source_id.as_str()) {
                r.warn(
                    "field_chain_source_missing",
                    format!(
                        "field_source_chains['{field}'] entry '{source_id}' does not match any \
                         configured source id and is dropped from the chain"
                    ),
                );
            } else if !enabled_source_ids.contains(source_id.as_str()) {
                r.warn(
                    "field_chain_source_disabled",
                    format!(
                        "field_source_chains['{field}'] entry '{source_id}' is disabled and is \
                         dropped from the chain until the source is re-enabled"
                    ),
                );
            }
        }
    }
    // The forecast pin only engages for an ENABLED, forecast-capable source
    // (runtime::forecast_priority_map bumps it to the winning priority only
    // when its id is in the enabled-forecast map); anything else is silently
    // inert and the per-source priority order applies.
    if let Some(pin) = cfg.forecast_provider.as_deref() {
        match cfg.sources.iter().find(|s| s.id == pin) {
            None => r.warn(
                "forecast_provider_source_missing",
                format!(
                    "forecast_provider references source '{pin}' which does not exist; the pin \
                     is ignored and forecast priority arbitration applies"
                ),
            ),
            Some(s) if !s.enabled => r.warn(
                "forecast_provider_source_disabled",
                format!(
                    "forecast_provider references source '{pin}' which is disabled; the pin is \
                     ignored until the source is re-enabled"
                ),
            ),
            Some(s) if !s.source.is_forecast() => r.warn(
                "forecast_provider_not_forecast_capable",
                format!(
                    "forecast_provider references source '{pin}' which is not a forecast-capable \
                     kind (open_meteo, nws, met_norway, openweather, pirate_weather, weatherkit); \
                     the pin is ignored"
                ),
            ),
            Some(_) => {}
        }
    }

    r
}

/// Whether a controller's own zone map holds an entry for `slug`, the
/// fallback half of the binding.
///
/// Hyphens normalize to underscores on BOTH sides, matching
/// `runtime::overlay_zone_entries`, `zones::from_pairs`, and the map-key
/// check above; a config keyed "back-yard" binds the zone dispatched as
/// "back_yard". `mqtt_command` counts here even though its value is a
/// command struct rather than a station string, because a mapped MQTT zone
/// genuinely does dispatch. Kinds with no map of their own (OpenSprinkler,
/// the DIY HTTP board) answer false: their binding is the zone's station
/// field and nothing else, which is exactly what the caller is already
/// testing. `dry_run` answers TRUE because it needs no binding at all, and
/// `esphome_native` answers false because its adapter is never built.
pub fn controller_zone_map_covers(
    kind: &crate::config::schema::ControllerKind,
    slug: &str,
) -> bool {
    use crate::config::schema::ControllerKind as K;
    let want = slug.replace('-', "_");
    let has = |keys: Vec<&String>| keys.into_iter().any(|k| k.replace('-', "_") == want);
    match kind {
        K::Rachio(c) => has(c.zone_uuid_map.keys().collect()),
        K::Hydrawise(c) => has(c.zone_relay_map.keys().collect()),
        K::Bhyve(c) => has(c.zone_station_map.keys().collect()),
        K::Rainbird(c) => has(c.zone_station_map.keys().collect()),
        K::HaServiceCall(c) => has(c.zone_entity_map.keys().collect()),
        K::MqttCommand(c) => has(c.zone_command_map.keys().collect()),
        // Simulated hardware accepts any slug: DryRunController::run_zone
        // consults no station and no map and never answers ZoneUnknown, so a
        // blank station on it is not an unbound zone. Reported as covered so
        // this function's only caller stays quiet, matching
        // `station_help("dry_run")`.
        K::DryRun(_) => true,
        // ESPHome native is never constructed, so a zone_entity_map entry
        // proves nothing. Its caller handles the kind before reaching here.
        K::EsphomeNative(_) => false,
        K::OpensprinklerDirect(_) | K::HttpGeneric(_) => false,
    }
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
    fn zone_max_run_minutes_gate_rejects_out_of_band_only() {
        let zone = |cap: serde_json::Value| {
            serde_json::from_value::<ZoneConfig>(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 800.0,
                "species": "other",
                "soil_texture": "loam",
                "sprinkler_type": "rotor",
                "controller_id": "ghost",
                "controller_station": "1",
                "max_run_minutes": cap,
            }))
            .unwrap()
        };
        let with = |cap: serde_json::Value| {
            let mut cfg = base();
            cfg.zones.insert("front".into(), zone(cap));
            validate(&cfg)
        };
        for bad in [serde_json::json!(4), serde_json::json!(400)] {
            let r = with(bad.clone());
            assert!(
                r.errors
                    .iter()
                    .any(|i| i.code == "zone_max_run_minutes_range"),
                "cap {bad} must fail the range gate"
            );
        }
        // In-band values pass, including above 60: the raise is confirmed in
        // the UI at save time, never blocked or warned about here (a warning
        // would nag on every later save).
        for ok in [
            serde_json::json!(5),
            serde_json::json!(90),
            serde_json::json!(360),
            serde_json::Value::Null,
        ] {
            let r = with(ok.clone());
            assert!(
                !r.errors
                    .iter()
                    .any(|i| i.code == "zone_max_run_minutes_range"),
                "cap {ok} must pass the range gate"
            );
            assert!(
                !r.warnings
                    .iter()
                    .any(|i| i.code == "zone_max_run_minutes_range"),
                "an in-band cap must not warn either"
            );
        }
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
    fn cloud_zone_map_key_unmatched_warns() {
        fn rachio_with_map(map: serde_json::Value) -> ControllerEntry {
            serde_json::from_value(serde_json::json!({
                "id": "rachio_main", "default": true, "enabled": true,
                "kind": "rachio", "config": {
                    "api_token": "example-token",
                    "device_id": "device-0001",
                    "zone_uuid_map": map,
                },
            }))
            .unwrap()
        }
        // A key with no matching zone warns; a hyphenated key matching a
        // hyphenated zone (both normalize the same) stays quiet.
        let mut cfg = base();
        cfg.zones.insert(
            "back-yard".to_string(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Back",
                "area_sqft": 1000.0,
                "species": "other",
                "soil_texture": "loam",
                "sprinkler_type": "rotor",
                "controller_id": "rachio_main",
                "controller_station": "uuid-1",
            }))
            .unwrap(),
        );
        cfg.controllers.push(rachio_with_map(serde_json::json!({
            "back_yard": "uuid-1",
            "ghost_zone": "uuid-2",
        })));
        let r = validate(&cfg);
        let warned: Vec<&String> = r
            .warnings
            .iter()
            .filter(|i| i.code == "controller_zone_map_key_unmatched")
            .map(|i| &i.detail)
            .collect();
        assert_eq!(
            warned.len(),
            1,
            "exactly the unmatched key warns: {warned:?}"
        );
        assert!(warned[0].contains("ghost_zone"));
    }

    // ---- zone_unbound ----

    fn zone_json(controller_id: &str, station: &str) -> crate::config::schema::ZoneConfig {
        serde_json::from_value(serde_json::json!({
            "display_name": "Zone",
            "area_sqft": 1000.0,
            "species": "other",
            "soil_texture": "loam",
            "sprinkler_type": "rotor",
            "controller_id": controller_id,
            "controller_station": station,
        }))
        .unwrap()
    }

    fn rachio_entry(map: serde_json::Value) -> ControllerEntry {
        serde_json::from_value(serde_json::json!({
            "id": "rachio_main", "default": true, "enabled": true,
            "kind": "rachio", "config": {
                "api_token": "example-token",
                "device_id": "device-0001",
                "zone_uuid_map": map,
            },
        }))
        .unwrap()
    }

    /// A zone bound by neither path validated completely clean before this,
    /// and simply never watered. It now says so.
    #[test]
    fn a_zone_bound_by_neither_path_warns_unbound() {
        let mut cfg = base();
        cfg.controllers.push(rachio_entry(serde_json::json!({})));
        cfg.zones
            .insert("front_yard".into(), zone_json("rachio_main", ""));
        let r = validate(&cfg);
        assert!(r.ok(), "unbound is a warning, never an error");
        let warned: Vec<&String> = r
            .warnings
            .iter()
            .filter(|i| i.code == "zone_unbound")
            .map(|i| &i.detail)
            .collect();
        assert_eq!(warned.len(), 1, "the unbound zone warns once: {warned:?}");
        assert!(warned[0].contains("front_yard"));
        assert!(
            warned[0].contains("nothing waters this zone"),
            "say the consequence, not just the state: {}",
            warned[0]
        );
    }

    /// The issue #8 reporter's shape must NOT warn: his zone map covers his
    /// zones, so his zones water even with every station field blank. A
    /// warning there would tell a working install it is broken.
    #[test]
    fn a_zone_covered_only_by_the_controller_zone_map_is_not_unbound() {
        let mut cfg = base();
        cfg.controllers.push(rachio_entry(serde_json::json!({
            "front_yard": "1f00aa00-0000-4000-8000-0000000000a1",
            // Hyphens normalize on both sides, exactly like dispatch.
            "back-yard": "1f00aa00-0000-4000-8000-0000000000a2",
        })));
        cfg.zones
            .insert("front_yard".into(), zone_json("rachio_main", ""));
        cfg.zones
            .insert("back_yard".into(), zone_json("rachio_main", ""));
        let r = validate(&cfg);
        assert!(
            !r.warnings.iter().any(|i| i.code == "zone_unbound"),
            "a map-covered zone is bound: {:?}",
            r.warnings
        );
    }

    #[test]
    fn a_zone_with_a_station_is_never_unbound_and_neither_is_a_zone_with_no_controller() {
        let mut cfg = base();
        cfg.controllers.push(rachio_entry(serde_json::json!({})));
        cfg.zones.insert(
            "front_yard".into(),
            zone_json("rachio_main", "1f00aa00-0000-4000-8000-0000000000a1"),
        );
        // A zone with no controller at all already has its own signal (the
        // "No controller" badge and the required-controller save gate); a
        // second warning on top of it is noise.
        cfg.zones.insert("orphan".into(), zone_json("", ""));
        // A zone naming a controller that does not exist is already an
        // error (zone_controller_missing); it must not also warn unbound.
        cfg.zones.insert("ghost".into(), zone_json("gone", ""));
        let r = validate(&cfg);
        assert!(
            !r.warnings.iter().any(|i| i.code == "zone_unbound"),
            "{:?}",
            r.warnings
        );
    }

    fn controller_of(id: &str, kind: &str, config: serde_json::Value) -> ControllerEntry {
        serde_json::from_value(serde_json::json!({
            "id": id, "default": true, "enabled": true,
            "kind": kind, "config": config,
        }))
        .unwrap()
    }

    /// MQTT is the one kind that never reads the station field, so a topic
    /// or an entity id typed there (which older builds invited, showing the
    /// same free-text box for every kind) binds nothing. Treating it as
    /// evidence certified the exact silent-never-waters state this warning
    /// exists to surface.
    #[test]
    fn an_mqtt_zone_with_a_station_but_no_command_map_still_warns_unbound() {
        let mut cfg = base();
        cfg.controllers.push(controller_of(
            "mqtt_main",
            "mqtt_command",
            serde_json::json!({ "broker_host": "broker.example", "zone_command_map": {} }),
        ));
        // The v0.1 upgrade path stamps "1".."4" into these.
        cfg.zones
            .insert("front_yard".into(), zone_json("mqtt_main", "1"));
        let r = validate(&cfg);
        let warned: Vec<&String> = r
            .warnings
            .iter()
            .filter(|i| i.code == "zone_unbound")
            .map(|i| &i.detail)
            .collect();
        assert_eq!(warned.len(), 1, "{:?}", r.warnings);
        assert!(
            warned[0].contains("zone_command_map"),
            "point at the map, not the station field: {}",
            warned[0]
        );
    }

    #[test]
    fn an_mqtt_zone_with_a_command_map_entry_is_bound() {
        let mut cfg = base();
        cfg.controllers.push(controller_of(
            "mqtt_main",
            "mqtt_command",
            serde_json::json!({
                "broker_host": "broker.example",
                "zone_command_map": {
                    "front_yard": { "topic": "t", "on_payload": "ON", "off_payload": "OFF" }
                }
            }),
        ));
        cfg.zones
            .insert("front_yard".into(), zone_json("mqtt_main", ""));
        let r = validate(&cfg);
        assert!(
            !r.warnings.iter().any(|i| i.code == "zone_unbound"),
            "{:?}",
            r.warnings
        );
    }

    /// Simulated hardware accepts any slug and never answers ZoneUnknown, so
    /// badging every dry_run zone Unbound trained an evaluating user to
    /// ignore the badge before they ever attached real hardware.
    #[test]
    fn a_dry_run_zone_with_no_station_is_never_unbound() {
        let mut cfg = base();
        cfg.controllers.push(controller_of(
            "demo_controller",
            "dry_run",
            serde_json::json!({ "simulate_runs": false }),
        ));
        cfg.zones
            .insert("front_yard".into(), zone_json("demo_controller", ""));
        let r = validate(&cfg);
        assert!(
            !r.warnings.iter().any(|i| i.code == "zone_unbound"),
            "{:?}",
            r.warnings
        );
    }

    /// The ESPHome adapter is never constructed, so testing its bindings
    /// says nothing. Name the real problem instead.
    #[test]
    fn an_esphome_zone_reports_the_missing_adapter_not_a_binding() {
        let mut cfg = base();
        cfg.controllers.push(controller_of(
            "esphome_main",
            "esphome_native",
            serde_json::json!({
                "host": "192.0.2.60",
                "password": null,
                // Even WITH a map entry, nothing waters.
                "zone_entity_map": { "front_yard": "switch.front_yard" }
            }),
        ));
        cfg.zones
            .insert("front_yard".into(), zone_json("esphome_main", ""));
        let r = validate(&cfg);
        assert!(r.ok(), "still a warning, never an error");
        let warned: Vec<&String> = r
            .warnings
            .iter()
            .filter(|i| i.code == "zone_controller_not_built")
            .map(|i| &i.detail)
            .collect();
        assert_eq!(warned.len(), 1, "{:?}", r.warnings);
        assert!(warned[0].contains("not built"), "{}", warned[0]);
        assert!(
            !r.warnings.iter().any(|i| i.code == "zone_unbound"),
            "one honest warning, not two: {:?}",
            r.warnings
        );
    }

    const UUID_A: &str = "1f00aa00-0000-4000-8000-0000000000a1";

    /// THE CASE THAT PRODUCED ISSUE #8, one step on. A zone moved from a
    /// Rachio to a Hydrawise keeps the UUID in its station field. Hydrawise
    /// addresses zones by relay NUMBER, so dispatch ignores it and the zone
    /// silently never waters, while the config check called it bound because
    /// the field was not empty.
    #[test]
    fn a_rachio_uuid_left_in_a_hydrawise_zone_warns_instead_of_reading_as_bound() {
        let mut cfg = base();
        cfg.controllers.push(controller_of(
            "hydrawise_main",
            "hydrawise",
            serde_json::json!({
                "api_key": "example-key",
                "controller_id": 7,
                "zone_relay_map": {}
            }),
        ));
        cfg.zones
            .insert("front_yard".into(), zone_json("hydrawise_main", UUID_A));
        let r = validate(&cfg);
        assert!(r.ok(), "a warning, never an error");
        let warned: Vec<&String> = r
            .warnings
            .iter()
            .filter(|i| i.code == "zone_station_unparseable")
            .map(|i| &i.detail)
            .collect();
        assert_eq!(warned.len(), 1, "{:?}", r.warnings);
        assert!(warned[0].contains("front_yard"), "{}", warned[0]);
        assert!(warned[0].contains(UUID_A), "name the offending value");
        assert!(warned[0].contains("hydrawise"), "name the kind");
        assert!(
            warned[0].contains("relay id"),
            "say what the kind expects: {}",
            warned[0]
        );
        assert!(
            warned[0].contains("nothing waters it"),
            "say the consequence: {}",
            warned[0]
        );
        // Not ALSO reported as plain unbound: one honest warning.
        assert!(!r.warnings.iter().any(|i| i.code == "zone_unbound"));
    }

    /// The reverse, and the original defect: a station NUMBER on a Rachio
    /// zone. Rachio addresses zones by UUID only.
    #[test]
    fn a_station_number_on_a_rachio_zone_warns_unparseable() {
        let mut cfg = base();
        cfg.controllers.push(rachio_entry(serde_json::json!({})));
        cfg.zones
            .insert("front_yard".into(), zone_json("rachio_main", "3"));
        let r = validate(&cfg);
        let warned: Vec<&String> = r
            .warnings
            .iter()
            .filter(|i| i.code == "zone_station_unparseable")
            .map(|i| &i.detail)
            .collect();
        assert_eq!(warned.len(), 1, "{:?}", r.warnings);
        assert!(warned[0].contains("UUID"), "{}", warned[0]);
    }

    /// A junk station whose zone IS covered by the controller's own map
    /// keeps watering: the overlay warn-skips the value and leaves the
    /// mapped entry in place. Nothing to report.
    #[test]
    fn an_unparseable_station_is_silent_when_the_map_still_covers_the_zone() {
        let mut cfg = base();
        cfg.controllers.push(rachio_entry(serde_json::json!({
            "front_yard": UUID_A,
        })));
        cfg.zones
            .insert("front_yard".into(), zone_json("rachio_main", "3"));
        let r = validate(&cfg);
        assert!(
            !r.warnings
                .iter()
                .any(|i| i.code == "zone_station_unparseable" || i.code == "zone_unbound"),
            "the map still binds this zone: {:?}",
            r.warnings
        );
    }

    #[test]
    fn a_valid_station_for_each_kind_never_warns() {
        for (kind, config, station) in [
            (
                "rachio",
                serde_json::json!({ "api_token": "t", "device_id": "d" }),
                UUID_A,
            ),
            (
                "hydrawise",
                serde_json::json!({ "api_key": "k", "controller_id": 7 }),
                "42",
            ),
            (
                "bhyve",
                serde_json::json!({ "email": "e@example.com", "password": "p", "device_id": "d" }),
                "2",
            ),
            (
                "rainbird",
                serde_json::json!({ "email": "e@example.com", "password": "p", "controller_id": "c" }),
                "2",
            ),
            (
                "opensprinkler_direct",
                serde_json::json!({ "host": "192.0.2.10", "password_md5": "" }),
                "1",
            ),
            (
                "http_generic",
                serde_json::json!({ "base_url": "http://192.0.2.50" }),
                "back_yard",
            ),
            (
                "ha_service_call",
                serde_json::json!({ "base_url": "http://ha.example:8123", "bearer_token": "t" }),
                "switch.front_yard",
            ),
        ] {
            let mut cfg = base();
            cfg.controllers.push(controller_of("c1", kind, config));
            cfg.zones
                .insert("front_yard".into(), zone_json("c1", station));
            let r = validate(&cfg);
            assert!(
                !r.warnings
                    .iter()
                    .any(|i| i.code == "zone_station_unparseable" || i.code == "zone_unbound"),
                "{kind} with station {station:?} must validate clean: {:?}",
                r.warnings
            );
        }
    }

    /// OpenSprinkler stations count from 1: a 0 aliases onto station 1 at
    /// the wire, so build_controllers drops the mapping and the zone is
    /// unbound. The check must agree rather than call it a valid number.
    #[test]
    fn opensprinkler_station_zero_is_not_a_binding() {
        let mut cfg = base();
        cfg.controllers.push(controller_of(
            "os_main",
            "opensprinkler_direct",
            serde_json::json!({ "host": "192.0.2.10", "password_md5": "" }),
        ));
        cfg.zones
            .insert("front_yard".into(), zone_json("os_main", "0"));
        let r = validate(&cfg);
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "zone_station_unparseable"));
    }

    /// The kinds that ignore the station field keep the semantics built for
    /// them: MQTT reports its own map, dry run is never unbound at all.
    #[test]
    fn mqtt_and_dry_run_keep_their_own_semantics_under_the_shape_check() {
        // Junk in an MQTT station is not a SHAPE problem, it is a field that
        // binds nothing: the message must still point at zone_command_map.
        let mut cfg = base();
        cfg.controllers.push(controller_of(
            "mqtt_main",
            "mqtt_command",
            serde_json::json!({ "broker_host": "b", "zone_command_map": {} }),
        ));
        cfg.zones
            .insert("front_yard".into(), zone_json("mqtt_main", UUID_A));
        let r = validate(&cfg);
        assert!(!r
            .warnings
            .iter()
            .any(|i| i.code == "zone_station_unparseable"));
        let warned: Vec<&String> = r
            .warnings
            .iter()
            .filter(|i| i.code == "zone_unbound")
            .map(|i| &i.detail)
            .collect();
        assert_eq!(warned.len(), 1, "{:?}", r.warnings);
        assert!(warned[0].contains("zone_command_map"), "{}", warned[0]);

        // Dry run accepts any slug, so nothing about its station matters.
        let mut cfg = base();
        cfg.controllers.push(controller_of(
            "demo_controller",
            "dry_run",
            serde_json::json!({ "simulate_runs": false }),
        ));
        cfg.zones
            .insert("front_yard".into(), zone_json("demo_controller", UUID_A));
        let r = validate(&cfg);
        assert!(
            !r.warnings
                .iter()
                .any(|i| i.code == "zone_station_unparseable" || i.code == "zone_unbound"),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn zone_map_coverage_matches_dispatch_for_every_kind() {
        use crate::config::schema::ControllerKind as K;
        let of = |kind: &str, config: serde_json::Value| -> K {
            serde_json::from_value(serde_json::json!({ "kind": kind, "config": config })).unwrap()
        };
        let rachio = of(
            "rachio",
            serde_json::json!({
                "api_token": "t", "device_id": "d",
                "zone_uuid_map": { "back-yard": "1f00aa00-0000-4000-8000-0000000000a1" }
            }),
        );
        // Hyphen/underscore normalizes on BOTH sides.
        assert!(controller_zone_map_covers(&rachio, "back_yard"));
        assert!(controller_zone_map_covers(&rachio, "back-yard"));
        assert!(!controller_zone_map_covers(&rachio, "front_yard"));

        let ha = of(
            "ha_service_call",
            serde_json::json!({
                "base_url": "http://ha.example:8123", "bearer_token": "t",
                "zone_entity_map": { "front_yard": "switch.front_yard_valve" }
            }),
        );
        assert!(controller_zone_map_covers(&ha, "front_yard"));

        // MQTT counts: its map value is a struct, but a mapped MQTT zone
        // genuinely dispatches, so it must not read as unbound.
        let mqtt = of(
            "mqtt_command",
            serde_json::json!({
                "broker_host": "broker.example",
                "zone_command_map": {
                    "front_yard": { "topic": "t", "on_payload": "ON", "off_payload": "OFF" }
                }
            }),
        );
        assert!(controller_zone_map_covers(&mqtt, "front_yard"));

        // The two map-less kinds bind by the station field alone, so the
        // map never covers anything for them.
        for (kind, config) in [
            (
                "opensprinkler_direct",
                serde_json::json!({ "host": "192.0.2.10", "password_md5": "" }),
            ),
            (
                "http_generic",
                serde_json::json!({ "base_url": "http://192.0.2.50" }),
            ),
        ] {
            assert!(
                !controller_zone_map_covers(&of(kind, config), "front_yard"),
                "{kind} holds no zone map"
            );
        }
        // Dry run needs no binding at all, so it reports as covered and the
        // unbound check stays quiet for it.
        assert!(controller_zone_map_covers(
            &of("dry_run", serde_json::json!({ "simulate_runs": false })),
            "front_yard"
        ));
        // ESPHome native holds a zone_entity_map but is never constructed,
        // so an entry in it certifies nothing.
        assert!(!controller_zone_map_covers(
            &of(
                "esphome_native",
                serde_json::json!({
                    "host": "192.0.2.60",
                    "password": null,
                    "zone_entity_map": { "front_yard": "switch.front_yard" }
                })
            ),
            "front_yard"
        ));
    }

    #[test]
    fn rachio_poll_interval_range_gate() {
        fn rachio_with_poll(poll: serde_json::Value) -> ControllerEntry {
            serde_json::from_value(serde_json::json!({
                "id": "rachio_main", "default": true, "enabled": true,
                "kind": "rachio", "config": {
                    "api_token": "example-token",
                    "device_id": "device-0001",
                    "poll_interval_s": poll,
                },
            }))
            .unwrap()
        }
        // Below the floor: rejected with the coded error.
        let mut cfg = base();
        cfg.controllers
            .push(rachio_with_poll(serde_json::json!(10)));
        let r = validate(&cfg);
        assert!(r
            .errors
            .iter()
            .any(|i| i.code == "controller_poll_interval_invalid"));
        // In range: clean. Absent (null): clean, the 120s default applies.
        for poll in [serde_json::json!(120), serde_json::Value::Null] {
            let mut cfg = base();
            cfg.controllers.push(rachio_with_poll(poll));
            let r = validate(&cfg);
            assert!(
                !r.errors
                    .iter()
                    .any(|i| i.code == "controller_poll_interval_invalid"),
                "in-range/absent poll interval must validate clean"
            );
        }
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
    fn empty_keyless_authority_user_agent_is_valid() {
        // NWS / Met.no user_agent is an identity, not a credential: empty is
        // now VALID and means "auto-derive the instance UA at request time".
        // The old non-empty requirement is what pushed the you@example.com
        // template into configs and onto both agencies' wires.
        let mut cfg = base();
        for (id, kind) in [("nws", "nws"), ("metno", "met_norway")] {
            cfg.sources.push(
                serde_json::from_value(serde_json::json!({
                    "id": id,
                    "kind": kind,
                    "enabled": true,
                    "config": { "user_agent": "" },
                }))
                .unwrap(),
            );
        }
        let r = validate(&cfg);
        assert!(
            !r.errors.iter().any(|i| i.code == "source_credential_empty"),
            "empty user_agent must not error: {:?}",
            r.errors
        );
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

    fn disabled_openweather_source(id: &str) -> SourceEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "kind": "openweather",
            "enabled": false,
            "config": { "api_key": "k" },
        }))
        .unwrap()
    }

    #[test]
    fn field_override_unknown_field_warns_not_errors() {
        // "rain_today" is the classic typo for "rain_today_in": the merge
        // silently ignores it, so validate must say so (warning, still saves).
        let mut cfg = base();
        cfg.sources.push(open_meteo_source("best_match"));
        cfg.field_source_overrides
            .insert("rain_today".into(), "open_meteo".into());
        let r = validate(&cfg);
        assert!(r.ok(), "an inert override must not block saving");
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "field_override_unknown_field"));
    }

    #[test]
    fn field_override_ghost_source_warns() {
        let mut cfg = base();
        cfg.field_source_overrides
            .insert("wind_mph".into(), "ghost".into());
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "field_override_source_missing"));
    }

    #[test]
    fn field_override_disabled_source_warns() {
        let mut cfg = base();
        cfg.sources.push(disabled_openweather_source("owm"));
        cfg.field_source_overrides
            .insert("wind_mph".into(), "owm".into());
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "field_override_source_disabled"));
    }

    #[test]
    fn field_chain_unknown_field_warns() {
        let mut cfg = base();
        cfg.sources.push(open_meteo_source("best_match"));
        cfg.field_source_chains
            .insert("sharknado".into(), vec!["open_meteo".into()]);
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "field_chain_unknown_field"));
    }

    #[test]
    fn field_chain_ghost_and_disabled_entries_warn_per_entry() {
        // A chain mixing a live id, a deleted id, and a disabled id gets one
        // warning per broken entry (the live entry keeps the chain running).
        let mut cfg = base();
        cfg.sources.push(open_meteo_source("best_match"));
        cfg.sources.push(disabled_openweather_source("owm"));
        cfg.field_source_chains.insert(
            "rain_today_in".into(),
            vec!["open_meteo".into(), "ghost".into(), "owm".into()],
        );
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "field_chain_source_missing"));
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "field_chain_source_disabled"));
    }

    #[test]
    fn valid_overrides_chains_and_forecast_pin_pass_clean() {
        let mut cfg = base();
        cfg.sources.push(open_meteo_source("best_match"));
        cfg.field_source_overrides
            .insert("wind_mph".into(), "open_meteo".into());
        cfg.field_source_chains
            .insert("rain_today_in".into(), vec!["open_meteo".into()]);
        cfg.forecast_provider = Some("open_meteo".into());
        let r = validate(&cfg);
        assert!(r.ok());
        for code in [
            "field_override_unknown_field",
            "field_override_source_missing",
            "field_override_source_disabled",
            "field_chain_unknown_field",
            "field_chain_source_missing",
            "field_chain_source_disabled",
            "forecast_provider_source_missing",
            "forecast_provider_source_disabled",
            "forecast_provider_not_forecast_capable",
        ] {
            assert!(
                !r.warnings.iter().any(|i| i.code == code),
                "clean config must not warn '{code}'"
            );
        }
    }

    #[test]
    fn forecast_provider_ghost_warns() {
        let mut cfg = base();
        cfg.forecast_provider = Some("ghost".into());
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "forecast_provider_source_missing"));
    }

    #[test]
    fn forecast_provider_disabled_warns() {
        let mut cfg = base();
        cfg.sources.push(disabled_openweather_source("owm"));
        cfg.forecast_provider = Some("owm".into());
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "forecast_provider_source_disabled"));
    }

    #[test]
    fn forecast_provider_non_forecast_kind_warns() {
        // An Ecowitt gateway is a live station: pinning the forecast to it can
        // never engage (forecast_priority_map only maps forecast kinds).
        let mut cfg = base();
        cfg.sources.push(
            serde_json::from_value(serde_json::json!({
                "id": "gw",
                "kind": "ecowitt_gw_poll",
                "config": { "host": "192.0.2.61" },
            }))
            .unwrap(),
        );
        cfg.forecast_provider = Some("gw".into());
        let r = validate(&cfg);
        assert!(r.ok());
        assert!(r
            .warnings
            .iter()
            .any(|i| i.code == "forecast_provider_not_forecast_capable"));
    }
}

#[cfg(test)]
mod error_summary_tests {
    use super::*;
    use crate::config::schema::Config;

    #[test]
    fn error_summary_flattens_for_flat_detail_consumers() {
        // Default config fails on the unset location only.
        let r = validate(&Config::default());
        assert!(!r.ok());
        assert!(r.error_summary().contains("location is 0,0"));

        // Multiple errors: first detail + a count, never an empty string.
        // Both coordinates out of range fires lat_range AND lon_range (a
        // lat of 200 with lon 0 is NOT location_unset, which needs 0,0).
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 200.0;
        cfg.deployment.location.lon = 200.0;
        let r = validate(&cfg);
        assert!(r.errors.len() >= 2, "got {:?}", r.errors);
        assert!(r.error_summary().contains("and"));

        // A clean report summarizes to nothing.
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        let r = validate(&cfg);
        assert!(r.ok(), "unexpected errors: {:?}", r.errors);
        assert_eq!(r.error_summary(), "");
    }
}
