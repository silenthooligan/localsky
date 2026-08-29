// ZoneDetail, the single, responsive per-zone view at /zones/:slug
// (reached from the Zone Canvas rail). Replaces the mobile-only detail on
// this route with one built on the v2 primitives: status header, KPI
// StatTiles, a 30-day watered-minutes LineChart, a Run (with duration
// stepper) / Stop control, and the "why this duration?" math breakdown.
// Reads the live IrrigationSnapshot + the existing /api/irrigation/history
// endpoint, no new backend.

use chrono::{Local, TimeZone};
use leptos::prelude::*;
use serde_json::json;

use crate::components::irrigation::controls::post_action_body_then;
use crate::components::ui::{
    use_toast, Button, Icon, LineChart, Series, Sparkline, StatTile, Stepper,
};
use crate::components::units_fmt::{
    depth_unit, depth_value_mm, fmt_rain_amount_mm, fmt_rain_rate_mm, temp_unit, temp_value,
    use_unit_prefs, UnitPrefs,
};
use crate::ha::snapshot::{IrrigationSnapshot, ZoneMath, ZoneState};
use crate::history::types::HistoryWindow;
use leptos_router::hooks::use_params_map;

/// How long the optimistic pending flag waits for the streamed snapshot to
/// confirm a dispatched change before it clears and says something. Two
/// snapshot ticks. Deliberately NOT stretched to cover a cloud controller's
/// poll throttle: a longer spinner is not more truthful than saying the
/// controller accepted the change and is not reporting yet.
// Read only from the hydrate-only deadline timer and the unit tests.
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
const CONFIRM_DEADLINE_S: u32 = 25;

/// Current UTC epoch in seconds. The instant is timezone-independent; the
/// deployment-TZ rendering happens later in `timefmt::format_md`.
fn now_epoch_secs() -> i64 {
    Local::now().timestamp()
}

/// A controller's status-readback interval in plain words, for the message
/// shown when a change was accepted but the controller has not reported it
/// inside the confirm window. Under two minutes stays in seconds, so a 60s
/// poll floor reads as "60 seconds" rather than being rounded into a
/// minute count that overstates the wait; from two minutes up it is
/// rounded minutes, which is always plural.
// Called only from the hydrate-only deadline timer and the unit tests.
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
fn poll_interval_phrase(seconds: u32) -> String {
    if seconds < 120 {
        return format!("{seconds} seconds");
    }
    format!("{} minutes", (seconds + 30) / 60)
}

/// Daily watered-minutes buckets for one zone, oldest -> newest.
/// Minutes are the union-clustered watering evidence (the shared
/// history::rollup rule, same filter + clustering the water balance
/// credits), so this chart and the balance can never disagree.
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
    let zone_rows: Vec<crate::history::types::RunRecord> = window
        .runs
        .iter()
        .filter(|r| r.zone == slug)
        .cloned()
        .collect();
    for events in crate::history::rollup::watering_events_per_zone(&zone_rows).values() {
        for e in events {
            let back = crate::components::time_bucket::days_back(today_mid, e.start_epoch).max(0);
            if (back as usize) < n {
                b[back as usize] += e.valve_open_s as f64 / 60.0;
            }
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

    // Per-device display-unit preferences. Read prefs.get() inside the
    // reactive body so a units change re-renders the converted tiles;
    // non-reactive child panels (ZoneMathPanel) take a UnitPrefs prop.
    let prefs = use_unit_prefs();

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

    // The toast hub, resolved ONCE at component scope. Resolving it inside a
    // detached continuation is a context lookup with no owner to read from;
    // the hub itself is shell-owned and outlives every view here.
    let toast = use_toast();

    // The dispatching controller's status-readback interval, from the action
    // response (null when it reads state on demand). A cloud controller
    // polls on a throttle longer than the confirm window, so a run can be
    // accepted and still not read back as running inside it. Knowing the
    // number is what lets the deadline message say that plainly instead of
    // implying the controller never answered.
    let confirm_within_s: RwSignal<Option<u32>> = RwSignal::new(None);

    // Whether a CONTROLLER took this command. The registry path stamps
    // `dispatched: "controller:<id>"` on its response; the legacy Home
    // Assistant service-call path never does. An HA-source deploy with no
    // controllers configured must not be sent to a controller page it has
    // nothing on, which is what a single unconditional message did.
    let via_controller: RwSignal<bool> = RwSignal::new(false);

    // Action results land here. Created at COMPONENT scope on purpose: built
    // inside the reactive body below it was owned by the render effect, and
    // the next streamed snapshot disposed it before a slow response arrived,
    // so a failed dispatch delivered its error to a dead callback and the
    // only thing the user ever saw was the deadline warning. Every signal
    // touch uses try_* because this runs from a detached continuation and
    // wasm-release is panic=abort.
    let action_done = Callback::new(move |result: Result<Option<serde_json::Value>, String>| {
        match result {
            Ok(body) => {
                let v = body.unwrap_or(serde_json::Value::Null);
                let _ = confirm_within_s.try_set(
                    v.get("confirm_within_s")
                        .and_then(|n| n.as_u64())
                        .map(|n| n as u32),
                );
                let _ = via_controller.try_set(v.get("dispatched").is_some());
                // A controller with no per-zone stop reports the real scope
                // (the whole device stopped); relay it verbatim.
                if let Some(note) = v.get("note").and_then(|n| n.as_str()) {
                    toast.info(note.to_string());
                }
            }
            Err(e) => {
                let _ = pending.try_set(None);
                toast.error(format!("Zone command failed: {e}"));
            }
        }
    });

    #[cfg(feature = "hydrate")]
    {
        // Deadline: clear a pending flag the snapshot never confirmed and
        // say what is actually known.
        Effect::new(move |_| {
            if pending.get().is_none() {
                return;
            }
            let gen = pending_gen.with_value(|g| *g);
            leptos::task::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(CONFIRM_DEADLINE_S * 1_000).await;
                // This continuation is detached, so it can outlive the
                // route-scoped view: navigating away disposes pending /
                // pending_gen while the timer is still pending. Read them
                // with the non-panicking try_* accessors (get_untracked /
                // with_value abort on a disposed signal, and wasm-release
                // is panic=abort, which would poison the whole app). If the
                // view is gone there is nothing to time out; just exit.
                let Some(cur_gen) = pending_gen.try_get_value() else {
                    return;
                };
                let still_current = cur_gen == gen;
                if still_current && pending.try_get_untracked().flatten().is_some() {
                    let _ = pending.try_set(None);
                    match confirm_within_s.try_get_untracked().flatten() {
                        // The controller took the change; its own status poll
                        // is simply slower than this window. Saying it failed
                        // would be wrong, so say what happened.
                        Some(s) if s > CONFIRM_DEADLINE_S => toast.info(format!(
                            "The controller accepted the change. It reports its state about \
                             every {}, so this zone can keep showing the old state for up \
                             to that long.",
                            poll_interval_phrase(s)
                        )),
                        // No readback lag to blame and a CONTROLLER took the
                        // command: it genuinely has not reported the change.
                        // Point at the page that lists controllers and at
                        // Scan zones, which is the live probe that page
                        // actually has. The Sensors hub cannot help: its
                        // inventory is sources and flow-capable controllers,
                        // and a Rachio honestly reports no flow meter, so it
                        // structurally never appears there.
                        _ if via_controller.try_get_untracked().unwrap_or(false) => toast.warn(
                            "The controller has not reported the change. Open Settings, then \
                             Devices, open the controller with Edit, and use Scan zones to \
                             check it answers.",
                        ),
                        // No controller took it: this is the legacy Home
                        // Assistant service-call path, where the entity is
                        // the thing that did not change.
                        _ => toast.warn(
                            "Home Assistant accepted the command, but the zone's entity has \
                             not changed. Check the sprinkler entities in Home Assistant.",
                        ),
                    }
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = pending_gen;

    move || {
        match zone() {
        // No matching zone. Before the first snapshot streams in we can't
        // tell "still loading" from "bad slug", so show the skeleton; once
        // the snapshot has loaded (last_refresh_epoch > 0) an unmatched
        // slug is a real miss and gets an explicit empty state instead of
        // an infinite skeleton.
        None if snap.get().last_refresh_epoch > 0 => view! {
            <div class="zone-detail">
                {back.then(|| view! { <a class="zone-detail__back" href="/zones"><Icon name="chevron-right" size=16 class="zone-detail__back-icon".to_string()/>"Zones"</a> })}
                <crate::components::ui::EmptyState
                    title="Zone not found"
                    body="No zone with this address exists in the current configuration; it may have been renamed or removed."
                    cta_label="Back to Zones"
                    cta_href="/zones"
                    icon="zones"
                />
            </div>
        }
        .into_any(),
        None => view! {
            <div class="zone-detail">
                {back.then(|| view! { <a class="zone-detail__back" href="/zones"><Icon name="chevron-right" size=16 class="zone-detail__back-icon".to_string()/>"Zones"</a> })}
                <div class="zone-detail__empty"><crate::components::ui::SkeletonRows count=5/></div>
            </div>
        }
        .into_any(),
        Some(z) => {
            // Assigned-probe data: live moisture + band from the snapshot's
            // soil_forecasts (keyed by slug), plus the probe's native
            // temp/EC/battery readings carried on the ZoneState itself.
            let soil_fc = snap
                .get()
                .soil_forecasts
                .iter()
                .find(|f| f.zone_slug == z.slug)
                .cloned();
            // Display-unit prefs for this render pass (reactive: re-reads on change).
            let p = prefs.get();
            // Deployment IANA timezone for every user-facing time render below
            // (24-hour, deployment-local, not the viewer's browser TZ).
            let tz = snap.get().timezone;
            let running = z.running;
            // A zone the engine will SKIP waters 0 minutes tonight regardless of
            // any leftover planned duration on the snapshot, so the "Planned"
            // tile and the status pill both reflect the skip (T4).
            let zone_skipping = z.verdict.as_ref().is_some_and(|v| v.verdict == "skip");
            let planned = if zone_skipping {
                "0".to_string()
            } else {
                ((z.planned_run_seconds + 30) / 60).to_string()
            };
            let today = format!("{:.0}", z.today_run_minutes);
            // Bucket deficit is stored in mm; convert at the display boundary.
            let deficit = depth_value_mm(z.bucket_mm, p);
            let deficit_unit = depth_unit(p);
            let last_run = if z.last_run_epoch > 0 {
                // "Jun 28, 14:05" in the deployment timezone (24-hour, local).
                format!(
                    "{}, {}",
                    crate::timefmt::format_md(z.last_run_epoch, &tz),
                    crate::timefmt::format_hm(z.last_run_epoch, &tz),
                )
            } else {
                "-".into()
            };
            let name = z.name.clone();
            let zslug = z.slug.clone();
            let stop_slug = zslug.clone();
            let run_slug = zslug.clone();
            // Both handlers hand their result to the component-scoped
            // `action_done` above. Building a fresh Callback here (or inside
            // the click handler) tied it to the render effect's owner, which
            // the next snapshot disposes.
            let on_stop = move |_: leptos::ev::MouseEvent| {
                pending_gen.update_value(|g| *g += 1);
                confirm_within_s.set(None);
                via_controller.set(false);
                pending.set(Some(false));
                post_action_body_then(
                    json!({ "kind": "stop", "zone": stop_slug.clone() }),
                    action_done,
                );
            };
            let on_run = move |_: leptos::ev::MouseEvent| {
                let seconds = (run_min.get_untracked() * 60.0).round().max(1.0) as u32;
                pending_gen.update_value(|g| *g += 1);
                confirm_within_s.set(None);
                via_controller.set(false);
                pending.set(Some(true));
                post_action_body_then(
                    json!({ "kind": "run", "zone": run_slug.clone(), "seconds": seconds }),
                    action_done,
                );
            };
            let pending_now = pending.get();
            // A zone the engine will SKIP must not read "SCHEDULED" even when it
            // carries a leftover planned duration: the skip verdict is the truth,
            // so it reads "SKIPPING" (after the in-flight/running states, which
            // still take precedence). (T4) Matches the zone card's status pill.
            // (zone_skipping computed above with `planned`.)
            let status_label = match pending_now {
                Some(true) if !running => "STARTING…",
                Some(false) if running => "STOPPING…",
                _ if running => "RUNNING",
                _ if zone_skipping => "SKIPPING",
                _ if z.planned_run_seconds > 0 => "SCHEDULED",
                _ => "IDLE",
            };
            let status_class = if pending_now.is_some() && running != pending_now.unwrap_or(false) {
                "zone-detail__status zone-detail__status--pending"
            } else if running {
                "zone-detail__status zone-detail__status--running"
            } else if zone_skipping {
                "zone-detail__status zone-detail__status--skipping"
            } else if z.planned_run_seconds > 0 {
                "zone-detail__status zone-detail__status--scheduled"
            } else {
                "zone-detail__status zone-detail__status--idle"
            };
            let math = z.math.clone();
            let chart_slug = zslug.clone();
            // Own copy of the deployment tz for the chart-label closure below.
            let chart_tz = tz.clone();

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
                    // P2 units architecture: render the reason unit-aware from the
                    // structured ZoneVerdict (soil reasons are percent /
                    // unit-invariant; global-bound reasons fall back to baked).
                    let r = crate::reason_render::render_zone_reason(v, p);
                    view! { <p class="zone-detail__verdict-reason">{r}</p> }
                });

            view! {
                <div class="zone-detail">
                    {back.then(|| view! { <a class="zone-detail__back" href="/zones"><Icon name="chevron-right" size=16 class="zone-detail__back-icon".to_string()/>"Zones"</a> })}
                    <header class="zone-detail__head">
                        <h1 class="zone-detail__name">{name}</h1>
                        <span class=status_class>{status_label}</span>
                        {verdict_pill}
                        <a
                            class="zone-detail__edit"
                            href=format!("/settings/zones?zone={zslug}")
                            title="Species, soil, sprinkler, sensor assignment, budgets"
                        >
                            <Icon name="settings" size=14/>
                            "Edit zone"
                        </a>
                    </header>
                    {verdict_reason}

                    <div class="zone-detail__stats">
                        <StatTile label="Planned" value=planned unit="min" icon="droplet"/>
                        <StatTile label="Today" value=today unit="min" icon="history" accent="var(--accent-good)".to_string()/>
                        <StatTile label="Deficit" value=deficit unit=deficit_unit icon="gauge" accent="var(--accent-cool)".to_string()/>
                        <StatTile label="Last run" value=last_run icon="calendar" accent="var(--accent-warm)".to_string()/>
                    </div>

                    {
                        let has_probe = soil_fc.as_ref().map(|f| f.current_pct.is_some()).unwrap_or(false)
                            || z.soil_temp_f.is_some()
                            || z.soil_ec.is_some()
                            || z.soil_battery_pct.is_some();
                        has_probe.then(|| {
                            let moisture = soil_fc.as_ref().and_then(|f| f.current_pct);
                            let band = soil_fc
                                .as_ref()
                                .map(|f| (f.target_min_pct, f.target_max_pct));
                            let predicted = soil_fc
                                .as_ref()
                                .map(|f| f.predicted_pct.clone())
                                .filter(|p| p.len() >= 2);
                            view! {
                                <section class="zone-detail__panel">
                                    <h2 class="zone-detail__panel-title">"Soil sensor"</h2>
                                    <div class="zone-soil__grid">
                                        {moisture.map(|pct| {
                                            let band_label = band
                                                .map(|(lo, hi)| format!("target {lo:.0}-{hi:.0}%"))
                                                .unwrap_or_default();
                                            let tone = match (pct, band) {
                                                (p, Some((lo, _))) if p < lo => "var(--verdict-extend)",
                                                (p, Some((_, hi))) if p >= hi => "var(--accent-cool)",
                                                _ => "var(--verdict-run)",
                                            };
                                            view! {
                                                <div class="zone-soil__stat" style=format!("--sc:{tone}")>
                                                    <span class="zone-soil__k">"Moisture"</span>
                                                    <span class="zone-soil__v">{format!("{pct:.0}")}<small>"%"</small></span>
                                                    <span class="zone-soil__band">{band_label}</span>
                                                </div>
                                            }
                                        })}
                                        {z.soil_temp_f.map(|t| view! {
                                            <div class="zone-soil__stat">
                                                <span class="zone-soil__k">"Soil temp"</span>
                                                <span class="zone-soil__v">{temp_value(t, p)}<small>{temp_unit(p)}</small></span>
                                                <span class="zone-soil__band">"frost gate input"</span>
                                            </div>
                                        })}
                                        {z.soil_ec.map(|ec| view! {
                                            <div class="zone-soil__stat">
                                                <span class="zone-soil__k">"Conductivity"</span>
                                                <span class="zone-soil__v">{format!("{ec:.0}")}<small>" µS/cm"</small></span>
                                                <span class="zone-soil__band">"salinity / fertility"</span>
                                            </div>
                                        })}
                                        {z.soil_battery_pct.map(|b| view! {
                                            <div class="zone-soil__stat" style=format!("--sc:{}", if b <= 20.0 { "var(--verdict-skip)" } else { "var(--verdict-run)" })>
                                                <span class="zone-soil__k">"Probe battery"</span>
                                                <span class="zone-soil__v">{format!("{b:.0}")}<small>"%"</small></span>
                                                <span class="zone-soil__band">{if b <= 20.0 { "replace soon" } else { "healthy" }}</span>
                                            </div>
                                        })}
                                    </div>
                                    {predicted.map(|p| view! {
                                        <div class="zone-soil__forecast">
                                            <span class="zone-soil__forecast-label">"7-day moisture projection (no watering)"</span>
                                            <Sparkline points=p accent="var(--accent-cool)".to_string() height=44/>
                                        </div>
                                    })}
                                </section>
                            }
                        })
                    }

                    <section class="zone-detail__panel">
                        <h2 class="zone-detail__panel-title">"Watered minutes, last 30 days"</h2>
                        {move || {
                            let b = zone_day_buckets(&history.get(), &chart_slug, 30);
                            let pts: Vec<(f64, f64)> = b.iter().enumerate().map(|(i, m)| (i as f64, *m)).collect();
                            let n = b.len();
                            // Buckets run oldest -> newest; bucket i is (n-1-i) days
                            // back. Label each from an epoch, rendered "Jun 28"-style
                            // in the DEPLOYMENT timezone (not the viewer's browser TZ).
                            let now_epoch = now_epoch_secs();
                            let labels: Vec<String> = (0..n)
                                .map(|i| {
                                    let epoch = now_epoch - ((n - 1 - i) as i64) * 86_400;
                                    crate::timefmt::format_md(epoch, &chart_tz)
                                })
                                .collect();
                            view! { <LineChart series=vec![Series::new("min", "var(--accent)", pts)] height=180 legend=false y_unit=" min".to_string() x_labels=labels/> }
                        }}
                    </section>

                    <crate::components::zones::tuning::ZoneTuningPanel slug=slug/>

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

                    {math.map(|m| view! { <ZoneMathPanel m prefs=p/> })}
                </div>
            }
            .into_any()
        }
    }
    }
}

#[component]
fn ZoneMathPanel(m: ZoneMath, prefs: UnitPrefs) -> impl IntoView {
    let cap = if m.cap_binding {
        format!("capped at {} min", m.max_duration_seconds / 60)
    } else {
        format!("under {} min cap", m.max_duration_seconds / 60)
    };
    // Bucket deficit (mm source) and throughput (mm/hr source) convert at
    // the display boundary; the engine math itself stays in mm.
    let deficit = fmt_rain_amount_mm(m.bucket_mm, prefs);
    let throughput = fmt_rain_rate_mm(m.throughput_mm_hr, prefs);
    view! {
        <section class="zone-detail__panel">
            <h2 class="zone-detail__panel-title">"Why this duration?"</h2>
            <dl class="zone-detail__math">
                <div><dt>"Bucket deficit"</dt><dd>{deficit}</dd></div>
                <div><dt>"Crop coefficient"</dt><dd>{format!("× {:.2}", m.kc)}</dd></div>
                <div><dt>"Heat multiplier"</dt><dd>{format!("× {:.2}", m.heat_mult)}</dd></div>
                <div><dt>"Throughput"</dt><dd>{format!("÷ {throughput}")}</dd></div>
                <div><dt>"Capture efficiency"</dt><dd>{format!("÷ {:.2}", m.capture_eff)}</dd></div>
                <div class="zone-detail__math-final"><dt>"Scheduled"</dt><dd>{format!("{} min ({cap})", m.scheduled_seconds / 60)}</dd></div>
            </dl>
        </section>
    }
}

/// Route wrapper for /zones/:slug, reads the slug param and shows the
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The confirm window is SHORTER than a cloud controller's poll floor,
    /// which is why the deadline message has to be able to say "accepted,
    /// not reported yet" instead of implying a failure: stretching the
    /// timer past the floor would only hide the wait behind a longer
    /// spinner. Both Rachio poll values must land on that branch.
    #[test]
    fn a_throttled_cloud_poll_outruns_the_confirm_window() {
        for interval in [60u32, 120] {
            assert!(
                interval > CONFIRM_DEADLINE_S,
                "a {interval}s status poll must take the accepted-but-not-reported branch"
            );
        }
    }

    #[test]
    fn poll_interval_phrase_reads_plainly() {
        // The Rachio floor. Rounding this into minutes would say "1 minute"
        // or "2 minutes" for a 60s wait, both worse than the real number.
        assert_eq!(poll_interval_phrase(60), "60 seconds");
        assert_eq!(poll_interval_phrase(90), "90 seconds");
        assert_eq!(poll_interval_phrase(119), "119 seconds");
        // The Rachio default, and the configurable band above it.
        assert_eq!(poll_interval_phrase(120), "2 minutes");
        assert_eq!(poll_interval_phrase(300), "5 minutes");
        assert_eq!(poll_interval_phrase(3600), "60 minutes");
        // Rounds to the nearest minute rather than truncating.
        assert_eq!(poll_interval_phrase(150), "3 minutes");
        // The minutes branch starts at 2, so it is never "1 minutes".
        assert!(!poll_interval_phrase(120).starts_with("1 "));
    }
}
