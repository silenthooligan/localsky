// Config loader. Reads /data/localsky.toml, interpolates ${VAR} env refs,
// parses + validates, and returns a typed Config. If the file is missing
// the caller falls back to env_compat::synthesize.

use std::env;
use std::path::Path;

use thiserror::Error;

use crate::config::schema::{Config, CURRENT_SCHEMA_VERSION};

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("config file not found: {0}")]
    NotFound(String),
    #[error("io error reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("toml parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("env var ${{{0}}} referenced in config but unset")]
    UnsetEnvVar(String),
    #[error("validation failed: {0}")]
    Validation(String),
    #[error(
        "schema_version {found} > known {known}; refusing to load a config newer than this binary"
    )]
    SchemaTooNew { found: u32, known: u32 },
}

/// Load + validate a Config from the given path. Errors propagate verbatim
/// so the boot path can choose to fall back to env_compat on `NotFound`.
pub fn load_from_path(path: &Path) -> Result<Config, LoadError> {
    let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => LoadError::NotFound(path.display().to_string()),
        _ => LoadError::Io(path.display().to_string(), e),
    })?;
    let interpolated = interpolate_env(&raw)?;
    let mut cfg: Config = toml::from_str(&interpolated)?;
    normalize_legacy_values(&mut cfg);
    validate(&cfg)?;
    Ok(cfg)
}

/// One-time value normalizations for fields whose persisted value was
/// never operator intent. Applied on every load (the next save persists
/// the normalized value), so upgrades cannot silently regress behavior.
///
/// Open-Meteo `past_days == 1` rewrites to 3: before 1.21.0 the fetch
/// HARDCODED 3 past days and ignored this field entirely, while the old
/// serde default and both UI templates stamped an explicit 1 into nearly
/// every persisted config. Honoring the field as-written would therefore
/// drop those installs from a 3-day model archive to 1 on upgrade. A
/// stored 1 was never a value anyone chose against observed behavior;
/// any other value (2..7, or a clamped-out-of-band one) is left alone.
///
/// `sessions_per_week` outside 1..=7 clamps into range. Nothing
/// constrained the field before 0.7.22, so a hand-edited config can carry
/// an 8, and the new `zone_sessions_per_week_range` validation error gates
/// WHOLE-config writes: the raw-TOML save, `PUT /api/config` and the
/// tuning apply all answer 422 while any zone is out of range. Without
/// this, one stale value on disk would refuse every unrelated save, from a
/// different zone's edit to a skip threshold, until the operator found and
/// hand-fixed it. The engine already clamps the same way
/// (`engine::budget::compute_zone`), so this only makes the stored value
/// agree with the value that decides. Writes arriving from outside are
/// still refused by the validator, so nothing is silently coerced on the
/// way in.
pub fn normalize_legacy_values(cfg: &mut Config) {
    use crate::config::schema::SourceKind;
    for src in cfg.sources.iter_mut() {
        if let SourceKind::OpenMeteo(c) = &mut src.source {
            if c.past_days == 1 {
                c.past_days = 3;
            }
        }
    }
    for (slug, z) in cfg.zones.iter_mut() {
        if let Some(n) = z.sessions_per_week {
            if !(1..=7).contains(&n) {
                let clamped = n.clamp(1, 7);
                tracing::warn!(
                    zone = %slug,
                    stored = n,
                    used = clamped,
                    "sessions_per_week is outside 1..=7; treating it as the clamped value"
                );
                z.sessions_per_week = Some(clamped);
            }
        }
    }
    let filled = backfill_zone_stations(cfg);
    if filled > 0 {
        tracing::info!(
            zones = filled,
            "copied the controller's zone-map entry onto unbound zones"
        );
    }
}

/// A controller's zone map flattened to `normalized slug -> station string`,
/// or `None` for a kind that holds no map a `controller_station` string can
/// carry.
///
/// Values are stringified exactly the way each kind's station parser reads
/// them back (`station_id::rachio_zone_id` takes the uuid verbatim,
/// `hydrawise_relay_id` and `station_number` parse a bare number), so a
/// backfilled value round-trips through the overlay unchanged.
///
/// `mqtt_command` is deliberately absent: its per-zone value is a command
/// struct (topic plus on/off payloads), which a single string cannot
/// express. MQTT zones bind in the controller's `zone_command_map` only.
///
/// Keys are hyphen-normalized to underscores, matching every other slug
/// comparison in the codebase (`runtime::overlay_zone_entries`,
/// `validate::validate`, `zones::from_pairs`).
fn controller_zone_map(
    kind: &crate::config::schema::ControllerKind,
) -> Option<std::collections::BTreeMap<String, String>> {
    use crate::config::schema::ControllerKind as K;
    fn norm<V: ToString>(
        m: &std::collections::BTreeMap<String, V>,
    ) -> std::collections::BTreeMap<String, String> {
        m.iter()
            .map(|(k, v)| (k.replace('-', "_"), v.to_string()))
            .collect()
    }
    match kind {
        K::Rachio(c) => Some(norm(&c.zone_uuid_map)),
        K::Hydrawise(c) => Some(norm(&c.zone_relay_map)),
        K::Bhyve(c) => Some(norm(&c.zone_station_map)),
        K::Rainbird(c) => Some(norm(&c.zone_station_map)),
        K::HaServiceCall(c) => Some(norm(&c.zone_entity_map)),
        K::EsphomeNative(c) => Some(norm(&c.zone_entity_map)),
        K::OpensprinklerDirect(_) | K::HttpGeneric(_) | K::DryRun(_) | K::MqttCommand(_) => None,
    }
}

/// Copy a controller's own zone-map entry into a zone whose
/// `controller_station` is empty, so the zone entry becomes the visible,
/// portable binding instead of a coincidence between two key spaces.
///
/// The controller-side zone maps are keyed by the SLUGIFIED VENDOR ZONE
/// NAME (that is what a controller scan writes) while dispatch looks them up
/// by the LocalSky zone slug, so they bind only when the two happen to
/// match. This copies the value that IS currently binding onto the zone,
/// where a person can see it, a picker can change it, and it survives the
/// user renaming their zones on the vendor's side. It never invents a
/// binding: only an entry already keyed by this zone's slug is copied, which
/// is exactly the set dispatch already resolves.
///
/// NEVER overwrites a non-empty `controller_station` (on the numeric kinds a
/// leftover station number already wins over the map; reversing that
/// precedence would silently repoint a valve). Never deletes or rewrites the
/// controller's map.
///
/// PRECEDENCE, PLAINLY. The map keeps binding a zone this pass did not
/// reach: a hand-written key, a key for a zone that does not exist yet, and
/// every zone on a config that is only ever read. But for a zone this pass
/// DID fill, the map is a live fallback only until the next config save.
/// Once the copied station is persisted, `runtime::overlay_zone_entries`
/// gives the zone entry priority for that zone forever, so a later edit to
/// the controller's own map does nothing for it. That is the intended end
/// state (the binding belongs on the zone, where it is visible and
/// editable); the overlay logs a line whenever the two disagree so the flip
/// is never silent. Editing the binding means editing the zone.
///
/// Idempotent: a second run finds every station non-empty and does nothing.
///
/// Returns how many zones were filled in, so the caller can log once.
pub fn backfill_zone_stations(cfg: &mut Config) -> usize {
    let maps: Vec<(String, std::collections::BTreeMap<String, String>)> = cfg
        .controllers
        .iter()
        .filter_map(|c| controller_zone_map(&c.controller).map(|m| (c.id.clone(), m)))
        .filter(|(_, m)| !m.is_empty())
        .collect();
    if maps.is_empty() {
        return 0;
    }
    let mut filled = 0usize;
    for (slug, zone) in cfg.zones.iter_mut() {
        if !zone.controller_station.trim().is_empty() {
            continue;
        }
        let Some((_, map)) = maps.iter().find(|(id, _)| id == &zone.controller_id) else {
            continue;
        };
        let Some(station) = map.get(&slug.replace('-', "_")) else {
            continue;
        };
        if station.trim().is_empty() {
            continue;
        }
        zone.controller_station = station.clone();
        filled += 1;
    }
    filled
}

/// `${VAR}` interpolation. Single pass; nested refs not supported.
/// Escape with `$${VAR}` for a literal dollar.
///
/// UTF-8 SAFETY: the previous implementation iterated `src.as_bytes()`
/// and pushed `byte as char` for every non-marker byte. That treats
/// each UTF-8 continuation byte as a standalone Latin-1 codepoint, so
/// every multi-byte char in the source (em-dashes, accented letters,
/// any non-ASCII string in the TOML) got silently corrupted on load.
/// The fix: byte-scan only for the ASCII `$` markers, but push proper
/// chars from the source string for everything else.
fn interpolate_env(src: &str) -> Result<String, LoadError> {
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Escape: $${VAR} -> ${VAR} literal.
        if c == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        if c == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let close = bytes[i + 2..]
                .iter()
                .position(|&b| b == b'}')
                .ok_or_else(|| {
                    LoadError::Validation("unterminated ${{...}} in config".to_string())
                })?;
            let var_name = std::str::from_utf8(&bytes[i + 2..i + 2 + close])
                .map_err(|_| LoadError::Validation("invalid utf8 in env ref".to_string()))?;
            let val =
                env::var(var_name).map_err(|_| LoadError::UnsetEnvVar(var_name.to_string()))?;
            out.push_str(&val);
            i += 2 + close + 1;
            continue;
        }
        // Non-marker: grab the proper UTF-8 char starting at byte index i,
        // push it whole, advance i by its byte length. This preserves
        // multi-byte sequences (em-dashes, accented letters, etc.).
        let ch = src[i..]
            .chars()
            .next()
            .expect("byte index i is a valid char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

/// Auto-mark the sole controller as default when none is set. The wizard
/// Controllers step (and a hand-written single-controller config) can leave
/// a single controller with `default = false`; the save gate below then
/// hard-rejects it ("at least one controller must have default = true"),
/// which is what made a happy-path single-controller wizard 422 at "Save and
/// finish". When there is EXACTLY ONE controller and none is default, the
/// choice is unambiguous, so mark it. With two or more controllers we do NOT
/// guess: `validate::validate` surfaces a field-level `controller_default_missing`
/// so the operator picks. Idempotent; a no-op when a default already exists.
pub fn auto_default_controller(cfg: &mut Config) {
    if cfg.controllers.len() == 1 && !cfg.controllers.iter().any(|c| c.default) {
        cfg.controllers[0].default = true;
    }
}

/// Basic post-parse invariants. Schema-level validation (types, enum
/// variants, required fields) is already handled by serde; this catches
/// structural things serde can't.
pub fn validate(cfg: &Config) -> Result<(), LoadError> {
    if cfg.schema_version > CURRENT_SCHEMA_VERSION {
        return Err(LoadError::SchemaTooNew {
            found: cfg.schema_version,
            known: CURRENT_SCHEMA_VERSION,
        });
    }

    // CANONICAL RULE SET (bug #9 parity). Every hard rule below is ALSO
    // encoded in validate::validate as a field-level error, so the wizard
    // Review step and this save/load gate can never diverge in the dangerous
    // direction (Review saying "valid" while save 422s). validate::validate is
    // a strict SUPERSET of this gate: anything rejected here is rejected there
    // too. We keep this function's narrower, proven hard-fail set as the
    // load/save gate (rather than delegating wholesale) so the LOAD path,
    // which falls back to env_compat on any error and would otherwise wipe a
    // config to defaults, does not start rejecting previously-loadable files
    // (e.g. an env_compat 0,0 location, a blitzortung radius edge case). The
    // single-controller zero-default case is auto-fixed before save by
    // finalize_for_apply -> auto_default_controller, so it never reaches here
    // from the wizard; a hand-written single-controller file is still caught
    // below for the same runtime-resolution reason it always was.

    // Each source needs a unique id.
    let mut seen = std::collections::HashSet::new();
    for src in &cfg.sources {
        if !seen.insert(&src.id) {
            return Err(LoadError::Validation(format!(
                "duplicate source id: {}",
                src.id
            )));
        }
    }

    // Each controller needs a unique id.
    seen.clear();
    let mut default_count = 0u32;
    for ctrl in &cfg.controllers {
        if !seen.insert(&ctrl.id) {
            return Err(LoadError::Validation(format!(
                "duplicate controller id: {}",
                ctrl.id
            )));
        }
        if ctrl.default {
            default_count += 1;
        }
    }
    if !cfg.controllers.is_empty() && default_count == 0 {
        return Err(LoadError::Validation(
            "at least one controller must have default = true".to_string(),
        ));
    }
    if default_count > 1 {
        return Err(LoadError::Validation(format!(
            "exactly one controller can be default; found {default_count}"
        )));
    }

    // Each zone's controller_id, WHEN SET, must reference a configured
    // controller. An EMPTY controller_id is allowed: a weather-station-only /
    // no-irrigation-hardware zone is a first-class setup (and validate::validate
    // treats it the same, so the two gates agree). Previously an empty
    // controller_id was rejected here because no controller has id "", which
    // would have made Review pass a config the save then 422'd.
    for (slug, zone) in &cfg.zones {
        if !zone.controller_id.is_empty()
            && !cfg.controllers.iter().any(|c| c.id == zone.controller_id)
        {
            return Err(LoadError::Validation(format!(
                "zone {slug} references unknown controller_id {}",
                zone.controller_id
            )));
        }
        // Zone's soil_sensor_id is a string the engine looks up at
        // merge time; we can't verify it dynamically (the sensor might
        // be a generic source_id:field pair). Document the convention
        // here but don't reject; merge layer no-ops if not found.
        let _ = zone.soil_sensor_id.as_ref();
        // Validate target_min < target_max so the moisture band makes sense.
        if zone.target_min_pct_soil >= zone.saturation_pct_soil {
            return Err(LoadError::Validation(format!(
                "zone {slug}: target_min_pct_soil ({}) must be less than saturation_pct_soil ({})",
                zone.target_min_pct_soil, zone.saturation_pct_soil
            )));
        }
        // Validate slope is non-negative; engine reads abs but a
        // negative value is operator confusion.
        if zone.slope_pct < 0.0 {
            return Err(LoadError::Validation(format!(
                "zone {slug}: slope_pct ({}) must be non-negative",
                zone.slope_pct
            )));
        }
        // Validate area_sqft positive; division by zero / negative
        // areas are catastrophic for the engine math.
        if zone.area_sqft <= 0.0 {
            return Err(LoadError::Validation(format!(
                "zone {slug}: area_sqft must be > 0 (got {})",
                zone.area_sqft
            )));
        }
        // Validate precip rate sane when measured (catalog defaults are fine).
        if let Some(pr) = zone.precip_rate_mm_hr {
            if pr <= 0.0 || pr > 200.0 {
                return Err(LoadError::Validation(format!(
                    "zone {slug}: precip_rate_mm_hr {pr} out of plausible range (0..200)"
                )));
            }
        }
    }

    // Each source id must be sane (non-empty, no whitespace, no slashes).
    for src in &cfg.sources {
        if src.id.is_empty() {
            return Err(LoadError::Validation("source id may not be empty".into()));
        }
        if src.id.contains(char::is_whitespace) || src.id.contains('/') {
            return Err(LoadError::Validation(format!(
                "source id {:?} contains whitespace or slash (use snake_case)",
                src.id
            )));
        }
    }
    for ctrl in &cfg.controllers {
        if ctrl.id.is_empty() {
            return Err(LoadError::Validation(
                "controller id may not be empty".into(),
            ));
        }
        if ctrl.id.contains(char::is_whitespace) || ctrl.id.contains('/') {
            return Err(LoadError::Validation(format!(
                "controller id {:?} contains whitespace or slash (use snake_case)",
                ctrl.id
            )));
        }
    }

    // Lat/lon sanity (engine catches degenerate values too, but flag early).
    let (lat, lon) = (cfg.deployment.location.lat, cfg.deployment.location.lon);
    if !(-90.0..=90.0).contains(&lat) {
        return Err(LoadError::Validation(format!(
            "latitude out of range: {lat}"
        )));
    }
    if !(-180.0..=180.0).contains(&lon) {
        return Err(LoadError::Validation(format!(
            "longitude out of range: {lon}"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_interpolation_basic() {
        std::env::set_var("LOCALSKY_TEST_FOO", "bar");
        let out = interpolate_env("hello ${LOCALSKY_TEST_FOO}!").unwrap();
        assert_eq!(out, "hello bar!");
    }

    #[test]
    fn env_interpolation_escape() {
        let out = interpolate_env("price: $${literal}").unwrap();
        assert_eq!(out, "price: ${literal}");
    }

    #[test]
    fn env_interpolation_missing() {
        let err = interpolate_env("x = ${LOCALSKY_NEVER_SET_42}").unwrap_err();
        assert!(matches!(err, LoadError::UnsetEnvVar(_)));
    }

    #[test]
    fn env_interpolation_preserves_multibyte_utf8() {
        // Regression: the previous implementation iterated src.as_bytes()
        // and did `byte as char`, which split every multi-byte UTF-8
        // sequence into Latin-1 codepoints. A toml with an em-dash got
        // loaded as three codepoints (U+00E2 U+0080 U+0094) instead of
        // the single U+2014 character. Visible to operators as `â`
        // (U+00E2) on the dashboard.
        let src = "name = \"St. Johns RWMD \u{2014} Daylight saving\"";
        let out = interpolate_env(src).unwrap();
        assert_eq!(
            out, src,
            "interpolate_env must round-trip non-ASCII chars unchanged"
        );
        assert!(
            out.contains('\u{2014}'),
            "em-dash should survive the interpolation pass"
        );
        // Also verify a few common accents + a Cyrillic char for good measure.
        let src2 = "city = \"São Paulo · 春日\"";
        assert_eq!(interpolate_env(src2).unwrap(), src2);
    }

    /// A `sessions_per_week` already on disk must not write-lock the whole
    /// config. Nothing constrained the field before 0.7.22, so an 8 can be
    /// sitting in a hand-edited file; the new validation error gates every
    /// whole-config write, so without the load-time clamp an unrelated save
    /// (a different zone, a skip threshold, a tuning apply) would answer 422
    /// until the operator found it. The clamp matches what the engine has
    /// always done with the value.
    #[test]
    fn out_of_range_sessions_per_week_on_disk_is_clamped_at_load() {
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.controllers
            .push(crate::config::schema::ControllerEntry {
                id: "c1".into(),
                default: true,
                enabled: true,
                controller: crate::config::schema::ControllerKind::DryRun(Default::default()),
            });
        for (slug, sessions) in [("front", 8u32), ("back", 0), ("side", 3)] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": slug,
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "sandy_loam",
                    "sprinkler_type": "rotor",
                    "controller_id": "c1",
                    "controller_station": "1",
                    "sessions_per_week": sessions,
                }))
                .unwrap(),
            );
        }
        // Before the clamp, one stale value refuses every whole-config write.
        assert!(
            !crate::config::validate::validate(&cfg).ok(),
            "an out-of-range value must still be an error on the way in"
        );

        normalize_legacy_values(&mut cfg);

        assert_eq!(cfg.zones["front"].sessions_per_week, Some(7), "8 clamps");
        assert_eq!(cfg.zones["back"].sessions_per_week, Some(1), "0 clamps");
        assert_eq!(
            cfg.zones["side"].sessions_per_week,
            Some(3),
            "an in-range value is untouched"
        );
        // With the stored values healed, the whole-config validator passes,
        // so an unrelated save is accepted instead of refused.
        assert!(crate::config::validate::validate(&cfg).ok());
    }

    #[test]
    fn validates_zone_target_band_ordering() {
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.controllers
            .push(crate::config::schema::ControllerEntry {
                id: "c1".into(),
                default: true,
                enabled: true,
                controller: crate::config::schema::ControllerKind::DryRun(Default::default()),
            });
        use crate::config::schema::*;
        cfg.zones.insert(
            "bad".into(),
            ZoneConfig {
                display_name: "Bad".into(),
                area_sqft: 100.0,
                species: GrassSpecies::StAugustine,
                soil_texture: SoilTexture::SandyLoam,
                slope_pct: 0.0,
                sun_exposure: SunExposure::Full,
                sprinkler_type: SprinklerType::Rotor,
                precip_rate_mm_hr: None,
                precip_rate_source: PrecipRateSource::Catalog,
                root_depth_mm: None,
                mad_pct_override: None,
                controller_id: "c1".into(),
                controller_station: "1".into(),
                controller_zone_name: None,
                soil_sensor_id: None,
                target_min_pct_soil: 80.0, // backwards!
                saturation_pct_soil: 60.0, // less than min
                photo_url: None,
                weekly_budget_in: None,
                sessions_per_week: None,
                max_run_minutes: None,
            },
        );
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, LoadError::Validation(_)));
        let msg = format!("{err}");
        assert!(msg.contains("target_min_pct_soil"));
    }

    #[test]
    fn rejects_zero_or_negative_area() {
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.controllers
            .push(crate::config::schema::ControllerEntry {
                id: "c1".into(),
                default: true,
                enabled: true,
                controller: crate::config::schema::ControllerKind::DryRun(Default::default()),
            });
        use crate::config::schema::*;
        cfg.zones.insert(
            "bad".into(),
            ZoneConfig {
                display_name: "Bad".into(),
                area_sqft: 0.0,
                species: GrassSpecies::StAugustine,
                soil_texture: SoilTexture::SandyLoam,
                slope_pct: 0.0,
                sun_exposure: SunExposure::Full,
                sprinkler_type: SprinklerType::Rotor,
                precip_rate_mm_hr: None,
                precip_rate_source: PrecipRateSource::Catalog,
                root_depth_mm: None,
                mad_pct_override: None,
                controller_id: "c1".into(),
                controller_station: "1".into(),
                controller_zone_name: None,
                soil_sensor_id: None,
                target_min_pct_soil: 30.0,
                saturation_pct_soil: 70.0,
                photo_url: None,
                weekly_budget_in: None,
                sessions_per_week: None,
                max_run_minutes: None,
            },
        );
        let err = validate(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("area_sqft"));
    }

    #[test]
    fn rejects_implausible_precip_rate() {
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.controllers
            .push(crate::config::schema::ControllerEntry {
                id: "c1".into(),
                default: true,
                enabled: true,
                controller: crate::config::schema::ControllerKind::DryRun(Default::default()),
            });
        use crate::config::schema::*;
        cfg.zones.insert(
            "bad".into(),
            ZoneConfig {
                display_name: "Bad".into(),
                area_sqft: 100.0,
                species: GrassSpecies::StAugustine,
                soil_texture: SoilTexture::SandyLoam,
                slope_pct: 0.0,
                sun_exposure: SunExposure::Full,
                sprinkler_type: SprinklerType::Rotor,
                precip_rate_mm_hr: Some(500.0), // implausible
                precip_rate_source: PrecipRateSource::Measured,
                root_depth_mm: None,
                mad_pct_override: None,
                controller_id: "c1".into(),
                controller_station: "1".into(),
                controller_zone_name: None,
                soil_sensor_id: None,
                target_min_pct_soil: 30.0,
                saturation_pct_soil: 70.0,
                photo_url: None,
                weekly_budget_in: None,
                sessions_per_week: None,
                max_run_minutes: None,
            },
        );
        let err = validate(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("precip_rate_mm_hr"));
    }

    #[test]
    fn rejects_invalid_source_id() {
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.sources.push(crate::config::schema::SourceEntry {
            id: "has space".into(), // invalid
            priority: 50,
            max_age_s: None,
            enabled: true,
            source: crate::config::schema::SourceKind::DemoReplay(Default::default()),
        });
        let err = validate(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("whitespace or slash"));
    }

    #[test]
    fn validates_unique_source_ids() {
        let mut cfg = Config::default();
        cfg.sources.push(crate::config::schema::SourceEntry {
            id: "a".into(),
            priority: 50,
            max_age_s: None,
            enabled: true,
            source: crate::config::schema::SourceKind::DemoReplay(Default::default()),
        });
        cfg.sources.push(crate::config::schema::SourceEntry {
            id: "a".into(),
            priority: 50,
            max_age_s: None,
            enabled: true,
            source: crate::config::schema::SourceKind::DemoReplay(Default::default()),
        });
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, LoadError::Validation(_)));
    }

    /// The legacy Open-Meteo past_days: an explicit 1 (the never-honored
    /// old default every template stamped) normalizes to the 3 the fetch
    /// always effectively used; any other explicit value is operator
    /// intent and stays.
    #[test]
    fn normalize_rewrites_legacy_open_meteo_past_days() {
        use crate::config::schema::{OpenMeteoConfig, SourceEntry, SourceKind};
        let om = |past_days: u32| SourceEntry {
            id: "open_meteo".into(),
            priority: 50,
            max_age_s: None,
            enabled: true,
            source: SourceKind::OpenMeteo(OpenMeteoConfig {
                forecast_days: 7,
                forecast_hours: 48,
                past_days,
                include_radar: true,
                model: "best_match".into(),
                endpoint: None,
            }),
        };
        let mut cfg = Config::default();
        cfg.sources.push(om(1));
        cfg.sources.push(om(5));
        normalize_legacy_values(&mut cfg);
        let days: Vec<u32> = cfg
            .sources
            .iter()
            .map(|s| match &s.source {
                SourceKind::OpenMeteo(c) => c.past_days,
                _ => 0,
            })
            .collect();
        assert_eq!(days, vec![3, 5], "1 normalizes to 3; a chosen 5 stays");
    }
    // ---- controller_station backfill (issue #8) ----

    /// A zone whose only distinguishing fields are its controller binding.
    /// Everything else is a valid default so the fixture stays readable.
    fn bound_zone(controller_id: &str, station: &str) -> crate::config::schema::ZoneConfig {
        use crate::config::schema::*;
        ZoneConfig {
            display_name: "Zone".into(),
            area_sqft: 1000.0,
            species: GrassSpecies::StAugustine,
            soil_texture: SoilTexture::SandyLoam,
            slope_pct: 0.0,
            sun_exposure: SunExposure::Full,
            sprinkler_type: SprinklerType::Rotor,
            precip_rate_mm_hr: None,
            precip_rate_source: PrecipRateSource::Catalog,
            root_depth_mm: None,
            mad_pct_override: None,
            controller_id: controller_id.into(),
            controller_station: station.into(),
            controller_zone_name: None,
            soil_sensor_id: None,
            target_min_pct_soil: 30.0,
            saturation_pct_soil: 70.0,
            photo_url: None,
            weekly_budget_in: None,
            sessions_per_week: None,
            max_run_minutes: None,
        }
    }

    fn controller(
        id: &str,
        kind: &str,
        config: serde_json::Value,
    ) -> crate::config::schema::ControllerEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "default": true,
            "enabled": true,
            "kind": kind,
            "config": config,
        }))
        .expect("controller fixture parses")
    }

    const UUID_A: &str = "1f00aa00-0000-4000-8000-0000000000a1";
    const UUID_B: &str = "1f00aa00-0000-4000-8000-0000000000a2";

    /// The issue #8 reporter's shape: he got his Rachio zones running by
    /// renaming them in the Rachio app until the slugified vendor names
    /// matched his LocalSky slugs, so his zone_uuid_map keys coincide with
    /// his zone slugs and every controller_station is EMPTY. The backfill
    /// must move those uuids onto the zones without him touching anything.
    #[test]
    fn backfill_rescues_the_coincidence_matched_rachio_config() {
        let mut cfg = Config::default();
        cfg.controllers.push(controller(
            "rachio_main",
            "rachio",
            serde_json::json!({
                "api_token": "example-token",
                "device_id": "device-0001",
                "zone_uuid_map": { "front_yard": UUID_A, "back_yard": UUID_B },
            }),
        ));
        cfg.zones
            .insert("front_yard".into(), bound_zone("rachio_main", ""));
        cfg.zones
            .insert("back_yard".into(), bound_zone("rachio_main", ""));
        assert_eq!(backfill_zone_stations(&mut cfg), 2);
        assert_eq!(cfg.zones["front_yard"].controller_station, UUID_A);
        assert_eq!(cfg.zones["back_yard"].controller_station, UUID_B);
        // The map is a live fallback, not a migration source to be consumed.
        match &cfg.controllers[0].controller {
            crate::config::schema::ControllerKind::Rachio(c) => {
                assert_eq!(c.zone_uuid_map.len(), 2, "the zone map is left in place");
            }
            other => panic!("expected rachio, got {other:?}"),
        }
    }

    #[test]
    fn backfill_never_overwrites_a_station_that_is_already_set() {
        let mut cfg = Config::default();
        cfg.controllers.push(controller(
            "rachio_main",
            "rachio",
            serde_json::json!({
                "api_token": "example-token",
                "device_id": "device-0001",
                "zone_uuid_map": { "front_yard": UUID_A }
            }),
        ));
        // A leftover station number already wins over the map on the numeric
        // kinds, and on Rachio it is rejected in favor of the map. Either
        // way the operator's value is theirs; the backfill leaves it alone.
        cfg.zones
            .insert("front_yard".into(), bound_zone("rachio_main", "3"));
        assert_eq!(backfill_zone_stations(&mut cfg), 0);
        assert_eq!(cfg.zones["front_yard"].controller_station, "3");
    }

    #[test]
    fn backfill_stringifies_the_numeric_kinds_and_resolves_hyphenated_slugs() {
        let mut cfg = Config::default();
        cfg.controllers.push(controller(
            "hydrawise_main",
            "hydrawise",
            serde_json::json!({
                "api_key": "example-key",
                "controller_id": 7,
                "zone_relay_map": { "front_yard": 42 }
            }),
        ));
        cfg.controllers.push(controller(
            "bhyve_main",
            "bhyve",
            serde_json::json!({
                "email": "someone@example.com",
                "password": "example",
                "device_id": "device-0001",
                // A hyphenated map key against an underscored zone key.
                "zone_station_map": { "side-yard": 5 }
            }),
        ));
        // A hyphenated ZONE key against an underscored map key.
        cfg.zones
            .insert("front-yard".into(), bound_zone("hydrawise_main", ""));
        cfg.zones
            .insert("side_yard".into(), bound_zone("bhyve_main", ""));
        assert_eq!(backfill_zone_stations(&mut cfg), 2);
        assert_eq!(cfg.zones["front-yard"].controller_station, "42");
        assert_eq!(cfg.zones["side_yard"].controller_station, "5");
    }

    #[test]
    fn backfill_binds_home_assistant_entities_and_leaves_mqtt_alone() {
        let mut cfg = Config::default();
        cfg.controllers.push(controller(
            "ha_main",
            "ha_service_call",
            serde_json::json!({
                "base_url": "http://homeassistant.example:8123",
                "bearer_token": "example-token",
                "zone_entity_map": { "front_yard": "switch.front_yard_valve" }
            }),
        ));
        cfg.controllers.push(controller(
            "mqtt_main",
            "mqtt_command",
            serde_json::json!({
                "broker_host": "broker.example",
                "zone_command_map": {
                    "back_yard": {
                        "topic": "irrig/zone_1/command",
                        "on_payload": "ON",
                        "off_payload": "OFF"
                    }
                }
            }),
        ));
        cfg.zones
            .insert("front_yard".into(), bound_zone("ha_main", ""));
        cfg.zones
            .insert("back_yard".into(), bound_zone("mqtt_main", ""));
        assert_eq!(backfill_zone_stations(&mut cfg), 1);
        assert_eq!(
            cfg.zones["front_yard"].controller_station,
            "switch.front_yard_valve"
        );
        // MQTT's map value is a struct a station string cannot carry, so the
        // zone stays unbound on the zone entry and keeps dispatching through
        // zone_command_map.
        assert_eq!(cfg.zones["back_yard"].controller_station, "");
    }

    #[test]
    fn backfill_is_idempotent_and_invents_nothing() {
        let mut cfg = Config::default();
        cfg.controllers.push(controller(
            "rachio_main",
            "rachio",
            serde_json::json!({
                "api_token": "example-token",
                "device_id": "device-0001",
                "zone_uuid_map": { "front_yard": UUID_A }
            }),
        ));
        cfg.zones
            .insert("front_yard".into(), bound_zone("rachio_main", ""));
        // A zone the map does not cover stays empty: the backfill copies only
        // what already binds, it never guesses.
        cfg.zones
            .insert("orchard".into(), bound_zone("rachio_main", ""));
        // A zone on another controller is never touched.
        cfg.zones
            .insert("patio".into(), bound_zone("other_controller", ""));
        assert_eq!(backfill_zone_stations(&mut cfg), 1);
        assert_eq!(cfg.zones["orchard"].controller_station, "");
        assert_eq!(cfg.zones["patio"].controller_station, "");
        let before = cfg.zones.clone();
        assert_eq!(backfill_zone_stations(&mut cfg), 0, "second run is a no-op");
        for (slug, z) in &before {
            assert_eq!(cfg.zones[slug].controller_station, z.controller_station);
        }
    }

    /// A pre-rework localsky.toml with no `controller_zone_name` anywhere and
    /// no `controller_station` on the zone must keep parsing.
    /// `load_from_path` is a plain `toml::from_str`, so a required field
    /// would turn every existing config into a parse error.
    #[test]
    fn a_pre_rework_zone_table_still_parses_and_backfills() {
        let toml_src = PRE_REWORK_TOML;
        let mut cfg: Config = toml::from_str(toml_src).expect("pre-rework config parses");
        assert_eq!(cfg.zones["front_yard"].controller_station, "");
        assert_eq!(cfg.zones["front_yard"].controller_zone_name, None);
        normalize_legacy_values(&mut cfg);
        assert_eq!(
            cfg.zones["front_yard"].controller_station, UUID_A,
            "load-time normalize backfills the binding"
        );
        // And it round-trips back out to TOML the store can write.
        let round = toml::to_string_pretty(&cfg).expect("serializes");
        let reparsed: Config = toml::from_str(&round).expect("re-parses");
        assert_eq!(
            reparsed.zones["front_yard"].controller_station,
            cfg.zones["front_yard"].controller_station
        );
    }

    const PRE_REWORK_TOML: &str = concat!(
        "schema_version = 1\n",
        "\n",
        "[deployment]\n",
        "display_name = \"Home\"\n",
        "\n",
        "[deployment.location]\n",
        "lat = 28.5\n",
        "lon = -81.4\n",
        "\n",
        "[[controllers]]\n",
        "id = \"rachio_main\"\n",
        "default = true\n",
        "enabled = true\n",
        "kind = \"rachio\"\n",
        "\n",
        "[controllers.config]\n",
        "api_token = \"example-token\"\n",
        "device_id = \"device-0001\"\n",
        "\n",
        "[controllers.config.zone_uuid_map]\n",
        "front_yard = \"1f00aa00-0000-4000-8000-0000000000a1\"\n",
        "\n",
        "[zones.front_yard]\n",
        "display_name = \"Front Yard\"\n",
        "area_sqft = 1000.0\n",
        "species = \"st_augustine\"\n",
        "soil_texture = \"sandy_loam\"\n",
        "sprinkler_type = \"rotor\"\n",
        "controller_id = \"rachio_main\"\n",
    );
}
