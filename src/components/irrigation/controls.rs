// Control surfaces for the irrigation page: Stop all, the rain delay,
// and the sticky overrides. Each POSTs to /api/irrigation/action with a
// {"kind": ...} body; the server writes LocalSky's own control store
// and the next snapshot reflects it. Thresholds are edited under
// Settings > Skip rules, nowhere else.

use crate::model::IrrigationSnapshot;
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
    // Confirmed running, or commanded on by LocalSky with no readback to
    // confirm it. A controller that cannot report state must not make
    // the UI claim water is moving on its own; a valve LocalSky opened
    // on one is a different matter, and Stop must reach it.
    let any_running = move || {
        snap.get()
            .zones
            .iter()
            .any(|z| z.is_running_or_unconfirmed())
    };
    let running_count = Signal::derive(move || {
        snap.get()
            .zones
            .iter()
            .filter(|z| z.is_running_or_unconfirmed())
            .count()
    });
    let unconfirmed_count = Signal::derive(move || {
        snap.get()
            .zones
            .iter()
            .filter(|z| z.run_state() == crate::model::RunState::Unconfirmed)
            .count()
    });

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
                            let u = unconfirmed_count.get();
                            let note = if u > 0 {
                                format!(
                                    " {u} of them on a controller that cannot confirm; LocalSky commanded it on and this closes it."
                                )
                            } else {
                                String::new()
                            };
                            match n {
                                0 => "All zones idle. Arms by itself the moment anything runs.".to_string(),
                                1 => format!("1 zone is running. Stops it instantly.{note}"),
                                n => format!("{n} zones are running. Stops every active station instantly.{note}"),
                            }
                        }}
                    </p>
                </div>
            </div>
            <crate::components::ui::Button
    variant="danger-solid"
    size="md"
    on_click=Callback::new(on_click)
    disabled=Signal::derive(move || !any_running())
    class="stop-all-btn">
                "STOP ALL ZONES"
            </crate::components::ui::Button>
            <StopAllConfirm visible=confirm_open running_count/>
        </section>
    }
}

/// Rain Delay one-tap. The category's most-used control: pause ALL
/// watering for a preset (or custom) number of hours, then resume automatically.
/// Wraps the existing timed vacation pause (`set_pause_until` / `clear_pause_until`),
/// and when a delay is active shows a live countdown chip + Cancel instead of the
/// preset buttons.
#[component]
pub fn RainDelayPanel(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let until = move || snap.get().pause_until_epoch;
    let active = move || {
        let u = until();
        u > 0 && u > chrono::Utc::now().timestamp()
    };
    // Re-renders on each snapshot (every ~10s via SSE), so the countdown stays
    // current without a dedicated ticker.
    let time_left = move || {
        let secs = (until() - chrono::Utc::now().timestamp()).max(0);
        let days = secs / 86_400;
        let hours = (secs % 86_400) / 3_600;
        let mins = (secs % 3_600) / 60;
        if days > 0 {
            format!("{days}d {hours}h left")
        } else if hours > 0 {
            format!("{hours}h {mins}m left")
        } else {
            format!("{mins}m left")
        }
    };

    let done = toast_on_err("Rain delay failed");
    let set_hours = move |hours: i64| {
        let epoch = chrono::Utc::now().timestamp() + hours * 3_600;
        post_action_then(json!({ "kind": "set_pause_until", "epoch": epoch }), done);
    };
    let cancel = move |_| {
        post_action_then(json!({ "kind": "clear_pause_until" }), done);
    };

    let custom = RwSignal::new(String::new());
    let apply_custom = move |_| {
        if let Ok(h) = custom.get_untracked().trim().parse::<i64>() {
            if (1..=720).contains(&h) {
                set_hours(h);
                custom.set(String::new());
            }
        }
    };

    view! {
        <section class="rain-delay" class:rain-delay--active=active>
            <div class="rain-delay__head">
                <span class="rain-delay__icon" aria-hidden="true">
                    <crate::components::ui::Icon name="droplet" size=18 stroke=2.0/>
                </span>
                <div class="rain-delay__text">
                    <h3 class="rain-delay__title">"Rain delay"</h3>
                    <p class="rain-delay__help">
                        "Pause every zone for a set time, then resume automatically."
                    </p>
                </div>
            </div>
            {move || {
                if active() {
                    view! {
                        <div class="rain-delay__row">
                            <span class="rain-delay__chip">{time_left}</span>
                            <crate::components::ui::Button
    variant="ghost"
    size="sm"
    on_click=Callback::new(cancel)
    class="rain-delay__cancel">
                                "Cancel"
                            </crate::components::ui::Button>
                        </div>
                    }
                    .into_any()
                } else {
                    view! {
                        <div class="rain-delay__row">
                            <crate::components::ui::Button
    variant="secondary"
    size="sm"
    on_click=Callback::new(move |_| set_hours(24))
    class="rain-delay__btn">"24h"</crate::components::ui::Button>
                            <crate::components::ui::Button
    variant="secondary"
    size="sm"
    on_click=Callback::new(move |_| set_hours(48))
    class="rain-delay__btn">"48h"</crate::components::ui::Button>
                            <crate::components::ui::Button
    variant="secondary"
    size="sm"
    on_click=Callback::new(move |_| set_hours(72))
    class="rain-delay__btn">"72h"</crate::components::ui::Button>
                            <input
                                type="number"
                                class="rain-delay__custom"
                                min="1"
                                max="720"
                                placeholder="hrs"
                                aria-label="Custom rain-delay hours"
                                prop:value=move || custom.get()
                                on:input=move |ev| custom.set(event_target_value(&ev))
                            />
                            <crate::components::ui::Button
    variant="secondary"
    size="sm"
    on_click=Callback::new(apply_custom)
    class="rain-delay__btn">"Set"</crate::components::ui::Button>
                        </div>
                    }
                    .into_any()
                }
            }}
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

/// The page owns override requests and their confirmation, because zone cards
/// are replaced on each snapshot. An open confirmation and an in-flight action
/// must outlive those cards. The dialog also sits outside their clipped bounds.
#[derive(Clone, Copy)]
struct OverrideActions {
    saving: RwSignal<bool>,
    choice: RwSignal<Option<(Option<String>, String)>>,
    force_target: RwSignal<Option<String>>,
    force_open: RwSignal<bool>,
    request: Callback<(Option<String>, String)>,
}

pub(crate) fn provide_override_actions() -> impl IntoView {
    let saving = RwSignal::new(false);
    let choice = RwSignal::new(None::<(Option<String>, String)>);
    let force_target = RwSignal::new(None::<String>);
    let force_open = RwSignal::new(false);
    let toast = crate::components::ui::use_toast();
    let done = Callback::new(move |result: Result<(), String>| {
        saving.set(false);
        if let Err(e) = result {
            choice.set(None);
            toast.error(format!("Couldn't set override: {e}"));
        }
    });
    let request = Callback::new(move |(zone, mode): (Option<String>, String)| {
        if saving.get_untracked() {
            return;
        }
        saving.set(true);
        choice.set(Some((zone.clone(), mode.clone())));
        post_action_then(override_action(&zone, &mode), done);
    });
    provide_context(OverrideActions {
        saving,
        choice,
        force_target,
        force_open,
        request,
    });
    view! {
        <crate::components::ui::ConfirmSheet
            visible=force_open
            title="Force scheduled watering?"
            body=Signal::derive(move || {
                let scope = force_target.get().map(|slug| format!("zone {slug}"))
                    .unwrap_or_else(|| "the yard".to_string());
                format!(
                    "Force scheduled watering for {scope} despite rain, soil, and condition-rule recommendations. \
                     This can overwater plants and waste water. Safety checks, watering restrictions, \
                     active holds, and script rules still apply. It stays on until you choose Auto; no valve starts now."
                )
            })
            confirm_label=Signal::derive(|| "Enable Force".to_string())
            danger=true
            on_confirm=Callback::new(move |()| request.run((force_target.get_untracked(), "run".to_string())))
        />
    }
}

/// Sticky override segmented control: Auto / Skip / Force. Drives the global
/// override (`zone = None`, rendered as a titled panel on the irrigation page)
/// or a single zone's override (`zone = Some(slug)`, rendered compact inside a
/// zone card). Sticky until changed: Force bypasses rain/soil/condition
/// recommendations, while safety gates, restrictions, operator holds, and
/// scripts remain binding.
/// The schedule still decides when to run; arming Force does not start a valve.
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
    let compact = zone.is_some();
    let actions = expect_context::<OverrideActions>();
    let target = StoredValue::new(zone);
    let mode = Signal::derive(move || {
        actions
            .choice
            .get()
            .filter(|(zone, _)| *zone == target.get_value())
            .map(|(_, mode)| mode)
            .unwrap_or_else(|| norm(current.get()))
    });
    // After the request finishes, the next snapshot owns the display again.
    // Do not wait for an exact echo: another device may already have changed
    // it back before we receive that echo.
    Effect::new(move |_| {
        let _ = current.get();
        if !actions.saving.get_untracked()
            && actions
                .choice
                .get_untracked()
                .is_some_and(|(zone, _)| zone == target.get_value())
        {
            actions.choice.set(None);
        }
    });
    let choose_auto = move |_| {
        actions
            .request
            .run((target.get_value(), "auto".to_string()));
    };
    let choose_skip = move |_| {
        actions
            .request
            .run((target.get_value(), "skip".to_string()));
    };

    let is = move |m: &'static str| mode.get() == m;
    let seg = view! {
        <div class="override-seg" role="group" aria-label="Irrigation override">
            <crate::components::ui::Button
    variant="secondary"
    size="sm"
    aria_pressed=Signal::derive(move || is("auto").to_string())
    disabled=Signal::derive(move || actions.saving.get())
    on_click=Callback::new(choose_auto)
    class=Signal::derive(move || format!("override-seg__btn{}", if is("auto") { " is-active" } else { "" }))>"Auto"</crate::components::ui::Button>
            <crate::components::ui::Button
    variant="secondary"
    size="sm"
    aria_pressed=Signal::derive(move || is("skip").to_string())
    disabled=Signal::derive(move || actions.saving.get())
    on_click=Callback::new(choose_skip)
    class=Signal::derive(move || format!("override-seg__btn override-seg__btn--skip{}", if is("skip") { " is-active" } else { "" }))>"Skip"</crate::components::ui::Button>
            <crate::components::ui::Button
    variant="secondary"
    size="sm"
    aria_pressed=Signal::derive(move || is("run").to_string())
    disabled=Signal::derive(move || actions.saving.get())
    on_click=Callback::new(move |_| {
                    actions.force_target.set(target.get_value());
                    actions.force_open.set(true);
                })
    class=Signal::derive(move || format!("override-seg__btn override-seg__btn--run{}", if is("run") { " is-active" } else { "" }))>"Force"</crate::components::ui::Button>
        </div>
    };

    if compact {
        // Zone card: just the segmented buttons (the card already names the zone).
        view! {
            <div class="override-ctl override-ctl--compact">
                {seg}
                {move || is("run").then(|| view! {
                    <p class="override-panel__help" role="status">
                        "Force stays on until Auto. Safety checks and holds still apply."
                    </p>
                })}
            </div>
        }
        .into_any()
    } else {
        // Irrigation page: a titled panel with a live explainer so the
        // override is unmistakable when active.
        let status = move || {
            match mode.get().as_str() {
            "skip" => "Skipping every zone until you switch back to Auto.".to_string(),
            "run" => "Force stays on until Auto, bypassing rain, soil, and condition-rule recommendations. Safety checks, restrictions, holds, and script rules still apply."
                .to_string(),
            _ => "Following the schedule. Set Skip or Force to take manual control.".to_string(),
        }
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

/// POST the action and read the outcome. On a non-2xx the response BODY is
/// what carries the reason: the server sends `{"error": ..., "hint": ...}`,
/// and for a cloud controller that error string is the vendor's own status
/// and message. This used to answer `format!("HTTP {}", resp.status())` and
/// throw the body away, which collapsed a rejected zone id, a revoked
/// token, an exhausted request budget, and a transport failure into one
/// indistinguishable "HTTP 502" with nothing for the user to report.
/// `load_error_message` is the same body reader the settings pages use.
#[cfg(feature = "hydrate")]
async fn post_action(body: serde_json::Value) -> Result<Option<serde_json::Value>, String> {
    let payload = body.to_string();
    let req = gloo_net::http::Request::post("/api/irrigation/action")
        .header("Content-Type", "application/json")
        .body(payload);
    match req {
        Ok(r) => match r.send().await {
            Ok(resp) if resp.ok() => Ok(resp.json::<serde_json::Value>().await.ok()),
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                Err(crate::components::settings_ui::load_error_message(
                    status, &text,
                ))
            }
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// Browser-side helper: POST a JSON body to /api/irrigation/action and
/// report completion so callers can surface failure (toast) or run
/// optimistic UI (pending state cleared by the next snapshot, or rolled
/// back on error). On SSR this is a no-op: the server doesn't fire
/// actions at itself. There is deliberately no fire-and-forget variant;
/// every mutating POST must report its outcome.
///
/// All three helpers deliver through `try_run`, never `run`. `run` reads
/// the callback's StoredValue with `with_value`, which PANICS once the
/// owner that created it is disposed, and wasm-release is panic=abort, so
/// a response landing after its view went away would take the whole app
/// down. Callers still hoist their callbacks to a scope that outlives the
/// request (that is what restores the message); this is the backstop that
/// keeps the failure mode a silent no-op rather than a dead page.
#[cfg(feature = "hydrate")]
pub(crate) fn post_action_then(body: serde_json::Value, done: Callback<Result<(), String>>) {
    leptos::task::spawn_local(async move {
        let _ = done.try_run(post_action(body).await.map(|_| ()));
    });
}

/// Like `post_action_then`, but a success also hands back the response's
/// optional `note` string. The Stop action sets it when the controller has
/// no per-zone stop (the whole device was stopped), so the caller's toast
/// can say what actually happened instead of implying one zone stopped.
#[cfg(feature = "hydrate")]
pub(crate) fn post_action_note_then(
    body: serde_json::Value,
    done: Callback<Result<Option<String>, String>>,
) {
    leptos::task::spawn_local(async move {
        let _ =
            done.try_run(post_action(body).await.map(|v| {
                v.and_then(|v| v.get("note").and_then(|n| n.as_str()).map(str::to_string))
            }));
    });
}

/// Like `post_action_then`, but a success hands back the whole response
/// body. Callers that need more than `note` (the zone page reads
/// `confirm_within_s`, the controller's status-readback lag) use this
/// rather than growing another single-field variant.
#[cfg(feature = "hydrate")]
pub(crate) fn post_action_body_then(
    body: serde_json::Value,
    done: Callback<Result<Option<serde_json::Value>, String>>,
) {
    leptos::task::spawn_local(async move {
        let _ = done.try_run(post_action(body).await);
    });
}

#[cfg(not(feature = "hydrate"))]
#[allow(dead_code)]
pub(crate) fn post_action_then(_body: serde_json::Value, _done: Callback<Result<(), String>>) {}

#[cfg(not(feature = "hydrate"))]
#[allow(dead_code)]
pub(crate) fn post_action_note_then(
    _body: serde_json::Value,
    _done: Callback<Result<Option<String>, String>>,
) {
}

#[cfg(not(feature = "hydrate"))]
#[allow(dead_code)]
pub(crate) fn post_action_body_then(
    _body: serde_json::Value,
    _done: Callback<Result<Option<serde_json::Value>, String>>,
) {
}

// `event_target_value` comes in from `leptos::prelude::*`. It's
// defined on both ssr and hydrate builds (SSR returns empty since the
// event closure never actually fires there).
