// Mobile zone-detail page at /irrigation/zone/:slug. Reads :slug from the
// route and finds the matching ZoneState in the live IrrigationSnapshot.
//
// Layout:
//   - Back chevron + zone name
//   - Big status block (state, planned, today, bucket, last-run)
//   - Primary "Run for X minutes" button (opens DurationSheet)
//   - "Stop" button when running
//   - 14-day vertical history strip (one row per day)
//
// History fetch reuses /api/irrigation/history with an extra ?zone= filter
// added in Phase 4. For now we filter client-side from the unfiltered
// response; harmless for our 4-zone fleet.

use crate::components::irrigation::controls::post_action;
use crate::components::irrigation::mobile::duration_sheet::DurationSheet;
use crate::ha::snapshot::{IrrigationSnapshot, ZoneMath, ZoneState};
use crate::history::types::{HistoryWindow, RunRecord};
use crate::nav_log::log_nav;
use chrono::{DateTime, Local, TimeZone};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
use leptos_router::hooks::{use_navigate, use_params_map};
use leptos_router::NavigateOptions;
use serde_json::json;

#[component]
pub fn MobileZoneDetail(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let params = use_params_map();
    let slug = move || {
        params
            .get()
            .get("slug")
            .map(|s| s.to_string())
            .unwrap_or_default()
    };

    // Find the matching zone in the snapshot. If the snapshot hasn't loaded
    // or the slug doesn't match anything (typo, stale URL), show a placeholder.
    let zone = move || -> Option<ZoneState> {
        let s = slug();
        snap.get().zones.iter().find(|z| z.slug == s).cloned()
    };

    // Duration-sheet state, lifted to this page so the sheet is mounted once
    // and the Run button just toggles visibility.
    let sheet_visible: RwSignal<bool> = RwSignal::new(false);
    let sheet_zone: RwSignal<Option<String>> = RwSignal::new(None);
    let sheet_label: RwSignal<String> = RwSignal::new(String::new());

    // History fetch — reuses the /api/irrigation/history endpoint and
    // filters client-side. 30-day window; SSR shows empty skeleton.
    let (history, set_history) = signal::<HistoryWindow>(HistoryWindow::default());
    #[cfg(not(feature = "hydrate"))]
    let _ = set_history;
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            // Re-run if slug changes (so back/forward between zones triggers
            // a refetch). We don't actually use the slug in the URL — the
            // backend returns all zones — but reading it here registers the
            // dependency.
            let _ = slug();
            leptos::task::spawn_local(async move {
                if let Ok(resp) = gloo_net::http::Request::get("/api/irrigation/history?days=30")
                    .send()
                    .await
                {
                    if let Ok(w) = resp.json::<HistoryWindow>().await {
                        set_history.set(w);
                    }
                }
            });
        });
    }

    let navigate = use_navigate();
    let on_back = move |ev: leptos::ev::MouseEvent| {
        ev.prevent_default();
        log_nav("zone-detail back");
        navigate("/zones", NavigateOptions::default());
    };

    move || {
        let z = match zone() {
            Some(z) => z,
            None => {
                return view! {
                    <div class="mobile-stack">
                        <a href="/zones" class="mobile-back-link" on:click=on_back.clone()>"‹ Zones"</a>
                        <div class="mobile-zone-detail-empty">"Loading zone..."</div>
                    </div>
                }.into_any();
            }
        };

        let zname = z.name.clone();
        let zslug = z.slug.clone();
        let zslug_for_run = zslug.clone();
        let zslug_for_stop = zslug.clone();
        let zname_for_run = zname.clone();
        let planned_min = (z.planned_run_seconds + 30) / 60;
        let last_run_str = if z.last_run_epoch > 0 {
            format_relative_past(z.last_run_epoch)
        } else {
            "—".to_string()
        };
        let running = z.running;
        let today_min = z.today_run_minutes;
        let bucket_mm = z.bucket_mm;

        let on_run = move |_| {
            sheet_zone.set(Some(zslug_for_run.clone()));
            sheet_label.set(zname_for_run.clone());
            sheet_visible.set(true);
        };
        let on_stop = move |_| {
            let slug = zslug_for_stop.clone();
            post_action(json!({"kind": "stop", "zone": slug}));
        };

        // Filter history to this zone, sort newest-first, take last 14 days.
        let zslug_history = zslug.clone();
        let rows = move || {
            let h = history.get();
            let mut runs: Vec<RunRecord> = h
                .runs
                .into_iter()
                .filter(|r| r.zone == zslug_history)
                .collect();
            runs.sort_by_key(|r| std::cmp::Reverse(r.start_epoch));
            runs.into_iter().take(14).collect::<Vec<_>>()
        };

        // Daily-totals buckets for the sparkline: oldest -> newest, 14 entries,
        // each is seconds run on that local day. Matches the desktop
        // PerZoneDailyBarsTile so the visual language is consistent.
        let zslug_sparkline = zslug.clone();
        let daily_buckets = move || -> Vec<i64> {
            let h = history.get();
            let now = Local::now();
            let today_midnight = now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|nd| Local.from_local_datetime(&nd).single())
                .unwrap_or(now)
                .timestamp();
            let mut buckets = vec![0i64; 14];
            for r in h
                .runs
                .iter()
                .filter(|r| r.skip_reason.is_none() && r.zone == zslug_sparkline)
            {
                let days_back = ((today_midnight - r.start_epoch) / 86400).max(0);
                if (0..14).contains(&days_back) {
                    buckets[days_back as usize] += r.duration_s;
                }
            }
            buckets.reverse();
            buckets
        };

        let zmath = z.math.clone();

        view! {
            <div class="mobile-stack mobile-zone-detail">
                <a href="/zones" class="mobile-back-link" on:click=on_back.clone()>"‹ Zones"</a>

                <header class="mobile-zone-header">
                    <h2 class="mobile-zone-title">{zname}</h2>
                    <span class=if running { "zone-row-badge zone-row-badge-running" } else { "zone-row-badge" }>
                        {if running { "RUNNING" } else { "IDLE" }}
                    </span>
                </header>

                <dl class="mobile-zone-stats">
                    <div class="mobile-zone-stat"><dt>"Planned"</dt><dd>{planned_min}" min"</dd></div>
                    <div class="mobile-zone-stat"><dt>"Today"</dt><dd>{format!("{:.0} min", today_min)}</dd></div>
                    <div class="mobile-zone-stat"><dt>"Deficit"</dt><dd>{format!("{:.1} mm", bucket_mm)}</dd></div>
                    <div class="mobile-zone-stat"><dt>"Last run"</dt><dd>{last_run_str}</dd></div>
                </dl>

                <div class="mobile-zone-actions">
                    {if running {
                        view! {
                            <button class="btn-clay btn-clay-hot mobile-primary-btn" on:click=on_stop>"STOP"</button>
                        }.into_any()
                    } else {
                        view! {
                            <button class="btn-clay btn-clay-good mobile-primary-btn" on:click=on_run>"Run for…"</button>
                        }.into_any()
                    }}
                </div>

                <h3 class="mobile-section-title">"Last 14 days"</h3>
                <div class="mobile-zone-sparkline">
                    {move || {
                        let bs = daily_buckets();
                        let max_min = bs.iter().map(|s| *s as f64 / 60.0).fold(0.0_f64, f64::max).max(1.0);
                        view! {
                            <div class="mobile-zone-sparkline-head">
                                <span class="mobile-zone-sparkline-scale">{format!("scale: {:.0} min", max_min)}</span>
                            </div>
                            <svg
                                class="mobile-zone-sparkline-svg"
                                viewBox="0 0 280 80"
                                preserveAspectRatio="none"
                                role="img"
                                aria-label="Daily watered minutes, last 14 days"
                            >
                                {
                                    let n = bs.len() as f64;
                                    let w = 280.0 / n;
                                    let bar_w = (w - 2.0).max(2.0);
                                    bs.iter().enumerate().map(|(i, sec)| {
                                        let mins = *sec as f64 / 60.0;
                                        let h = (mins / max_min * 76.0).max(if mins > 0.0 { 1.0 } else { 0.0 });
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
                                }
                            </svg>
                        }.into_any()
                    }}
                </div>

                <details class="mobile-zone-math-details">
                    <summary class="mobile-zone-math-summary">"Why this duration?"</summary>
                    {match zmath {
                        Some(m) => view! { <MobileZoneMathRows m=m/> }.into_any(),
                        None => view! {
                            <p class="mobile-zone-math-empty">
                                "Engine hasn\u{2019}t computed yet (waiting for the next 23:00 tick)."
                            </p>
                        }.into_any(),
                    }}
                </details>

                <h3 class="mobile-section-title">"Last 14 runs"</h3>
                <div class="mobile-history-list">
                    {move || {
                        let r = rows();
                        if r.is_empty() {
                            view! { <div class="mobile-history-empty">"No history yet."</div> }.into_any()
                        } else {
                            r.into_iter().map(|run| view! {
                                <HistoryRow run/>
                            }.into_any()).collect::<Vec<_>>().into_any()
                        }
                    }}
                </div>

                <DurationSheet visible=sheet_visible zone_slug=sheet_zone zone_label=sheet_label/>
            </div>
        }.into_any()
    }
}

#[component]
fn MobileZoneMathRows(m: ZoneMath) -> impl IntoView {
    let bucket = if m.bucket_mm < 0.0 {
        format!("{:.2} mm (deficit)", m.bucket_mm)
    } else if m.bucket_mm > 0.0 {
        format!("+{:.2} mm (surplus)", m.bucket_mm)
    } else {
        "0.00 mm (at field capacity)".to_string()
    };
    let kc_kind = if m.kc >= 1.0 { "turf" } else { "shrubs / drip" };
    let heat_kind = if m.heat_mult >= 1.25 {
        "heat advisory"
    } else if m.heat_mult >= 1.10 {
        "warm"
    } else {
        "no boost"
    };
    let thr_kind = if m.throughput_mm_hr <= 0.0 {
        "unset"
    } else if m.throughput_mm_hr < 4.0 {
        "drip / low-precip rotor"
    } else if m.throughput_mm_hr < 10.0 {
        "rotor"
    } else if m.throughput_mm_hr < 20.0 {
        "R-VAN / MP"
    } else {
        "fixed spray"
    };
    let raw = format_seconds_pretty(m.raw_seconds);
    let cap_row = if m.cap_binding {
        let pct = ((m.raw_seconds - m.max_duration_seconds) as f64 / m.raw_seconds.max(1) as f64
            * 100.0)
            .round() as i64;
        format!(
            "capped at {} ({}% short)",
            format_seconds_pretty(m.max_duration_seconds),
            pct
        )
    } else {
        format!(
            "under cap ({} ceiling)",
            format_seconds_pretty(m.max_duration_seconds)
        )
    };
    let final_row = format!(
        "scheduled tonight: {}",
        format_seconds_pretty(m.scheduled_seconds)
    );
    let row_cap_class = if m.cap_binding {
        "mobile-zone-math-row mobile-zone-math-row-cap is-binding"
    } else {
        "mobile-zone-math-row mobile-zone-math-row-cap"
    };

    view! {
        <dl class="mobile-zone-math-rows">
            <div class="mobile-zone-math-row"><dt>"bucket deficit"</dt><dd>{bucket}</dd></div>
            <div class="mobile-zone-math-row"><dt>"crop coefficient"</dt><dd>{format!("\u{00d7} {:.2} ({})", m.kc, kc_kind)}</dd></div>
            <div class="mobile-zone-math-row"><dt>"heat multiplier"</dt><dd>{format!("\u{00d7} {:.2} ({})", m.heat_mult, heat_kind)}</dd></div>
            <div class="mobile-zone-math-row"><dt>"throughput"</dt><dd>{format!("\u{00f7} {:.2} mm/hr ({})", m.throughput_mm_hr, thr_kind)}</dd></div>
            <div class="mobile-zone-math-row"><dt>"capture efficiency"</dt><dd>{format!("\u{00f7} {:.2}", m.capture_eff)}</dd></div>
            <div class="mobile-zone-math-row mobile-zone-math-row-raw"><dt>"raw need"</dt><dd>{format!("= {raw}")}</dd></div>
            <div class=row_cap_class><dt>"safety ceiling"</dt><dd>{cap_row}</dd></div>
            <div class="mobile-zone-math-row mobile-zone-math-row-final"><dt>"final"</dt><dd>{final_row}</dd></div>
        </dl>
    }
}

fn format_seconds_pretty(s: u32) -> String {
    let m = s / 60;
    let r = s % 60;
    if m == 0 {
        format!("{r}s")
    } else if r == 0 {
        format!("{m}min")
    } else {
        format!("{m}min {r}s")
    }
}

#[component]
fn HistoryRow(run: RunRecord) -> impl IntoView {
    let dt = epoch_to_local(run.start_epoch);
    let day = dt.format("%a %b %-d").to_string();
    let time = dt.format("%-I:%M %p").to_string();
    let dur_min = ((run.duration_s as f64) / 60.0).round() as i64;
    let skipped = run.skip_reason.is_some();
    let reason = run.skip_reason.unwrap_or_default();

    view! {
        <div class=if skipped { "mobile-history-row is-skip" } else { "mobile-history-row" }>
            <div class="mobile-history-day">{day}</div>
            <div class="mobile-history-mid">
                <div class="mobile-history-time">{time}</div>
                {if skipped {
                    view! { <div class="mobile-history-reason">{reason}</div> }.into_any()
                } else {
                    view! { <div class="mobile-history-dur">{dur_min}" min"</div> }.into_any()
                }}
            </div>
            <div class="mobile-history-marker" aria-hidden="true">
                {if skipped { "—" } else { "●" }}
            </div>
        </div>
    }
}

fn epoch_to_local(epoch: i64) -> DateTime<Local> {
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).unwrap())
}

fn format_relative_past(epoch: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let diff = now - epoch;
    if diff < 60 {
        return "just now".to_string();
    }
    if diff < 3600 {
        return format!("{}m ago", diff / 60);
    }
    if diff < 86_400 {
        return format!("{}h ago", diff / 3600);
    }
    let days = diff / 86_400;
    if days == 1 {
        "yesterday".to_string()
    } else {
        format!("{days}d ago")
    }
}
