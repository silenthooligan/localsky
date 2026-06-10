// ZoneDetail — the single, responsive per-zone view at /zones/:slug
// (reached from the Zone Canvas rail). Replaces the mobile-only detail on
// this route with one built on the v2 primitives: status header, KPI
// StatTiles, a 30-day watered-minutes LineChart, a Run (with duration
// stepper) / Stop control, and the "why this duration?" math breakdown.
// Reads the live IrrigationSnapshot + the existing /api/irrigation/history
// endpoint — no new backend.

use chrono::{Local, TimeZone};
use leptos::prelude::*;
use serde_json::json;

use crate::components::irrigation::controls::post_action_then;
use crate::components::ui::{use_toast, Button, Icon, LineChart, Series, StatTile, Stepper};
use crate::ha::snapshot::{IrrigationSnapshot, ZoneMath, ZoneState};
use crate::history::types::HistoryWindow;
use leptos_router::hooks::use_params_map;

/// Daily watered-minutes buckets for one zone, oldest -> newest.
fn zone_day_buckets(window: &HistoryWindow, slug: &str, days: i64) -> Vec<f64> {
    let now = Local::now();
    let today_mid = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|nd| Local.from_local_datetime(&nd).single())
        .unwrap_or(now)
        .timestamp();
    let n = days.max(1) as usize;
    let mut b = vec![0f64; n];
    for r in window
        .runs
        .iter()
        .filter(|r| r.skip_reason.is_none() && r.zone == slug)
    {
        let back = ((today_mid - r.start_epoch) / 86_400).max(0);
        if (back as usize) < n {
            b[back as usize] += r.duration_s as f64 / 60.0;
        }
    }
    b.reverse();
    b
}

/// The zone detail body, parameterized by a reactive slug so it can render
/// both standalone on `/zones/:slug` (back link shown) and inline in the
/// Zones master-detail pane (back link hidden, selection-driven).
#[component]
pub fn ZoneDetailView(
    snap: ReadSignal<IrrigationSnapshot>,
    slug: Signal<String>,
    #[prop(default = false)] back: bool,
) -> impl IntoView {
    let zone = move || -> Option<ZoneState> {
        let s = slug.get();
        snap.get().zones.iter().find(|z| z.slug == s).cloned()
    };

    // 30-day history for the watered-minutes chart.
    let history = RwSignal::new(HistoryWindow::default());
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let _ = slug.get();
            leptos::task::spawn_local(async move {
                if let Ok(resp) = gloo_net::http::Request::get("/api/irrigation/history?days=30")
                    .send()
                    .await
                {
                    if let Ok(w) = resp.json::<HistoryWindow>().await {
                        history.set(w);
                    }
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = history;

    // Run-duration stepper (minutes), seeded to a sane 10.
    let run_min: RwSignal<f64> = RwSignal::new(10.0);

    // Optimistic control state: Some(true) = start requested, Some(false)
    // = stop requested. The reconcile Effect clears it once the streamed
    // snapshot confirms the new running state (or rolls back with a toast
    // if the controller never confirms within the deadline).
    let pending: RwSignal<Option<bool>> = RwSignal::new(None);
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let Some(expect_running) = pending.get() else {
            return;
        };
        let confirmed = snap
            .get()
            .zones
            .iter()
            .find(|z| z.slug == slug.get_untracked())
            .map(|z| z.running == expect_running)
            .unwrap_or(false);
        if confirmed {
            pending.set(None);
        }
    });
    // Generation guard so a stale deadline timer can't clear a newer
    // request: each pending set bumps the generation, and a timer only
    // acts if its generation is still current.
    let pending_gen = StoredValue::new(0u64);
    #[cfg(feature = "hydrate")]
    {
        // Deadline: clear a pending flag that never confirmed after 25s
        // (two snapshot ticks) and tell the user.
        Effect::new(move |_| {
            if pending.get().is_none() {
                return;
            }
            let gen = pending_gen.with_value(|g| *g);
            leptos::task::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(25_000).await;
                let still_current = pending_gen.with_value(|g| *g) == gen;
                if still_current && pending.get_untracked().is_some() {
                    pending.set(None);
                    use_toast()
                        .warn("Controller didn't confirm the change; check the Sensors hub.");
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = pending_gen;

    move || {
        match zone() {
        None => view! {
            <div class="zone-detail">
                {back.then(|| view! { <a class="zone-detail__back" href="/zones"><Icon name="chevron-right" size=16 class="zone-detail__back-icon".to_string()/>"Zones"</a> })}
                <div class="zone-detail__empty"><crate::components::ui::SkeletonRows count=5/></div>
            </div>
        }
        .into_any(),
        Some(z) => {
            let running = z.running;
            let planned = ((z.planned_run_seconds + 30) / 60).to_string();
            let today = format!("{:.0}", z.today_run_minutes);
            let deficit = format!("{:.1}", z.bucket_mm);
            let last_run = if z.last_run_epoch > 0 {
                Local
                    .timestamp_opt(z.last_run_epoch, 0)
                    .single()
                    .map(|dt| dt.format("%b %-d, %-I:%M %p").to_string())
                    .unwrap_or_else(|| "—".into())
            } else {
                "—".into()
            };
            let name = z.name.clone();
            let zslug = z.slug.clone();
            let stop_slug = zslug.clone();
            let run_slug = zslug.clone();
            let action_done = Callback::new(move |result: Result<(), String>| {
                if let Err(e) = result {
                    pending.set(None);
                    use_toast().error(format!("Zone command failed: {e}"));
                }
            });
            let on_stop = move |_: leptos::ev::MouseEvent| {
                pending_gen.update_value(|g| *g += 1);
                pending.set(Some(false));
                post_action_then(
                    json!({ "kind": "stop", "zone": stop_slug.clone() }),
                    action_done,
                );
            };
            let on_run = move |_: leptos::ev::MouseEvent| {
                let seconds = (run_min.get_untracked() * 60.0).round().max(1.0) as u32;
                pending_gen.update_value(|g| *g += 1);
                pending.set(Some(true));
                post_action_then(
                    json!({ "kind": "run", "zone": run_slug.clone(), "seconds": seconds }),
                    action_done,
                );
            };
            let pending_now = pending.get();
            let status_label = match pending_now {
                Some(true) if !running => "STARTING…",
                Some(false) if running => "STOPPING…",
                _ if running => "RUNNING",
                _ if z.planned_run_seconds > 0 => "SCHEDULED",
                _ => "IDLE",
            };
            let status_class = if pending_now.is_some() && running != pending_now.unwrap_or(false) {
                "zone-detail__status zone-detail__status--pending"
            } else if running {
                "zone-detail__status zone-detail__status--running"
            } else if z.planned_run_seconds > 0 {
                "zone-detail__status zone-detail__status--scheduled"
            } else {
                "zone-detail__status zone-detail__status--idle"
            };
            let math = z.math.clone();
            let chart_slug = zslug.clone();

            // Per-zone verdict (decide_per_zone): colored pill + reason line.
            let verdict = z.verdict.clone();
            let verdict_pill = verdict.as_ref().map(|v| {
                let vc = crate::components::verdict::verdict_token(&v.verdict);
                let vl = crate::components::verdict::verdict_label(&v.verdict);
                view! { <span class="zone-detail__verdict" style=format!("--vc:{vc}")>{vl}</span> }
            });
            let verdict_reason = verdict
                .as_ref()
                .filter(|v| !v.reason.is_empty())
                .map(|v| {
                    let r = v.reason.clone();
                    view! { <p class="zone-detail__verdict-reason">{r}</p> }
                });

            view! {
                <div class="zone-detail">
                    {back.then(|| view! { <a class="zone-detail__back" href="/zones"><Icon name="chevron-right" size=16 class="zone-detail__back-icon".to_string()/>"Zones"</a> })}
                    <header class="zone-detail__head">
                        <h1 class="zone-detail__name">{name}</h1>
                        <span class=status_class>{status_label}</span>
                        {verdict_pill}
                    </header>
                    {verdict_reason}

                    <div class="zone-detail__stats">
                        <StatTile label="Planned" value=planned unit="min" icon="droplet"/>
                        <StatTile label="Today" value=today unit="min" icon="history" accent="var(--accent-good)".to_string()/>
                        <StatTile label="Deficit" value=deficit unit="mm" icon="gauge" accent="var(--accent-cool)".to_string()/>
                        <StatTile label="Last run" value=last_run icon="calendar" accent="var(--accent-warm)".to_string()/>
                    </div>

                    <section class="zone-detail__panel">
                        <h2 class="zone-detail__panel-title">"Watered minutes — last 30 days"</h2>
                        {move || {
                            let b = zone_day_buckets(&history.get(), &chart_slug, 30);
                            let pts: Vec<(f64, f64)> = b.iter().enumerate().map(|(i, m)| (i as f64, *m)).collect();
                            view! { <LineChart series=vec![Series::new("min", "var(--accent)", pts)] height=180 legend=false y_unit=" min".to_string()/> }
                        }}
                    </section>

                    <section class="zone-detail__panel zone-detail__actions">
                        {if running {
                            view! {
                                <Button
                                    variant="danger"
                                    icon="stop"
                                    loading=Signal::derive(move || pending.get().is_some())
                                    on_click=Callback::new(on_stop)
                                >"Stop zone"</Button>
                            }.into_any()
                        } else {
                            view! {
                                <div class="zone-detail__run">
                                    <Stepper value=run_min min=1.0 max=120.0 step=1.0 suffix=" min"/>
                                    <Button
                                        variant="primary"
                                        icon="play"
                                        loading=Signal::derive(move || pending.get().is_some())
                                        on_click=Callback::new(on_run)
                                    >"Run now"</Button>
                                </div>
                            }.into_any()
                        }}
                    </section>

                    {math.map(|m| view! { <ZoneMathPanel m/> })}
                </div>
            }
            .into_any()
        }
    }
    }
}

#[component]
fn ZoneMathPanel(m: ZoneMath) -> impl IntoView {
    let cap = if m.cap_binding {
        format!("capped at {} min", m.max_duration_seconds / 60)
    } else {
        format!("under {} min cap", m.max_duration_seconds / 60)
    };
    view! {
        <section class="zone-detail__panel">
            <h2 class="zone-detail__panel-title">"Why this duration?"</h2>
            <dl class="zone-detail__math">
                <div><dt>"Bucket deficit"</dt><dd>{format!("{:.2} mm", m.bucket_mm)}</dd></div>
                <div><dt>"Crop coefficient"</dt><dd>{format!("× {:.2}", m.kc)}</dd></div>
                <div><dt>"Heat multiplier"</dt><dd>{format!("× {:.2}", m.heat_mult)}</dd></div>
                <div><dt>"Throughput"</dt><dd>{format!("÷ {:.1} mm/hr", m.throughput_mm_hr)}</dd></div>
                <div><dt>"Capture efficiency"</dt><dd>{format!("÷ {:.2}", m.capture_eff)}</dd></div>
                <div class="zone-detail__math-final"><dt>"Scheduled"</dt><dd>{format!("{} min ({cap})", m.scheduled_seconds / 60)}</dd></div>
            </dl>
        </section>
    }
}

/// Route wrapper for /zones/:slug — reads the slug param and shows the
/// detail standalone with a back link.
#[component]
pub fn ZoneDetailPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let params = use_params_map();
    let slug = Signal::derive(move || {
        params
            .get()
            .get("slug")
            .map(|s| s.to_string())
            .unwrap_or_default()
    });
    view! { <ZoneDetailView snap slug back=true/> }
}
