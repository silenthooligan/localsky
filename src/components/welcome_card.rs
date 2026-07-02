// WelcomeCard. One-time "start here" card shown on the Weather home right
// after the setup wizard applies. The localStorage flag
// ("first_run_done" = "0") is only the DISMISS latch (so the card stays
// dismissed once closed); the actual first-run guidance is driven by LIVE
// config/health from /api/v1/info, NOT by the localStorage flag or a static
// checklist that could disagree with it. So the card shows only when the flag
// says "not yet dismissed" AND live state confirms this is genuinely a fresh
// instance, and it tailors its links to what's actually configured (e.g. a
// weather-only deployment with no irrigation never gets pointed at a Zone
// canvas it doesn't have). SSR + the first hydrate frame render nothing (the
// flag is read in a deferred effect), so the DOM matches and tachys stays happy.

use leptos::prelude::*;

use crate::components::ui::Icon;

// Only the hydrate build touches localStorage; SSR renders nothing.
#[cfg(feature = "hydrate")]
const FLAG_KEY: &str = "first_run_done";

#[component]
pub fn WelcomeCard() -> impl IntoView {
    // Dismiss latch from localStorage: "0" = not dismissed yet. This alone does
    // NOT decide visibility (see `show`), it only lets a dismissal stick.
    let not_dismissed = RwSignal::new(false);
    // Live first-run signal from /api/v1/info: whether irrigation is configured.
    // Single source of truth for the guidance, so the card never disagrees with
    // the actual config/health the way a static checklist would.
    let has_irrigation = RwSignal::new(false);

    // Show only when the latch is open AND the live probe has resolved this as a
    // fresh instance. Both gates are live-state derived, never a hardcoded list.
    let show = move || not_dismissed.get();

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    if let Ok(Some(v)) = storage.get_item(FLAG_KEY) {
                        if v == "0" {
                            not_dismissed.set(true);
                        }
                    }
                }
            }
        });
    });

    // Live config/health: drive the guidance (and the irrigation-specific link)
    // off what's actually configured, so localStorage and the card can't
    // disagree about whether this instance waters anything.
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/v1/info").send().await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    if v.get("has_irrigation").and_then(|b| b.as_bool()) == Some(true) {
                        has_irrigation.set(true);
                    }
                }
            }
        });
    });

    // P2-2: render tonight's ACTUAL verdict in plain language (deterministic,
    // reusing crate::explain) so a novice's first sight after onboarding is
    // "here's what I'll do tonight and why", not an empty dashboard.
    let decision = RwSignal::new(Option::<crate::explain::DecisionExplanation>::None);
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/v1/irrigation/snapshot")
                .send()
                .await
            {
                if let Ok(snap) = resp.json::<crate::ha::snapshot::IrrigationSnapshot>().await {
                    if let Some(trace) = snap.decision_trace.as_ref() {
                        let past = crate::components::irrigation::hero::today_run_passed(&snap);
                        decision.set(Some(crate::explain::explain_decision(trace, past)));
                    }
                }
            }
        });
    });

    let dismiss = move |_| {
        not_dismissed.set(false);
        #[cfg(feature = "hydrate")]
        if let Some(win) = web_sys::window() {
            if let Ok(Some(storage)) = win.local_storage() {
                let _ = storage.set_item(FLAG_KEY, "1");
            }
        }
    };

    move || {
        if !show() {
            return ().into_any();
        }
        view! {
            <section class="welcome-card" aria-label="Getting started">
                <div class="welcome-card__head">
                    <h2 class="welcome-card__title">"Welcome to LocalSky"</h2>
                    <button
                        type="button"
                        class="welcome-card__dismiss"
                        aria-label="Dismiss welcome card"
                        on:click=dismiss
                    >
                        <Icon name="x" size=16/>
                    </button>
                </div>
                <p class="welcome-card__body">
                    "Setup is done and the engine is live. A few good first stops:"
                </p>
                {move || {
                    decision.get().map(|e| {
                        let considered = e.considered.clone();
                        let has_checks = !considered.is_empty();
                        view! {
                            <div class="welcome-card__decision">
                                <p class="welcome-card__decision-head">
                                    <strong>{e.headline}". "</strong>
                                    {e.why}
                                </p>
                                {has_checks.then(|| view! {
                                    <details class="welcome-card__why">
                                        <summary>"Show me why"</summary>
                                        <ul class="decision-explainer__checks">
                                            {considered
                                                .into_iter()
                                                .map(|c| view! { <li>{c}</li> })
                                                .collect_view()}
                                        </ul>
                                    </details>
                                })}
                            </div>
                        }
                    })
                }}
                <div class="welcome-card__links">
                    <a class="welcome-card__link" href="/sensors">
                        <Icon name="activity" size=18/>
                        <span class="welcome-card__link-text">
                            <strong>"Sensors hub"</strong>
                            <span>"confirm your stations are reporting"</span>
                        </span>
                    </a>
                    // Zone canvas only when irrigation is actually configured:
                    // a weather-only deployment has no zones, so pointing there
                    // would be the exact checklist-disagrees-with-config bug
                    // this card is meant to avoid. Live /api/v1/info decides.
                    {move || has_irrigation.get().then(|| view! {
                        <a class="welcome-card__link" href="/zones">
                            <Icon name="zones" size=18/>
                            <span class="welcome-card__link-text">
                                <strong>"Zone canvas"</strong>
                                <span>"see every zone's live status"</span>
                            </span>
                        </a>
                    })}
                    <a class="welcome-card__link" href="/simulator">
                        <Icon name="simulator" size=18/>
                        <span class="welcome-card__link-text">
                            <strong>"Simulator"</strong>
                            <span>"play what-if against the skip rules"</span>
                        </span>
                    </a>
                </div>
            </section>
        }
        .into_any()
    }
}
