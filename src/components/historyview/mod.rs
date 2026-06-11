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

use crate::components::ui::{Button, LineChart, Series, Sparkline, StatTile};
#[cfg(feature = "hydrate")]
use crate::history::types::DecisionWindow;
use crate::history::types::{DecisionRecord, HistoryWindow, RunRecord};

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

/// Categorize skip *days* into headline buckets for the "why" breakdown.
/// Takes the decision feed (the engine re-evaluates many times a day), keeps
/// the latest verdict per calendar day, and counts the days that ended in a
/// skip by reason. Returns (label, count, css-color), largest bucket first.
fn skip_breakdown(decisions: &[DecisionRecord]) -> Vec<(&'static str, usize, &'static str)> {
    use std::collections::HashMap;
    let mut latest: HashMap<String, &DecisionRecord> = HashMap::new();
    for d in decisions {
        let key = Local
            .timestamp_opt(d.epoch, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
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
        let l = d.reason.to_lowercase();
        if l.contains("rain") {
            rain += 1;
        } else if l.contains("wind") {
            wind += 1;
        } else if l.contains("restrict") || l.contains("allowed day") || l.contains("forbidden") {
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
/// happened" precisely instead of only in totals.
fn run_log_days(runs: &[RunRecord]) -> Vec<(String, Vec<RunRecord>)> {
    use std::collections::BTreeMap;
    let mut by_day: BTreeMap<String, Vec<RunRecord>> = BTreeMap::new();
    for r in runs {
        let key = Local
            .timestamp_opt(r.start_epoch, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        by_day.entry(key).or_default().push(r.clone());
    }
    let mut days: Vec<(String, Vec<RunRecord>)> = by_day.into_iter().collect();
    days.reverse();
    for (_, rs) in days.iter_mut() {
        rs.sort_by_key(|r| r.start_epoch);
    }
    days
}

fn fmt_day_header(key: &str) -> String {
    chrono::NaiveDate::parse_from_str(key, "%Y-%m-%d")
        .map(|d| d.format("%A, %b %-d").to_string())
        .unwrap_or_else(|_| key.to_string())
}

fn fmt_clock(epoch: i64) -> String {
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|dt| dt.format("%-I:%M %p").to_string())
        .unwrap_or_default()
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

#[component]
pub fn HistoryPage() -> impl IntoView {
    let days = RwSignal::new(30i64);
    // Run-log display range (independent of the page window): 0 = all.
    let runlog_days = RwSignal::new(7i64);
    // Month jump overrides the range chips while set.
    let runlog_month: RwSignal<Option<(i32, u32)>> = RwSignal::new(None);
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

    #[cfg(feature = "hydrate")]
    {
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
    let _ = (window, decisions, runlog_window);

    view! {
        <div class="hist-page">
            <header class="hist-page__header">
                <div>
                    <p class="hist-page__eyebrow">"Analyze"</p>
                    <h1 class="hist-page__title">"History"</h1>
                </div>
                <div class="hist-page__tools">
                    <RangeBtn label="30d" d=30 days/>
                    <RangeBtn label="90d" d=90 days/>
                    <RangeBtn label="1yr" d=365 days/>
                    <Button variant="ghost" icon="print" on_click=Callback::new(move |_| print_page())>"Print"</Button>
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
                let skip_count: usize = skip_breakdown(&decisions.get())
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
                    // Index i is "i days ago" (day_buckets orientation).
                    let today = Local::now().date_naive();
                    let n = b.len();
                    let labels: Vec<String> = (0..n)
                        .map(|i| {
                            // Buckets run oldest -> newest; label to match.
                            today
                                .checked_sub_days(chrono::Days::new((n - 1 - i) as u64))
                                .map(|d| d.format("%b %-d").to_string())
                                .unwrap_or_default()
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
                                on:click=move |_| { runlog_month.set(None); runlog_days.set(d); }
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
                            match v.split_once('-').and_then(|(a, b)| Some((a.parse::<i32>().ok()?, b.parse::<u32>().ok()?))) {
                                Some(ym) => runlog_month.set(Some(ym)),
                                None => runlog_month.set(None),
                            }
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
                    let days = run_log_days(&runs);
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
                    days.into_iter().map(|(day, rows)| {
                        let watered_s: i64 = rows.iter().filter(|r| r.skip_reason.is_none()).map(|r| r.duration_s).sum();
                        let header = fmt_day_header(&day);
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
                                            <span class="runlog-row__time">{fmt_clock(r.start_epoch)}</span>
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
                <p class="hist-panel__hint">"Each square is a day; greener = more watering, empty = a skip day."</p>
                {move || {
                    let w = window.get();
                    let b = day_buckets(&w.runs, days.get(), None);
                    let max = b.iter().cloned().fold(0.0f64, f64::max).max(1.0);
                    view! {
                        <div class="hist-cal">
                            {b.into_iter().map(|m| {
                                let bg = if m <= 0.0 {
                                    "var(--elev-1)".to_string()
                                } else {
                                    let pct = (18.0 + (m / max).min(1.0) * 67.0) as i32;
                                    format!("color-mix(in oklab, var(--accent) {pct}%, transparent)")
                                };
                                view! { <span class="hist-cal__cell" style=format!("background:{bg}") title=format!("{m:.0} min")></span> }
                            }).collect_view()}
                        </div>
                    }
                }}
            </section>

            // Why it skipped, the headline "story" of the period.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"Why it skipped"</h2>
                <p class="hist-panel__hint">"Days the engine chose to skip, by reason."</p>
                {move || {
                    let bd = skip_breakdown(&decisions.get());
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
fn RangeBtn(label: &'static str, d: i64, days: RwSignal<i64>) -> impl IntoView {
    let cls = move || {
        if days.get() == d {
            "hist-range is-on"
        } else {
            "hist-range"
        }
    };
    view! {
        <button type="button" class=cls on:click=move |_| days.set(d)>{label}</button>
    }
}
