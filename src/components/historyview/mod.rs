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

use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};
use leptos_router::NavigateOptions;

use crate::components::ui::{Button, LineChart, Series, Sparkline, StatTile};
use crate::components::units_fmt::{fmt_rain_amount, use_unit_prefs};
mod daily;
use crate::history::types::{HistoryWindow, RunRecord};
#[cfg(feature = "hydrate")]
use crate::model::IrrigationSnapshot;
use crate::timefmt::{format_hm, format_md, format_wday_short};

use crate::timefmt::day_key_in_tz;

/// Daily watered-minutes buckets, oldest -> newest, length `days`.
/// Minutes are the union-clustered watering evidence (the shared
/// history::rollup rule): a manual run recorded twice (manual row +
/// observer row) counts once, and dry-run/skip rows never count, so the
/// chart can never show more minutes than the water balance credits.
fn day_buckets(runs: &[RunRecord], days: i64, zone: Option<&str>, tz: &str) -> Vec<f64> {
    day_buckets_at(runs, days, zone, tz, crate::timefmt::now_epoch())
}

/// `day_buckets` judged at `now_epoch`, in the deployment's timezone.
/// Days are calendar days in `tz`, the same rule `run_log_days` keys on,
/// so a run at 04:00 local lands in the same day on the chart, the run
/// log and the water balance. This used to bucket from the render
/// clock's midnight: the server's zone at SSR, the browser's on hydrate.
fn day_buckets_at(
    runs: &[RunRecord],
    days: i64,
    zone: Option<&str>,
    tz: &str,
    now_epoch: i64,
) -> Vec<f64> {
    let n = days.max(1) as usize;
    let mut b = vec![0f64; n];
    let filtered: Vec<RunRecord> = runs
        .iter()
        .filter(|r| zone.map(|z| r.zone == z).unwrap_or(true))
        .cloned()
        .collect();
    for events in crate::history::rollup::watering_intervals_per_zone(&filtered).values() {
        for e in events {
            for (start, end) in crate::timefmt::split_local_days(e.start_epoch, e.end_epoch, tz) {
                let Some(back) = crate::timefmt::days_between_in_tz(now_epoch, start, tz) else {
                    continue;
                };
                if back >= 0 && (back as usize) < n {
                    b[back as usize] += (end - start) as f64 / 60.0;
                }
            }
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
fn classify_skip_code(code: &str, reason: &str) -> Option<&'static str> {
    use crate::gates_catalog::GateFamily;
    match code {
        // The soil rows tally as soil, not water: this page counts what
        // held the yard, and moist soil is its own answer here.
        "soil_saturation" | "soil_quarantine" | "soil_floor" => return Some("soil"),
        // Recognized but non-headline codes (control gates, live-data, a clean
        // run): bucket as "other" without dropping to substring guessing.
        "override" | "live_data" | "dry_run" | "condition" | "run" => return Some("other"),
        "" => return None,
        _ => {}
    }
    match GateFamily::of(code, reason) {
        GateFamily::Water => Some("rain"),
        GateFamily::Wind => Some("wind"),
        GateFamily::Restriction => Some("restriction"),
        GateFamily::Freeze => Some("cold"),
        GateFamily::Pause | GateFamily::SoilModel | GateFamily::NoData => Some("other"),
        GateFamily::Other => None,
    }
}

/// Categorize skip *days* into headline buckets for the "why" breakdown.
/// Recorded automatic outcomes, one final result per zone and local day.
/// Live verdict transitions do not establish that the scheduled run skipped.
fn skip_breakdown(runs: &[RunRecord], tz: &str) -> Vec<(&'static str, usize, &'static str)> {
    use std::collections::HashMap;
    let mut latest: HashMap<(String, String), &RunRecord> = HashMap::new();
    for row in runs.iter().filter(|r| r.source == "smart_morning") {
        let key = (day_key_in_tz(row.start_epoch, tz), row.zone.clone());
        latest
            .entry(key)
            .and_modify(|current| {
                if row.start_epoch >= current.start_epoch {
                    *current = row;
                }
            })
            .or_insert(row);
    }
    let (mut rain, mut wind, mut restriction, mut cold, mut soil, mut other) = (0, 0, 0, 0, 0, 0);
    for row in latest.values() {
        if row.status != "skipped" {
            continue;
        }
        let reason = row.skip_reason.as_deref().unwrap_or("");
        let code = "";
        match classify_skip_code(code, reason) {
            Some("rain") => rain += 1,
            Some("wind") => wind += 1,
            Some("restriction") => restriction += 1,
            Some("cold") => cold += 1,
            Some("soil") => soil += 1,
            Some(_) => other += 1,
            None => {
                // Legacy / uncoded row: the shared prose ladder, with moist
                // soil tallied as soil the way the coded rows are.
                use crate::gates_catalog::{GateFamily, WaterKind};
                match GateFamily::from_prose(reason) {
                    GateFamily::Water => match crate::gates_catalog::water_kind("", reason) {
                        WaterKind::Soil => soil += 1,
                        _ => rain += 1,
                    },
                    GateFamily::Wind => wind += 1,
                    GateFamily::Restriction => restriction += 1,
                    GateFamily::Freeze => cold += 1,
                    _ => {
                        let l = reason.to_lowercase();
                        if l.contains("soil") || l.contains("enough") || l.contains("budget") {
                            soil += 1;
                        } else {
                            other += 1;
                        }
                    }
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
    for group in crate::history::rollup::group_run_records(runs) {
        let key = day_key_in_tz(group[0].start_epoch, tz);
        by_day.entry(key).or_default().extend(group);
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
fn month_bounds(y: i32, m: u32, tz: &str) -> (i64, i64) {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (month_start_in_tz(y, m, tz), month_start_in_tz(ny, nm, tz))
}

/// Local midnight on the first of a month in `tz`. Probes from noon UTC on
/// the first, which lands inside the local first everywhere from UTC-11
/// to UTC+11, and corrects by a day for the zones past that.
fn month_start_in_tz(y: i32, m: u32, tz: &str) -> i64 {
    let Some(first) = chrono::NaiveDate::from_ymd_opt(y, m, 1) else {
        return 0;
    };
    let noon_utc = first
        .and_hms_opt(12, 0, 0)
        .expect("noon")
        .and_utc()
        .timestamp();
    let mut probe = noon_utc;
    for _ in 0..3 {
        match crate::timefmt::day_in_tz(probe, tz) {
            Some(d) if d == first => return crate::timefmt::local_midnight_epoch(probe, tz),
            Some(d) if d > first => probe -= 86_400,
            Some(_) => probe += 86_400,
            None => break,
        }
    }
    crate::timefmt::local_midnight_epoch(noon_utc, tz)
}

/// The last 24 months, newest first, as (year, month, "April 2026") for
/// the month-jump select. Built client-side after hydration so the SSR
/// frame never depends on the render clock.
#[cfg(feature = "hydrate")]
fn month_options(tz: &str) -> Vec<(i32, u32, String)> {
    use chrono::Datelike;
    let today = crate::timefmt::day_in_tz(crate::timefmt::now_epoch(), tz)
        .unwrap_or_else(|| chrono::Utc::now().date_naive());
    let (mut y, mut m) = (today.year(), today.month());
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

fn render_run_record(r: RunRecord, tz: &str) -> impl IntoView {
    let skipped = r.skip_reason.is_some();
    let detail = r.skip_reason.clone().unwrap_or_else(|| {
        let duration = fmt_duration(r.duration_s);
        match r.note.as_deref() {
            Some(note) => format!("{duration} · {note}"),
            None => duration,
        }
    });
    let cycle = r
        .cycle_index
        .zip(r.cycle_count)
        .map(|(i, n)| format!(" · cycle {} of {n}", i + 1))
        .unwrap_or_default();
    view! {
        <div class="runlog-row" class:runlog-row--skip=skipped>
            <span class="runlog-row__time">{fmt_clock(r.start_epoch, tz)}</span>
            <span class="runlog-row__zone">{r.zone.replace('_', " ")}</span>
            <span class="runlog-row__badge">{if skipped { "skipped" } else { "recorded" }}</span>
            <span class="runlog-row__detail">{detail}{cycle}<small>{format!(" · {}", r.source)}</small></span>
        </div>
    }
}

fn render_run_group(group: Vec<RunRecord>, tz: &str) -> AnyView {
    let first = &group[0];
    if first.session_id.is_none() {
        return render_run_record(first.clone(), tz).into_any();
    }
    let seconds: i64 = crate::history::rollup::watering_intervals_per_zone(&group)
        .values()
        .flatten()
        .map(|e| e.valve_open_s)
        .sum();
    let cycles = group
        .iter()
        .filter_map(|r| r.cycle_index)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let description = if cycles > 1 {
        format!("{} · {cycles} cycles", fmt_duration(seconds))
    } else {
        fmt_duration(seconds)
    };
    let watered = group.iter().any(crate::history::rollup::is_watering_record);
    view! {
        <details class="runlog-session">
            <summary class="runlog-row">
                <span class="runlog-row__time">{fmt_clock(first.start_epoch, tz)}</span>
                <span class="runlog-row__zone">{first.zone.replace('_', " ")}</span>
                <span class="runlog-row__badge">{if watered { "watered" } else { "recorded" }}</span>
                <span class="runlog-row__detail">{description}" · details"</span>
            </summary>
            {group.into_iter().map(|r| render_run_record(r, tz)).collect_view()}
        </details>
    }.into_any()
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
    let now_epoch = crate::timefmt::now_epoch();
    // Today in the DEPLOYMENT timezone, so the grid's geometry and the
    // cells' hover titles agree with the buckets on which day is which.
    let today =
        crate::timefmt::day_in_tz(now_epoch, tz).unwrap_or_else(|| chrono::Utc::now().date_naive());
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
        // A heat map, not a data grid: the cells are read one at a time
        // by their titles, so each is an image with a name and the whole
        // is a named group (a `grid` would need rows the CSS layout has
        // no element for).
        <div class="hist-cal" role="group" aria-label="Watering calendar by week">
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
                                    role="img"
                                    aria-label=title.clone()
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
fn history_url(range: i64, log: i64, month: Option<(i32, u32)>, daily: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if daily {
        parts.push("view=daily".into());
    }
    if range != 30 {
        parts.push(format!("range={range}"));
    }
    if log != 0 {
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
    let daily_view =
        Signal::derive(move || search_param(&loc.search.get(), "view").as_deref() == Some("daily"));
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
    // All months means all retained history, independent of chart ranges.
    let runlog_days = Signal::derive(move || {
        search_param(&loc.search.get(), "log")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|d| matches!(d, 0 | 7 | 30 | 90))
            .unwrap_or(0)
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
        let url = history_url(
            d,
            runlog_days.get_untracked(),
            runlog_month.get_untracked(),
            daily_view.get_untracked(),
        );
        n_days(&url, replace_nav());
    });
    // Set the run-log range chip (clears any month jump), preserving the window.
    let n_log = nav.clone();
    let set_runlog_days: Callback<i64> = Callback::new(move |d: i64| {
        let url = history_url(days.get_untracked(), d, None, daily_view.get_untracked());
        n_log(&url, replace_nav());
    });
    // Set / clear the month jump, preserving window + run-log range.
    let n_view = nav.clone();
    let set_daily_view: Callback<bool> = Callback::new(move |daily| {
        n_view(
            &history_url(
                days.get_untracked(),
                runlog_days.get_untracked(),
                runlog_month.get_untracked(),
                daily,
            ),
            replace_nav(),
        );
    });
    let n_month = nav;
    let set_runlog_month: Callback<Option<(i32, u32)>> = Callback::new(move |m| {
        let url = history_url(days.get_untracked(), 0, m, daily_view.get_untracked());
        n_month(&url, replace_nav());
    });

    let runlog_query = RwSignal::new(String::new());
    let month_opts: RwSignal<Vec<(i32, u32, String)>> = RwSignal::new(Vec::new());
    // The run log fetches its own window sized to the selection, so "All"
    // and month jumps reach past the page-level 30/90/365 range.
    let runlog_window = RwSignal::new(HistoryWindow::default());
    let runlog_loaded = RwSignal::new(false);
    let runlog_error = RwSignal::new(false);
    #[cfg(feature = "hydrate")]
    let runlog_generation = RwSignal::new(0u64);
    let window = RwSignal::new(HistoryWindow::default());
    let loaded = RwSignal::new(false);
    let window_error = RwSignal::new(false);
    #[cfg(feature = "hydrate")]
    let window_generation = RwSignal::new(0u64);
    // Forecast-accuracy scoreboard (per-day forecast vs observed rain).
    let scoreboard = RwSignal::new(crate::model::AccuracyResult::default());
    let scoreboard_loaded = RwSignal::new(false);
    let scoreboard_error = RwSignal::new(false);
    #[cfg(feature = "hydrate")]
    let scoreboard_generation = RwSignal::new(0u64);
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
            let request = scoreboard_generation.get_untracked() + 1;
            scoreboard_generation.set(request);
            scoreboard_loaded.set(false);
            scoreboard_error.set(false);
            leptos::task::spawn_local(async move {
                let url = format!("/api/v1/irrigation/accuracy?days={d}");
                let result = async {
                    let response = gloo_net::http::Request::get(&url).send().await.ok()?;
                    if !response.ok() {
                        return None;
                    }
                    response.json::<crate::model::AccuracyResult>().await.ok()
                }
                .await;
                if scoreboard_generation.try_get_untracked() != Some(request) {
                    return;
                }
                match result {
                    Some(value) => scoreboard.set(value),
                    None => scoreboard_error.set(true),
                }
                scoreboard_loaded.set(true);
            });
        });
        Effect::new(move |_| {
            let d = days.get();
            let request = window_generation.get_untracked() + 1;
            window_generation.set(request);
            loaded.set(false);
            window_error.set(false);
            leptos::task::spawn_local(async move {
                let url = format!("/api/irrigation/history?days={d}");
                let result = async {
                    let response = gloo_net::http::Request::get(&url).send().await.ok()?;
                    if !response.ok() {
                        return None;
                    }
                    response.json::<HistoryWindow>().await.ok()
                }
                .await;
                if window_generation.try_get_untracked() != Some(request) {
                    return;
                }
                match result {
                    Some(value) => window.set(value),
                    None => window_error.set(true),
                }
                loaded.set(true);
            });
        });
        Effect::new(move |_| {
            let sel = runlog_days.get();
            let fetch_days: i64 = match runlog_month.get() {
                Some((y, m)) => {
                    let (start, _) = month_bounds(y, m, &tz.get());
                    ((chrono::Utc::now().timestamp() - start) / 86_400 + 2).max(1)
                }
                None if sel == 0 => 0,
                None => sel,
            };
            let request = runlog_generation.get_untracked() + 1;
            runlog_generation.set(request);
            runlog_loaded.set(false);
            runlog_error.set(false);
            leptos::task::spawn_local(async move {
                let url = format!("/api/irrigation/history?days={fetch_days}");
                let result = async {
                    let response = gloo_net::http::Request::get(&url).send().await.ok()?;
                    if !response.ok() {
                        return None;
                    }
                    response.json::<HistoryWindow>().await.ok()
                }
                .await;
                if runlog_generation.try_get_untracked() != Some(request) {
                    return;
                }
                match result {
                    Some(value) => runlog_window.set(value),
                    None => runlog_error.set(true),
                }
                runlog_loaded.set(true);
            });
        });
        Effect::new(move |_| {
            month_opts.set(month_options(&tz.get()));
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = (window, runlog_window, scoreboard, scoreboard_loaded, tz);

    view! {
        <div class="hist-page">
            <header class="hist-page__header">
                <div>
                    <p class="page-eyebrow">"Watering activity"</p>
                    <h1 class="page-title">"History"</h1>
                </div>
                <div class="hist-page__tools">
                    <Button variant="ghost" icon="print" on_click=Callback::new(move |_| print_page())>"Print"</Button>
                    // Portable export. The attachment header makes it a
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

            // Run log: the precise record, one row per run or skip. Its own
            // range chips (default 7 days) so a long memory doesn't shove
            // the rest of the page below the fold.
            <section class="hist-panel">
                <div class="hist-panel__head-row">
                    <div>
                        <h2 class="hist-panel__title">{move || if daily_view.get() { "Daily log" } else { "Run log" }}</h2>
                        <p class="hist-panel__sub">{move || if daily_view.get() { "What watered, what held, and why. Expand a day to read its recorded reasons." } else { "Sessions grouped by start date. Expand one to inspect its cycles and original records." }}</p>
                    </div>
                    <div class="runlog-range" role="group" aria-label="Run log range">
                        {[(7i64, "7d"), (30, "30d"), (90, "90d"), (0, "All")].into_iter().map(|(d, label)| view! {
                            <crate::components::ui::Button
    variant="secondary"
    size="sm"
    on_click=Callback::new(move |_| set_runlog_days.run(d))
    class=Signal::derive(move || format!("runlog-range__btn{}", if runlog_month.get().is_none() && runlog_days.get() == d { " is-active" } else { "" }))>{label}</crate::components::ui::Button>
                        }).collect_view()}
                    </div>
                </div>
                <div class="runlog-range" role="group" aria-label="History view">
                    <crate::components::ui::Button variant="secondary" size="sm" on_click=Callback::new(move |_| set_daily_view.run(false)) class=Signal::derive(move || format!("runlog-range__btn{}", if !daily_view.get() { " is-active" } else { "" }))>"Run log"</crate::components::ui::Button>
                    <crate::components::ui::Button variant="secondary" size="sm" on_click=Callback::new(move |_| set_daily_view.run(true)) class=Signal::derive(move || format!("runlog-range__btn{}", if daily_view.get() { " is-active" } else { "" }))>"Daily log"</crate::components::ui::Button>
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
                        <option value="recent" disabled hidden selected=move || runlog_month.get().is_none() && runlog_days.get() != 0>"Recent range"</option>
                        <option value="" selected=move || runlog_month.get().is_none() && runlog_days.get() == 0>"All months"</option>
                        {move || month_opts.get().into_iter().map(|(y, m, label)| view! {
                            <option value=format!("{y}-{m:02}") selected=move || runlog_month.get() == Some((y, m))>{label}</option>
                        }).collect_view()}
                    </select>
                </div>
                {move || {
                    if daily_view.get() {
                        return view! { <daily::DailyLog window=runlog_window loaded=runlog_loaded error=runlog_error tz month=runlog_month query=runlog_query/> }.into_any();
                    }
                    if !runlog_loaded.get() {
                        return view! { <crate::components::ui::SkeletonRows count=4/> }.into_any();
                    }
                    if runlog_error.get() {
                        return view! { <p role="alert">"Run records could not be loaded. Change the range or reload to try again."</p> }.into_any();
                    }
                    let mut runs: Vec<RunRecord> = match runlog_month.get() {
                        Some((y, m)) => {
                            let (lo, hi) = month_bounds(y, m, &tz.get_untracked());
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
                        // Day-total minutes reduce through the same union
                        // clustering as every other minutes surface; the
                        // row list below stays the precise record.
                        let watered_s: i64 = crate::history::rollup::watering_intervals_per_zone(&rows)
                            .values()
                            .flatten()
                            .map(|e| e.valve_open_s)
                            .sum();
                        let header = fmt_day_header(rep_epoch, &tzs);
                        view! {
                            <div class="runlog-day">
                                <div class="runlog-day__head">
                                    <span class="runlog-day__date">{header}</span>
                                    <span class="runlog-day__total">{
                                        if watered_s >= 60 {
                                            format!("{} min total", watered_s / 60)
                                        } else if watered_s > 0 {
                                            "under a minute watered".to_string()
                                        } else {
                                            "no watering".to_string()
                                        }
                                    }</span>
                                </div>
                                {crate::history::rollup::group_run_records(&rows).into_iter()
                                    .map(|group| render_run_group(group, &row_tz)).collect_view()}
                            </div>
                        }
                    }).collect_view().into_any()
                }}
                <p class="hist-panel__hint">
                    "History is kept forever by default, which is what makes year-over-year trends possible. A retention cap is available under Settings if you ever want one."
                </p>
            </section>

            <div class="hist-insights-heading">
                <div><h2 class="hist-panel__title">"Watering insights"</h2>
                <p class="hist-panel__sub">{move || format!("Past {} days · recorded watering and automatic holds", days.get())}</p></div>
                <div class="hist-page__tools" role="group" aria-label="Insights range">
                    <RangeBtn label="30d" d=30 days set_days/>
                    <RangeBtn label="90d" d=90 days set_days/>
                    <RangeBtn label="1yr" d=365 days set_days/>
                </div>
            </div>

            <Show when=move || !window_error.get() fallback=|| view! {
                <p role="alert" class="hist-panel">"Watering insights could not be loaded. Change the insights range or reload to try again."</p>
            }>
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
                // Union-clustered watering evidence (the shared rollup
                // rule): totals agree with the water balance's applied
                // credit, and "Runs" counts irrigation events, not the
                // duplicate rows a manual run persists.
                let by_zone = crate::history::rollup::watering_intervals_per_zone(&w.runs);
                let total_min: f64 = by_zone
                    .values()
                    .flatten()
                    .map(|e| e.valve_open_s as f64 / 60.0)
                    .sum();
                let run_count: usize = crate::history::rollup::watering_sessions_per_zone(&w.runs).values().map(|v| v.len()).sum();
                // Skip *days* (from the decision feed), not run records, runs
                // are only actual waterings, so that count is always ~0.
                let skip_count: usize = skip_breakdown(&window.get().runs, &tz.get())
                    .iter()
                    .map(|(_, c, _)| c)
                    .sum();
                let overall = day_buckets(&w.runs, days.get(), None, &tz.get());
                view! {
                    <div class="hist-kpis">
                        <StatTile label="Watering time" value=fmt_min(total_min) unit="min" icon="droplet" spark=overall.clone() accent="var(--accent)".to_string()/>
                        <StatTile label="Watering sessions" value=run_count.to_string() icon="play" accent="var(--accent-good)".to_string()/>
                        <StatTile label="Skipped zone mornings" value=skip_count.to_string() icon="ban" accent="var(--accent-rain)".to_string()/>
                        <StatTile label="Avg / day" value=fmt_min(overall.iter().sum::<f64>() / overall.len().max(1) as f64) unit="min" icon="gauge" accent="var(--accent-warm)".to_string()/>
                    </div>
                }
                .into_any()
            }}

            // Daily watered-minutes line chart.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"Watered minutes per day"</h2>
                {move || {
                    if !loaded.get() {
                        return view! { <crate::components::ui::Skeleton variant="chart"/> }.into_any();
                    }
                    let w = window.get();
                    let b = day_buckets(&w.runs, days.get(), None, &tz.get());
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
                    let now_epoch = crate::timefmt::now_epoch();
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

            <section class="hist-panel">
                <h2 class="hist-panel__title">"Watering calendar"</h2>
                <p class="hist-panel__hint">"Each square is a day, aligned by weekday; greener = more watering, empty = no watering recorded."</p>
                {move || {
                    // Gate on `loaded` like the KPI + line-chart sections. cal_weeks
                    // derives its grid structure (leading blanks + whole-week row
                    // count) from today's date, and the snapshot that carries the
                    // deployment timezone arrives after hydration: if SSR and the
                    // browser's first frame disagreed on the date across a week
                    // boundary they would emit a different number of <div>
                    // children and the tachys hydration walker would panic
                    // (=abort, dead app). Keeping both first frames on the
                    // skeleton makes them structurally identical; cal_weeks only
                    // runs post-load on the client.
                    if !loaded.get() {
                        return view! { <crate::components::ui::Skeleton variant="chart"/> }.into_any();
                    }
                    let w = window.get();
                    let b = day_buckets(&w.runs, days.get(), None, &tz.get());
                    let max = b.iter().cloned().fold(0.0f64, f64::max).max(1.0);
                    cal_weeks(&b, max, &tz.get()).into_any()
                }}
            </section>

            // Why it skipped, the headline "story" of the period.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"Why scheduled zones held"</h2>
                <p class="hist-panel__hint">"Recorded automatic holds, counted once per zone and local day. A later watering outcome replaces an earlier hold."</p>
                {move || {
                    let bd = skip_breakdown(&window.get().runs, &tz.get());
                    let total: usize = bd.iter().map(|(_, c, _)| *c).sum();
                    if total == 0 {
                        return view! { <div class="hist-empty">"No automatic holds were recorded in this window. Missing records do not confirm that watering ran."</div> }.into_any();
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
                        let b = day_buckets(&w.runs, d, Some(&z), &tz.get());
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
            </Show>
            <details class="hist-panel scoreboard hist-forecast-review">
                <summary class="hist-panel__title">"Rain forecast review"</summary>
                <p class="hist-panel__hint">"How completed-day rain forecasts compared with the gauge. This is forecast feedback, not a record of watering or water saved."</p>
                {move || {
                    if !scoreboard_loaded.get() {
                        return view! { <crate::components::ui::Skeleton variant="chart"/> }.into_any();
                    }
                    if scoreboard_error.get() {
                        return view! { <p role="alert">"Forecast review could not be loaded. Change the insights range or reload to try again."</p> }.into_any();
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
            </details>

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

#[cfg(all(test, feature = "ssr"))]
mod tz_bucket_tests {
    use super::*;

    #[test]
    fn recorded_holds_count_zones_and_later_automatic_watering_resolves_a_hold() {
        let row = RunRecord {
            zone: "front".into(),
            start_epoch: 1_788_624_000,
            source: "smart_morning".into(),
            status: "skipped".into(),
            skip_reason: Some("Rain expected within 4h".into()),
            ..Default::default()
        };
        let mut rows = vec![row.clone(), row.clone()];
        rows.push(RunRecord {
            zone: "back".into(),
            ..row.clone()
        });
        rows.push(RunRecord {
            source: "manual".into(),
            zone: "bed".into(),
            ..row
        });
        assert_eq!(skip_breakdown(&rows, "Pacific/Auckland")[0].1, 2);
        rows.push(RunRecord {
            zone: "front".into(),
            start_epoch: 1_788_624_060,
            duration_s: 60,
            source: "smart_morning".into(),
            status: "completed".into(),
            ..Default::default()
        });
        assert_eq!(skip_breakdown(&rows, "Pacific/Auckland")[0].1, 1);
    }

    /// A run at 04:00 local in Auckland lands in today's bucket on the chart
    /// and under today's key in the run log, whatever zone the test runs in.
    /// Bucketing from the render clock's midnight put it in yesterday on a
    /// UTC server and in tomorrow on a browser west of the date line.
    #[test]
    fn auckland_chart_and_run_log_agree_on_the_day() {
        let tz = "Pacific/Auckland";
        // 6 September 2026, 04:00 NZST (UTC+12) = 5 September 16:00 UTC.
        let run_start = 1_788_624_000;
        // Looked at the same day at 10:00 NZST.
        let now = run_start + 6 * 3600;
        let runs = vec![RunRecord {
            zone: "front".into(),
            start_epoch: run_start,
            duration_s: 600,
            skip_reason: None,
            source: "manual".into(),
            status: "completed".into(),
            ..Default::default()
        }];
        let b = day_buckets_at(&runs, 7, None, tz, now);
        assert!(
            b[6] > 9.9 && b[6] < 10.1,
            "today's bucket holds the run: {b:?}"
        );
        assert!(b[..6].iter().all(|m| *m == 0.0), "{b:?}");
        let log = run_log_days(&runs, tz);
        assert_eq!(log.len(), 1);
        assert_eq!(
            crate::timefmt::day_key_in_tz(log[0].0, tz),
            crate::timefmt::day_key_in_tz(now, tz),
            "the run log keys the run under today"
        );
        assert_eq!(crate::timefmt::day_key_in_tz(now, tz), "2026-09-06");
    }
    #[test]
    fn physical_intervals_split_at_real_midnight_without_duplicating_observations() {
        use chrono::TimeZone;
        for (tz, year, month, day, expected_hours) in [
            ("America/New_York", 2026, 3, 8, 23),
            ("America/New_York", 2026, 11, 1, 25),
            ("Asia/Kathmandu", 2026, 9, 12, 24),
            ("America/Santiago", 2026, 9, 6, 23),
        ] {
            let zone: chrono_tz::Tz = tz.parse().unwrap();
            let date = chrono::NaiveDate::from_ymd_opt(year, month, day).unwrap();
            // Noon is valid even on Santiago's missing-midnight transition.
            let noon = zone
                .from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
                .single()
                .unwrap()
                .timestamp();
            let next_noon = zone
                .from_local_datetime(&date.succ_opt().unwrap().and_hms_opt(12, 0, 0).unwrap())
                .single()
                .unwrap()
                .timestamp();
            let begin = crate::timefmt::split_local_days(noon - 36 * 3600, noon, tz)
                .last()
                .unwrap()
                .0;
            let end = crate::timefmt::split_local_days(noon, next_noon, tz)[0].1;
            assert_eq!(end - begin, expected_hours * 3600, "{tz}");
            let row = RunRecord {
                session_id: Some("overnight".into()),
                zone: "front".into(),
                start_epoch: begin - 600,
                duration_s: end - begin + 1200,
                source: "manual".into(),
                status: "completed".into(),
                ..Default::default()
            };
            let mut observed = row.clone();
            observed.source = "ha_refresher".into();
            let runs = [row, observed];
            assert_eq!(
                day_buckets_at(&runs, 3, None, tz, next_noon),
                vec![10.0, expected_hours as f64 * 60.0, 10.0],
                "{tz}"
            );
            assert_eq!(run_log_days(&runs, tz).len(), 1);
        }
    }

    #[test]
    fn session_log_does_not_split_a_job_at_midnight_or_count_cycles_as_jobs() {
        let start = 1_749_945_600 + 23 * 3600;
        let row = |session: &str, epoch| RunRecord {
            session_id: Some(session.into()),
            zone: "orchard".into(),
            start_epoch: epoch,
            duration_s: 600,
            source: "manual".into(),
            status: "completed".into(),
            ..Default::default()
        };
        let runs = vec![
            row("a", start),
            row("b", start + 1200),
            row("a", start + 7200),
        ];
        let days = run_log_days(&runs, "UTC");
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].1.len(), 3);
        assert_eq!(
            crate::history::rollup::watering_sessions_per_zone(&runs)["orchard"].len(),
            2
        );
        assert_eq!(
            crate::history::rollup::watering_intervals_per_zone(&runs)["orchard"].len(),
            3
        );
        let per_day = day_buckets_at(&runs, 2, None, "UTC", start + 10800);
        assert_eq!(per_day, vec![20.0, 10.0]);
    }
}
