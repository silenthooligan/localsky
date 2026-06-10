// ZoneCard — a rich, scannable card per zone for the Zones master-detail.
// Clicking the card selects it (the detail slides into the right pane, no
// navigation). Shows live status (color-coded), the key numbers, an
// optional zone photo, and an inline Stop when running.

use leptos::prelude::*;
use serde_json::json;

use crate::components::irrigation::controls::post_action_then;
use crate::components::ui::{use_toast, Icon};
use crate::ha::snapshot::ZoneState;

/// (status key, label, color token) for a zone's live state.
pub fn zone_status(z: &ZoneState) -> (&'static str, &'static str, &'static str) {
    if z.running {
        ("running", "RUNNING", "var(--verdict-run)")
    } else if z.planned_run_seconds > 0 {
        ("scheduled", "TONIGHT", "var(--accent)")
    } else {
        ("idle", "IDLE", "var(--verdict-off)")
    }
}

#[component]
pub fn ZoneCard(zone: ZoneState, selected: RwSignal<Option<String>>) -> impl IntoView {
    let (status, label, color) = zone_status(&zone);
    let name = zone.name.clone();
    let slug = zone.slug.clone();
    let slug_sel = slug.clone();
    let slug_active = slug.clone();
    let is_active = move || selected.get().as_deref() == Some(slug_active.as_str());
    let planned = ((zone.planned_run_seconds + 30) / 60).to_string();
    let today = format!("{:.0}", zone.today_run_minutes);
    let deficit = format!("{:.1}", zone.bucket_mm);
    let running = zone.running;
    let stop_slug = slug.clone();
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
        .filter(|v| v.verdict == "skip" && !v.reason.is_empty())
        .map(|v| {
            let r = v.reason.clone();
            view! { <div class="zone-card__reason">{r}</div> }
        });

    let select_label = format!("Open {} details", zone.name);
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
                on:click=move |_| selected.set(Some(slug_sel.clone()))
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
                        <span class="zone-card__v">{deficit}<small>" mm"</small></span>
                    </div>
                </div>
                {running.then(|| view! {
                    <div class="zone-card__foot">
                        <button
                            type="button"
                            class="zone-card__stop"
                            prop:disabled=move || stopping.get()
                            on:click=on_stop
                        >
                            <Icon name="stop" size=14/>
                            {move || if stopping.get() { "Stopping…" } else { "Stop" }}
                        </button>
                    </div>
                })}
            </div>
        </div>
    }
}
