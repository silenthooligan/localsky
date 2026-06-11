// WelcomeStep. The first ten seconds of the product: what LocalSky is,
// what setup will ask for, and the license acknowledgement, framed as
// an onboarding moment rather than a legal wall.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, SetupFooter};
use crate::components::ui::{Icon, Toggle};

#[component]
pub fn WelcomeStep() -> impl IntoView {
    let license_accepted = RwSignal::new(false);

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
                    <li>"Optional: a weather station on your network (Tempest, Ecowitt, Davis...)"</li>
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
