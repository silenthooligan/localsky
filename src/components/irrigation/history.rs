// Run-history view. Pulls /api/irrigation/history?days=30 (or 90/365
// via the segmented selector), renders a per-zone SVG Gantt strip
// and a compliance counter row across the bottom.
//
// SSR renders an empty skeleton; the actual fetch happens in the
// hydrate effect after the page mounts. This keeps the SSR pass fast
// (no DB hit on every page view) and makes the data load feel snappy
// because the rest of the page is already painted.

use crate::history::types::HistoryWindow;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn HistoryPanel() -> impl IntoView {
    let (days, set_days) = signal(30u32);
    let (window, set_window) = signal::<HistoryWindow>(HistoryWindow::default());
    #[cfg(not(feature = "hydrate"))]
    let _ = set_window;

    // Refetch whenever the window selector changes. Effect runs after
    // hydration; SSR shows the empty skeleton until the first fetch.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let d = days.get();
            leptos::task::spawn_local(async move {
                let url = format!("/api/irrigation/history?days={d}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if let Ok(w) = resp.json::<HistoryWindow>().await {
                        set_window.set(w);
                    }
                }
            });
        });
    }
    view! {
        <section class="history-panel">
            <header class="history-head">
                <h3 class="history-title">"History"</h3>
                {view! { <RangeSelector current=days set_current=set_days/> }.into_any()}
            </header>
            {view! { <ComplianceRow window/> }.into_any()}
            {view! { <Gantt window days/> }.into_any()}
        </section>
    }
}

#[component]
fn RangeSelector(
    current: ReadSignal<u32>,
    set_current: WriteSignal<u32>,
) -> impl IntoView {
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
    // Three big mono numbers: total run count, total minutes, distinct
    // run-days. Computed off the window data (no extra round-trip).
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

    view! {
        <div class="compliance-row">
            <ComplianceCell label="Total runs" value=Signal::derive(move || total_runs().to_string())/>
            <ComplianceCell label="Total minutes" value=Signal::derive(move || format!("{:.0}", total_minutes()))/>
            <ComplianceCell label="Active days" value=Signal::derive(move || active_days().to_string())/>
        </div>
    }
}

#[component]
fn ComplianceCell(label: &'static str, value: Signal<String>) -> impl IntoView {
    view! {
        <div class="compliance-cell">
            <span class="compliance-label">{label}</span>
            <span class="compliance-value">{move || value.get()}</span>
        </div>
    }
}

#[component]
fn Gantt(
    window: ReadSignal<HistoryWindow>,
    days: ReadSignal<u32>,
) -> impl IntoView {
    // 4 zone rows × N day columns. SVG sized by viewBox so it scales
    // with the panel; height is fixed in CSS for legibility.
    let svg = move || {
        let win = window.get();
        let d = days.get() as i64;
        let to = win.to_epoch.max(1);
        let from = to - d * 86400;
        // 4 zone rows, ordered to match the rest of the dashboard.
        let rows: [(&str, &str); 4] = [
            ("back_yard", "Back Yard"),
            ("front_yard", "Front Yard"),
            ("side_yard", "Side Yard"),
            ("back_yard_shrubs", "Shrubs"),
        ];
        let span = (to - from).max(1) as f64;
        let row_h = 28.0_f64;
        let label_w = 100.0_f64;
        let total_w = 1000.0_f64; // viewBox width; CSS scales to container
        let track_w = total_w - label_w - 10.0;

        let mut bars: Vec<_> = Vec::new();
        for (i, (slug, _name)) in rows.iter().enumerate() {
            let y = (i as f64) * row_h + 6.0;
            for r in &win.runs {
                if r.zone != *slug || r.skip_reason.is_some() {
                    continue;
                }
                let rel = (r.start_epoch - from) as f64 / span;
                if !(0.0..=1.0).contains(&rel) {
                    continue;
                }
                let dur = r.duration_s.max(60) as f64;
                let bar_w = (dur / span * track_w).max(2.0);
                let x = label_w + rel * track_w;
                bars.push((x, y, bar_w));
            }
        }

        let labels: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, (_, name))| {
                let y = (i as f64) * row_h + 21.0;
                view! {
                    <text x="6" y={y.to_string()} class="gantt-label">
                        {*name}
                    </text>
                }
                .into_any()
            })
            .collect::<Vec<_>>();

        let row_bg: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let y = (i as f64) * row_h + 4.0;
                view! {
                    <rect
                        x={label_w.to_string()}
                        y={y.to_string()}
                        width={track_w.to_string()}
                        height={(row_h - 4.0).to_string()}
                        class="gantt-row-bg"
                    />
                }
                .into_any()
            })
            .collect::<Vec<_>>();

        let bars_view: Vec<_> = bars
            .into_iter()
            .map(|(x, y, w)| {
                view! {
                    <rect
                        x={x.to_string()}
                        y={y.to_string()}
                        width={w.to_string()}
                        height="16"
                        rx="3"
                        class="gantt-bar"
                    />
                }
                .into_any()
            })
            .collect::<Vec<_>>();

        let total_h = (rows.len() as f64) * row_h + 4.0;

        view! {
            <svg
                class="gantt"
                viewBox={format!("0 0 {} {}", total_w, total_h)}
                preserveAspectRatio="none"
            >
                {row_bg}
                {labels}
                {bars_view}
            </svg>
        }
    };

    view! {
        <div class="gantt-wrap">
            {svg}
        </div>
    }
}
