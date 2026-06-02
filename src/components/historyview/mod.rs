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
use crate::history::types::{HistoryWindow, RunRecord};

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
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = window;

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
                let w = window.get();
                let runs: Vec<&RunRecord> = w.runs.iter().filter(|r| r.skip_reason.is_none()).collect();
                let total_min: f64 = runs.iter().map(|r| r.duration_s as f64 / 60.0).sum();
                let run_count = runs.len();
                let skip_count = w.runs.iter().filter(|r| r.skip_reason.is_some()).count();
                let overall = day_buckets(&w.runs, days.get(), None);
                view! {
                    <div class="hist-kpis">
                        <StatTile label="Water applied" value=format!("{:.0}", total_min) unit="min" icon="droplet" spark=overall.clone() accent="var(--accent)".to_string()/>
                        <StatTile label="Runs" value=run_count.to_string() icon="play" accent="var(--accent-good)".to_string()/>
                        <StatTile label="Skips" value=skip_count.to_string() icon="ban" accent="var(--accent-rain)".to_string()/>
                        <StatTile label="Avg / day" value=format!("{:.0}", overall.iter().sum::<f64>() / overall.len().max(1) as f64) unit="min" icon="gauge" accent="var(--accent-warm)".to_string()/>
                    </div>
                }
            }}

            // Daily watered-minutes line chart.
            <section class="hist-panel">
                <h2 class="hist-panel__title">"Watered minutes per day"</h2>
                {move || {
                    let w = window.get();
                    let b = day_buckets(&w.runs, days.get(), None);
                    let pts: Vec<(f64, f64)> = b.iter().enumerate().map(|(i, m)| (i as f64, *m)).collect();
                    let series = vec![Series::new("Watered (min)", "var(--accent)", pts)];
                    view! { <LineChart series height=200 y_unit=" min".to_string()/> }
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
