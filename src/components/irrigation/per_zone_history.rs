// Per-zone history visualization. The existing HistoryPanel renders a
// system-wide Gantt + aggregate compliance row; this is the per-zone
// view that answers "how is each zone individually being watered?"
//
// Three components stacked vertically:
//   1. PerZoneSummary    — 4 tiles in a row (count, total min, last run, avg duration; trailing 7 days)
//   2. PerZoneRunsList   — collapsible per-zone list of last 10 events
//   3. PerZoneDailyBars  — 2x2 grid of mini bar charts (last 14 days, daily total minutes per zone)
//
// All three share a single /api/irrigation/history?days=30 fetch and
// aggregate client-side. The schema is already per-zone (zone column
// in the `runs` SQLite table) so no backend changes are needed.

use crate::history::types::{HistoryWindow, RunRecord};
use chrono::{Local, TimeZone, Utc};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

const ZONE_SLUGS: [(&str, &str); 4] = [
    ("back_yard", "Back Yard"),
    ("front_yard", "Front Yard"),
    ("side_yard", "Side Yard"),
    ("back_yard_shrubs", "Back Yard Shrubs"),
];

#[component]
pub fn PerZoneHistory() -> impl IntoView {
    // Shared fetch: 30 days of runs, filtered client-side per zone.
    let (window, set_window) = signal::<HistoryWindow>(HistoryWindow::default());
    #[cfg(not(feature = "hydrate"))]
    let _ = set_window;

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            leptos::task::spawn_local(async move {
                let url = "/api/irrigation/history?days=30";
                if let Ok(resp) = gloo_net::http::Request::get(url).send().await {
                    if let Ok(w) = resp.json::<HistoryWindow>().await {
                        set_window.set(w);
                    }
                }
            });
        });
    }

    view! {
        <section class="per-zone-history">
            <h2 class="per-zone-history-title">"Per-zone history"</h2>
            <p class="per-zone-history-sub">
                "Last 7 days at a glance, the most recent runs per zone, and "
                "daily watered-minutes over the last 14 days."
            </p>
            <PerZoneSummary window/>
            <PerZoneDailyBars window/>
            <PerZoneRunsList window/>
        </section>
    }
}

// ── Summary tiles (last 7 days) ─────────────────────────────────────

#[component]
fn PerZoneSummary(window: ReadSignal<HistoryWindow>) -> impl IntoView {
    view! {
        <div class="per-zone-summary-grid">
            {ZONE_SLUGS.iter().map(|(slug, name)| {
                let s = *slug;
                let n = *name;
                view! { <PerZoneSummaryTile slug=s name=n window/> }.into_any()
            }).collect::<Vec<_>>()}
        </div>
    }
}

#[component]
fn PerZoneSummaryTile(
    slug: &'static str,
    name: &'static str,
    window: ReadSignal<HistoryWindow>,
) -> impl IntoView {
    // All stats derived over the last 7 days (604800 epoch seconds).
    let stats = Memo::new(move |_| {
        let w = window.get();
        let now = Utc::now().timestamp();
        let cutoff = now - 7 * 86400;
        let runs: Vec<&RunRecord> = w
            .runs
            .iter()
            .filter(|r| r.skip_reason.is_none() && r.zone == slug && r.start_epoch >= cutoff)
            .collect();
        let count = runs.len();
        let total_sec: i64 = runs.iter().map(|r| r.duration_s).sum();
        let avg_sec = if count > 0 { total_sec / count as i64 } else { 0 };
        // last_run: scan all entries (not just last 7 days) so the tile
        // shows the last real run even if it was longer ago.
        let last_run_epoch = w
            .runs
            .iter()
            .filter(|r| r.skip_reason.is_none() && r.zone == slug)
            .map(|r| r.start_epoch)
            .max()
            .unwrap_or(0);
        (count, total_sec, avg_sec, last_run_epoch)
    });

    let count_str = move || stats.get().0.to_string();
    let total_str = move || format!("{} min", (stats.get().1 as f64 / 60.0).round() as i64);
    let avg_str = move || {
        let s = stats.get().2;
        if s == 0 {
            "\u{2014}".to_string()
        } else {
            format!("{} min", (s as f64 / 60.0).round() as i64)
        }
    };
    let last_run_str = move || {
        let e = stats.get().3;
        if e == 0 {
            "no runs yet".to_string()
        } else {
            format_relative_then_clock(e)
        }
    };

    view! {
        <article class="per-zone-summary-tile">
            <header class="per-zone-summary-head">
                <h3 class="per-zone-summary-name">{name}</h3>
            </header>
            <dl class="per-zone-summary-stats">
                <div class="kv"><dt class="k">"runs (7d)"</dt><dd class="v">{count_str}</dd></div>
                <div class="kv"><dt class="k">"total"</dt><dd class="v">{total_str}</dd></div>
                <div class="kv"><dt class="k">"avg/run"</dt><dd class="v">{avg_str}</dd></div>
                <div class="kv"><dt class="k">"last run"</dt><dd class="v">{last_run_str}</dd></div>
            </dl>
        </article>
    }
}

// ── Daily-minutes bar chart (last 14 days) ──────────────────────────

#[component]
fn PerZoneDailyBars(window: ReadSignal<HistoryWindow>) -> impl IntoView {
    view! {
        <div class="per-zone-daily-bars-grid">
            {ZONE_SLUGS.iter().map(|(slug, name)| {
                let s = *slug;
                let n = *name;
                view! { <PerZoneDailyBarsTile slug=s name=n window/> }.into_any()
            }).collect::<Vec<_>>()}
        </div>
    }
}

#[component]
fn PerZoneDailyBarsTile(
    slug: &'static str,
    name: &'static str,
    window: ReadSignal<HistoryWindow>,
) -> impl IntoView {
    // Bucketize this zone's runs into the last 14 local days. The
    // bucket index 0 = today, 13 = 13 days ago. We render the bars
    // left-to-right as oldest-to-newest so the visual reads "today is
    // on the right" like every other timeseries on the dashboard.
    let buckets = Memo::new(move |_| {
        let w = window.get();
        let now = Local::now();
        let today_midnight = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|nd| Local.from_local_datetime(&nd).single())
            .unwrap_or(now)
            .timestamp();
        let mut buckets = vec![0i64; 14];
        for r in w.runs.iter().filter(|r| r.skip_reason.is_none() && r.zone == slug) {
            let days_back = ((today_midnight - r.start_epoch) / 86400).max(0);
            if (0..14).contains(&days_back) {
                buckets[days_back as usize] += r.duration_s;
            }
        }
        // Render oldest -> newest left-to-right.
        buckets.reverse();
        buckets
    });

    let max_minutes = Memo::new(move |_| {
        buckets
            .get()
            .iter()
            .map(|s| *s as f64 / 60.0)
            .fold(0.0_f64, f64::max)
            .max(1.0) // avoid div-by-zero; 1 min minimum scale
    });

    view! {
        <article class="per-zone-daily-bars-tile">
            <header class="per-zone-daily-bars-head">
                <h3 class="per-zone-daily-bars-name">{name}</h3>
                <span class="per-zone-daily-bars-scale">
                    {move || format!("scale: {:.0} min", max_minutes.get())}
                </span>
            </header>
            <svg
                class="per-zone-daily-bars-svg"
                viewBox="0 0 280 80"
                preserveAspectRatio="none"
                role="img"
                aria-label="Daily watered minutes, last 14 days"
            >
                {move || {
                    let bs = buckets.get();
                    let mx = max_minutes.get();
                    let n = bs.len() as f64;
                    let w = 280.0 / n;
                    let bar_w = (w - 2.0).max(2.0);
                    bs.iter().enumerate().map(|(i, sec)| {
                        let mins = *sec as f64 / 60.0;
                        let h = (mins / mx * 76.0).max(if mins > 0.0 { 1.0 } else { 0.0 });
                        let x = i as f64 * w + (w - bar_w) / 2.0;
                        let y = 80.0 - h;
                        let cls = if i == 13 { "per-zone-daily-bar per-zone-daily-bar-today" } else { "per-zone-daily-bar" };
                        view! {
                            <rect
                                class=cls
                                x=format!("{:.1}", x)
                                y=format!("{:.1}", y)
                                width=format!("{:.1}", bar_w)
                                height=format!("{:.1}", h)
                            >
                                <title>{format!("{} min", mins.round() as i64)}</title>
                            </rect>
                        }.into_any()
                    }).collect::<Vec<_>>()
                }}
            </svg>
        </article>
    }
}

// ── Recent runs list (last 10 per zone) ─────────────────────────────

#[component]
fn PerZoneRunsList(window: ReadSignal<HistoryWindow>) -> impl IntoView {
    view! {
        <div class="per-zone-runs-grid">
            {ZONE_SLUGS.iter().map(|(slug, name)| {
                let s = *slug;
                let n = *name;
                view! { <PerZoneRunsTile slug=s name=n window/> }.into_any()
            }).collect::<Vec<_>>()}
        </div>
    }
}

#[component]
fn PerZoneRunsTile(
    slug: &'static str,
    name: &'static str,
    window: ReadSignal<HistoryWindow>,
) -> impl IntoView {
    let runs = Memo::new(move |_| {
        let w = window.get();
        let mut filtered: Vec<RunRecord> = w
            .runs
            .iter()
            .filter(|r| r.zone == slug)
            .cloned()
            .collect();
        // Newest first.
        filtered.sort_by(|a, b| b.start_epoch.cmp(&a.start_epoch));
        filtered.truncate(10);
        filtered
    });

    view! {
        <article class="per-zone-runs-tile">
            <header class="per-zone-runs-head">
                <h3 class="per-zone-runs-name">{name}</h3>
                <span class="per-zone-runs-count">
                    {move || format!("{} recent", runs.get().len())}
                </span>
            </header>
            <ul class="per-zone-runs-list">
                {move || {
                    let rs = runs.get();
                    if rs.is_empty() {
                        return vec![view! { <li class="per-zone-runs-empty">"no history yet"</li> }.into_any()];
                    }
                    rs.into_iter().map(|r| {
                        let when = format_relative_then_clock(r.start_epoch);
                        let (kind_class, kind_label, detail) = if let Some(reason) = &r.skip_reason {
                            ("per-zone-run-skipped", "skip", reason.clone())
                        } else {
                            ("per-zone-run-ok", "ran", format!("{} min", (r.duration_s as f64 / 60.0).round() as i64))
                        };
                        view! {
                            <li class=format!("per-zone-run {}", kind_class)>
                                <span class="per-zone-run-when">{when}</span>
                                <span class="per-zone-run-kind">{kind_label}</span>
                                <span class="per-zone-run-detail">{detail}</span>
                            </li>
                        }.into_any()
                    }).collect::<Vec<_>>()
                }}
            </ul>
        </article>
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Format an epoch as "Wed 5:42 AM" if within the last 7 days, else
/// "May 12, 5:42 AM". Compact for at-a-glance tile reads.
fn format_relative_then_clock(epoch: i64) -> String {
    if epoch == 0 {
        return "\u{2014}".to_string();
    }
    let when = Utc.timestamp_opt(epoch, 0).single();
    let now = Utc::now();
    match when {
        Some(dt) => {
            let local = dt.with_timezone(&Local);
            let age = now.signed_duration_since(dt);
            if age.num_days() < 7 {
                local.format("%a %-I:%M %p").to_string()
            } else {
                local.format("%b %-d, %-I:%M %p").to_string()
            }
        }
        None => format!("epoch {epoch}"),
    }
}

