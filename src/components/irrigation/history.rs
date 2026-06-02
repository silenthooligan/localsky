// Run-history overview. Pulls /api/irrigation/history?days=30 (or
// 90/365 via the segmented selector) and renders the system masthead:
// headline KPIs + a multi-line chart of daily watered minutes per zone.
//
// Each zone is a colored line. X axis = day, Y axis = minutes watered
// that day; skips become amber dots along the baseline. The SVG holds
// only the chart paths (preserveAspectRatio=none, stretches to fill);
// the legend, axes, and tick labels are HTML so type stays crisp at
// any width.
//
// SSR renders an empty skeleton; the page-level effect fetches the
// window into a shared signal that this component reads.

use crate::history::types::HistoryWindow;
use chrono::{Local, TimeZone, Utc};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::ui::HelpHint;

#[component]
pub fn HistoryPanel(
    days: ReadSignal<u32>,
    set_days: WriteSignal<u32>,
    window: ReadSignal<HistoryWindow>,
) -> impl IntoView {
    view! {
        <section class="history-panel">
            <header class="history-head">
                <h3 class="history-title">"History"</h3>
                <div class="history-head__right">
                    <HelpHint topic="history"/>
                    <RangeSelector current=days set_current=set_days/>
                </div>
            </header>
            <ComplianceRow window/>
            <ZoneLineChart window days/>
        </section>
    }
}

#[component]
fn RangeSelector(current: ReadSignal<u32>, set_current: WriteSignal<u32>) -> impl IntoView {
    let opts: [(u32, &'static str); 3] = [(30, "30D"), (90, "90D"), (365, "1Y")];
    view! {
        <div class="range-selector">
            {opts.into_iter().map(|(value, label)| view! {
                <button
                    class="btn-clay range-btn"
                    class:is-on=move || current.get() == value
                    on:click=move |_| set_current.set(value)
                >
                    {label}
                </button>
            }.into_any()).collect::<Vec<_>>()}
        </div>
    }
}

#[component]
fn ComplianceRow(window: ReadSignal<HistoryWindow>) -> impl IntoView {
    let total_runs = move || {
        window
            .get()
            .runs
            .iter()
            .filter(|r| r.skip_reason.is_none())
            .count()
    };
    let total_minutes = move || {
        window
            .get()
            .runs
            .iter()
            .filter(|r| r.skip_reason.is_none())
            .map(|r| r.duration_s as f64 / 60.0)
            .sum::<f64>()
    };
    let active_days = move || {
        let runs = window.get().runs;
        let mut days = std::collections::HashSet::new();
        for r in &runs {
            if r.skip_reason.is_none() {
                days.insert(r.start_epoch / 86400);
            }
        }
        days.len()
    };
    let total_skips = move || {
        window
            .get()
            .runs
            .iter()
            .filter(|r| r.skip_reason.is_some())
            .count()
    };

    view! {
        <div class="compliance-row">
            <ComplianceCell label="Total runs" value=Signal::derive(move || total_runs().to_string())/>
            <ComplianceCell label="Total minutes" value=Signal::derive(move || format!("{:.0}", total_minutes()))/>
            <ComplianceCell label="Active days" value=Signal::derive(move || active_days().to_string())/>
            <ComplianceCell label="Skips" value=Signal::derive(move || total_skips().to_string())/>
        </div>
    }
}

#[component]
fn ComplianceCell(label: &'static str, value: Signal<String>) -> impl IntoView {
    view! {
        <div class="compliance-cell">
            <span class="compliance-value">{move || value.get()}</span>
            <span class="compliance-label">{label}</span>
        </div>
    }
}

/// Title-case a zone slug ("back_yard" -> "Back Yard").
fn prettify(slug: &str) -> String {
    slug.split('_')
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Dark-bg-friendly palette assigned in order to the zone list. Cycles
/// if a deployment has more zones than colors.
const ZONE_COLORS: &[&str] = &[
    "#4dd2ff", // water cyan (brand accent-rain)
    "#6ee7c7", // mint
    "#b48cff", // soft purple
    "#ff80c0", // soft pink
    "#7ed957", // green
    "#ffd76b", // muted yellow
];

#[component]
fn ZoneLineChart(window: ReadSignal<HistoryWindow>, days: ReadSignal<u32>) -> impl IntoView {
    // Multi-line chart, one line per zone, x = day, y = watered minutes
    // that day. Skips appear as amber dots along the baseline so a dry
    // stretch is still visible. SVG holds paths only and uses
    // preserveAspectRatio=none + vector-effect=non-scaling-stroke so the
    // chart stretches horizontally without distorting line weight; the
    // legend, axes, today marker, and tick labels live in HTML to keep
    // text crisp at any container width.
    //
    // The legend chips are clickable buttons that toggle which zones
    // show. The Y axis rescales to the currently-visible zones so a
    // small-volume zone reads at full resolution once the loud zones
    // are hidden. State lives in one set of "hidden" slugs (declared
    // outside `body` so it survives signal-driven re-renders).
    let hidden: RwSignal<std::collections::HashSet<String>> =
        RwSignal::new(std::collections::HashSet::new());

    let body = move || {
        let win = window.get();
        let d = days.get() as i64;
        let to = win.to_epoch.max(1);
        let from = to - d * 86400;
        let n_days = d.max(1) as usize;
        let span_secs = (d * 86400).max(1) as f64;
        let hidden_set = hidden.get();

        // Zones present in the window, sorted for a stable color
        // assignment across reloads.
        let mut zone_slugs: Vec<String> = win
            .runs
            .iter()
            .map(|r| r.zone.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        zone_slugs.sort();
        let zones: Vec<(String, String)> = zone_slugs
            .iter()
            .map(|s| (s.clone(), prettify(s)))
            .collect();

        // Bucket runs into per-zone daily minute totals; collect skips
        // separately so they don't pull down the line value. Skip events
        // track their zone index so the toggle can hide them with the
        // matching line.
        let mut buckets: Vec<Vec<f64>> = zones.iter().map(|_| vec![0.0_f64; n_days]).collect();
        let mut skip_events: Vec<(usize, f64, String)> = Vec::new();
        let fmt_clock = |e: i64| {
            Utc.timestamp_opt(e, 0)
                .single()
                .map(|dt| {
                    dt.with_timezone(&Local)
                        .format("%b %-d, %-I:%M %p")
                        .to_string()
                })
                .unwrap_or_default()
        };
        let fmt_date = |e: i64| {
            Utc.timestamp_opt(e, 0)
                .single()
                .map(|dt| dt.with_timezone(&Local).format("%b %-d").to_string())
                .unwrap_or_default()
        };

        for r in &win.runs {
            let z_idx = match zones.iter().position(|(s, _)| s == &r.zone) {
                Some(i) => i,
                None => continue,
            };
            let day_idx = ((r.start_epoch - from) / 86400).clamp(0, n_days as i64 - 1) as usize;
            if let Some(reason) = &r.skip_reason {
                let pct = ((r.start_epoch - from) as f64 / span_secs).clamp(0.0, 1.0) * 100.0;
                let tip = format!(
                    "{} · {} · skipped: {}",
                    zones[z_idx].1,
                    fmt_clock(r.start_epoch),
                    reason
                );
                skip_events.push((z_idx, pct, tip));
            } else {
                buckets[z_idx][day_idx] += r.duration_s as f64 / 60.0;
            }
        }

        // Y-axis scale spans only the visible zones so hiding a tall
        // zone reveals the shorter ones at full resolution.
        let max_min: f64 = zones
            .iter()
            .enumerate()
            .filter(|(_, (s, _))| !hidden_set.contains(s))
            .flat_map(|(i, _)| buckets[i].iter().copied())
            .fold(0.0_f64, f64::max)
            .max(1.0);

        let total_w = 1000.0_f64;
        let total_h = 240.0_f64;

        // Horizontal gridlines at 0%, 50%, 100% of peak. Labels are in
        // HTML so they stay at native size.
        let gridlines: Vec<_> = [0.0_f64, 0.5_f64, 1.0_f64]
            .iter()
            .map(|p| {
                let y = total_h - p * total_h;
                view! {
                    <line
                        x1="0"
                        y1={format!("{:.2}", y)}
                        x2={total_w.to_string()}
                        y2={format!("{:.2}", y)}
                        class="line-chart__grid"
                        vector-effect="non-scaling-stroke"
                    />
                }
                .into_any()
            })
            .collect();

        // One smooth polyline per visible zone.
        let zone_paths: Vec<_> = zones
            .iter()
            .enumerate()
            .filter(|(_, (s, _))| !hidden_set.contains(s))
            .map(|(z_idx, _)| {
                let color = ZONE_COLORS[z_idx % ZONE_COLORS.len()];
                let mut d_attr = String::new();
                for (i, mins) in buckets[z_idx].iter().enumerate() {
                    let x = if n_days <= 1 {
                        0.0
                    } else {
                        (i as f64) / ((n_days - 1) as f64) * total_w
                    };
                    let y = total_h - (mins / max_min) * total_h;
                    if i == 0 {
                        d_attr.push_str(&format!("M {:.2},{:.2}", x, y));
                    } else {
                        d_attr.push_str(&format!(" L {:.2},{:.2}", x, y));
                    }
                }
                view! {
                    <path
                        d=d_attr
                        class="line-chart__line"
                        stroke=color
                        fill="none"
                        stroke-width="2"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                        vector-effect="non-scaling-stroke"
                    />
                }
                .into_any()
            })
            .collect();

        // X axis date ticks: weekly for ~month, fortnightly to a quarter,
        // monthly for a year. End tick is always labeled and right-aligned.
        let tick_days: i64 = if d <= 31 {
            7
        } else if d <= 120 {
            14
        } else {
            30
        };
        let mut xticks: Vec<_> = Vec::new();
        let mut t = from;
        while t <= to {
            let pct = (t - from) as f64 / span_secs * 100.0;
            if pct <= 92.0 {
                let transform = if pct < 1.0 {
                    "none"
                } else {
                    "translateX(-50%)"
                };
                xticks.push(
                    view! {
                        <span class="line-chart__xtick" style={format!("left:{:.2}%; transform:{}", pct, transform)}>
                            {fmt_date(t)}
                        </span>
                    }
                    .into_any(),
                );
            }
            t += tick_days * 86400;
        }
        xticks.push(
            view! {
                <span class="line-chart__xtick" style="left:100%; transform:translateX(-100%)">
                    {fmt_date(to)}
                </span>
            }
            .into_any(),
        );

        // Y axis labels: peak, mid, 0 (top -> bottom).
        let max_label = max_min.round() as i64;
        let mid_label = (max_min / 2.0).round() as i64;
        let yticks = view! {
            <span class="line-chart__ytick" style="top:0">{format!("{}m", max_label)}</span>
            <span class="line-chart__ytick" style="top:50%">{format!("{}m", mid_label)}</span>
            <span class="line-chart__ytick" style="top:100%">"0"</span>
        };

        // Legend: a colored dot + zone name per zone, rendered as a
        // button so it toggles. Click adds/removes the slug from the
        // `hidden` set; the chart re-renders with that zone gone (or
        // back). Off-state styling lives on `.is-off`.
        let legend: Vec<_> = zones
            .iter()
            .enumerate()
            .map(|(z_idx, (slug, name))| {
                let color = ZONE_COLORS[z_idx % ZONE_COLORS.len()];
                let slug_class = slug.clone();
                let slug_click = slug.clone();
                let slug_aria = slug.clone();
                view! {
                    <button
                        type="button"
                        class="line-chart__legend-item"
                        class:is-off=move || hidden.get().contains(&slug_class)
                        aria-pressed=move || if hidden.get().contains(&slug_aria) { "false" } else { "true" }
                        on:click=move |_| {
                            hidden.update(|h| {
                                if h.contains(&slug_click) {
                                    h.remove(&slug_click);
                                } else {
                                    h.insert(slug_click.clone());
                                }
                            });
                        }
                    >
                        <span class="line-chart__legend-dot" style={format!("background:{}", color)}></span>
                        <span>{name.clone()}</span>
                    </button>
                }
                .into_any()
            })
            .collect();

        // Skip dots: amber circles along the baseline, hover for the
        // reason. Hidden zones drop their skips too so the toggle keeps
        // each zone's whole record together.
        let skip_view: Vec<_> = skip_events
            .into_iter()
            .filter(|(z_idx, _, _)| !hidden_set.contains(&zones[*z_idx].0))
            .map(|(_, pct, tip)| {
                view! {
                    <span
                        class="line-chart__skip"
                        style={format!("left:{:.3}%", pct)}
                        title=tip
                    ></span>
                }
                .into_any()
            })
            .collect();

        view! {
            <div class="line-chart">
                <div class="line-chart__head">
                    <div class="line-chart__legend">{legend}</div>
                    <span class="line-chart__scale">{format!("peak {:.0}m/day", max_min)}</span>
                </div>
                <div class="line-chart__body">
                    <div class="line-chart__yaxis">{yticks}</div>
                    <div class="line-chart__plot">
                        <svg
                            class="line-chart__svg"
                            viewBox={format!("0 0 {} {}", total_w, total_h)}
                            preserveAspectRatio="none"
                        >
                            {gridlines}
                            {zone_paths}
                        </svg>
                        <div class="line-chart__skips">{skip_view}</div>
                        <span class="line-chart__today"></span>
                    </div>
                </div>
                <div class="line-chart__xaxis">
                    <span class="line-chart__xaxis-spacer"></span>
                    <div class="line-chart__xaxis-track">{xticks}</div>
                </div>
            </div>
        }
    };

    view! {
        <div class="gantt-wrap">
            {body}
        </div>
    }
}
