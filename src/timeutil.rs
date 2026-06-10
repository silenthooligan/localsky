// Timezone resolution. The deployment's effective timezone, resolved
// once per call site as: explicit deployment.timezone > inferred from
// lat/lon (tzf-rs, offline) > container-local. Smart-morning dispatch
// hours depend on this; before it existed, an unset TZ env meant UTC
// and sunrise math fired at the wrong wall-clock hour.

use std::str::FromStr;
use std::sync::OnceLock;

use tzf_rs::DefaultFinder;

fn finder() -> &'static DefaultFinder {
    static FINDER: OnceLock<DefaultFinder> = OnceLock::new();
    FINDER.get_or_init(DefaultFinder::new)
}

/// IANA timezone name for a lat/lon, e.g. "America/New_York".
/// Empty result (open ocean) returns None.
pub fn tz_name_for(lat: f64, lon: f64) -> Option<String> {
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    // tzf-rs takes (lng, lat).
    let name = finder().get_tz_name(lon, lat);
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// The deployment's effective timezone. Explicit config wins; else
/// inferred from location; else None (callers fall back to Local).
pub fn resolve_tz(cfg: &crate::config::schema::Config) -> Option<chrono_tz::Tz> {
    if let Some(name) = cfg.deployment.timezone.as_deref() {
        if let Ok(tz) = chrono_tz::Tz::from_str(name) {
            return Some(tz);
        }
        tracing::warn!(
            timezone = name,
            "configured timezone is not a valid IANA name; inferring from location"
        );
    }
    let loc = &cfg.deployment.location;
    tz_name_for(loc.lat, loc.lon).and_then(|n| chrono_tz::Tz::from_str(&n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_locations_resolve() {
        assert_eq!(
            tz_name_for(29.65, -82.32).as_deref(),
            Some("America/New_York")
        );
        assert_eq!(tz_name_for(48.85, 2.35).as_deref(), Some("Europe/Paris"));
        assert_eq!(
            tz_name_for(-33.87, 151.21).as_deref(),
            Some("Australia/Sydney")
        );
        assert_eq!(tz_name_for(0.0, 0.0), None);
    }

    #[test]
    fn resolve_prefers_explicit() {
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 29.65;
        cfg.deployment.location.lon = -82.32;
        cfg.deployment.timezone = Some("Europe/Berlin".into());
        assert_eq!(resolve_tz(&cfg), Some(chrono_tz::Tz::Europe__Berlin));
        cfg.deployment.timezone = None;
        assert_eq!(resolve_tz(&cfg), Some(chrono_tz::Tz::America__New_York));
    }
}
