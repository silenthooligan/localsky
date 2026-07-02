// ZoneCard, a rich, scannable card per zone for the Zones master-detail.
// Clicking the card selects it (the detail slides into the right pane, no
// navigation). Shows live status (color-coded), the key numbers, an
// optional zone photo, and an inline Stop when running.

use leptos::prelude::*;
use serde_json::json;

use crate::components::irrigation::controls::{post_action_then, OverrideControl};
use crate::components::ui::{use_toast, Button, Icon};
use crate::components::units_fmt::{depth_unit, depth_value_mm, use_unit_prefs};
use crate::ha::snapshot::ZoneState;

/// (status key, label, color token) for a zone's live state. A zone the engine
/// will SKIP must never read "TONIGHT" even if it carries a leftover planned
/// duration: the skip verdict is the truth, so it reads "SKIPPING" (blue) and
/// the planned minutes below are suppressed. Running and idle are unaffected.
pub fn zone_status(z: &ZoneState) -> (&'static str, &'static str, &'static str) {
    let skipping = z.verdict.as_ref().is_some_and(|v| v.verdict == "skip");
    if z.running {
        ("running", "RUNNING", "var(--verdict-run)")
    } else if skipping {
        ("skipping", "SKIPPING", "var(--verdict-skip)")
    } else if z.planned_run_seconds > 0 {
        ("scheduled", "TONIGHT", "var(--accent)")
    } else {
        ("idle", "IDLE", "var(--verdict-off)")
    }
}

#[component]
pub fn ZoneCard(
    zone: ZoneState,
    selected: RwSignal<Option<String>>,
    /// Live soil moisture % from the zone's assigned probe (joined from
    /// the snapshot's soil_forecasts by the caller). None = no probe.
    #[prop(optional_no_strip)]
    soil_pct: Option<f64>,
) -> impl IntoView {
    let (status, label, color) = zone_status(&zone);
    // Per-device display-unit preference; read prefs.get() in render
    // closures so a units change (or post-hydration localStorage load)
    // re-renders the convertible values.
    let prefs = use_unit_prefs();
    let name = zone.name.clone();
    let slug = zone.slug.clone();
    let slug_sel = slug.clone();
    let slug_active = slug.clone();
    let is_active = move || selected.get().as_deref() == Some(slug_active.as_str());
    // A zone the engine will SKIP waters 0 minutes tonight, regardless of any
    // leftover planned duration on the snapshot: show "0" so the "Tonight" stat
    // matches the SKIPPING status and the zone's verdict (T4), never a planned
    // figure the zone will not actually run.
    let zone_skipping = zone.verdict.as_ref().is_some_and(|v| v.verdict == "skip");
    let planned = if zone_skipping {
        "0".to_string()
    } else {
        ((zone.planned_run_seconds + 30) / 60).to_string()
    };
    let today = format!("{:.0}", zone.today_run_minutes);
    // Deficit is a soil-water DEPTH stored in millimeters; convert at the
    // display boundary (helpers respect units_rain). Engine math + wire
    // format stay mm.
    let deficit_mm = zone.bucket_mm;
    let running = zone.running;
    let stop_slug = slug.clone();
    // Sticky per-zone override (Auto/Skip/Force). Card is re-created per
    // snapshot, so this value is current; the control POSTs set_zone_override.
    let ov_mode = zone.override_mode.clone();
    let ov_slug = slug.clone();
    // Disabled-after-click guard; the next streamed snapshot recreates the
    // card with the real state, so this only needs to cover the gap.
    let stopping = RwSignal::new(false);
    let on_stop = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        if stopping.get_untracked() {
            return;
        }
        stopping.set(true);
        post_action_then(
            json!({ "kind": "stop", "zone": stop_slug.clone() }),
            Callback::new(move |result: Result<(), String>| {
                if let Err(e) = result {
                    stopping.set(false);
                    use_toast().error(format!("Stop failed: {e}"));
                }
            }),
        );
    };
    let photo = zone.photo_url.clone().filter(|p| !p.is_empty());

    // Per-zone verdict (from decide_per_zone): a colored pill + reason so a
    // zone skipping on its own soil reading is visible at a glance.
    let verdict = zone.verdict.clone();
    let verdict_pill = verdict.as_ref().map(|v| {
        let vc = crate::components::verdict::verdict_token(&v.verdict);
        let vl = crate::components::verdict::verdict_label(&v.verdict);
        view! {
            <span class="zone-card__verdict" style=format!("--vc:{vc}")>{vl}</span>
        }
    });
    let verdict_reason = verdict
        .as_ref()
        // Show the reason on skips and on a soil-floor run (P1-2), so the green
        // WATER pill explains "soil below minimum; forecast-rain skip overridden".
        .filter(|v| (v.verdict == "skip" || v.source == "soil_floor") && !v.reason.is_empty())
        .cloned()
        .map(|v| {
            // P2 units architecture: render the reason unit-aware from the
            // structured ZoneVerdict (soil reasons are percent / unit-invariant;
            // global-bound reasons fall back to the baked string). Read prefs.get()
            // inside the closure so a units toggle re-renders.
            view! {
                <div class="zone-card__reason">
                    {move || crate::reason_render::render_zone_reason(&v, prefs.get())}
                </div>
            }
        });

    let select_label = format!("Open {} details", zone.name);
    // Selection pattern is form-factor aware: on desktop/tablet the card
    // drives the side-by-side detail pane; on phones (where that pane
    // would land below the fold) the tap pushes the standalone
    // /zones/:slug page instead, the list -> detail flow phones expect.
    let is_mobile = use_context::<RwSignal<bool>>();
    let nav_slug = slug.clone();
    let navigate = leptos_router::hooks::use_navigate();
    let on_select = move |_| {
        let mobile = is_mobile.map(|s| s.get_untracked()).unwrap_or(false);
        if mobile {
            navigate(
                &crate::base::url(&format!("/zones/{nav_slug}")),
                leptos_router::NavigateOptions::default(),
            );
        } else {
            selected.set(Some(slug_sel.clone()));
        }
    };
    view! {
        <div
            class=format!("zone-card zone-card--{status}")
            class:is-selected=is_active
            style=format!("--zc:{color}")
        >
            // A real <button> overlay carries the select action (keyboard +
            // AT correct); the inline Stop sits above it via z-index, so no
            // nested-interactive markup.
            <button
                type="button"
                class="zone-card__hit"
                aria-label=select_label
                on:click=on_select
            ></button>
            {photo.map(|src| view! {
                <div class="zone-card__photo" style=format!("background-image:url('{src}')")></div>
            })}
            <div class="zone-card__body">
                <div class="zone-card__head">
                    <span class="zone-card__dot"></span>
                    <span class="zone-card__name">{name}</span>
                    <span class="zone-card__pill">{label}</span>
                    {verdict_pill}
                </div>
                {verdict_reason}
                <div class="zone-card__stats">
                    <div class="zone-card__stat">
                        <span class="zone-card__k">"Tonight"</span>
                        <span class="zone-card__v">{planned}<small>" min"</small></span>
                    </div>
                    <div class="zone-card__stat">
                        <span class="zone-card__k">"Today"</span>
                        <span class="zone-card__v">{today}<small>" min"</small></span>
                    </div>
                    <div class="zone-card__stat">
                        <span class="zone-card__k">"Deficit"</span>
                        <span class="zone-card__v">
                            {move || depth_value_mm(deficit_mm, prefs.get())}
                            <small>{move || format!(" {}", depth_unit(prefs.get()))}</small>
                        </span>
                    </div>
                    {soil_pct.map(|pct| view! {
                        <div class="zone-card__stat zone-card__stat--soil">
                            <span class="zone-card__k">"Soil"</span>
                            <span class="zone-card__v">{format!("{pct:.0}")}<small>"%"</small></span>
                        </div>
                    })}
                </div>
                // Per-zone override. stop_propagation so tapping a segment sets
                // the override instead of selecting/opening the zone.
                <div
                    class="zone-card__override"
                    on:click=move |ev: leptos::ev::MouseEvent| ev.stop_propagation()
                >
                    <span class="zone-card__override-label">"Override"</span>
                    <OverrideControl current=Signal::derive(move || ov_mode.clone()) zone=ov_slug/>
                </div>
                {running.then(|| view! {
                    <div class="zone-card__foot">
                        <Button
                            variant="danger"
                            class="zone-card__stop"
                            disabled=Signal::derive(move || stopping.get())
                            on_click=Callback::new(on_stop)
                        >
                            <Icon name="stop" size=14/>
                            {move || if stopping.get() { "Stopping…" } else { "Stop" }}
                        </Button>
                    </div>
                })}
            </div>
        </div>
    }
}
