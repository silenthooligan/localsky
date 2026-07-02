// History, "history that sings" (marquee feature 4, first cut). Reads the
// existing /api/irrigation/history window and renders it on the new chart
// primitives: KPI stat tiles, a daily-watered-minutes line chart across
// the range, and per-zone rows each with a sparkline. A range switch
// (30/90/365 days) drives every panel; a Print button turns the page into
// a seasonal report (@media print hides the app chrome).
//
// Year-over-year + rain-vs-watered correlation need a longer/wider data
// feed (rainfall history) and are the follow-ups; this cut delivers the
// scannable "what happened" view from data we already have.

use chrono::{Local, TimeZone};
use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};
use leptos_router::NavigateOptions;

use crate::components::ui::{Button, LineChart, Series, Sparkline, StatTile};
use crate::components::units_fmt::{fmt_rain_amount, use_unit_prefs};
#[cfg(feature = "hydrate")]
use crate::ha::snapshot::IrrigationSnapshot;
#[cfg(feature = "hydrate")]
use crate::history::types::DecisionWindow;
use crate::history::types::{DecisionRecord, HistoryWindow, RunRecord};
use crate::timefmt::{format_hm, format_md, format_wday_short};

/// Sortable calendar-day key "YYYY-MM-DD" for `epoch`, in the DEPLOYMENT
/// timezone `tz` (not the viewer's browser TZ), so the run log groups runs
/// under the day they happened where the controller lives. Mirrors
/// `crate::timefmt`'s feature split: browser `Intl` on hydrate (the en-CA
/// locale yields ISO "YYYY-MM-DD"), chrono-tz on SSR, UTC otherwise. Empty /
/// invalid tz falls back to browser-local (hydrate) or UTC, never panics.
#[cfg(feature = "hydrate")]
fn day_key_in_tz(epoch: i64, tz: &str) -> String {
    use wasm_bindgen::JsValue;
    let date = js_sys::Date::new(&JsValue::from_f64(epoch as f64 * 1000.0));
    let opts = js_sys::Object::new();
    if !tz.is_empty() {
        let _ = js_sys::Reflect::set(
            &opts,
            &JsValue::from_str("timeZone"),
            &JsValue::from_str(tz),
        );
    }
    // en-CA renders dates as ISO 8601 "YYYY-MM-DD", which sorts lexically.
    date.to_locale_date_string("en-CA", opts.as_ref())
        .as_string()
        .unwrap_or_default()
}

#[cfg(all(feature = "ssr", not(feature = "hydrate")))]
fn day_key_in_tz(epoch: i64, tz: &str) -> String {
    use std::str::FromStr;
    // `TimeZone` (for `timestamp_opt`) is already in module scope.
    let zone = chrono_tz::Tz::from_str(tz).unwrap_or(chrono_tz::Tz::UTC);
    match zone.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

#[cfg(all(not(feature = "hydrate"), not(feature = "ssr")))]
fn day_key_in_tz(epoch: i64, _tz: &str) -> String {
    // `TimeZone` (for `timestamp_opt`) is already in module scope.
    match chrono::Utc.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

/// Daily watered-minutes buckets, oldest -> newest, length `days`.
/// Skips are excluded (skip_reason is Some). Optional zone filter.
fn day_buckets(runs: &[RunRecord], days: i64, zone: Option<&str>) -> Vec<f64> {
    let now = Local::now();
    let today_mid = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|nd| Local.from_local_datetime(&nd).single())
        .unwrap_or(now)
        .timestamp();
    let n = days.max(1) as usize;
    let mut b = vec![0f64; n];
    for r in runs.iter().filter(|r| r.skip_reason.is_none()) {
        if let Some(z) = zone {
            if r.zone != z {
                continue;
            }
        }
        let back = crate::components::time_bucket::days_back(today_mid, r.start_epoch).max(0);
        if (back as usize) < n {
            b[back as usize] += r.duration_s as f64 / 60.0;
        }
    }
    b.reverse();
    b
}

/// Map a structured `reason_code` (carried on captured decision traces) to a
/// "why it skipped" headline bucket. `Some(bucket)` for a recognized code (incl.
/// `Some("other")` for codes that don't map to a weather/soil headline);
/// `None` for an empty/unknown code so the caller falls back to substring
/// classification of the legacy baked reason.
fn classify_skip_code(code: &str) -> Option<&'static str> {
    match code {
        "rain_now" | "already_wet" | "observed_rain" | "rain_next_4h" | "tomorrow_rain"
        | "rain_3day" => Some("rain"),
        "wind_now" | "wind_forecast" => Some("wind"),
        "restrictions" => Some("restriction"),
        "freeze_now" | "overnight_freeze" | "soil_frost" => Some("cold"),
        "soil_saturation" | "soil_quarantine" | "soil_floor" => Some("soil"),
        // Recognized but non-headline codes (control gates, live-data, a clean
        // run): bucket as "other" without dropping to substring guessing.
        "override" | "paused" | "pause_until" | "live_data" | "dry_run" | "condition" | "run" => {
            Some("other")
        }
        _ => None,
    }
}

/// Categorize skip *days* into headline buckets for the "why" breakdown.
/// Takes the decision feed (the engine re-evaluates many times a day), keeps
/// the latest verdict per calendar day, and counts the days that ended in a
/// skip by reason. Returns (label, count, css-color), largest bucket first.
fn skip_breakdown(
    decisions: &[DecisionRecord],
    tz: &str,
) -> Vec<(&'static str, usize, &'static str)> {
    use std::collections::HashMap;
    let mut latest: HashMap<String, &DecisionRecord> = HashMap::new();
    for d in decisions {
        // Group by the deployment-TZ calendar day so "latest verdict per day"
        // matches the day the controller experienced, not the viewer's.
        let key = day_key_in_tz(d.epoch, tz);
        latest
            .entry(key)
            .and_modify(|cur| {
                if d.epoch > cur.epoch {
                    *cur = d;
                }
            })
            .or_insert(d);
    }
    let (mut rain, mut wind, mut restriction, mut cold, mut soil, mut other) = (0, 0, 0, 0, 0, 0);
    for d in latest.values() {
        if d.verdict != "skip" {
            continue;
        }
        // P2 units architecture: classify on the structured reason_code (carried on
        // the captured trace) so the tally is unit-independent; legacy rows with no
        // trace / an empty code fall back to the baked-reason substring match.
        let code = d
            .trace
            .as_ref()
            .map(|t| t.reason_code.as_str())
            .unwrap_or("");
        match classify_skip_code(code) {
            Some("rain") => rain += 1,
            Some("wind") => wind += 1,
            Some("restriction") => restriction += 1,
            Some("cold") => cold += 1,
            Some("soil") => soil += 1,
            Some(_) => other += 1,
            None => {
                // Legacy / uncoded row: fall back to substring classification.
                let l = d.reason.to_lowercase();
                if l.contains("rain") {
                    rain += 1;
                } else if l.contains("wind") {
                    wind += 1;
                } else if l.contains("restrict")
                    || l.contains("allowed day")
                    || l.contains("forbidden")
                {
                    restriction += 1;
                } else if l.contains("freez") || l.contains("cold") || l.contains("temp") {
                    cold += 1;
                } else if l.contains("saturat")
                    || l.contains("moist")
                    || l.contains("soil")
                    || l.contains("enough")
                    || l.contains("budget")
                {
                    soil += 1;
                } else {
                    other += 1;
                }
            }
        }
    }
    let mut v = vec![
        ("Rain", rain, "var(--accent-rain)"),
        ("Wind", wind, "var(--accent-warm)"),
        ("Restriction", restriction, "var(--accent)"),
        ("Cold / freeze", cold, "var(--verdict-skip)"),
        ("Soil / budget", soil, "var(--accent-good)"),
        ("Other", other, "var(--text-faint)"),
    ];
    v.retain(|(_, c, _)| *c > 0);
    v.sort_by_key(|r| std::cmp::Reverse(r.1));
    v
}

/// Format minutes with negative-zero normalized away ("-0" reads as a
/// bug, and float sums love producing it).
fn fmt_min(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v:.0}")
}

/// Chronological run log grouped by day, newest first: when each zone
/// actually ran (or why it was skipped), so History answers "what
/// happened" precisely instead of only in totals. Grouped by the deployment
/// timezone `tz` calendar day; each group carries a representative epoch (the
/// day's earliest run) so the header renders in that same TZ.
fn run_log_days(runs: &[RunRecord], tz: &str) -> Vec<(i64, Vec<RunRecord>)> {
    use std::collections::BTreeMap;
    // Key on the deployment-TZ day string (sortable) so grouping matches the
    // controller's calendar day; value carries the runs for that day.
    let mut by_day: BTreeMap<String, Vec<RunRecord>> = BTreeMap::new();
    for r in runs {
        let key = day_key_in_tz(r.start_epoch, tz);
        by_day.entry(key).or_default().push(r.clone());
    }
    let mut days: Vec<(i64, Vec<RunRecord>)> = by_day
        .into_values()
        .map(|mut rs| {
            rs.sort_by_key(|r| r.start_epoch);
            // Representative epoch for the day header: the earliest run's start.
            let rep = rs.first().map(|r| r.start_epoch).unwrap_or(0);
            (rep, rs)
        })
        .collect();
    // BTreeMap iterates oldest-first by key; reverse for newest-first display.
    days.reverse();
    days
}

/// "Sunday, Jun 28" style day header for a run-log group, in the deployment
/// timezone. Long-weekday + short month + day, all from `epoch` via timefmt
/// (timefmt has no long-weekday helper, so the full names live here).
fn fmt_day_header(epoch: i64, tz: &str) -> String {
    let wday = match format_wday_short(epoch, tz).as_str() {
        "Mon" => "Monday",
        "Tue" => "Tuesday",
        "Wed" => "Wednesday",
        "Thu" => "Thursday",
        "Fri" => "Friday",
        "Sat" => "Saturday",
        "Sun" => "Sunday",
        // Some locales/Intl may already return a long name; pass it through.
        other => other,
    }
    .to_string();
    let md = format_md(epoch, tz);
    if wday.is_empty() {
        md
    } else {
        format!("{wday}, {md}")
    }
}

/// 24-hour, deployment-local clock "HH:MM" for a run-log row's start time.
fn fmt_clock(epoch: i64, tz: &str) -> String {
    format_hm(epoch, tz)
}

fn fmt_duration(s: i64) -> String {
    let m = s / 60;
    let sec = s % 60;
    if m == 0 {
        format!("{sec}s")
    } else if sec == 0 {
        format!("{m} min")
    } else {
        format!("{m}m {sec:02}s")
    }
}

/// Local-time epoch bounds [start, end) of a calendar month, for the run
/// log's month jump.
fn month_bounds(y: i32, m: u32) -> (i64, i64) {
    let start = Local
        .with_ymd_and_hms(y, m, 1, 0, 0, 0)
        .single()
        .map(|dt| dt.timestamp())
        .unwrap_or(0);
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let end = Local
        .with_ymd_and_hms(ny, nm, 1, 0, 0, 0)
        .single()
        .map(|dt| dt.timestamp())
        .unwrap_or(i64::MAX);
    (start, end)
}

/// The last 24 months, newest first, as (year, month, "April 2026") for
/// the month-jump select. Built client-side after hydration so the SSR
/// frame never depends on the render clock.
#[cfg(feature = "hydrate")]
fn month_options() -> Vec<(i32, u32, String)> {
    use chrono::Datelike;
    let now = Local::now();
    let (mut y, mut m) = (now.year(), now.month());
    let mut out = Vec::with_capacity(24);
    for _ in 0..24 {
        let label = chrono::NaiveDate::from_ymd_opt(y, m, 1)
            .map(|d| d.format("%B %Y").to_string())
            .unwrap_or_default();
        out.push((y, m, label));
        if m == 1 {
            y -= 1;
            m = 12;
        } else {
            m -= 1;
        }
    }
    out
}

fn print_page() {
    #[cfg(feature = "hydrate")]
    if let Some(win) = web_sys::window() {
        let _ = win.print();
    }
}

/// Render the watering heatmap as a week-aligned, GitHub-contribution-graph
/// grid: 7 weekday columns (Sunday-first, US convention), one row per week,
/// oldest week on top. `b` is the daily watered-minutes buckets oldest ->
/// newest (the `day_buckets` orientation), so its last element is today; the
/// range never extends past today.
///
/// The single status encoding (watered intensity vs skip vs none) is the
/// cell background, unchanged from before; this adds spatial structure
/// (week rows + weekday columns), weekday header letters, per-month-boundary
/// labels, and a ring on today's cell.
fn cal_weeks(b: &[f64], max: f64, tz: &str) -> impl IntoView {
    use chrono::Datelike;

    let n = b.len();
    let today = Local::now().date_naive();
    // "Now" epoch so each cell's hover title renders its weekday + date in the
    // DEPLOYMENT timezone (the grid geometry below stays on browser-local
    // NaiveDate, which is layout, not a clock render).
    let now_epoch = Local::now().timestamp();
    // The first rendered bucket (index 0) is the oldest day in the window.
    let first = today.checked_sub_days(chrono::Days::new((n.saturating_sub(1)) as u64));

    // Leading blanks: push the first day under its correct weekday column.
    // Week starts Sunday, so the offset is the first day's distance from
    // Sunday (Sun=0 .. Sat=6). E.g. first day Wednesday => 3 blanks
    // (Sun, Mon, Tue) before it.
    let lead = first
        .map(|d| d.weekday().num_days_from_sunday() as usize)
        .unwrap_or(0);

    // Flatten into a slot stream: `lead` placeholders, then one slot per day,
    // padded to a whole number of weeks. Each real slot carries its date so
    // we can place month labels and mark today.
    #[derive(Clone)]
    enum Slot {
        Blank,
        Day {
            date: chrono::NaiveDate,
            epoch: i64,
            minutes: f64,
        },
    }
    let mut slots: Vec<Slot> = Vec::with_capacity(lead + n + 6);
    for _ in 0..lead {
        slots.push(Slot::Blank);
    }
    for (i, &m) in b.iter().enumerate() {
        let date = first.and_then(|d| d.checked_add_days(chrono::Days::new(i as u64)));
        // Bucket i is (n-1-i) days back from today (oldest -> newest).
        let epoch = now_epoch - ((n - 1 - i) as i64) * 86_400;
        match date {
            Some(date) => slots.push(Slot::Day {
                date,
                epoch,
                minutes: m,
            }),
            None => slots.push(Slot::Blank),
        }
    }
    // Pad the trailing partial week so every row has 7 columns.
    while !slots.len().is_multiple_of(7) {
        slots.push(Slot::Blank);
    }

    let weeks: Vec<Vec<Slot>> = slots.chunks(7).map(|c| c.to_vec()).collect();

    let header = ["S", "M", "T", "W", "T", "F", "S"];

    view! {
        <div class="hist-cal" role="grid" aria-label="Watering calendar by week">
            <div class="hist-cal__corner" aria-hidden="true"></div>
            {header.iter().map(|d| view! {
                <div class="hist-cal__dow" aria-hidden="true">{*d}</div>
            }).collect_view()}
            {weeks.into_iter().map(|week| {
                // Month label for the row: shown when this week introduces a
                // new month (the first real day whose day-of-month <= 7, i.e.
                // the week the month begins), GitHub-style.
                let month_label = week.iter().find_map(|s| match s {
                    Slot::Day { date, .. } if date.day() <= 7 => {
                        Some(month_abbr(date.month()).to_string())
                    }
                    _ => None,
                }).unwrap_or_default();
                view! {
                    <div class="hist-cal__month" aria-hidden="true">{month_label}</div>
                    {week.into_iter().map(|slot| match slot {
                        Slot::Blank => view! {
                            <span class="hist-cal__cell hist-cal__cell--blank" aria-hidden="true"></span>
                        }.into_any(),
                        Slot::Day { date, epoch, minutes } => {
                            let bg = if minutes <= 0.0 {
                                "var(--elev-1)".to_string()
                            } else {
                                let pct = (18.0 + (minutes / max).min(1.0) * 67.0) as i32;
                                format!("color-mix(in oklab, var(--accent) {pct}%, transparent)")
                            };
                            let is_today = date == today;
                            // Weekday + date in the deployment TZ (e.g. "Sun, Jun 28").
                            let title = format!(
                                "{}, {}: {:.0} min",
                                format_wday_short(epoch, tz),
                                format_md(epoch, tz),
                                minutes,
                            );
                            view! {
                                <span
                                    class="hist-cal__cell"
                                    class:is-today=is_today
                                    role="gridcell"
                                    style=format!("background:{bg}")
                                    title=title
                                ></span>
                            }.into_any()
                        }
                    }).collect_view()}
                }
            }).collect_view()}
        </div>
    }
}

/// Three-letter month abbreviation for the calendar's month labels (avoids a
/// chrono format alloc per row and is locale-stable for the label rail).
fn month_abbr(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "",
    }
}

/// Read one `key=value` from a raw search string ("?a=1&b=2"), value-decoded
/// only enough for our numeric/`y-m` params (no percent-encoding in play here).
fn search_param(search: &str, key: &str) -> Option<String> {
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_string))
        .filter(|v| !v.is_empty())
}

/// Replace-mode navigate options: a filter change updates the URL (so refresh
/// and share keep the range) without pushing a back-stack entry.
fn replace_nav() -> NavigateOptions {
    NavigateOptions {
        replace: true,
        ..Default::default()
    }
}

/// Build the History URL from the three filter values, omitting defaults so a
/// clean range yields a bare `/history`. Pure (no captures) so each navigate
/// callback can call it freely.
fn history_url(range: i64, log: i64, month: Option<(i32, u32)>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if range != 30 {
        parts.push(format!("range={range}"));
    }
    if log != 7 {
        parts.push(format!("log={log}"));
    }
    if let Some((y, m)) = month {
        parts.push(format!("month={y}-{m}"));
    }
    if parts.is_empty() {
        "/history".to_string()
    } else {
        format!("/history?{}", parts.join("&"))
    }
}

#[component]
pub fn HistoryPage() -> impl IntoView {
    // The three range filters are URL state (?range / ?log / ?month), not bare
    // signals, so a refresh or a shared link keeps the range. Changes navigate
    // with replace:true: a filter tweak shouldn't push a back-stack entry (back
    // should leave History, not undo a chip), it just makes the URL durable.
    // SSR + hydrate both derive from the same URL, so no mismatch. An
    // unknown/missing value maps to the default (no phantom state).
    let loc = use_location();
    let nav = use_navigate();
    // Per-device display-unit preference (depth shown in the scoreboard's
    // forecast-vs-gauge rows). Read prefs.get() inside the render closures so a
    // unit toggle (or the post-hydration localStorage load) re-renders.
    let prefs = use_unit_prefs();

    // Page window (KPIs, charts, calendar): 30 / 90 / 365, default 30.
    let days = Signal::derive(move || {
        search_param(&loc.search.get(), "range")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|d| matches!(d, 30 | 90 | 365))
            .unwrap_or(30)
    });
    // Run-log display range (independent of the page window): 0 = all, default 7.
    let runlog_days = Signal::derive(move || {
        search_param(&loc.search.get(), "log")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|d| matches!(d, 0 | 7 | 30 | 90))
            .unwrap_or(7)
    });
    // Month jump (?month=YYYY-M) overrides the run-log range chips while set.
    let runlog_month = Signal::derive(move || -> Option<(i32, u32)> {
        let v = search_param(&loc.search.get(), "month")?;
        let (y, m) = v.split_once('-')?;
        let (y, m) = (y.parse::<i32>().ok()?, m.parse::<u32>().ok()?);
        (1..=12).contains(&m).then_some((y, m))
    });

    // Merge the three current values (only the changed one differs) and
    // replace-navigate, so each filter is durable without adding history.
    // Callbacks are Copy so they reuse freely across buttons / map closures;
    // each clones its own navigate handle (use_navigate's closure is Clone).
    // Set just the page window, preserving the run-log selection.
    let n_days = nav.clone();
    let set_days: Callback<i64> = Callback::new(move |d: i64| {
        let url = history_url(d, runlog_days.get_untracked(), runlog_month.get_untracked());
        n_days(&url, replace_nav());
    });
    // Set the run-log range chip (clears any month jump), preserving the window.
    let n_log = nav.clone();
    let set_runlog_days: Callback<i64> = Callback::new(move |d: i64| {
        let url = history_url(days.get_untracked(), d, None);
        n_log(&url, replace_nav());
    });
    // Set / clear the month jump, preserving window + run-log range.
    let n_month = nav;
    let set_runlog_month: Callback<Option<(i32, u32)>> = Callback::new(move |m| {
        let url = history_url(days.get_untracked(), runlog_days.get_untracked(), m);
        n_month(&url, replace_nav());
    });

    let runlog_query = RwSignal::new(String::new());
    let month_opts: RwSignal<Vec<(i32, u32, String)>> = RwSignal::new(Vec::new());
    // The run log fetches its own window sized to the selection, so "All"
    // and month jumps reach past the page-level 30/90/365 range.
    let runlog_window = RwSignal::new(HistoryWindow::default());
    let runlog_loaded = RwSignal::new(false);
    let window = RwSignal::new(HistoryWindow::default());
    let loaded = RwSignal::new(false);
    // Decisions feed: the skip *story* (rain/restriction/...) lives here, not
    // in the run records (which are only actual waterings).
    let decisions = RwSignal::new(Vec::<DecisionRecord>::new());
    // P3-4: forecast-accuracy scoreboard (per-day forecast vs observed rain).
    let scoreboard = RwSignal::new(crate::ha::snapshot::AccuracyResult::default());
    let scoreboard_loaded = RwSignal::new(false);
    // Deployment IANA timezone for every user-facing date/time render on this
    // page. History mounts with no snapshot prop, so we fetch one irrigation
    // snapshot to learn the controller's timezone; empty until it lands, which
    // timefmt treats as browser-local (hydrate) / UTC (ssr) -- the prior
    // behavior, so no SSR/first-paint mismatch.
    let tz = RwSignal::new(String::new());

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            leptos::task::spawn_local(async move {
                if let Ok(resp) = gloo_net::http::Request::get("/api/irrigation/snapshot")
                    .send()
                    .await
                {
                    if let Ok(s) = resp.json::<IrrigationSnapshot>().await {
                        if !s.timezone.is_empty() {
                            tz.set(s.timezone);
                        }
                    }
                }
            });
        });
        Effect::new(move |_| {
            let d = days.get();
            leptos::task::spawn_local(async move {
                let url = format!("/api/v1/irrigation/accuracy?days={d}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if let Ok(r) = resp.json::<crate::ha::snapshot::AccuracyResult>().await {
                        scoreboard.set(r);
                    }
                }
                scoreboard_loaded.set(true);
            });
        });
        Effect::new(move |_| {
            let d = days.get();
            leptos::task::spawn_local(async move {
                let url = format!("/api/irrigation/history?days={d}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if let Ok(w) = resp.json::<HistoryWindow>().await {
                        window.set(w);
                    }
                }
                loaded.set(true);
            });
        });
        Effect::new(move |_| {
            let d = days.get();
            leptos::task::spawn_local(async move {
                let url = format!("/api/v1/irrigation/decisions?days={d}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if let Ok(w) = resp.json::<DecisionWindow>().await {
                        decisions.set(w.decisions);
                    }
                }
            });
        });
        Effect::new(move |_| {
            let sel = runlog_days.get();
            let fetch_days: i64 = match runlog_month.get() {
                Some((y, m)) => {
                    let (start, _) = month_bounds(y, m);
                    ((chrono::Utc::now().timestamp() - start) / 86_400 + 2).max(1)
                }
                None if sel == 0 => 36_500,
                None => sel,
            };
            leptos::task::spawn_local(async move {
                let url = format!("/api/irrigation/history?days={fetch_days}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if let Ok(w) = resp.json::<HistoryWindow>().await {
                        runlog_window.set(w);
                    }
                }
                runlog_loaded.set(true);
            });
        });
        Effect::new(move |_| {
            month_opts.set(month_options());
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = (
        window,
        decisions,
        runlog_window,
        scoreboard,
        scoreboard_loaded,
        tz,
    );

    view! {
        <div class="hist-page">
            <header class="hist-page__header">
                <div>
                    <p class="hist-page__eyebrow">"Analyze"</p>
                    <h1 class="hist-page__title">"History"</h1>
                </div>
                <div class="hist-page__tools">
                    <RangeBtn label="30d" d=30 days set_days/>
                    <RangeBtn label="90d" d=90 days set_days/>
                    <RangeBtn label="1yr" d=365 days set_days/>
                    <Button variant="ghost" icon="print" on_click=Callback::new(move |_| print_page())>"Print"</Button>
                    // P2-11: portable export. The attachment header makes it a
                    // download; the endpoint defaults to the full year.
                    <Button
                        variant="ghost"
                        icon="download"
                        href=crate::base::url("/api/v1/irrigation/export?format=csv")
                    >
                        "Download CSV"
                    </Button>
                </div>
            </header>

            // KPI tiles.
            {move || {
                if !loaded.get() {
                    return view! {
                        <div class="hist-kpis">
                            {(0..4).map(|_| view! { <crate::components::ui::Skeleton variant="tile"/> }).collect_view()}
                        </div>
                    }
                    .into_any();
                }
                let w = window.get();
                let runs: Vec<&RunRecord> = w.runs.iter().filter(|r| r.skip_reason.is_none()).collect();
                let total_min: f64 = runs.iter().map(|r| r.duration_s as f64 / 60.0).sum();
                let run_count = runs.len();
                // Skip *days* (from the decision feed), not run records, runs
                // are only actual waterings, so that count is always ~0.
                let skip_count: usize = skip_breakdown(&decisions.get(), &tz.get())
                    .iter()
                    .map(|(_, c, _)| c)
                    .sum();
                let overall = day_buckets(&w.runs, days.get(), None);
                view! {
                    <div class="hist-kpis">
                        <StatTile label="Water applied" value=fmt_min(total_min) unit="min" icon="droplet" spark=overall.clone() accent="var(--accent)".to_string()/>
                        <StatTile label="Runs" value=run_count.to_string() icon="play" accent="var(--accent-good)".to_string()/>
                        <StatTile label="Skips" value=skip_count.to_string() icon="ban" accent="var(--accent-rain)".to_string()/>
                        <StatTile label="Avg / day" value=fmt_min(overall.iter().sum::<f64>() / overall.len().max(1) as f64) unit="min" icon="gauge" accent="var(--accent-warm)".to_string()/>
                    </div>
                }
                .into_any()
            }}

            // P3-4: forecast scoreboard. The honest "was it right" artifact:
            // every rain call graded against the rain that actually fell.
            <section class="hist-panel scoreboard">
                <h2 class="hist-panel__title">"Forecast scoreboard"</h2>
                {move || {
                    if !scoreboard_loaded.get() {
                        return view! { <crate::components::ui::Skeleton variant="chart"/> }.into_any();
                    }
                    let sb = scoreboard.get();
                    let (scored, matched) = (sb.scored, sb.matched);
                    if scored == 0 {
                        return view! {
                            <p class="scoreboard__empty">
                                "No rain calls to grade in this window yet. As rain is forecast or "
                                "falls on watering days, LocalSky's calls land here, graded honestly "
                                "against the gauge."
                            </p>
                        }
                        .into_any();
                    }
                    let pct = (matched as f64 / scored as f64 * 100.0).round() as u32;
                    let rain_days: Vec<_> =
                        sb.days.into_iter().filter(|d| d.correct.is_some()).collect();
                    let p = prefs.get();
                    view! {
                        <div class="scoreboard__headline">
                            <span class="scoreboard__big">{matched}" / "{scored}</span>
                            <span class="scoreboard__big-sub">
                                "rain calls matched the sky · "{pct}"%"
                            </span>
                        </div>
                        <ul class="scoreboard__list">
                            {rain_days.into_iter().map(|d| {
                                let ok = d.correct.unwrap_or(false);
                                let mark_cls = if ok { "scoreboard__mark is-ok" } else { "scoreboard__mark is-miss" };
                                let pred = d.predicted_in.map(|v| fmt_rain_amount(v, p)).unwrap_or_else(|| "-".into());
                                let obs = d.observed_in.map(|v| fmt_rain_amount(v, p)).unwrap_or_else(|| "-".into());
                                view! {
                                    <li class="scoreboard__row">
                                        <span class=mark_cls>{if ok { "✓" } else { "✗" }}</span>
                                        <span class="scoreboard__date">{d.date}</span>
                                        <span class="scoreboard__assess">{d.assessment}</span>
                                        <span class="scoreboard__rain">"forecast "{pred}" · gauge "{obs}</span>
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    }
                    .into_any()
                }}
            </section>

            // Daily watered-minutes line chart.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"Watered minutes per day"</h2>
                {move || {
                    if !loaded.get() {
                        return view! { <crate::components::ui::Skeleton variant="chart"/> }.into_any();
                    }
                    let w = window.get();
                    let b = day_buckets(&w.runs, days.get(), None);
                    if b.iter().all(|m| *m <= 0.0) {
                        return view! {
                            <div class="hist-empty">
                                "No watering recorded in this window yet. Once zones run, every "
                                "minute lands here automatically."
                            </div>
                        }
                        .into_any();
                    }
                    let pts: Vec<(f64, f64)> = b.iter().enumerate().map(|(i, m)| (i as f64, *m)).collect();
                    // Index i is "i days ago" (day_buckets orientation). Label each
                    // from an epoch rendered "Jun 28"-style in the DEPLOYMENT TZ.
                    let now_epoch = Local::now().timestamp();
                    let tzs = tz.get();
                    let n = b.len();
                    let labels: Vec<String> = (0..n)
                        .map(|i| {
                            // Buckets run oldest -> newest; label to match.
                            let epoch = now_epoch - ((n - 1 - i) as i64) * 86_400;
                            format_md(epoch, &tzs)
                        })
                        .collect();
                    let series = vec![Series::new("Watered (min)", "var(--accent)", pts)];
                    view! { <LineChart series height=200 y_unit=" min".to_string() x_labels=labels/> }.into_any()
                }}
            </section>

            // Watering calendar heatmap, the at-a-glance "which days watered".
            // Run log: the precise record, one row per run or skip. Its own
            // range chips (default 7 days) so a long memory doesn't shove
            // the rest of the page below the fold.
            <section class="hist-panel">
                <div class="hist-panel__head-row">
                    <div>
                        <h2 class="hist-panel__title">"Run log"</h2>
                        <p class="hist-panel__sub">"Every start, duration, and skip, exactly as it happened."</p>
                    </div>
                    <div class="runlog-range" role="tablist" aria-label="Run log range">
                        {[(7i64, "7d"), (30, "30d"), (90, "90d"), (0, "All")].into_iter().map(|(d, label)| view! {
                            <button
                                type="button"
                                class="runlog-range__btn"
                                class:is-active=move || runlog_month.get().is_none() && runlog_days.get() == d
                                on:click=move |_| set_runlog_days.run(d)
                            >{label}</button>
                        }).collect_view()}
                    </div>
                </div>
                <div class="runlog-tools">
                    <input
                        type="search"
                        class="runlog-tools__search"
                        placeholder="Search zone or reason"
                        aria-label="Search run log"
                        prop:value=move || runlog_query.get()
                        on:input=move |ev| runlog_query.set(event_target_value(&ev))
                    />
                    <select
                        class="runlog-tools__month"
                        aria-label="Jump to a month"
                        on:change=move |ev| {
                            let v = event_target_value(&ev);
                            let ym = v.split_once('-').and_then(|(a, b)| Some((a.parse::<i32>().ok()?, b.parse::<u32>().ok()?)));
                            set_runlog_month.run(ym);
                        }
                    >
                        <option value="" selected=move || runlog_month.get().is_none()>"All months"</option>
                        {move || month_opts.get().into_iter().map(|(y, m, label)| view! {
                            <option value=format!("{y}-{m:02}") selected=move || runlog_month.get() == Some((y, m))>{label}</option>
                        }).collect_view()}
                    </select>
                </div>
                {move || {
                    if !runlog_loaded.get() {
                        return view! { <crate::components::ui::SkeletonRows count=4/> }.into_any();
                    }
                    let mut runs: Vec<RunRecord> = match runlog_month.get() {
                        Some((y, m)) => {
                            let (lo, hi) = month_bounds(y, m);
                            runlog_window.get().runs.into_iter()
                                .filter(|r| r.start_epoch >= lo && r.start_epoch < hi)
                                .collect()
                        }
                        None => {
                            let sel = runlog_days.get();
                            if sel == 0 {
                                runlog_window.get().runs
                            } else {
                                let cutoff = chrono::Utc::now().timestamp() - sel * 86_400;
                                runlog_window.get().runs.into_iter()
                                    .filter(|r| r.start_epoch >= cutoff)
                                    .collect()
                            }
                        }
                    };
                    let q = runlog_query.get().trim().to_lowercase();
                    if !q.is_empty() {
                        runs.retain(|r| {
                            r.zone.to_lowercase().replace('_', " ").contains(&q.replace('_', " "))
                                || r.skip_reason.as_deref().is_some_and(|s| s.to_lowercase().contains(&q))
                                || (r.skip_reason.is_none() && "watered".contains(&q))
                                || (r.skip_reason.is_some() && "skipped".contains(&q))
                        });
                    }
                    let tzs = tz.get();
                    let days = run_log_days(&runs, &tzs);
                    if days.is_empty() {
                        if !q.is_empty() {
                            return view! {
                                <div class="hist-empty">"No runs or skips match that search in this range."</div>
                            }.into_any();
                        }
                        return view! {
                            <div class="hist-empty">"Nothing recorded in this range yet. Widen the range above, or wait: runs and skips land here the moment they happen."</div>
                        }.into_any();
                    }
                    days.into_iter().map(|(rep_epoch, rows)| {
                        let row_tz = tzs.clone();
                        let watered_s: i64 = rows.iter().filter(|r| r.skip_reason.is_none()).map(|r| r.duration_s).sum();
                        let header = fmt_day_header(rep_epoch, &tzs);
                        view! {
                            <div class="runlog-day">
                                <div class="runlog-day__head">
                                    <span class="runlog-day__date">{header}</span>
                                    <span class="runlog-day__total">{
                                        if watered_s >= 60 {
                                            format!("{} min watered", watered_s / 60)
                                        } else if watered_s > 0 {
                                            "under a minute watered".to_string()
                                        } else {
                                            "no watering".to_string()
                                        }
                                    }</span>
                                </div>
                                {rows.into_iter().map(|r| {
                                    let skipped = r.skip_reason.is_some();
                                    let detail = match &r.skip_reason {
                                        Some(reason) => reason.clone(),
                                        None => fmt_duration(r.duration_s),
                                    };
                                    view! {
                                        <div class="runlog-row" class:runlog-row--skip=skipped>
                                            <span class="runlog-row__time">{fmt_clock(r.start_epoch, &row_tz)}</span>
                                            <span class="runlog-row__zone">{r.zone.replace('_', " ")}</span>
                                            <span class="runlog-row__badge">{if skipped { "skipped" } else { "watered" }}</span>
                                            <span class="runlog-row__detail">{detail}</span>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        }
                    }).collect_view().into_any()
                }}
                <p class="hist-panel__hint">
                    "History is kept forever by default, which is what makes year-over-year trends possible. A retention cap is available under Settings if you ever want one."
                </p>
            </section>

            <section class="hist-panel">
                <h2 class="hist-panel__title">"Watering calendar"</h2>
                <p class="hist-panel__hint">"Each square is a day, aligned by weekday; greener = more watering, empty = a skip day."</p>
                {move || {
                    let w = window.get();
                    let b = day_buckets(&w.runs, days.get(), None);
                    let max = b.iter().cloned().fold(0.0f64, f64::max).max(1.0);
                    cal_weeks(&b, max, &tz.get())
                }}
            </section>

            // Why it skipped, the headline "story" of the period.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"Why it skipped"</h2>
                <p class="hist-panel__hint">"Days the engine chose to skip, by reason."</p>
                {move || {
                    let bd = skip_breakdown(&decisions.get(), &tz.get());
                    let total: usize = bd.iter().map(|(_, c, _)| *c).sum();
                    if total == 0 {
                        return view! { <div class="hist-empty">"No skips in this window, everything ran as planned."</div> }.into_any();
                    }
                    view! {
                        <div class="hist-breakdown">
                            {bd.into_iter().map(|(label, count, color)| {
                                let pct = (count as f64 / total as f64 * 100.0).round() as i32;
                                view! {
                                    <div class="hist-bar">
                                        <span class="hist-bar__label">{label}</span>
                                        <span class="hist-bar__track">
                                            <span class="hist-bar__fill" style=format!("width:{pct}%; background:{color}")></span>
                                        </span>
                                        <span class="hist-bar__val">{count}" ("{pct}"%)"</span>
                                    </div>
                                }
                            }).collect_view()}
                        </div>
                    }.into_any()
                }}
            </section>

            // Per-zone breakdown.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"By zone"</h2>
                {move || {
                    let w = window.get();
                    let d = days.get();
                    let mut zones: Vec<String> = w.runs.iter().map(|r| r.zone.clone()).collect();
                    zones.sort();
                    zones.dedup();
                    if zones.is_empty() {
                        return view! { <div class="hist-empty">"No runs recorded in this window yet."</div> }.into_any();
                    }
                    zones.into_iter().map(|z| {
                        let b = day_buckets(&w.runs, d, Some(&z));
                        let total: f64 = b.iter().sum();
                        view! {
                            <div class="hist-zone-row">
                                <span class="hist-zone-row__name">{z}</span>
                                <span class="hist-zone-row__spark"><Sparkline points=b accent="var(--accent)".to_string() height=34/></span>
                                <span class="hist-zone-row__total">{format!("{:.0} min", total)}</span>
                            </div>
                        }
                    }).collect_view().into_any()
                }}
            </section>
        </div>
    }
}

#[component]
fn RangeBtn(
    label: &'static str,
    d: i64,
    days: Signal<i64>,
    set_days: Callback<i64>,
) -> impl IntoView {
    let cls = move || {
        if days.get() == d {
            "hist-range is-on"
        } else {
            "hist-range"
        }
    };
    view! {
        <button type="button" class=cls on:click=move |_| set_days.run(d)>{label}</button>
    }
}
