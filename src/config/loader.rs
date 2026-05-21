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
    #[error("schema_version {found} > known {known}; refusing to load a config newer than this binary")]
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
    let cfg: Config = toml::from_str(&interpolated)?;
    validate(&cfg)?;
    Ok(cfg)
}

/// `${VAR}` interpolation. Single pass; nested refs not supported.
/// Escape with `$${VAR}` for a literal dollar.
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
            let val = env::var(var_name).map_err(|_| LoadError::UnsetEnvVar(var_name.to_string()))?;
            out.push_str(&val);
            i += 2 + close + 1;
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    Ok(out)
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

    // Each zone's controller_id must reference a configured controller.
    for (slug, zone) in &cfg.zones {
        if !cfg.controllers.iter().any(|c| c.id == zone.controller_id) {
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
            return Err(LoadError::Validation("controller id may not be empty".into()));
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
        return Err(LoadError::Validation(format!("latitude out of range: {lat}")));
    }
    if !(-180.0..=180.0).contains(&lon) {
        return Err(LoadError::Validation(format!("longitude out of range: {lon}")));
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
    fn validates_zone_target_band_ordering() {
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.controllers.push(crate::config::schema::ControllerEntry {
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
                soil_sensor_id: None,
                target_min_pct_soil: 80.0,    // backwards!
                saturation_pct_soil: 60.0,    // less than min
                photo_url: None,
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
        cfg.controllers.push(crate::config::schema::ControllerEntry {
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
                soil_sensor_id: None,
                target_min_pct_soil: 30.0,
                saturation_pct_soil: 70.0,
                photo_url: None,
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
        cfg.controllers.push(crate::config::schema::ControllerEntry {
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
                precip_rate_mm_hr: Some(500.0),    // implausible
                precip_rate_source: PrecipRateSource::Measured,
                root_depth_mm: None,
                mad_pct_override: None,
                controller_id: "c1".into(),
                controller_station: "1".into(),
                soil_sensor_id: None,
                target_min_pct_soil: 30.0,
                saturation_pct_soil: 70.0,
                photo_url: None,
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
            id: "has space".into(),  // invalid
            priority: 50,
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
            enabled: true,
            source: crate::config::schema::SourceKind::DemoReplay(Default::default()),
        });
        cfg.sources.push(crate::config::schema::SourceEntry {
            id: "a".into(),
            priority: 50,
            enabled: true,
            source: crate::config::schema::SourceKind::DemoReplay(Default::default()),
        });
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, LoadError::Validation(_)));
    }
}
