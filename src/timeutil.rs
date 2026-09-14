// Timezone resolution. The deployment's effective timezone, resolved
// once per call site as: explicit deployment.timezone > inferred from
// lat/lon (tzf-rs, offline) > container-local. Smart-morning dispatch
// hours depend on this; before it existed, an unset TZ env meant UTC
// and sunrise math fired at the wrong wall-clock hour.

use std::str::FromStr;
use std::sync::{LazyLock, OnceLock};

use chrono::{DateTime, FixedOffset, Local, NaiveDate, TimeZone, Utc};
use tzf_rs::DefaultFinder;

/// The timezone-boundary table, built on first use (tens of
/// milliseconds) and immutable after.
static FINDER: LazyLock<DefaultFinder> = LazyLock::new(DefaultFinder::new);

fn finder() -> &'static DefaultFinder {
    &FINDER
}

/// The deployment timezone, resolved once from the boot config before
/// any task spawns (`boot::config`), and one of the two boot constants
/// that are not threaded through state (the other is the instance id):
/// ninety call sites in thirty files read the deployment's calendar
/// through `now_local`, `local_date`, `local_day_bounds_utc` and
/// `DEPLOYMENT_OPS`, and a value that never changes for the life of the
/// process is a constant, not a handle. Wall-clock firing and
/// day-rollover dedupe key off the CONFIGURED timezone, not the
/// container's `TZ` env. `None` = not set or unresolvable, in which case
/// the helpers fall back to the system local time.
static CONFIGURED_TZ: OnceLock<Option<chrono_tz::Tz>> = OnceLock::new();

/// Resolve the deployment timezone from config. Idempotent (first writer
/// wins); the boot calls it once before the schedulers spawn.
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

/// Local calendar date for a UNIX epoch in the CONFIGURED timezone
/// (system-local fallback, mirroring `now_local`). `None` for an
/// unrepresentable epoch. Everything that keys rows or buckets by "the
/// deployment's calendar day" derives it here so writers and readers can
/// never disagree on the calendar.
pub fn local_date(epoch: i64) -> Option<NaiveDate> {
    match configured_tz() {
        Some(tz) => Utc
            .timestamp_opt(epoch, 0)
            .single()
            .map(|dt| dt.with_timezone(&tz).date_naive()),
        None => Local
            .timestamp_opt(epoch, 0)
            .single()
            .map(|dt| dt.date_naive()),
    }
}

/// Local calendar-day ordinal (num_days_from_ce of the local date) for a UNIX
/// epoch, in the CONFIGURED timezone (system-local fallback, mirroring
/// `now_local`). Day-bucketed accumulators (rain-today, ET0-today) key on this
/// so they roll at the deployment's midnight, not the container's: a UTC
/// container serving a US deployment would otherwise reset them mid-evening
/// local time. Falls back to the integer UTC day for an unrepresentable epoch.
pub fn local_day_ordinal(epoch: i64) -> i32 {
    use chrono::Datelike;
    match local_date(epoch) {
        Some(d) => d.num_days_from_ce(),
        None => (epoch / 86400) as i32,
    }
}

/// The `[start, end)` UTC instants of `day` as a calendar day in the configured
/// timezone, for day-boundary history queries (e.g. the smart-morning boot
/// dedupe). Falls back to the system local timezone. `None` if the local
/// midnight is non-existent/ambiguous (DST transition) or the date overflows.
pub fn local_day_bounds_utc(day: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    // Via LocalStart, so a day whose local midnight is skipped or
    // repeated by a clock change still HAS bounds. It does happen at
    // midnight: Santiago, Havana, Asuncion and Beirut all move their
    // clocks then.
    //
    // The old body used `.single()?`, which collapsed both cases to "no
    // answer", and the three production readers of that None disagreed
    // about what it meant. The worst of them disarmed the smart-morning
    // restart dedupe, so a restart on a transition day could run a second
    // full irrigation cycle.
    let start = deployment_day_start(day).instant()?;
    let end = deployment_day_start(day.succ_opt()?).instant()?;
    Some((
        Utc.timestamp_opt(start, 0).single()?,
        Utc.timestamp_opt(end, 0).single()?,
    ))
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

/// The deployment's calendar, as the engine's `Calendar` value.
///
/// One construction site. Six byte-identical hand-copied struct literals
/// used to spell this, which is how the scheduler and the tuning report
/// drifted apart from the refresher: a copied literal does not follow the
/// original when the original learns something.
pub fn deployment_calendar() -> crate::engine::calendar::Calendar {
    crate::engine::calendar::Calendar::zone(&DEPLOYMENT_OPS)
}

/// The zone questions the engine cannot answer for itself, because
/// resolving a named zone needs the timezone database and the engine has
/// to compile for the browser.
pub static DEPLOYMENT_OPS: crate::engine::calendar::ZoneOps = crate::engine::calendar::ZoneOps {
    offset_at: deployment_offset_at,
    day_start: deployment_day_start,
};

/// The UTC offset in force AT `epoch`, in the configured timezone
/// (system-local fallback, mirroring `now_local`).
///
/// Per-instant on purpose. Sampling an offset once and applying it to a
/// week of forward days is wrong twice a year, and wrong in a way that
/// changes which jurisdictional rule set applies rather than merely
/// shifting an hour.
fn deployment_offset_at(epoch: i64) -> Option<i32> {
    use chrono::Offset;
    match configured_tz() {
        Some(tz) => Utc
            .timestamp_opt(epoch, 0)
            .single()
            .map(|dt| dt.with_timezone(&tz).offset().fix().local_minus_utc()),
        None => Local
            .timestamp_opt(epoch, 0)
            .single()
            .map(|dt| dt.offset().local_minus_utc()),
    }
}

/// When a local calendar day begins, naming the two cases where local
/// midnight is not a single instant instead of collapsing them to
/// "no answer".
///
/// Santiago, Havana, Asuncion and Beirut all move their clocks AT
/// midnight, so these are not hypothetical.
fn deployment_day_start(date: chrono::NaiveDate) -> crate::engine::clock::LocalStart {
    use crate::engine::clock::LocalStart;
    use chrono::LocalResult;

    let Some(naive) = date.and_hms_opt(0, 0, 0) else {
        return LocalStart::Unrepresentable;
    };
    let resolved = match configured_tz() {
        Some(tz) => match tz.from_local_datetime(&naive) {
            LocalResult::Single(dt) => LocalResult::Single(dt.timestamp()),
            LocalResult::Ambiguous(a, b) => LocalResult::Ambiguous(a.timestamp(), b.timestamp()),
            LocalResult::None => LocalResult::None,
        },
        None => match Local.from_local_datetime(&naive) {
            LocalResult::Single(dt) => LocalResult::Single(dt.timestamp()),
            LocalResult::Ambiguous(a, b) => LocalResult::Ambiguous(a.timestamp(), b.timestamp()),
            LocalResult::None => LocalResult::None,
        },
    };
    match resolved {
        LocalResult::Single(e) => LocalStart::At(e),
        LocalResult::Ambiguous(first, second) => LocalStart::Twice { first, second },
        // Local midnight was skipped by a spring-forward. The day still
        // happens; it starts at the transition. Local date is monotonic
        // in the epoch, so the first instant that lands on `date` is a
        // clean binary search rather than a guess at the step size.
        LocalResult::None => {
            let centre = naive.and_utc().timestamp();
            let (mut lo, mut hi) = (centre - 86_400, centre + 86_400);
            if local_date(hi).is_none_or(|d| d < date) {
                return LocalStart::Unrepresentable;
            }
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                match local_date(mid) {
                    Some(d) if d >= date => hi = mid,
                    Some(_) => lo = mid + 1,
                    None => return LocalStart::Unrepresentable,
                }
            }
            match local_date(lo) {
                Some(d) if d == date => LocalStart::Skipped { resumes_at: lo },
                _ => LocalStart::Unrepresentable,
            }
        }
    }
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
