// Interactive control surfaces for the irrigation page. All buttons,
// sliders, toggles, and number inputs POST to /api/irrigation/action
// with a {"kind":"...", ...} JSON body. The Axum handler turns that
// into an HA service call.
//
// Each editable control owns a local signal initialised from the
// first SSR snapshot. The display reads from local, NOT from snap,
// so dragging a slider or typing in a number input feels instant.
// On commit (slider release / input blur / toggle click), local is
// already the source of truth and the action POST tells HA to catch
// up. The 10s refresher cycle's eventual snap arrival is a no-op
// (same value). External-to-the-dashboard HA changes won't reflect
// until page refresh — acceptable for thresholds set once a season.

use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;
use serde_json::json;

/// Big "Stop All Zones" panel. Hot-red claymorphic surface so it's
/// unmistakable. Desktop: single-tap. Mobile (is_mobile context = true):
/// opens a confirm bottom sheet so a stray tap on a tiny screen doesn't
/// kill an in-progress watering by accident.
#[component]
pub fn StopAllPanel(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    use crate::components::irrigation::mobile::stop_confirm::StopAllConfirm;

    let is_mobile = use_context::<RwSignal<bool>>();
    let any_running = move || snap.get().zones.iter().any(|z| z.running);
    let running_count = Signal::derive(move || snap.get().zones.iter().filter(|z| z.running).count());

    let confirm_open: RwSignal<bool> = RwSignal::new(false);

    let on_click = move |_| {
        if !any_running() {
            return;
        }
        if is_mobile.map(|s| s.get()).unwrap_or(false) {
            confirm_open.set(true);
        } else {
            post_action(json!({ "kind": "stop_all" }));
        }
    };

    view! {
        <section class="stop-all">
            <h3 class="stop-all-title">"Emergency Stop"</h3>
            <p class="stop-all-help">
                {move || if any_running() {
                    "One or more zones are running. Tap to stop every active station immediately.".to_string()
                } else {
                    "No zones are running.".to_string()
                }}
            </p>
            <button
                class="btn-clay btn-clay-hot stop-all-btn"
                on:click=on_click
                disabled=move || !any_running()
            >
                "STOP ALL ZONES"
            </button>
            <StopAllConfirm visible=confirm_open running_count/>
        </section>
    }
}

/// Skip-threshold tuners + vacation/dry-run toggles. Each control
/// follows the server-side value (via snap) until the user first
/// interacts with it; from that point on, local is authoritative.
/// Page refresh re-arms the follow.
#[component]
pub fn ThresholdsPanel(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <section class="thresholds">
            <h3 class="thresholds-title">"Skip thresholds"</h3>

            {view! {
                <ThresholdControl
                    label="Max wind"
                    key="max_wind_mph"
                    unit="mph"
                    min=0.0
                    max=30.0
                    step=1.0
                    decimals=0
                    current=Signal::derive(move || snap.get().skip_check.max_wind_mph)
                />
            }}
            {view! {
                <ThresholdControl
                    label="Min temp"
                    key="min_temp_f"
                    unit="°F"
                    min=20.0
                    max=60.0
                    step=1.0
                    decimals=0
                    current=Signal::derive(move || snap.get().skip_check.min_temp_f)
                />
            }}
            {view! {
                <ThresholdControl
                    label="Rain skip"
                    key="rain_skip_in"
                    unit="in"
                    min=0.0
                    max=1.0
                    step=0.05
                    decimals=2
                    current=Signal::derive(move || snap.get().skip_check.rain_skip_in)
                />
            }}

            <div class="toggle-row">
                <ToggleControl
                    key="irrigation_pause"
                    label="Vacation pause"
                    current=Signal::derive(move || snap.get().skip_check.is_paused)
                />
                <ToggleControl
                    key="irrigation_dry_run"
                    label="Dry-run"
                    current=Signal::derive(move || snap.get().skip_check.is_dry_run)
                />
            </div>
        </section>
    }
}

/// One threshold row. Local signal mirrors `current` (the snap-
/// derived server value) until the user first interacts; after
/// that, local is authoritative and the snap-driven Effect early-
/// exits. This handles the SSR-vs-hydrate gap where the WASM
/// client's first read of snap returns `IrrigationSnapshot::default()`
/// (all zeros) before SSE has populated the real values.
#[component]
fn ThresholdControl(
    label: &'static str,
    key: &'static str,
    unit: &'static str,
    min: f64,
    max: f64,
    step: f64,
    decimals: usize,
    current: Signal<f64>,
) -> impl IntoView {
    let (val, set_val) = signal(current.get_untracked());
    let user_touched = RwSignal::new(false);

    // Follow the server value until the user first touches the control.
    Effect::new(move |_| {
        let server = current.get();
        if !user_touched.get_untracked() {
            set_val.set(server);
        }
    });

    let fmt_value = move |v: f64| match decimals {
        0 => format!("{:.0}", v),
        2 => format!("{:.2}", v),
        _ => format!("{}", v),
    };

    let commit = move |v: f64| {
        let clamped = v.clamp(min, max);
        user_touched.set(true);
        set_val.set(clamped);
        post_action(json!({
            "kind": "set_threshold",
            "key": key,
            "value": clamped,
        }));
    };

    view! {
        <div class="threshold-row">
            <label class="threshold-label">{label}</label>
            <div class="threshold-input-pair">
                <input
                    type="number"
                    class="num-clay"
                    min=min
                    max=max
                    step=step
                    inputmode="decimal"
                    prop:value=move || fmt_value(val.get())
                    on:change=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            commit(v);
                        }
                    }
                />
                <span class="threshold-unit">{unit}</span>
            </div>
            <input
                type="range"
                class="slider-clay"
                min=min
                max=max
                step=step
                prop:value=move || val.get().to_string()
                on:input=move |ev| {
                    if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                        user_touched.set(true);
                        set_val.set(v);
                    }
                }
                on:change=move |ev| {
                    if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                        commit(v);
                    }
                }
            />
        </div>
    }
}

#[component]
fn ToggleControl(
    key: &'static str,
    label: &'static str,
    current: Signal<bool>,
) -> impl IntoView {
    let (is_on, set_is_on) = signal(current.get_untracked());
    let user_touched = RwSignal::new(false);

    // Same follow-until-touched pattern as the threshold sliders so
    // the toggle reflects the real HA value once SSE arrives, then
    // stops fighting the user once clicked.
    Effect::new(move |_| {
        let server = current.get();
        if !user_touched.get_untracked() {
            set_is_on.set(server);
        }
    });

    let on_click = move |_| {
        let next = !is_on.get();
        user_touched.set(true);
        set_is_on.set(next);
        post_action(json!({"kind":"toggle","key":key,"on":next}));
    };
    view! {
        <div class="toggle-pair">
            <label class="toggle-label">{label}</label>
            <span
                role="button"
                tabindex="0"
                class=move || if is_on.get() { "toggle-clay is-on" } else { "toggle-clay" }
                on:click=on_click
            ></span>
        </div>
    }
}

/// Browser-side helper: POST a JSON body to /api/irrigation/action.
/// On SSR this is a no-op (the server doesn't fire actions at
/// itself); on the client it spawns a local task and ignores the
/// response (the next 10s refresher cycle reflects the new state).
#[cfg(feature = "hydrate")]
pub(super) fn post_action(body: serde_json::Value) {
    use leptos::task::spawn_local;
    spawn_local(async move {
        let payload = body.to_string();
        let _ = gloo_net::http::Request::post("/api/irrigation/action")
            .header("Content-Type", "application/json")
            .body(payload)
            .ok()
            .unwrap()
            .send()
            .await;
    });
}

#[cfg(not(feature = "hydrate"))]
#[allow(dead_code)]
pub(super) fn post_action(_body: serde_json::Value) {}

// `event_target_value` comes in from `leptos::prelude::*`. It's
// defined on both ssr and hydrate builds (SSR returns empty since the
// event closure never actually fires there).
