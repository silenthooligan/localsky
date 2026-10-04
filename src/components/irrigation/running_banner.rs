//! Persistent app-wide watering controls, including gaps between Quick Run zones.
use super::quick_run_client::QuickRunClient;
use crate::{
    components::ui::{Button, Icon},
    model::{quick_run::QuickRunPhase, IrrigationSnapshot},
};
use leptos::prelude::*;

#[component]
pub fn RunningBanner(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let client = expect_context::<QuickRunClient>();
    let location = leptos_router::hooks::use_location();
    let pending = RwSignal::new(false);
    let problem = RwSignal::new(String::new());
    let sent = RwSignal::new(String::new());
    let running = Memo::new(move |_| {
        snap.get()
            .zones
            .into_iter()
            .filter(|z| z.is_running_or_unconfirmed())
            .collect::<Vec<_>>()
    });
    let visible = move || {
        !location.pathname.get().ends_with("/login")
            && (client.active() || !running.get().is_empty())
    };
    let attention = move || {
        (client.active() && client.offline.get())
            || !problem.get().is_empty()
            || (client.active() && !client.error.get().is_empty())
            || running
                .get()
                .iter()
                .any(|z| z.run_state() == crate::model::RunState::Unconfirmed)
            || client
                .state
                .get()
                .and_then(|s| s.run)
                .is_some_and(|r| r.stop_unconfirmed)
    };
    let headline = move || {
        if let Some(run) = client
            .state
            .get()
            .and_then(|s| s.run)
            .filter(|r| r.phase.active() || r.stop_unconfirmed)
        {
            if run.stop_unconfirmed {
                return "Check watering".into();
            }
            if run.phase == QuickRunPhase::Stopping {
                return "Stopping Quick Run…".into();
            }
            return run
                .current
                .and_then(|i| run.names.get(i).cloned())
                .unwrap_or_else(|| "Quick Run is starting".into());
        }
        let zones = running.get();
        zones
            .first()
            .map(|z| {
                if zones.len() > 1 {
                    format!("{} + {} more", z.name, zones.len() - 1)
                } else {
                    z.name.clone()
                }
            })
            .unwrap_or_default()
    };
    let detail = move || {
        if !problem.get().is_empty() {
            return problem.get();
        }
        if client.active() && !client.error.get().is_empty() {
            return client.error.get();
        }
        if client.active() && client.offline.get() {
            return "Status unavailable. Check your connection.".into();
        }
        if let Some(run) = client
            .state
            .get()
            .and_then(|s| s.run)
            .filter(|r| r.phase.active() || r.stop_unconfirmed)
        {
            if run.stop_unconfirmed {
                return "Stop wasn’t confirmed. Retry Stop.".into();
            }
            if run.phase == QuickRunPhase::Stopping {
                return "Cancelling the remaining zones".into();
            }
            let progress = format!("{} of {} zones finished", run.completed, run.zones.len());
            if let Some(end) = run
                .current_ends_epoch
                .filter(|_| run.phase == QuickRunPhase::Running)
            {
                let seconds = (end - client.clock.get()).max(0);
                return if seconds > 0 {
                    format!("About {} min left · {progress}", (seconds + 59) / 60)
                } else {
                    format!("Finishing this zone · {progress}")
                };
            }
            return format!(
                "{} · {progress}",
                if run.phase == QuickRunPhase::Finishing {
                    "Finishing this zone"
                } else {
                    "Preparing watering"
                }
            );
        }
        if !sent.get().is_empty() {
            return sent.get();
        }
        if running
            .get()
            .iter()
            .any(|z| z.run_state() == crate::model::RunState::Unconfirmed)
        {
            return "Controller status is unconfirmed".into();
        }
        "Watering now".into()
    };
    Effect::new(move |_| {
        if running.get().is_empty() {
            sent.set(String::new());
            problem.set(String::new());
        }
    });
    let done = Callback::new(
        move |result: Result<
            Option<serde_json::Value>,
            crate::components::request_error::RequestError,
        >| {
            pending.set(false);
            match result {
                Ok(Some(body))
                    if body["ok"] == true
                        && !body["failed"].as_array().is_some_and(|v| !v.is_empty()) =>
                {
                    sent.set(if body["scope"] == "device" {
                        "Stop sent for all zones on this controller".into()
                    } else {
                        "Stop sent · waiting for the controller".into()
                    })
                }
                _ => problem.set("Stop wasn’t confirmed. Try again.".into()),
            }
        },
    );
    let stop = Callback::new(move |_| {
        if client.active() {
            client.stop();
            return;
        }
        if pending.get_untracked() {
            return;
        }
        let zones = running.get_untracked();
        let Some(first) = zones.first() else {
            return;
        };
        pending.set(true);
        problem.set(String::new());
        let body = if zones.len() > 1 {
            serde_json::json!({"kind":"stop_all"})
        } else {
            serde_json::json!({"kind":"stop", "zone":first.slug})
        };
        super::controls::post_action_body_then(body, done);
    });
    view! {
        <Show when=visible>
            <section class="watering-strip" class:watering-strip--attention=attention aria-label="Current watering">
                <span class="watering-strip__icon" aria-hidden="true"><Icon name="sprinkler" size=24/></span>
                <div class="watering-strip__copy">
                    <strong>{headline}</strong>
                    <span role="status" aria-live="polite">{detail}</span>
                    {move || snap.get().flow.rate_gpm.map(|gpm| view! { <span>{format!("Flow: {gpm:.1} gpm")}</span> })}
                </div>
                <div class="watering-strip__actions">
                    <Button variant="secondary" href=crate::base::url("/irrigation")>"View"</Button>
                    <Button variant="danger" icon="stop" class="running-banner-stop" loading=Signal::derive(move || pending.get() || client.pending.get()) on_click=stop>
                        {move || if client.active() { "Stop Quick Run" } else if running.get().len() > 1 { "Stop all" } else { "Stop" }}
                    </Button>
                </div>
            </section>
        </Show>
    }
}
