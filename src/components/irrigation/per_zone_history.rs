// Per-zone history. The HistoryPanel above renders the system-wide
// timeline + headline KPIs; this is the per-zone breakdown that answers
// "how is each zone individually being watered?"
//
// One card per zone, in a responsive grid. Each card unifies what used
// to be three separate stacked grids (summary tiles, daily-bar charts,
// recent-run lists) so everything about a single zone reads in one
// place instead of forcing the eye across three sections. A card shows,
// always: 7-day stats + a 14-day daily-minutes sparkline. Recent runs
// expand on demand so the grid stays compact for multi-zone deployments.
//
// Zone list comes from the live irrigation snapshot, so multi-zone
// deployments work without recompiling. All cards share one
// /api/irrigation/history?days=30 fetch and aggregate client-side; the
// `runs` table is already per-zone so no backend change is needed.

use crate::ha::snapshot::IrrigationSnapshot;
use crate::history::types::{HistoryWindow, RunRecord};
use chrono::{Local, TimeZone, Utc};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

/// Pull the (slug, display_name) list from the live snapshot. Falls back
/// to the legacy four zones when the snapshot hasn't loaded so SSR has
/// something to render.
fn zone_list(snap: ReadSignal<IrrigationSnapshot>) -> Vec<(String, String)> {
    let s = snap.get();
    if s.zones.is_empty() {
        vec![
            ("back_yard".to_string(), "Back Yard".to_string()),
            ("front_yard".to_string(), "Front Yard".to_string()),
            ("side_yard".to_string(), "Side Yard".to_string()),
            (
                "back_yard_shrubs".to_string(),
                "Back Yard Shrubs".to_string(),
            ),
        ]
    } else {
        s.zones
            .iter()
            .map(|z| (z.slug.clone(), z.name.clone()))
            .collect()
    }
}

#[component]
pub fn PerZoneHistory(
    snap: ReadSignal<IrrigationSnapshot>,
    /// Shared window data fetched by the page (mirrors the
    /// range selector at the top of `HistoryPanel`). The cards
    /// derive their 7-day stats and 14-day buckets from this so
    /// switching 30D / 90D / 1Y updates both views together.
    window: ReadSignal<HistoryWindow>,
) -> impl IntoView {
    view! {
        <section class="per-zone-history">
            <header class="per-zone-history-head">
                <h2 class="per-zone-history-title">"Per-zone history"</h2>
                <p class="per-zone-history-sub">
                    "Each zone's last 7 days of activity and 14-day daily-minutes "
                    "trend. Expand a card for its most recent runs and skips."
                </p>
            </header>
            <div class="zone-card-grid">
                {move || zone_list(snap).into_iter().map(|(slug, name)| {
                    view! { <PerZoneCard slug=slug name=name window/> }.into_any()
                }).collect::<Vec<_>>()}
            </div>
        </section>
    }
}

#[component]
fn PerZoneCard(slug: String, name: String, window: ReadSignal<HistoryWindow>) -> impl IntoView {
    let stats_slug = slug.clone();
    let bars_slug = slug.clone();
    let runs_slug = slug.clone();

    // 7-day headline stats: run count, total minutes, average per run,
    // and the timestamp of the most recent real run (scanned across the
    // whole window, not just 7 days, so it stays meaningful when a zone
    // has been quiet).
    let stats = Memo::new(move |_| {
        let w = window.get();
        let now = Utc::now().timestamp();
        let cutoff = now - 7 * 86400;
        let runs: Vec<&RunRecord> = w
            .runs
            .iter()
            .filter(|r| r.skip_reason.is_none() && r.zone == stats_slug && r.start_epoch >= cutoff)
            .collect();
        let count = runs.len();
        let total_sec: i64 = runs.iter().map(|r| r.duration_s).sum();
        let avg_sec = if count > 0 {
            total_sec / count as i64
        } else {
            0
        };
        let last_run_epoch = w
            .runs
            .iter()
            .filter(|r| r.skip_reason.is_none() && r.zone == stats_slug)
            .map(|r| r.start_epoch)
            .max()
            .unwrap_or(0);
        (count, total_sec, avg_sec, last_run_epoch)
    });

    let count_str = move || stats.get().0.to_string();
    let total_str = move || format!("{}", (stats.get().1 as f64 / 60.0).round() as i64);
    let avg_str = move || {
        let s = stats.get().2;
        if s == 0 {
            "\u{2014}".to_string()
        } else {
            format!("{}", (s as f64 / 60.0).round() as i64)
        }
    };
    let last_run_str = move || {
        let e = stats.get().3;
        if e == 0 {
            "no runs yet".to_string()
        } else {
            format!("last {}", format_relative_then_clock(e))
        }
    };

    // 14-day daily-minutes buckets, oldest -> newest left-to-right.
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
        for r in w
            .runs
            .iter()
            .filter(|r| r.skip_reason.is_none() && r.zone == bars_slug)
        {
            let days_back =
                crate::components::time_bucket::days_back(today_midnight, r.start_epoch).max(0);
            if (0..14).contains(&days_back) {
                buckets[days_back as usize] += r.duration_s;
            }
        }
        buckets.reverse();
        buckets
    });
    let max_minutes = Memo::new(move |_| {
        buckets
            .get()
            .iter()
            .map(|s| *s as f64 / 60.0)
            .fold(0.0_f64, f64::max)
            .max(1.0)
    });

    // Recent events (runs + skips), newest first, capped at 10.
    let runs = Memo::new(move |_| {
        let w = window.get();
        let mut filtered: Vec<RunRecord> = w
            .runs
            .iter()
            .filter(|r| r.zone == runs_slug)
            .cloned()
            .collect();
        filtered.sort_by_key(|r| std::cmp::Reverse(r.start_epoch));
        filtered.truncate(10);
        filtered
    });

    let expanded = RwSignal::new(false);
    let toggle = move |_| expanded.update(|v| *v = !*v);
    let card_class = move || {
        if expanded.get() {
            "zone-card is-expanded"
        } else {
            "zone-card"
        }
    };
    let chevron_class = move || {
        if expanded.get() {
            "zone-card__chev is-open"
        } else {
            "zone-card__chev"
        }
    };
    let runs_count = move || runs.get().len();

    view! {
        <article class=card_class>
            <header class="zone-card__head">
                <h3 class="zone-card__name">{name}</h3>
                <span class="zone-card__last">{last_run_str}</span>
            </header>

            <dl class="zone-card__stats">
                <div class="zone-stat">
                    <dd class="zone-stat__v">{count_str}</dd>
                    <dt class="zone-stat__k">"runs · 7d"</dt>
                </div>
                <div class="zone-stat">
                    <dd class="zone-stat__v">{total_str}<span class="zone-stat__u">"m"</span></dd>
                    <dt class="zone-stat__k">"total"</dt>
                </div>
                <div class="zone-stat">
                    <dd class="zone-stat__v">{avg_str}<span class="zone-stat__u">"m"</span></dd>
                    <dt class="zone-stat__k">"avg/run"</dt>
                </div>
            </dl>

            <div class="zone-card__spark">
                <div class="zone-card__spark-head">
                    <span class="zone-card__spark-label">"14-day minutes"</span>
                    <span class="zone-card__spark-scale">
                        {move || format!("peak {:.0}m", max_minutes.get())}
                    </span>
                </div>
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
            </div>

            <button
                type="button"
                class="zone-card__toggle"
                aria-expanded=move || if expanded.get() { "true" } else { "false" }
                on:click=toggle
            >
                <span>{move || format!("Recent runs ({})", runs_count())}</span>
                <span class=chevron_class aria-hidden="true">"\u{203A}"</span>
            </button>

            <Show when=move || expanded.get()>
                <ul class="per-zone-runs-list zone-card__runs">
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
            </Show>
        </article>
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Format an epoch as "Wed 5:42 AM" if within the last 7 days, else
/// "May 12, 5:42 AM". Compact for at-a-glance reads.
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
