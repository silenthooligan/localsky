// WelcomeStep. The first ten seconds of the product: what LocalSky is,
// what setup will ask for, and the license acknowledgement, framed as
// an onboarding moment rather than a legal wall.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, SetupFooter};
use crate::components::ui::Icon;

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
            if let Some(d) = crate::components::setup::draft::fetch().await {
                draft.set(d);
                loaded.set(true);
                // The license is a note, not a gate: reaching this step is
                // the acceptance, and it is recorded in the draft now, after
                // the load, so the persist effect below actually saves it.
                // (Setting it before the load let the loaded draft's `false`
                // overwrite it, and apply then refused the whole wizard.)
                license_accepted.set(true);
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
            let _ = crate::components::setup::draft::save(&candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    });

    let next_href = move || next_step_href("welcome");

    view! {
        <div class="setup-step">
            <div class="setup-hero">
                <span class="setup-hero__icon"><Icon name="weather" size=30/></span>
                <h2 class="setup-hero__title">"Good watering starts with your yard."</h2>
                <p class="setup-hero__sub">
                    "Start with your location. Add weather sources and a controller, or use LocalSky for weather alone."
                </p>
            </div>

            <div class="setup-pillars">
                <div class="setup-pillar">
                    <Icon name="home" size=18/>
                    <strong>"Local-first"</strong>
                    <span>"Runs on your hardware. You choose which cloud services to connect."</span>
                </div>
                <div class="setup-pillar">
                    <Icon name="sources" size=18/>
                    <strong>"Weather your way"</strong>
                    <span>"Use a supported station, free forecasts, or both."</span>
                </div>
                <div class="setup-pillar">
                    <Icon name="zap" size=18/>
                    <strong>"Room to grow"</strong>
                    <span>"Add watering zones and Home Assistant when you're ready."</span>
                </div>
            </div>

            <div class="setup-needs">
                <p class="setup-needs__title">"What you'll need"</p>
                <ul class="setup-needs__list">
                    <li>"Your address or coordinates"</li>
                    <li>"Optional: a weather station or soil sensors"</li>
                    <li>"Optional: your sprinkler controller's connection details"</li>
                </ul>
            </div>

            <p class="setup-license">
                "Free and open source under Apache 2.0. No account or email signup required."
            </p>

            <SetupFooter prev={None::<String>} next=Signal::derive(next_href)/>
        </div>
    }
}
