// Timezone resolution. The deployment's effective timezone, resolved
// once per call site as: explicit deployment.timezone > inferred from
// lat/lon (tzf-rs, offline) > container-local. Smart-morning dispatch
// hours depend on this; before it existed, an unset TZ env meant UTC
// and sunrise math fired at the wrong wall-clock hour.

use std::str::FromStr;
use std::sync::OnceLock;

use chrono::{DateTime, FixedOffset, Local, NaiveDate, TimeZone, Utc};
use tzf_rs::DefaultFinder;

fn finder() -> &'static DefaultFinder {
    static FINDER: OnceLock<DefaultFinder> = OnceLock::new();
    FINDER.get_or_init(DefaultFinder::new)
}

/// Process-wide resolved deployment timezone, set once at boot from config.
/// Schedulers read it via `now_local` / `local_day_bounds_utc` so wall-clock
/// firing and day-rollover dedupe key off the CONFIGURED timezone (P1-8c), not
/// the container's `TZ` env, which may disagree. `None` = not yet set or
/// unresolvable, in which case the helpers fall back to the system local time
/// (the prior behavior).
static CONFIGURED_TZ: OnceLock<Option<chrono_tz::Tz>> = OnceLock::new();

/// Set the process-wide timezone from config. Idempotent (first writer wins);
/// call once at boot before the schedulers spawn.
pub fn set_configured_tz(cfg: &crate::config::schema::Config) {
    let _ = CONFIGURED_TZ.set(resolve_tz(cfg));
}

fn configured_tz() -> Option<chrono_tz::Tz> {
    CONFIGURED_TZ.get().copied().flatten()
}

/// Current wall-clock in the configured timezone, as a fixed-offset DateTime so
/// it composes with chrono regardless of source. Falls back to the system local
/// time when no timezone is configured/resolvable.
pub fn now_local() -> DateTime<FixedOffset> {
    match configured_tz() {
        Some(tz) => Utc::now().with_timezone(&tz).fixed_offset(),
        None => Local::now().fixed_offset(),
    }
}

/// The `[start, end)` UTC instants of `day` as a calendar day in the configured
/// timezone, for day-boundary history queries (e.g. the smart-morning boot
/// dedupe). Falls back to the system local timezone. `None` if the local
/// midnight is non-existent/ambiguous (DST transition) or the date overflows.
pub fn local_day_bounds_utc(day: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start_naive = day.and_hms_opt(0, 0, 0)?;
    let end_naive = day.succ_opt()?.and_hms_opt(0, 0, 0)?;
    match configured_tz() {
        Some(tz) => Some((
            tz.from_local_datetime(&start_naive)
                .single()?
                .with_timezone(&Utc),
            tz.from_local_datetime(&end_naive)
                .single()?
                .with_timezone(&Utc),
        )),
        None => Some((
            Local
                .from_local_datetime(&start_naive)
                .single()?
                .with_timezone(&Utc),
            Local
                .from_local_datetime(&end_naive)
                .single()?
                .with_timezone(&Utc),
        )),
    }
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
