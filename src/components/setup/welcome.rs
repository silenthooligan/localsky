// WelcomeStep. License acknowledgement + telemetry opt-in. Required
// before Apply on the Review step.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, SetupFooter};
use crate::components::ui::Toggle;

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
            <h2 class="setup-step__title">"Welcome to LocalSky"</h2>
            <p class="setup-step__body">
                "LocalSky is local-first weather and irrigation control. Your "
                "data stays on your hardware. This wizard walks through the "
                "essential pieces: where you are, what weather you can see, "
                "which controller drives your zones, and which grasses and "
                "soils you actually have."
            </p>

            <Toggle
                checked=license_accepted
                label="I accept the Apache-2.0 license".to_string()
                helptext="LocalSky is open source under Apache-2.0. The full text lives in LICENSE.".to_string()
            />

            <p class="setup-step__body">
                "LocalSky does not send telemetry. There is no analytics ping, no "
                "anonymous-usage report, no account, no email signup. If that changes "
                "in a future release it will be opt-in and disclosed here."
            </p>

            <SetupFooter prev=None next=Signal::derive(next_href).get_untracked()/>
            <p class="setup-step__hint" class:setup-step__hint--visible=move || !can_advance()>
                "Accept the license to continue."
            </p>
        </div>
    }
}
