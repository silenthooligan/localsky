// Shared client-side time formatter.
//
// Every epoch the irrigation + forecast UI renders must read out in the
// DEPLOYMENT's timezone (the IANA name carried on the snapshot), in 24-hour
// LOCAL form, NOT the viewer's browser timezone. chrono with `wasmbind` resolves
// `chrono::Local` to the BROWSER's timezone, so a phone in another zone would
// show the wrong wall clock; chrono-tz (which would let us convert to an
// arbitrary IANA zone) is an ssr-only dependency and cannot link into the WASM
// bundle. So:
//
//   * hydrate (WASM): format via the browser's built-in `Intl.DateTimeFormat`
//     (through `js_sys::Date::to_locale_*_string`), pinned to the snapshot's
//     `timeZone` and `hour12: false`. This is always available in the browser and
//     needs no extra dependency.
//   * ssr (server render): convert with chrono-tz, the same engine the schedulers
//     use, so a server-rendered first paint matches the hydrated client.
//   * neither feature (bare dev build): a minimal chrono/UTC fallback so the crate
//     still compiles and the helpers have a body.
//
// All helpers take the epoch in SECONDS and the IANA tz string from the snapshot.
// An empty / unrecognized tz falls back to browser-local (hydrate) or UTC (ssr),
// never panicking, so an older producer that didn't send a timezone still renders.
//
// Public API (Wave 2 components call these):
//   format_hm(epoch_secs: i64, tz_iana: &str) -> String        // "14:05" (24h)
//   format_wday_short(epoch_secs: i64, tz_iana: &str) -> String // "Mon"
//   format_md(epoch_secs: i64, tz_iana: &str) -> String         // "Jun 28"

// ── hydrate (WASM): browser Intl.DateTimeFormat ───────────────────────────────
/// The calendar day an epoch falls on in `tz`, as sortable ISO
/// "YYYY-MM-DD". Empty on an unrepresentable instant. Browser `Intl` on
/// hydrate, chrono-tz on the server, UTC otherwise, so the History page
/// and the zone page bucket a run under the day it happened where the
/// controller lives rather than where the page is rendered.
/// The current instant as UNIX seconds. The one clock read the views make;
/// every "today" and "how long ago" derives from it plus the deployment
/// timezone, never from the browser's or the server's local zone.
pub fn now_epoch() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn day_key_in_tz(epoch_secs: i64, tz_iana: &str) -> String {
    imp::day_key_in_tz(epoch_secs, tz_iana)
}

/// The day key parsed, for arithmetic on days.
pub fn day_in_tz(epoch_secs: i64, tz_iana: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(&day_key_in_tz(epoch_secs, tz_iana), "%Y-%m-%d").ok()
}

/// Whole days from the day `epoch` falls on to the day `now` falls on,
/// both in `tz`. Negative for a later day. Judged on calendar days, so a
/// clock change between them cannot shift a run into the wrong bucket.
pub fn days_between_in_tz(now_epoch: i64, epoch: i64, tz_iana: &str) -> Option<i64> {
    let today = day_in_tz(now_epoch, tz_iana)?;
    let day = day_in_tz(epoch, tz_iana)?;
    Some(today.signed_duration_since(day).num_days())
}

/// The epoch of local midnight for the day `now_epoch` falls on in
/// `tz`. Server: chrono-tz's own resolution (the earlier of two on a
/// repeated hour). Browser: the day's key and the local clock reading
/// from `Intl`, subtracted; an hour off on the one day a year the clock
/// changes at midnight, and nothing reads this for that precision.
pub fn local_midnight_epoch(now_epoch: i64, tz_iana: &str) -> i64 {
    imp::local_midnight_epoch(now_epoch, tz_iana)
}

/// Split a physical interval at the deployment's actual date boundaries.
/// Uses the same IANA date conversion on server and browser, including DST
/// and midnight clock changes, without assuming that a day lasts 86,400 s.
pub fn split_local_days(start: i64, end: i64, tz: &str) -> Vec<(i64, i64)> {
    let mut pieces = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let day = day_key_in_tz(cursor, tz);
        if day.is_empty() {
            break;
        }
        let mut boundary = end;
        if day_key_in_tz(end - 1, tz) != day {
            // First instant whose local date differs. Binary search avoids
            // approximating a midnight from a possibly different UTC offset.
            let mut low = cursor;
            let mut high = end - 1;
            while high - low > 1 {
                let middle = low + (high - low) / 2;
                if day_key_in_tz(middle, tz) == day {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            boundary = high;
        }
        pieces.push((cursor, boundary));
        cursor = boundary;
    }
    pieces
}

/// The viewer's clock preference. Set by the units preference loader on
/// hydrate; false (24-hour) on the server, which renders the first frame,
/// and until the preference is read. Thread-local: wasm is one thread,
/// and a server thread renders for one request at a time in the default.
pub fn set_clock_12h(on: bool) {
    CLOCK_12H.with(|c| c.set(on));
}

pub fn clock_12h() -> bool {
    CLOCK_12H.with(|c| c.get())
}

thread_local! {
    static CLOCK_12H: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "hydrate")]
mod imp {
    use wasm_bindgen::JsValue;

    /// A `js_sys::Date` for the epoch (ms). Intl reads the instant, the options'
    /// `timeZone` decides the rendered wall clock.
    fn date(epoch_secs: i64) -> js_sys::Date {
        js_sys::Date::new(&JsValue::from_f64(epoch_secs as f64 * 1000.0))
    }

    /// Build an options object, setting `timeZone` only when a non-empty IANA name
    /// is provided (empty -> browser-local). Always sets `hour12: false` for the
    /// time helper via the caller.
    fn options(tz_iana: &str) -> js_sys::Object {
        let opts = js_sys::Object::new();
        if !tz_iana.is_empty() {
            let _ = js_sys::Reflect::set(
                &opts,
                &JsValue::from_str("timeZone"),
                &JsValue::from_str(tz_iana),
            );
        }
        opts
    }

    pub fn format_hm(epoch_secs: i64, tz_iana: &str) -> String {
        let twelve = super::clock_12h();
        let opts = options(tz_iana);
        // 24-hour by default, "14:05"; 12-hour with AM/PM when the viewer chose it.
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("hour12"),
            &JsValue::from_bool(twelve),
        );
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("hour"),
            &JsValue::from_str("2-digit"),
        );
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("minute"),
            &JsValue::from_str("2-digit"),
        );
        // `to_locale_time_string_with_options` is the stable js-sys binding that
        // takes an options object (the plain variant takes only a locale).
        date(epoch_secs)
            .to_locale_time_string_with_options(
                if twelve { "en-US" } else { "en-GB" },
                opts.as_ref(),
            )
            .as_string()
            .unwrap_or_default()
    }

    pub fn format_wday_short(epoch_secs: i64, tz_iana: &str) -> String {
        let opts = options(tz_iana);
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("weekday"),
            &JsValue::from_str("short"),
        );
        // `to_locale_date_string` (no `_with_options` suffix) already takes the
        // options object as its third argument in this js-sys version.
        date(epoch_secs)
            .to_locale_date_string("en-US", opts.as_ref())
            .as_string()
            .unwrap_or_default()
    }

    pub fn format_wday_full(epoch_secs: i64, tz_iana: &str) -> String {
        let opts = options(tz_iana);
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("weekday"),
            &JsValue::from_str("long"),
        );
        date(epoch_secs)
            .to_locale_date_string("en-US", opts.as_ref())
            .as_string()
            .unwrap_or_default()
    }

    pub fn day_key_in_tz(epoch_secs: i64, tz_iana: &str) -> String {
        let opts = options(tz_iana);
        // en-CA renders dates as ISO 8601 "YYYY-MM-DD", which sorts lexically.
        date(epoch_secs)
            .to_locale_date_string("en-CA", opts.as_ref())
            .as_string()
            .unwrap_or_default()
    }

    pub fn local_midnight_epoch(now_epoch: i64, tz_iana: &str) -> i64 {
        // The local clock reading, then subtracted from the instant.
        let opts = options(tz_iana);
        for (k, v) in [
            ("hour", "2-digit"),
            ("minute", "2-digit"),
            ("second", "2-digit"),
        ] {
            let _ = js_sys::Reflect::set(&opts, &JsValue::from_str(k), &JsValue::from_str(v));
        }
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("hour12"),
            &JsValue::from_bool(false),
        );
        let hms = date(now_epoch)
            .to_locale_time_string_with_options("en-GB", opts.as_ref())
            .as_string()
            .unwrap_or_default();
        let mut parts = hms.split(':').filter_map(|p| p.trim().parse::<i64>().ok());
        let (h, m, s) = (
            parts.next().unwrap_or(0) % 24,
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        );
        now_epoch - (h * 3600 + m * 60 + s)
    }

    pub fn format_md(epoch_secs: i64, tz_iana: &str) -> String {
        let opts = options(tz_iana);
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("month"),
            &JsValue::from_str("short"),
        );
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("day"),
            &JsValue::from_str("numeric"),
        );
        date(epoch_secs)
            .to_locale_date_string("en-US", opts.as_ref())
            .as_string()
            .unwrap_or_default()
    }
}

// ── ssr (server render): chrono-tz ────────────────────────────────────────────
#[cfg(all(feature = "ssr", not(feature = "hydrate")))]
mod imp {
    use chrono::TimeZone;
    use std::str::FromStr;

    /// Resolve the IANA name to a chrono-tz zone; empty/invalid -> UTC so the
    /// server-render never panics on a missing/garbled timezone.
    fn zone(tz_iana: &str) -> chrono_tz::Tz {
        chrono_tz::Tz::from_str(tz_iana).unwrap_or(chrono_tz::Tz::UTC)
    }

    fn fmt(epoch_secs: i64, tz_iana: &str, pattern: &str) -> String {
        match zone(tz_iana).timestamp_opt(epoch_secs, 0).single() {
            Some(dt) => dt.format(pattern).to_string(),
            None => String::new(),
        }
    }

    pub fn format_hm(epoch_secs: i64, tz_iana: &str) -> String {
        if super::clock_12h() {
            fmt(epoch_secs, tz_iana, "%-I:%M %p")
        } else {
            fmt(epoch_secs, tz_iana, "%H:%M")
        }
    }

    pub fn format_wday_short(epoch_secs: i64, tz_iana: &str) -> String {
        fmt(epoch_secs, tz_iana, "%a")
    }

    pub fn format_wday_full(epoch_secs: i64, tz_iana: &str) -> String {
        fmt(epoch_secs, tz_iana, "%A")
    }

    pub fn format_md(epoch_secs: i64, tz_iana: &str) -> String {
        fmt(epoch_secs, tz_iana, "%b %-d")
    }

    pub fn day_key_in_tz(epoch_secs: i64, tz_iana: &str) -> String {
        fmt(epoch_secs, tz_iana, "%Y-%m-%d")
    }

    pub fn local_midnight_epoch(now_epoch: i64, tz_iana: &str) -> i64 {
        let z = zone(tz_iana);
        let Some(now) = z.timestamp_opt(now_epoch, 0).single() else {
            return now_epoch - now_epoch.rem_euclid(86_400);
        };
        let midnight = now.date_naive().and_hms_opt(0, 0, 0).expect("midnight");
        match z.from_local_datetime(&midnight).earliest() {
            Some(dt) => dt.timestamp(),
            None => now_epoch - now_epoch.rem_euclid(86_400),
        }
    }
}

// ── bare dev build (neither feature): chrono / UTC fallback ────────────────────
// Keeps the crate compiling under the dev-default feature set (no ssr, no
// hydrate). chrono is always available; chrono-tz is not, so this renders in UTC.
#[cfg(all(not(feature = "hydrate"), not(feature = "ssr")))]
mod imp {
    use chrono::TimeZone;

    fn fmt(epoch_secs: i64, _tz_iana: &str, pattern: &str) -> String {
        match chrono::Utc.timestamp_opt(epoch_secs, 0).single() {
            Some(dt) => dt.format(pattern).to_string(),
            None => String::new(),
        }
    }

    pub fn format_hm(epoch_secs: i64, tz_iana: &str) -> String {
        if super::clock_12h() {
            fmt(epoch_secs, tz_iana, "%-I:%M %p")
        } else {
            fmt(epoch_secs, tz_iana, "%H:%M")
        }
    }

    pub fn format_wday_short(epoch_secs: i64, tz_iana: &str) -> String {
        fmt(epoch_secs, tz_iana, "%a")
    }

    pub fn format_wday_full(epoch_secs: i64, tz_iana: &str) -> String {
        fmt(epoch_secs, tz_iana, "%A")
    }

    pub fn format_md(epoch_secs: i64, tz_iana: &str) -> String {
        fmt(epoch_secs, tz_iana, "%b %-d")
    }

    pub fn day_key_in_tz(epoch_secs: i64, tz_iana: &str) -> String {
        fmt(epoch_secs, tz_iana, "%Y-%m-%d")
    }

    pub fn local_midnight_epoch(now_epoch: i64, _tz_iana: &str) -> i64 {
        now_epoch - now_epoch.rem_euclid(86_400)
    }
}

/// 24-hour local time `HH:MM` (e.g. "14:05") for `epoch_secs`, rendered in the
/// deployment timezone `tz_iana`. Empty `tz_iana` -> browser-local (hydrate) or
/// UTC (ssr/dev). Never panics.
pub fn format_hm(epoch_secs: i64, tz_iana: &str) -> String {
    imp::format_hm(epoch_secs, tz_iana)
}

/// Short weekday name (e.g. "Mon") for `epoch_secs` in `tz_iana`. Calendar
/// contexts only (strip / week column headers, where the row is unmistakably
/// days): RUN DESCRIPTIONS must use `format_wday_full`, because "Sun" in
/// running prose reads as sunshine (a weather reason), not a day.
pub fn format_wday_short(epoch_secs: i64, tz_iana: &str) -> String {
    imp::format_wday_short(epoch_secs, tz_iana)
}

/// Full weekday name (e.g. "Monday") for `epoch_secs` in `tz_iana`. Use this in
/// run-description prose ("next likely run Sunday").
pub fn format_wday_full(epoch_secs: i64, tz_iana: &str) -> String {
    imp::format_wday_full(epoch_secs, tz_iana)
}

/// Short month + day (e.g. "Jun 28") for `epoch_secs` in `tz_iana`.
pub fn format_md(epoch_secs: i64, tz_iana: &str) -> String {
    imp::format_md(epoch_secs, tz_iana)
}

// SSR-side tests (the chrono-tz path is the one we can assert deterministically;
// the hydrate path depends on the browser's Intl and is exercised in the app).
#[cfg(all(test, feature = "ssr", not(feature = "hydrate")))]
mod tests {
    use super::*;

    // 2026-06-28 18:05:00 UTC (a Sunday). New York is UTC-4 in June (EDT) -> 14:05.
    const EPOCH: i64 = 1_782_669_900;

    #[test]
    fn format_hm_is_24h_in_the_named_zone() {
        assert_eq!(format_hm(EPOCH, "America/New_York"), "14:05");
        // Same instant, Los Angeles (UTC-7 PDT) -> 11:05.
        assert_eq!(format_hm(EPOCH, "America/Los_Angeles"), "11:05");
        // A post-noon hour stays 24h (never "2:05 PM").
        assert_eq!(format_hm(EPOCH, "UTC"), "18:05");
    }

    #[test]
    fn empty_or_bad_zone_falls_back_to_utc_not_panic() {
        assert_eq!(format_hm(EPOCH, ""), "18:05");
        assert_eq!(format_hm(EPOCH, "Not/AZone"), "18:05");
    }

    #[test]
    fn weekday_and_month_day_render_in_zone() {
        // 18:05 UTC on a Sunday; New York is still the same calendar day.
        assert_eq!(format_wday_short(EPOCH, "America/New_York"), "Sun");
        assert_eq!(format_md(EPOCH, "America/New_York"), "Jun 28");
    }
}
