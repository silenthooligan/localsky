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
// until page refresh, acceptable for thresholds set once a season.

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
    let running_count =
        Signal::derive(move || snap.get().zones.iter().filter(|z| z.running).count());

    let confirm_open: RwSignal<bool> = RwSignal::new(false);

    let stop_done = toast_on_err("Stop all failed; zones may still be running");
    let on_click = move |_| {
        if !any_running() {
            return;
        }
        if is_mobile.map(|s| s.get()).unwrap_or(false) {
            confirm_open.set(true);
        } else {
            post_action_then(json!({ "kind": "stop_all" }), stop_done);
        }
    };

    view! {
        <section class="stop-all" class:stop-all--armed=any_running>
            <div class="stop-all__lead">
                <span class="stop-all__icon" aria-hidden="true">
                    <crate::components::ui::Icon name="stop" size=18 stroke=2.0/>
                </span>
                <div class="stop-all__text">
                    <h3 class="stop-all-title">"Emergency stop"</h3>
                    <p class="stop-all-help">
                        {move || {
                            let n = running_count.get();
                            match n {
                                0 => "All zones idle. Arms by itself the moment anything runs.".to_string(),
                                1 => "1 zone is running. Stops it instantly.".to_string(),
                                n => format!("{n} zones are running. Stops every active station instantly."),
                            }
                        }}
                    </p>
                </div>
            </div>
            <button
                class="stop-all-btn"
                on:click=on_click
                disabled=move || !any_running()
            >
                "STOP ALL ZONES"
            </button>
            <StopAllConfirm visible=confirm_open running_count/>
        </section>
    }
}

/// Build the /action body for an override choice. `zone = None` drives the
/// sticky global override; `Some(slug)` drives that one zone's override.
fn override_action(zone: &Option<String>, mode: &str) -> serde_json::Value {
    match zone {
        Some(slug) => json!({ "kind": "set_zone_override", "zone": slug, "mode": mode }),
        None => json!({ "kind": "set_global_override", "mode": mode }),
    }
}

/// Sticky override segmented control: Auto / Skip / Force. Drives the global
/// override (`zone = None`, rendered as a titled panel on the irrigation page)
/// or a single zone's override (`zone = Some(slug)`, rendered compact inside a
/// zone card). Sticky: the choice persists until changed. "Force" overrides
/// every skip condition for the next scheduled run; the schedule still decides
/// WHEN. A zone override beats the global one.
#[component]
pub fn OverrideControl(
    /// Current mode from the snapshot ("auto" | "skip" | "run"); the control
    /// follows it until the user first interacts (same pattern as the toggles).
    current: Signal<String>,
    /// None = global override; Some(slug) = a single zone's override.
    #[prop(optional, into)]
    zone: Option<String>,
) -> impl IntoView {
    // Normalize the empty default-snapshot value (pre-SSE hydrate frame) to
    // "auto" so a segment is always highlighted, never a blank control.
    let norm = |s: String| if s.is_empty() { "auto".to_string() } else { s };
    let (mode, set_mode) = signal(norm(current.get_untracked()));
    let user_touched = RwSignal::new(false);
    let compact = zone.is_some();

    // Follow the server value until first interaction, then stop fighting.
    Effect::new(move |_| {
        let server = current.get();
        if !user_touched.get_untracked() {
            set_mode.set(norm(server));
        }
    });

    let toast = crate::components::ui::use_toast();
    let save_done = Callback::new(move |result: Result<(), String>| {
        if let Err(e) = result {
            // Optimistic choice didn't stick: roll back + re-arm the follow.
            user_touched.set(false);
            set_mode.set(current.get_untracked());
            toast.error(format!("Couldn't set override: {e}"));
        }
    });

    let z_skip = zone.clone();
    let z_run = zone.clone();
    let z_auto = zone.clone();
    let choose_auto = move |_| {
        user_touched.set(true);
        set_mode.set("auto".to_string());
        post_action_then(override_action(&z_auto, "auto"), save_done);
    };
    let choose_skip = move |_| {
        user_touched.set(true);
        set_mode.set("skip".to_string());
        post_action_then(override_action(&z_skip, "skip"), save_done);
    };
    let choose_run = move |_| {
        user_touched.set(true);
        set_mode.set("run".to_string());
        post_action_then(override_action(&z_run, "run"), save_done);
    };

    let is = move |m: &'static str| mode.get() == m;
    let seg = view! {
        <div class="override-seg" role="group" aria-label="Irrigation override">
            <button
                type="button"
                class="override-seg__btn"
                class:is-active=move || is("auto")
                on:click=choose_auto
            >"Auto"</button>
            <button
                type="button"
                class="override-seg__btn override-seg__btn--skip"
                class:is-active=move || is("skip")
                on:click=choose_skip
            >"Skip"</button>
            <button
                type="button"
                class="override-seg__btn override-seg__btn--run"
                class:is-active=move || is("run")
                on:click=choose_run
            >"Force"</button>
        </div>
    };

    if compact {
        // Zone card: just the segmented buttons (the card already names the zone).
        view! { <div class="override-ctl override-ctl--compact">{seg}</div> }.into_any()
    } else {
        // Irrigation page: a titled panel with a live explainer so the
        // override is unmistakable when active.
        let status = move || match mode.get().as_str() {
            "skip" => "Skipping every zone until you switch back to Auto.".to_string(),
            "run" => "Forcing the next run past all skip conditions. Zones can still override."
                .to_string(),
            _ => "Following the engine. Set Skip or Force to take manual control.".to_string(),
        };
        view! {
            <section class="override-panel" class:override-panel--active=move || !is("auto")>
                <div class="override-panel__head">
                    <span class="override-panel__icon" aria-hidden="true">
                        <crate::components::ui::Icon name="settings" size=18 stroke=2.0/>
                    </span>
                    <div class="override-panel__text">
                        <h3 class="override-panel__title">"Override"</h3>
                        <p class="override-panel__help">{status}</p>
                    </div>
                </div>
                {seg}
            </section>
        }
        .into_any()
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

    let toast = crate::components::ui::use_toast();
    let save_done = Callback::new(move |result: Result<(), String>| {
        if let Err(e) = result {
            // Re-arm the server follow so the next snapshot restores the
            // real value; the optimistic local edit didn't stick.
            user_touched.set(false);
            toast.error(format!("Couldn't save {label}: {e}"));
        }
    });
    let commit = move |v: f64| {
        let clamped = v.clamp(min, max);
        user_touched.set(true);
        set_val.set(clamped);
        post_action_then(
            json!({
                "kind": "set_threshold",
                "key": key,
                "value": clamped,
            }),
            save_done,
        );
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
fn ToggleControl(key: &'static str, label: &'static str, current: Signal<bool>) -> impl IntoView {
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

    let toast = crate::components::ui::use_toast();
    let save_done = Callback::new(move |result: Result<(), String>| {
        if let Err(e) = result {
            // Roll the switch back to the server value and re-arm the
            // follow; the optimistic flip didn't stick.
            user_touched.set(false);
            set_is_on.set(current.get_untracked());
            toast.error(format!("Couldn't switch {label}: {e}"));
        }
    });
    let on_click = move |_| {
        let next = !is_on.get();
        user_touched.set(true);
        set_is_on.set(next);
        post_action_then(json!({"kind":"toggle","key":key,"on":next}), save_done);
    };
    // A real <button role="switch"> so the toggle is reachable and
    // operable from the keyboard (the old span had tabindex but no
    // key handling, so Space/Enter did nothing). Matches ui::Toggle.
    view! {
        <div class="toggle-pair">
            <label class="toggle-label">{label}</label>
            <button
                type="button"
                role="switch"
                aria-checked=move || if is_on.get() { "true" } else { "false" }
                aria-label=label
                class=move || if is_on.get() { "toggle-clay is-on" } else { "toggle-clay" }
                on:click=on_click
            ></button>
        </div>
    }
}

/// Build the standard completion callback for action buttons: failures
/// surface as an error toast, successes stay quiet (the next snapshot
/// reflects the change). Must be called from component scope, where the
/// ToastHub context resolves; the returned Callback is then safe to run
/// from the detached async task inside `post_action_then`.
pub(crate) fn toast_on_err(prefix: &'static str) -> Callback<Result<(), String>> {
    let toast = crate::components::ui::use_toast();
    Callback::new(move |result: Result<(), String>| {
        if let Err(e) = result {
            toast.error(format!("{prefix} ({e})"));
        }
    })
}

/// Browser-side helper: POST a JSON body to /api/irrigation/action and
/// report completion so callers can surface failure (toast) or run
/// optimistic UI (pending state cleared by the next snapshot, or rolled
/// back on error). On SSR this is a no-op: the server doesn't fire
/// actions at itself. There is deliberately no fire-and-forget variant;
/// every mutating POST must report its outcome.
#[cfg(feature = "hydrate")]
pub(crate) fn post_action_then(body: serde_json::Value, done: Callback<Result<(), String>>) {
    use leptos::task::spawn_local;
    spawn_local(async move {
        let payload = body.to_string();
        let req = gloo_net::http::Request::post("/api/irrigation/action")
            .header("Content-Type", "application/json")
            .body(payload);
        let result = match req {
            Ok(r) => match r.send().await {
                Ok(resp) if resp.ok() => Ok(()),
                Ok(resp) => Err(format!("HTTP {}", resp.status())),
                Err(e) => Err(e.to_string()),
            },
            Err(e) => Err(e.to_string()),
        };
        done.run(result);
    });
}

#[cfg(not(feature = "hydrate"))]
#[allow(dead_code)]
pub(crate) fn post_action_then(_body: serde_json::Value, _done: Callback<Result<(), String>>) {}

// `event_target_value` comes in from `leptos::prelude::*`. It's
// defined on both ssr and hydrate builds (SSR returns empty since the
// event closure never actually fires there).
