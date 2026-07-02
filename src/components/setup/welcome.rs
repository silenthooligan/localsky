// WelcomeStep. The first ten seconds of the product: what LocalSky is,
// what setup will ask for, and the license acknowledgement, framed as
// an onboarding moment rather than a legal wall.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, SetupFooter};
use crate::components::ui::{Icon, Toggle};

#[cfg(feature = "hydrate")]
async fn fetch_draft() -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get("/api/wizard/draft")
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

#[cfg(feature = "hydrate")]
async fn save_draft(draft: serde_json::Value) -> Result<(), String> {
    let resp = gloo_net::http::Request::put("/api/wizard/draft")
        .json(&draft)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

#[component]
pub fn WelcomeStep() -> impl IntoView {
    // License acceptance is persisted into the wizard draft (load on mount,
    // save on change). It must round-trip through the draft so it survives
    // step navigation AND so the server-side apply, which rejects an
    // unaccepted license, actually sees it. A bare local signal here silently
    // reset on remount and never reached apply, blocking wizard completion.
    let license_accepted = RwSignal::new(false);
    let draft = RwSignal::new(serde_json::Value::Null);
    let loaded = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                license_accepted.set(
                    d.get("license_accepted")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                );
                draft.set(d);
                loaded.set(true);
            }
        });
    });

    // Persist acceptance into the draft whenever it changes after load. The
    // changed-guard makes the post-hydration re-run a no-op (the draft
    // already holds the loaded value), so only a real user toggle saves.
    Effect::new(move |_| {
        let val = license_accepted.get();
        if !loaded.get_untracked() {
            return;
        }
        let mut changed = false;
        draft.update(|d| {
            if let Some(obj) = d.as_object_mut() {
                if obj.get("license_accepted").and_then(|v| v.as_bool()) != Some(val) {
                    obj.insert("license_accepted".into(), val.into());
                    changed = true;
                }
            }
        });
        if !changed {
            return;
        }
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    });

    let can_advance = move || license_accepted.get();
    let next_href = move || {
        if can_advance() {
            next_step_href("welcome")
        } else {
            None
        }
    };

    view! {
        <div class="setup-step">
            <div class="setup-hero">
                <span class="setup-hero__icon"><Icon name="weather" size=30/></span>
                <h2 class="setup-hero__title">"Let's get your weather and watering dialed in"</h2>
                <p class="setup-hero__sub">
                    "LocalSky watches the sky over your yard and waters exactly "
                    "what each zone needs: no more, no less."
                </p>
            </div>

            <div class="setup-pillars">
                <div class="setup-pillar">
                    <Icon name="home" size=18/>
                    <strong>"Local-first"</strong>
                    <span>"Runs on your hardware. Your data never leaves home."</span>
                </div>
                <div class="setup-pillar">
                    <Icon name="sources" size=18/>
                    <strong>"Any hardware, or none"</strong>
                    <span>"Works with a backyard station, or just your address and a forecast."</span>
                </div>
                <div class="setup-pillar">
                    <Icon name="zap" size=18/>
                    <strong>"Plays well with others"</strong>
                    <span>"Home Assistant optional; one integration when you want it."</span>
                </div>
            </div>

            <div class="setup-needs">
                <p class="setup-needs__title">"Setup takes about five minutes. Helpful to have:"</p>
                <ul class="setup-needs__list">
                    <li>"Your address (or coordinates); weather and sun math start there"</li>
                    <li>"Optional: a weather or soil-sensor device on your network (Tempest, Ecowitt, Davis...). Some do both."</li>
                    <li>"Optional: your sprinkler controller (it can be found by a network scan)"</li>
                </ul>
            </div>

            <Toggle
                checked=license_accepted
                label="I accept the Apache-2.0 license".to_string()
                helptext="Free and open source. The full text lives in LICENSE.".to_string()
            />

            <p class="setup-step__hint" style="opacity:0.8">
                "No telemetry, no analytics, no account requirement, no email signup. "
                "If that ever changes it will be opt-in and disclosed right here."
            </p>

            <SetupFooter prev={None::<String>} next=Signal::derive(next_href)/>
            <p class="setup-step__hint" class:setup-step__hint--visible=move || !can_advance()>
                "Accept the license to continue."
            </p>
        </div>
    }
}
