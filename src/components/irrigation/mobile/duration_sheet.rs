// Custom-duration bottom sheet. Slides up from the bottom on mobile, lets
// the user pick a run duration in minutes, then fires the run action.
//
// UI:
//   - Preset chips:    5  10  15  30  45  60  90  120
//   - Stepper:         [-5]  <minutes>  [+5]
//   - Slider:          0 ─────●──────── 120
//   - Confirm button:  "Run for X minutes"
//   - Backdrop tap or Cancel: closes without firing.
//
// State: a single RwSignal<u32> for minutes, owned by this component. Caller
// passes in a `visible: RwSignal<bool>` and a `zone_slug: Signal<Option<String>>`
// so multiple zone rows / detail pages can share one sheet instance.
//
// Server clamp: actions cap at 120 minutes (Phase 4 will enforce server-side).

use crate::components::irrigation::controls::post_action;
use leptos::prelude::*;
use serde_json::json;

#[component]
pub fn DurationSheet(
    visible: RwSignal<bool>,
    zone_slug: RwSignal<Option<String>>,
    zone_label: RwSignal<String>,
) -> impl IntoView {
    let minutes: RwSignal<u32> = RwSignal::new(15);

    let close = move |_| visible.set(false);

    let confirm = move |_| {
        let m = minutes.get().min(120).max(1);
        if let Some(slug) = zone_slug.get() {
            post_action(json!({"kind": "run", "zone": slug, "seconds": (m as u64) * 60}));
        }
        visible.set(false);
    };

    let bump = move |delta: i32| {
        let cur = minutes.get() as i32;
        let next = (cur + delta).clamp(1, 120) as u32;
        minutes.set(next);
    };
    let on_minus = move |_| bump(-5);
    let on_plus = move |_| bump(5);

    let on_slider = move |ev: leptos::ev::Event| {
        // <input type=range> emits Event, not InputEvent. Read from target.
        if let Some(target) = event_target_value_u32(&ev) {
            minutes.set(target.clamp(1, 120));
        }
    };

    let preset = move |m: u32| {
        let cls = move || {
            if minutes.get() == m { "duration-chip is-on" } else { "duration-chip" }
        };
        view! {
            <button class=cls on:click=move |_| minutes.set(m)>{m}" m"</button>
        }
    };

    move || {
        if !visible.get() {
            return ().into_any();
        }
        let label = zone_label.get();
        view! {
            <div class="bottom-sheet-backdrop" on:click=close aria-hidden="true"></div>
            <div class="bottom-sheet" role="dialog" aria-modal="true" aria-label="Pick run duration">
                <div class="bottom-sheet-handle" aria-hidden="true"></div>
                <div class="bottom-sheet-title">"Run "{label}</div>

                <div class="duration-presets">
                    {preset(5)}
                    {preset(10)}
                    {preset(15)}
                    {preset(30)}
                    {preset(45)}
                    {preset(60)}
                    {preset(90)}
                    {preset(120)}
                </div>

                <div class="duration-stepper">
                    <button class="btn-clay duration-step" on:click=on_minus aria-label="Decrease 5 minutes">"–5"</button>
                    <div class="duration-readout">
                        <span class="duration-readout-num">{move || minutes.get()}</span>
                        <span class="duration-readout-unit">"min"</span>
                    </div>
                    <button class="btn-clay duration-step" on:click=on_plus aria-label="Increase 5 minutes">"+5"</button>
                </div>

                <input
                    class="duration-slider"
                    type="range"
                    min="1"
                    max="120"
                    step="1"
                    prop:value=move || minutes.get().to_string()
                    on:input=on_slider
                    aria-label="Duration minutes"
                />

                <div class="bottom-sheet-actions">
                    <button class="btn-clay" on:click=close>"Cancel"</button>
                    <button class="btn-clay btn-clay-good" on:click=confirm>
                        "Run for "{move || minutes.get()}" min"
                    </button>
                </div>
            </div>
        }
        .into_any()
    }
}

#[cfg(feature = "hydrate")]
fn event_target_value_u32(ev: &leptos::ev::Event) -> Option<u32> {
    use wasm_bindgen::JsCast;
    let target = ev.target()?;
    let input: web_sys::HtmlInputElement = target.dyn_into().ok()?;
    input.value().parse::<u32>().ok()
}

#[cfg(not(feature = "hydrate"))]
fn event_target_value_u32(_ev: &leptos::ev::Event) -> Option<u32> {
    None
}
