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
    deficit_amount_mm, deficit_value_mm, depth_phrase_mm, depth_unit, fmt_rain_rate_mm, temp_unit,
    temp_value, use_unit_prefs, UnitPrefs,
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
            // No producer on either path: a dash, not a fabricated zero.
            let (today, today_unit) = match z.today_run_minutes {
                Some(v) => (format!("{v:.0}"), "min"),
                None => ("-".to_string(), ""),
            };
            // Bucket deficit is stored in mm; convert at the display boundary
            // through the shared deficit formatter (magnitude; the label
            // carries the direction, matching the soil panel's positive
            // phrasing). The soil model's evidence replay produces it for
            // every zone with agronomy config (negative = needs water);
            // `None` means no bucket could be derived (env-var zones), and
            // the tile renders a dash for that rather than a fabricated 0.00.
            let (deficit, deficit_unit) = match z.bucket_mm {
                Some(v) => (deficit_value_mm(v, p), depth_unit(p)),
                None => ("-".to_string(), ""),
            };
            // This zone's row from the allocator that governs it (weekly
            // by default; the soil model when selected), soil preview
            // fields included.
            let budget = snap
                .get()
                .water_budgets
                .iter()
                .find(|b| b.zone_slug == z.slug)
                .cloned();
            let budget_reason = budget
                .as_ref()
                .filter(|b| b.today_seconds == 0 && !b.today_reason.is_empty())
                .map(|b| b.today_reason.clone());
            let suppression = z.smart_suppressed.clone();
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
            // A zone the weekly budget zeroed is not idle: the engine decided
            // about it and names the gate. Matches the zone card's pill. An
            // Override schedule covering today zeroes the plan AFTER the
            // allocator sized it, so it leaves no reason behind and has to be
            // its own hold signal or the zone reads IDLE on the one day the
            // schedule explains everything.
            let suppressed_today = suppression.as_ref().is_some_and(|x| x.active_today);
            let budget_held = (budget_reason.is_some() || suppressed_today)
                && z.planned_run_seconds == 0
                && !running
                && !zone_skipping
                && pending_now.is_none();
            let status_label = match pending_now {
                Some(true) if !running => "STARTING…",
                Some(false) if running => "STOPPING…",
                _ if running => "RUNNING",
                _ if zone_skipping => "SKIPPING",
                _ if z.planned_run_seconds > 0 => "SCHEDULED",
                _ if budget_held => "ON HOLD",
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
            } else if budget_held {
                "zone-detail__status zone-detail__status--held"
            } else {
                "zone-detail__status zone-detail__status--idle"
            };
            // Which model governs this zone, and whether the soil panel
            // below will render (it needs the soil block). When it does,
            // it already narrates the hold and the clamp in plain,
            // unit-converted sentences, so the standalone notes stand
            // down on this page rather than stating the same fact twice
            // in two registers. The card keeps both notes: no panel
            // exists there. An Override suppression still surfaces here
            // in full; the panel never names schedules.
            let soil_governed_row = budget
                .as_ref()
                .is_some_and(|b| b.scheduling_model == "soil");
            let soil_panel_renders = soil_governed_row
                && budget
                    .as_ref()
                    .is_some_and(|b| b.soil_depletion_mm.is_some());
            let budget_note = (budget_held && !(soil_panel_renders && !suppressed_today)).then(
                || {
                    // A schedule covering today outranks the allocator's
                    // sentence: the suppression overwrote the allocator's
                    // number, so its reason describes a plan that is not
                    // what zeroed the zone. A held soil row composes its
                    // body client-side from the structured soil fields
                    // (unit-aware, no stacked wire lead-ins).
                    let composed = budget
                        .as_ref()
                        .and_then(|b| crate::components::zones::card::soil_hold_body(b, p));
                    // The also-held sentence names the soil model only
                    // when the soil composition actually produced the
                    // body (a governed-but-starved zone's reason is the
                    // weekly allocator's and reads as such).
                    let soil = composed.is_some();
                    let body = composed.or_else(|| budget_reason.clone());
                    let text = crate::components::zones::card::hold_reason_text(
                        suppression.as_ref(),
                        body.as_deref(),
                        soil,
                    );
                    view! {
                        <p class="zone-detail__reason zone-detail__reason--budget">{text}</p>
                    }
                },
            );
            // A partial ceiling clamp on a run that still waters: the
            // composed delivered-of-target sentence renders beside the
            // nonzero minutes it explains (the zero-headroom day rides
            // the ON HOLD path above). Suppressed when the soil panel
            // renders: the panel states the clamp and carries the same
            // numbers as its second line.
            let ceiling_note = (!soil_panel_renders)
                .then(|| {
                    budget
                        .as_ref()
                        .and_then(|b| crate::components::zones::card::ceiling_note_text(b, p))
                })
                .flatten()
                .map(|body| {
                    view! {
                        <p class="zone-detail__reason zone-detail__reason--budget">{body}</p>
                    }
                });
            // The hold line names WHICH schedule; this line names HOW OFTEN,
            // so it stays on the day the Override fires. It shortens to the
            // frequency alone when the hold line already named today's
            // schedule, so the pane does not say it twice.
            let suppression_note = suppression
                .as_ref()
                .filter(|x| !x.weekdays.is_empty())
                .map(|x| {
                    let body = crate::components::zones::card::override_days_text(
                        x,
                        budget_held && x.active_today,
                    );
                    view! { <p class="zone-detail__reason zone-detail__reason--suppressed">{body}</p> }
                });
            let math = z.math.clone();
            let chart_slug = zslug.clone();
            // Own copy of the deployment tz for the chart-label closure below.
            let chart_tz = tz.clone();
            // Which model governs this zone's watering; "weekly" on JSON
            // from a producer that predates the soil model.
            let model = budget
                .as_ref()
                .map(|b| b.scheduling_model.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "weekly".to_string());
            // The soil-model block: the soil plan for this zone, computed
            // on every install. While the weekly plan governs it is a
            // preview; on a soil-governed zone it states that it governs.
            // A governed zone with NO soil block is the cold-start
            // window every adopter passes through (the evidence-starved
            // guard publishes absence): the panel renders a
            // gathering-evidence state instead of silently vanishing,
            // because an owner who just switched sees nothing change
            // otherwise. A weekly zone with no block renders nothing.
            let soil_preview = budget.as_ref().and_then(|b| {
                let governs = model == "soil";
                let head_pill = |governs: bool| {
                    let (label, modifier) = if governs {
                        ("GOVERNS", "zone-detail__panel-pill--governs")
                    } else {
                        ("PREVIEW", "zone-detail__panel-pill--preview")
                    };
                    view! {
                        <span class=format!("zone-detail__panel-pill {modifier}")>{label}</span>
                    }
                };
                let Some(depletion_mm) = b.soil_depletion_mm else {
                    if governs {
                        return Some(
                            view! {
                                <section class="zone-detail__panel">
                                    <h2 class="zone-detail__panel-title">
                                        "Soil model" {head_pill(true)}
                                    </h2>
                                    <p class="zone-detail__reason">
                                        "The soil model governs this zone and is still \
                                         gathering evidence: the weekly plan sizes the \
                                         minutes until a few measured days of water use, \
                                         rain, or completed runs land."
                                    </p>
                                </section>
                            }
                            .into_any(),
                        );
                    }
                    return None;
                };
                // Today's crop water use for the next-watering estimate,
                // through the ENGINE's own ETc: ET0 times this zone's Kc
                // times the heat multiplier. The multiplier used to be
                // dropped here, which understated demand by up to 30% on
                // exactly the days it matters and stretched the "waters
                // next in N days" estimate past what the engine planned.
                // Absent ET0 omits the estimate rather than fabricating
                // one.
                let etc_today_mm = snap.get().forecast.eto_today_mm.map(|e| {
                    let m = z.math.as_ref();
                    crate::engine::etc_mm(
                        e,
                        m.map(|m| m.kc).unwrap_or(1.0),
                        m.map(|m| m.heat_mult).unwrap_or(1.0),
                    )
                });
                // The ladder's hold, governed zones only: the panel must
                // agree with the SKIPPING pill computed above. The
                // verdict reason may be empty; the copy degrades to a
                // bare hold sentence there.
                let held_reason = (governs && zone_skipping)
                    .then(|| {
                        z.verdict
                            .as_ref()
                            .map(|v| v.reason.clone())
                            .unwrap_or_default()
                    });
                // On a governed zone the panel states what DISPATCHES:
                // the post-seasonal-dial seconds (`planned_run_seconds`,
                // what `apply_budget_plan` produced), naming the dial
                // when it changed the figure, so this panel and the
                // Planned tile above it state one number. The shadow
                // preview keeps the pre-dial soil figure: no dispatched
                // number exists there to disagree with. A governed zone
                // an Override zeroed keeps the soil figure too; the hold
                // note above names the schedule that zeroed it.
                let soil_mins = (b.soil_planned_seconds + 30) / 60;
                let (shown_seconds, dial_scaled) =
                    if governs && b.soil_planned_seconds > 0 && z.planned_run_seconds > 0 {
                        (
                            z.planned_run_seconds,
                            (z.planned_run_seconds + 30) / 60 != soil_mins,
                        )
                    } else {
                        (b.soil_planned_seconds, false)
                    };
                let (status_line, next_line) = soil_preview_lines(
                    governs,
                    shown_seconds,
                    b.soil_due,
                    b.soil_deferred_reason.as_deref(),
                    b.soil_ceiling_binding,
                    // The row's session_capped is the soil plan's flag
                    // only on a governed row; the weekly-governed shadow
                    // preview keeps its unqualified sentence.
                    governs && b.session_capped,
                    held_reason.as_deref(),
                    depletion_mm,
                    b.soil_raw_mm.unwrap_or(0.0),
                    etc_today_mm,
                    dial_scaled,
                    p,
                );
                // The clamp's numbers, folded in as the panel's second
                // line on the day the explicit weekly target shorts a
                // governed run (the standalone note above is suppressed
                // while this panel renders).
                let ceiling_line = crate::components::zones::card::ceiling_note_text(b, p);
                // The first post-starvation mornings publish a deficit
                // built mostly on the assumed-dry fallback; when those
                // days dominate the replay window the panel says so, and
                // the clause drops on its own as measured coverage
                // accumulates past the assumption.
                let early_estimate = (b.soil_fallback_days > b.soil_evidence_days).then_some(
                    "Early estimate: days without measurements are assumed dry; the \
                     figures firm up as evidence lands.",
                );
                // The preview keeps its explanatory note; the governed
                // state is carried by the GOVERNS pill in the head.
                let govern_line = (!governs).then_some(
                    "The weekly plan governs this zone; this preview shows what the soil \
                     model would do.",
                );
                Some(
                    view! {
                        <section class="zone-detail__panel">
                            <h2 class="zone-detail__panel-title">
                                "Soil model" {head_pill(governs)}
                            </h2>
                            <p class="zone-detail__reason">{status_line}</p>
                            {ceiling_line.map(|l| view! { <p class="zone-detail__reason">{l}</p> })}
                            {next_line.map(|l| view! { <p class="zone-detail__reason">{l}</p> })}
                            {early_estimate
                                .map(|l| view! { <p class="zone-detail__panel-note">{l}</p> })}
                            {govern_line
                                .map(|l| view! { <p class="zone-detail__panel-note">{l}</p> })}
                        </section>
                    }
                    .into_any(),
                )
            });

            // The model chip, mixed installs only: rendered when this
            // zone's effective model differs from the engine default,
            // matching the zone card's chip so the list and the detail
            // narrate the pin the same way.
            let engine_model = snap.get().engine_scheduling_model;
            let model_chip = budget
                .as_ref()
                .map(|b| b.scheduling_model.as_str())
                .filter(|m| !m.is_empty() && !engine_model.is_empty() && *m != engine_model)
                .map(|m| {
                    let label = if m == "soil" { "SOIL" } else { "WEEKLY" };
                    view! { <span class="zone-card__model">{label}</span> }
                });

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
                        {model_chip}
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
                    {budget_note}
                    {ceiling_note}
                    {suppression_note}

                    <div class="zone-detail__stats">
                        <StatTile label="Planned" value=planned unit="min" icon="droplet"/>
                        <StatTile label="Today" value=today unit=today_unit icon="history" accent="var(--accent-good)".to_string()/>
                        <StatTile label="Deficit" value=deficit unit=deficit_unit icon="gauge" accent="var(--accent-cool)".to_string()/>
                        <StatTile label="Last run" value=last_run icon="calendar" accent="var(--accent-warm)".to_string()/>
                    </div>

                    {soil_preview}

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

                    {math.map(|m| view! { <ZoneMathPanel m model=model.clone() prefs=p/> })}
                </div>
            }
            .into_any()
        }
    }
    }
}

/// The soil-model block's sentences: the status line and the optional
/// next-watering estimate. Pure so the copy is pinned by tests.
/// `etc_today_mm` is today's crop water use (published ET0 times this
/// zone's Kc); when absent or non-positive the estimate is omitted
/// rather than fabricated. Wire reason strings carry their own prefixes
/// ("deferred: ", "waits for tomorrow: "), stripped here so the sentence
/// does not stack two lead-ins. `session_capped` and `ceiling_binding`
/// name the clamp that shorted a nonzero run, so a partial delivery is
/// never described as a full refill. `held_reason` is Some when the
/// skip ladder holds this governed zone today (the verdict reason,
/// possibly empty): the panel must not promise "Waters about N min
/// today" beside a SKIPPING pill. `dial_scaled` says the caller's
/// seconds are the post-seasonal-dial dispatch figure and the dial
/// changed them, so the sentence names the dial instead of stating a
/// number the soil arithmetic alone does not explain.
#[allow(clippy::too_many_arguments)]
pub(crate) fn soil_preview_lines(
    governs: bool,
    planned_seconds: u32,
    due: bool,
    deferred_reason: Option<&str>,
    ceiling_binding: bool,
    session_capped: bool,
    held_reason: Option<&str>,
    depletion_mm: f64,
    raw_mm: f64,
    etc_today_mm: Option<f64>,
    dial_scaled: bool,
    prefs: UnitPrefs,
) -> (String, Option<String>) {
    let dep = depth_phrase_mm(depletion_mm, prefs);
    // Name the seasonal dial when it changed the minutes the sentence
    // states, so the figure cannot silently disagree with the soil
    // arithmetic the panel describes.
    let with_dial = |s: String| -> String {
        if dial_scaled {
            format!(
                "{}; the seasonal adjustment scaled the minutes.",
                s.trim_end_matches('.')
            )
        } else {
            s
        }
    };
    if planned_seconds > 0 {
        let mins = (planned_seconds + 30) / 60;
        if governs {
            if let Some(hold) = held_reason {
                // The ladder holds this zone today (wind, freeze, rain
                // now, saturation): reflect the effective verdict, the
                // same skip awareness the Planned tile carries.
                let lead = if hold.is_empty() {
                    "Holds today.".to_string()
                } else {
                    format!("Holds today: {hold}.")
                };
                return (
                    with_dial(format!(
                        "{lead} Waters about {mins} min once the hold clears, refilling \
                         the {dep} deficit."
                    )),
                    None,
                );
            }
        }
        let verb = if governs { "Waters" } else { "Would water" };
        if ceiling_binding {
            return (
                with_dial(format!(
                    "{verb} about {mins} min today toward the {dep} deficit, held to the \
                     weekly target."
                )),
                None,
            );
        }
        if session_capped {
            return (
                with_dial(format!(
                    "{verb} about {mins} min today toward the {dep} deficit, shorted by \
                     the run cap; the rest carries to tomorrow."
                )),
                None,
            );
        }
        return (
            with_dial(format!(
                "{verb} about {mins} min today, refilling the {dep} deficit."
            )),
            None,
        );
    }
    if due {
        let verb = if governs { "Holds" } else { "Would hold" };
        // The rare due-and-zero shape with no wire reason and no ceiling
        // degrades to the bare verb rather than the tautology "the plan
        // holds today" explains nothing with.
        let Some(reason) = deferred_reason
            .map(|r| {
                let r = r.strip_prefix("deferred: ").unwrap_or(r);
                r.strip_prefix("waits for tomorrow: ")
                    .unwrap_or(r)
                    .to_string()
            })
            .or_else(|| {
                ceiling_binding
                    .then(|| "the weekly target leaves no headroom this week".to_string())
            })
        else {
            return (format!("{verb} today."), None);
        };
        return (format!("{verb} today: {reason}."), None);
    }
    let raw = depth_phrase_mm(raw_mm, prefs);
    let verb = if governs { "Holds" } else { "Would hold" };
    let status = format!("{verb} today; the {dep} deficit is under the {raw} trigger.");
    let next = etc_today_mm.filter(|e| *e > 0.0).map(|etc| {
        let days = ((raw_mm - depletion_mm).max(0.0) / etc).ceil().max(1.0) as i64;
        let verb = if governs { "Waters" } else { "Would water" };
        if days == 1 {
            format!("{verb} next in about 1 day at today's drying rate.")
        } else {
            format!("{verb} next in about {days} days at today's drying rate.")
        }
    });
    (status, next)
}

/// The panel is split because only some of its numbers reach the dispatch.
/// Under the weekly model, throughput divides the session depth into
/// seconds and the ceiling can shorten the result; the deficit, Kc, heat
/// multiplier and capture efficiency feed ETc and the soil projection,
/// not the run length. Under the soil model the deficit and the capture
/// efficiency ARE the run length's inputs (gross = deficit / capture /
/// throughput), so those two rows move above the line and the note says
/// which arithmetic produced the minutes.
#[component]
fn ZoneMathPanel(m: ZoneMath, model: String, prefs: UnitPrefs) -> impl IntoView {
    // A zone with no run planned has nothing to compare against its ceiling,
    // so the row states the zero and stops. The card says why the zone is at
    // zero; repeating a cap here described a run that does not exist.
    let cap = if m.scheduled_seconds == 0 {
        String::new()
    } else if m.cap_binding {
        format!(" (capped at {} min)", m.max_duration_seconds / 60)
    } else {
        format!(" (under {} min cap)", m.max_duration_seconds / 60)
    };
    let final_class = if m.cap_binding {
        "zone-detail__math-final zone-detail__math-final--capped"
    } else {
        "zone-detail__math-final"
    };
    // Bucket deficit (mm source) and throughput (mm/hr source) convert at
    // the display boundary; the engine math itself stays in mm. The
    // deficit routes through the shared formatter (magnitude; the row
    // label carries the direction) so this row and the Deficit tile above
    // it print the same number. A dash marks the zones no bucket can be
    // derived for (env-var zones), never a fabricated zero.
    let deficit = match m.bucket_mm {
        Some(v) => deficit_amount_mm(v, prefs),
        None => "-".to_string(),
    };
    let throughput = fmt_rain_rate_mm(m.throughput_mm_hr, prefs);
    // The soil-arithmetic layout and note require the bucket to actually
    // exist: on the evidence-starved cold-start window the model tag
    // says "soil" while the weekly allocator sized the minutes, and
    // describing a deficit division that never ran beside a dashed
    // Soil-deficit row was a lie on the first screens every adopter
    // reads. The starved state gets the weekly layout plus a note that
    // says which arithmetic ran and why.
    let soil_governed = model == "soil" && m.bucket_mm.is_some();
    let soil_starved = model == "soil" && m.bucket_mm.is_none();
    let note = if soil_governed {
        "The minutes above refill the soil deficit: the deficit divided by the capture \
         efficiency and the throughput, then the seasonal adjustment and any condition \
         rule's multiplier apply, held to the zone's cap. A forced run waters a bounded \
         default even when the plan is zero. The crop coefficient and heat multiplier \
         feed the ETc figure and the soil projection."
    } else if soil_starved {
        "The soil model governs this zone and is still gathering evidence, so the weekly \
         plan sized these minutes: the weekly target divided by the throughput, then the \
         seasonal adjustment and any condition rule's multiplier, held to the zone's cap. \
         A forced run waters a bounded default even when the target is zero. The soil \
         arithmetic takes over once a few measured days of water use, rain, or completed \
         runs land."
    } else {
        "The soil deficit comes from the soil model's replay of measured water use, rain \
         and completed runs; it is shown for reference while the weekly plan governs this \
         zone. The minutes above start from the weekly target divided by the throughput, \
         then take the seasonal adjustment and any condition rule's multiplier, and are \
         held to the zone's cap. A forced run waters a bounded default even when the \
         target is zero. The crop coefficient, heat multiplier and capture efficiency \
         feed the ETc figure and the soil projection."
    };
    view! {
        <section class="zone-detail__panel">
            <h2 class="zone-detail__panel-title">"Why this duration?"</h2>
            <dl class="zone-detail__math">
                <div><dt>"Throughput"</dt><dd>{throughput}</dd></div>
                {soil_governed.then(|| view! {
                    <div><dt>"Soil deficit"</dt><dd>{deficit.clone()}</dd></div>
                    <div><dt>"Capture efficiency"</dt><dd>{format!("{:.2}", m.capture_eff)}</dd></div>
                })}
                <div class=final_class><dt>"Scheduled"</dt><dd>{format!("{} min{cap}", m.scheduled_seconds / 60)}</dd></div>
            </dl>
            <h3 class="zone-detail__panel-subtitle">"Not part of this morning's minutes"</h3>
            <p class="zone-detail__panel-note">{note}</p>
            <dl class="zone-detail__math">
                {(!soil_governed).then(|| view! {
                    <div><dt>"Soil deficit"</dt><dd>{deficit.clone()}</dd></div>
                })}
                <div><dt>"Crop coefficient"</dt><dd>{format!("{:.2}", m.kc)}</dd></div>
                <div><dt>"Heat multiplier"</dt><dd>{format!("{:.2}", m.heat_mult)}</dd></div>
                {(!soil_governed).then(|| view! {
                    <div><dt>"Capture efficiency"</dt><dd>{format!("{:.2}", m.capture_eff)}</dd></div>
                })}
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

    /// The soil block's copy is user-facing; pin each shape. Imperial
    /// prefs (the default) render depths as inches with the flush glyph.
    #[test]
    fn soil_preview_planned_run_states_the_refill() {
        let p = UnitPrefs::default();
        let (status, next) = soil_preview_lines(
            false, 1560, true, None, false, false, None, 5.2, 4.5, None, false, p,
        );
        assert_eq!(
            status,
            "Would water about 26 min today, refilling the 0.20\" deficit."
        );
        assert!(next.is_none());
        // The governed variant states the action, not the hypothesis.
        let (status, _) = soil_preview_lines(
            true, 1560, true, None, false, false, None, 5.2, 4.5, None, false, p,
        );
        assert_eq!(
            status,
            "Waters about 26 min today, refilling the 0.20\" deficit."
        );
    }

    /// A holding zone gets the trigger framing plus the drying-rate
    /// estimate; the estimate is omitted when no ETc resolved, never
    /// fabricated.
    #[test]
    fn soil_preview_holding_zone_estimates_next_watering() {
        let p = UnitPrefs::default();
        let (status, next) = soil_preview_lines(
            true,
            0,
            false,
            None,
            false,
            false,
            None,
            2.0,
            6.0,
            Some(2.0),
            false,
            p,
        );
        assert_eq!(
            status,
            "Holds today; the 0.08\" deficit is under the 0.24\" trigger."
        );
        assert_eq!(
            next.as_deref(),
            Some("Waters next in about 2 days at today's drying rate.")
        );
        let (_, next) = soil_preview_lines(
            false, 0, false, None, false, false, None, 2.0, 6.0, None, false, p,
        );
        assert!(next.is_none(), "no ETc, no estimate");
        // Metric prefs carry the unit with a space.
        let metric = UnitPrefs {
            rain_mm: true,
            ..Default::default()
        };
        let (status, _) = soil_preview_lines(
            false, 0, false, None, false, false, None, 2.0, 6.0, None, false, metric,
        );
        assert_eq!(
            status,
            "Would hold today; the 2.0 mm deficit is under the 6.0 mm trigger."
        );
    }

    /// Wire reasons carry their own lead-ins; the sentence strips them so
    /// the block never reads "hold today: deferred: ...".
    #[test]
    fn soil_preview_strips_wire_reason_prefixes() {
        let p = UnitPrefs::default();
        let (status, _) = soil_preview_lines(
            false,
            0,
            true,
            Some("deferred: forecast rain refills the deficit (3.2 of 5.1 mm expected)"),
            false,
            false,
            None,
            5.2,
            4.5,
            None,
            false,
            p,
        );
        assert_eq!(
            status,
            "Would hold today: forecast rain refills the deficit (3.2 of 5.1 mm expected)."
        );
        let (status, _) = soil_preview_lines(
            true,
            0,
            true,
            Some(
                "waits for tomorrow: the morning window fits 1 of 3 zones that need water, \
                 most depleted first",
            ),
            false,
            false,
            None,
            5.2,
            4.5,
            None,
            false,
            p,
        );
        assert_eq!(
            status,
            "Holds today: the morning window fits 1 of 3 zones that need water, most \
             depleted first."
        );
        // Due, zero seconds, no wire reason: the weekly ceiling at zero
        // headroom is the one remaining shape.
        let (status, _) = soil_preview_lines(
            false, 0, true, None, true, false, None, 5.2, 4.5, None, false, p,
        );
        assert_eq!(
            status,
            "Would hold today: the weekly target leaves no headroom this week."
        );
    }

    /// A partial clamp is never described as a full refill: the ceiling
    /// branch names the ceiling, the run-cap branch names the carry, and
    /// only an unclamped run claims to refill the deficit. The first
    /// shape mirrors the demo side_yard state (nonzero seconds with the
    /// ceiling binding), which used to render the plain refill sentence.
    #[test]
    fn soil_preview_partial_clamps_name_the_clamp() {
        let p = UnitPrefs::default();
        let (status, next) = soil_preview_lines(
            true, 1560, true, None, true, false, None, 5.2, 4.5, None, false, p,
        );
        assert_eq!(
            status,
            "Waters about 26 min today toward the 0.20\" deficit, held to the weekly \
             target."
        );
        assert!(next.is_none());
        // Run-cap short without the ceiling: the carry is named.
        let (status, _) = soil_preview_lines(
            true, 3600, true, None, false, true, None, 9.0, 4.5, None, false, p,
        );
        assert_eq!(
            status,
            "Waters about 60 min today toward the 0.35\" deficit, shorted by the run \
             cap; the rest carries to tomorrow."
        );
        // The weekly-governed preview keeps the conditional verb.
        let (status, _) = soil_preview_lines(
            false, 1560, true, None, true, false, None, 5.2, 4.5, None, false, p,
        );
        assert_eq!(
            status,
            "Would water about 26 min today toward the 0.20\" deficit, held to the \
             weekly target."
        );
    }

    /// The governed panel states the post-seasonal-dial minutes (what
    /// dispatches), and names the dial when it changed the figure, so
    /// the panel and the Planned tile can never disagree by the dial's
    /// percentage.
    #[test]
    fn soil_preview_names_the_dial_when_it_scaled_the_minutes() {
        let p = UnitPrefs::default();
        // 1560s pre-dial at 80% -> the caller passes the 1248s dispatch
        // figure with the dial flag set.
        let (status, _) = soil_preview_lines(
            true, 1248, true, None, false, false, None, 5.2, 4.5, None, true, p,
        );
        assert_eq!(
            status,
            "Waters about 21 min today, refilling the 0.20\" deficit; the seasonal \
             adjustment scaled the minutes."
        );
        // The clamped shapes carry the same clause.
        let (status, _) = soil_preview_lines(
            true, 1248, true, None, true, false, None, 5.2, 4.5, None, true, p,
        );
        assert_eq!(
            status,
            "Waters about 21 min today toward the 0.20\" deficit, held to the weekly \
             target; the seasonal adjustment scaled the minutes."
        );
    }

    /// The rare due-and-zero shape with no wire reason and no ceiling
    /// degrades to the bare verb instead of the old tautology ("Holds
    /// today: the plan holds today.").
    #[test]
    fn soil_preview_degenerate_hold_is_the_bare_verb() {
        let p = UnitPrefs::default();
        let (status, _) = soil_preview_lines(
            true, 0, true, None, false, false, None, 5.2, 4.5, None, false, p,
        );
        assert_eq!(status, "Holds today.");
        let (status, _) = soil_preview_lines(
            false, 0, true, None, false, false, None, 5.2, 4.5, None, false, p,
        );
        assert_eq!(status, "Would hold today.");
    }

    /// The ladder's hold outranks the refill promise: a governed zone
    /// with planned seconds and a skip verdict says it holds today and
    /// what happens once the hold clears, matching the SKIPPING pill on
    /// the same page.
    #[test]
    fn soil_preview_reflects_a_ladder_hold() {
        let p = UnitPrefs::default();
        let (status, next) = soil_preview_lines(
            true,
            1560,
            true,
            None,
            false,
            false,
            Some("Wind 28 mph now"),
            5.2,
            4.5,
            None,
            false,
            p,
        );
        assert_eq!(
            status,
            "Holds today: Wind 28 mph now. Waters about 26 min once the hold clears, \
             refilling the 0.20\" deficit."
        );
        assert!(next.is_none());
        // An empty verdict reason degrades to the bare hold sentence.
        let (status, _) = soil_preview_lines(
            true,
            1560,
            true,
            None,
            false,
            false,
            Some(""),
            5.2,
            4.5,
            None,
            false,
            p,
        );
        assert_eq!(
            status,
            "Holds today. Waters about 26 min once the hold clears, refilling the \
             0.20\" deficit."
        );
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
