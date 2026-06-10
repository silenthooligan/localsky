// History — "history that sings" (marquee feature 4, first cut). Reads the
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
use crate::history::types::{DecisionRecord, DecisionWindow, HistoryWindow, RunRecord};

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
        let back = ((today_mid - r.start_epoch) / 86_400).max(0);
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
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
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
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = (window, decisions);

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
                // Skip *days* (from the decision feed), not run records — runs
                // are only actual waterings, so that count is always ~0.
                let skip_count: usize = skip_breakdown(&decisions.get())
                    .iter()
                    .map(|(_, c, _)| c)
                    .sum();
                let overall = day_buckets(&w.runs, days.get(), None);
                view! {
                    <div class="hist-kpis">
                        <StatTile label="Water applied" value=format!("{:.0}", total_min) unit="min" icon="droplet" spark=overall.clone() accent="var(--accent)".to_string()/>
                        <StatTile label="Runs" value=run_count.to_string() icon="play" accent="var(--accent-good)".to_string()/>
                        <StatTile label="Skips" value=skip_count.to_string() icon="ban" accent="var(--accent-rain)".to_string()/>
                        <StatTile label="Avg / day" value=format!("{:.0}", overall.iter().sum::<f64>() / overall.len().max(1) as f64) unit="min" icon="gauge" accent="var(--accent-warm)".to_string()/>
                    </div>
                }
                .into_any()
            }}

            // Daily watered-minutes line chart.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"Watered minutes per day"</h2>
                {move || {
                    let w = window.get();
                    let b = day_buckets(&w.runs, days.get(), None);
                    let pts: Vec<(f64, f64)> = b.iter().enumerate().map(|(i, m)| (i as f64, *m)).collect();
                    // Index i is "i days ago" (day_buckets orientation).
                    let today = Local::now().date_naive();
                    let labels: Vec<String> = (0..b.len())
                        .map(|i| {
                            today
                                .checked_sub_days(chrono::Days::new(i as u64))
                                .map(|d| d.format("%b %-d").to_string())
                                .unwrap_or_default()
                        })
                        .collect();
                    let series = vec![Series::new("Watered (min)", "var(--accent)", pts)];
                    view! { <LineChart series height=200 y_unit=" min".to_string() x_labels=labels/> }
                }}
            </section>

            // Watering calendar heatmap — the at-a-glance "which days watered".
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

            // Why it skipped — the headline "story" of the period.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"Why it skipped"</h2>
                <p class="hist-panel__hint">"Days the engine chose to skip, by reason."</p>
                {move || {
                    let bd = skip_breakdown(&decisions.get());
                    let total: usize = bd.iter().map(|(_, c, _)| *c).sum();
                    if total == 0 {
                        return view! { <div class="hist-empty">"No skips in this window \u{2014} everything ran as planned."</div> }.into_any();
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
