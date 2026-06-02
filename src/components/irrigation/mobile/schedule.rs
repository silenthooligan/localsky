// Mobile "Schedule" tab — vertical 7-day verdict strip + thresholds +
// history + Phase 4 control surfaces (vacation pause until <date>,
// one-day override, run sequence now).
//
// When the HA-side helpers (input_datetime.irrigation_pause_until,
// input_select.irrigation_override_tomorrow) are missing, the controls
// render disabled with a "(HA helper not configured)" hint. The IU
// run-now button works regardless because it only needs the existing
// IU sequence entity that already powers the rest of the dashboard.

use crate::components::irrigation::controls::{post_action, ThresholdsPanel};
use crate::components::irrigation::history::HistoryPanel;
use crate::components::irrigation::per_zone_history::PerZoneHistory;
use crate::components::irrigation::soil_sensors::SoilSensors;
use crate::components::irrigation::verdict_strip::VerdictStrip;
use crate::components::irrigation::water_budget::WaterBudgetPanel;
use crate::components::irrigation::zone_math::ZoneMathPanel;
use crate::ha::snapshot::IrrigationSnapshot;
use crate::history::types::HistoryWindow;
use chrono::{DateTime, Local, TimeZone, Utc};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
use serde_json::json;

#[component]
pub fn MobileSchedule(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // Shared window state so the per-zone block and the History panel
    // below it stay in sync with one fetch; mirrors the desktop page.
    let (days, set_days) = signal(30u32);
    let (window, set_window) = signal::<HistoryWindow>(HistoryWindow::default());
    #[cfg(not(feature = "hydrate"))]
    let _ = set_window;
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let d = days.get();
            leptos::task::spawn_local(async move {
                let url = format!("/api/irrigation/history?days={d}");
                if let Ok(resp) = gloo_net::http::Request::get(&url).send().await {
                    if let Ok(w) = resp.json::<HistoryWindow>().await {
                        set_window.set(w);
                    }
                }
            });
        });
    }
    view! {
        <div class="mobile-stack">
            <h2 class="mobile-section-title">"7-day outlook"</h2>
            {view! { <VerdictStrip snap/> }.into_any()}

            <h2 class="mobile-section-title">"Soil sensors"</h2>
            {view! { <SoilSensors snap/> }.into_any()}

            <h2 class="mobile-section-title">"Why this duration?"</h2>
            {view! { <ZoneMathPanel snap/> }.into_any()}

            <h2 class="mobile-section-title">"Weekly water budget"</h2>
            {view! { <WaterBudgetPanel snap/> }.into_any()}

            <h2 class="mobile-section-title">"Per-zone history"</h2>
            {view! { <PerZoneHistory snap window/> }.into_any()}

            <h2 class="mobile-section-title">"Controls"</h2>
            <VacationPauseRow snap/>
            <OverrideTomorrowRow snap/>
            <RunSequenceNowRow snap/>

            <h2 class="mobile-section-title">"Thresholds"</h2>
            {view! { <ThresholdsPanel snap/> }.into_any()}

            <h2 class="mobile-section-title">"History"</h2>
            {view! { <HistoryPanel days set_days window/> }.into_any()}

            <h2 class="mobile-section-title">"Notifications"</h2>
            <NotificationsCard/>
        </div>
    }
}

#[component]
fn NotificationsCard() -> impl IntoView {
    // Subscription state. Defaults to "unknown" until the post-hydrate
    // probe runs, so SSR and the initial hydrate frame render the same
    // empty status line.
    let subscribed: RwSignal<Option<bool>> = RwSignal::new(None);
    let busy: RwSignal<bool> = RwSignal::new(false);
    let last_msg: RwSignal<String> = RwSignal::new(String::new());
    let permission: RwSignal<String> = RwSignal::new("unknown".to_string());

    #[cfg(feature = "hydrate")]
    {
        leptos::task::spawn_local(async move {
            // One-frame yield matches the rest of the file's hydration
            // contract — never set signals before the initial hydrate sweep
            // that would change the DOM count.
            gloo_timers::future::TimeoutFuture::new(0).await;
            if let Ok(p) = crate::push_client::permission_state() {
                permission.set(p);
            }
            let s = crate::push_client::is_subscribed().await;
            subscribed.set(Some(s));
        });
    }

    let on_enable = move |_| {
        if busy.get() {
            return;
        }
        busy.set(true);
        last_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match crate::push_client::subscribe().await {
                Ok(()) => {
                    subscribed.set(Some(true));
                    last_msg.set("Notifications enabled.".into());
                    if let Ok(p) = crate::push_client::permission_state() {
                        permission.set(p);
                    }
                }
                Err(e) => last_msg.set(format!("Failed: {e}")),
            }
            busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            busy.set(false);
        }
    };

    let on_disable = move |_| {
        if busy.get() {
            return;
        }
        busy.set(true);
        last_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match crate::push_client::unsubscribe().await {
                Ok(()) => {
                    subscribed.set(Some(false));
                    last_msg.set("Notifications disabled.".into());
                }
                Err(e) => last_msg.set(format!("Failed: {e}")),
            }
            busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            busy.set(false);
        }
    };

    view! {
        <div class="mobile-control-card">
            <div class="mobile-control-head">
                <h3 class="mobile-control-title">"Push notifications"</h3>
                <div class="mobile-control-status">
                    {move || match subscribed.get() {
                        None => "Checking…".to_string(),
                        Some(true) => "Enabled on this device".to_string(),
                        Some(false) => match permission.get().as_str() {
                            "denied" => "Blocked. Re-enable in browser settings.".to_string(),
                            _ => "Not enabled on this device".to_string(),
                        },
                    }}
                </div>
            </div>
            {move || match subscribed.get() {
                Some(true) => view! {
                    <button class="btn-clay mobile-primary-btn" on:click=on_disable disabled=move || busy.get()>
                        {move || if busy.get() { "Working…" } else { "Disable notifications" }}
                    </button>
                }.into_any(),
                _ => view! {
                    <button class="btn-clay btn-clay-good mobile-primary-btn" on:click=on_enable disabled=move || busy.get()>
                        {move || if busy.get() { "Working…" } else { "Enable notifications" }}
                    </button>
                }.into_any(),
            }}
            <p class="mobile-control-help">
                "Get a push when a zone starts or stops, and a daily verdict heads-up. iOS requires this app to be installed (Add to Home Screen) before notifications work."
            </p>
            {move || {
                let m = last_msg.get();
                if m.is_empty() { ().into_any() } else {
                    view! { <p class="mobile-control-help" style="color: var(--text-bright)">{m}</p> }.into_any()
                }
            }}
        </div>
    }
}

#[component]
fn VacationPauseRow(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // Local input value mirrors snapshot but is owned by the user once they
    // start typing. Same pattern as the threshold sliders: snap is read on
    // mount, locally edited, action POST commits, refresher catches up.
    let helpers_ok = move || snap.get().override_helpers_present;
    let pause_epoch = move || snap.get().pause_until_epoch;
    let active = move || {
        let e = pause_epoch();
        e > 0 && Utc::now().timestamp() < e
    };

    let local_str: RwSignal<String> = RwSignal::new(String::new());
    // Sync local from snap on first non-zero load.
    Effect::new(move |prev: Option<i64>| {
        let cur = pause_epoch();
        if Some(cur) != prev && cur > 0 {
            local_str.set(epoch_to_input_value(cur));
        }
        cur
    });

    let on_input = move |ev: leptos::ev::Event| {
        if let Some(v) = event_target_value_str(&ev) {
            local_str.set(v);
        }
    };

    let on_save = move |_| {
        let s = local_str.get();
        if s.is_empty() {
            return;
        }
        if let Some(epoch) = input_value_to_epoch(&s) {
            post_action(json!({"kind": "set_pause_until", "epoch": epoch}));
        }
    };

    let on_clear = move |_| {
        local_str.set(String::new());
        post_action(json!({"kind": "clear_pause_until"}));
    };

    move || {
        if !helpers_ok() {
            return view! {
                <div class="mobile-settings-stub">
                    <div class="mobile-settings-row">
                        <div class="mobile-settings-label">"Vacation pause until"</div>
                        <div class="mobile-settings-hint">"(HA helper not configured)"</div>
                    </div>
                </div>
            }
            .into_any();
        }

        let active_now = active();
        let label_now = if active_now {
            format!("Active until {}", format_human(pause_epoch()))
        } else if pause_epoch() > 0 {
            format!("Set to {} (already past)", format_human(pause_epoch()))
        } else {
            "No vacation pause set".to_string()
        };

        view! {
            <div class="mobile-control-card">
                <div class="mobile-control-head">
                    <h3 class="mobile-control-title">"Vacation pause until"</h3>
                    <div class="mobile-control-status" class:is-active=move || active_now>{label_now}</div>
                </div>
                <input
                    class="mobile-control-input"
                    type="datetime-local"
                    prop:value=move || local_str.get()
                    on:input=on_input
                    aria-label="Pause until datetime"
                />
                <div class="mobile-control-actions">
                    <button class="btn-clay" on:click=on_clear>"Clear"</button>
                    <button class="btn-clay btn-clay-good" on:click=on_save>"Pause"</button>
                </div>
                <p class="mobile-control-help">
                    "Skips every irrigation run until this date and time. Honored by the skip-check ahead of weather rules."
                </p>
            </div>
        }.into_any()
    }
}

#[component]
fn OverrideTomorrowRow(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let helpers_ok = move || snap.get().override_helpers_present;
    let current = move || snap.get().override_tomorrow.clone();

    let click = move |mode: &'static str| {
        let m = mode.to_string();
        move |_| {
            post_action(json!({"kind": "set_override_tomorrow", "mode": m.clone()}));
        }
    };
    let on_none = click("none");
    let on_skip = click("skip");
    let on_run = click("run");

    move || {
        if !helpers_ok() {
            return view! {
                <div class="mobile-settings-stub">
                    <div class="mobile-settings-row">
                        <div class="mobile-settings-label">"Override tomorrow"</div>
                        <div class="mobile-settings-hint">"(HA helper not configured)"</div>
                    </div>
                </div>
            }
            .into_any();
        }
        let cur = current();
        let cls = move |target: &'static str| {
            if cur == target {
                "mobile-segment is-on"
            } else {
                "mobile-segment"
            }
        };
        view! {
            <div class="mobile-control-card">
                <div class="mobile-control-head">
                    <h3 class="mobile-control-title">"Override tomorrow"</h3>
                </div>
                <div class="mobile-segmented" role="group" aria-label="Override tomorrow">
                    <button class=cls("none") on:click=on_none.clone()>"Auto"</button>
                    <button class=cls("skip") on:click=on_skip.clone()>"Skip"</button>
                    <button class=cls("run") on:click=on_run.clone()>"Force run"</button>
                </div>
                <p class="mobile-control-help">
                    "Overrides only tomorrow's verdict. The HA midnight automation resets to Auto each day."
                </p>
            </div>
        }.into_any()
    }
}

#[component]
fn RunSequenceNowRow(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let confirm_open: RwSignal<bool> = RwSignal::new(false);
    let on_open = move |_| confirm_open.set(true);
    let on_cancel = move |_| confirm_open.set(false);
    let on_confirm = move |_| {
        post_action(json!({"kind": "run_sequence_now"}));
        confirm_open.set(false);
    };
    let any_running = move || snap.get().zones.iter().any(|z| z.running);

    view! {
        <div class="mobile-control-card">
            <div class="mobile-control-head">
                <h3 class="mobile-control-title">"Run sequence now"</h3>
            </div>
            <button
                class="btn-clay btn-clay-good mobile-primary-btn"
                on:click=on_open
                disabled=move || any_running()
            >
                "Run full sequence"
            </button>
            <p class="mobile-control-help">
                "Triggers the IU sequence immediately, bypassing the morning skip-check. Disabled while a zone is already running."
            </p>
            {move || if confirm_open.get() {
                view! {
                    <div class="bottom-sheet-backdrop" on:click=on_cancel aria-hidden="true"></div>
                    <div class="bottom-sheet bottom-sheet-confirm" role="dialog" aria-modal="true">
                        <div class="bottom-sheet-handle" aria-hidden="true"></div>
                        <div class="bottom-sheet-title">"Run full sequence?"</div>
                        <p class="bottom-sheet-body">
                            "Starts every zone for its currently-planned duration, ignoring skip-check rules. Are you sure?"
                        </p>
                        <div class="bottom-sheet-actions">
                            <button class="btn-clay" on:click=on_cancel>"Cancel"</button>
                            <button class="btn-clay btn-clay-good" on:click=on_confirm>"Run sequence"</button>
                        </div>
                    </div>
                }.into_any()
            } else {
                ().into_any()
            }}
        </div>
    }
}

fn epoch_to_input_value(epoch: i64) -> String {
    let dt = Local
        .timestamp_opt(epoch, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).unwrap());
    // <input type=datetime-local> wants ISO 8601 in local time, no timezone:
    // YYYY-MM-DDTHH:MM. Strip seconds and timezone.
    dt.format("%Y-%m-%dT%H:%M").to_string()
}

fn input_value_to_epoch(s: &str) -> Option<i64> {
    // datetime-local emits "YYYY-MM-DDTHH:MM" interpreted as local time.
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
        .ok()
        .and_then(|naive| Local.from_local_datetime(&naive).single())
        .map(|dt| dt.timestamp())
}

fn format_human(epoch: i64) -> String {
    let dt: DateTime<Local> = Local
        .timestamp_opt(epoch, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).unwrap());
    dt.format("%a %b %-d, %-I:%M %p").to_string()
}

#[cfg(feature = "hydrate")]
fn event_target_value_str(ev: &leptos::ev::Event) -> Option<String> {
    use wasm_bindgen::JsCast;
    let target = ev.target()?;
    let input: web_sys::HtmlInputElement = target.dyn_into().ok()?;
    Some(input.value())
}

#[cfg(not(feature = "hydrate"))]
fn event_target_value_str(_ev: &leptos::ev::Event) -> Option<String> {
    None
}
