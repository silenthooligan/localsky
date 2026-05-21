// ControllersStep. Info pane for the irrigation controller HAL.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::Panel;

#[component]
pub fn ControllersStep() -> impl IntoView {
    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Irrigation controller"</h2>
            <p class="setup-step__body">
                "Which hardware fires your valves? LocalSky abstracts the "
                "controller as a port; you can swap implementations without "
                "changing zones or schedules. Pick one as default; the rest "
                "can stand by as backup or test targets."
            </p>

            <Panel title="Adapters this build ships".to_string()>
                <ul class="setup-source-list">
                    <li>
                        <strong>"OpenSprinkler Direct"</strong>
                        " (HTTP API). Talks to the OS controller on the LAN; "
                        "firmware 2.1.9+. No cloud, no HA."
                    </li>
                    <li>
                        <strong>"HA Service Call"</strong>
                        " (continuity). For deployments already driving "
                        "irrigation through Home Assistant's "
                        "opensprinkler / irrigation_unlimited / rachio integrations."
                    </li>
                    <li>
                        <strong>"DryRun"</strong>
                        " (no-op). Records intent without firing anything; "
                        "great for trying out scheduling without watering."
                    </li>
                    <li class="setup-source-list__planned">
                        <em>"Coming soon: "</em>
                        "ESPHome native, Rachio cloud."
                    </li>
                </ul>
            </Panel>

            <Panel title="What happens if I skip this".to_string()>
                <p class="setup-step__body" style="margin-bottom: 0">
                    "If you have HA_URL + HA_LONG_LIVED_TOKEN env vars set, "
                    "LocalSky synthesizes a HA Service Call controller "
                    "automatically. Otherwise, you'll need to add one under "
                    <a href="/settings/controllers" style="color: var(--accent)">"/settings/controllers"</a>
                    " before zone runs work."
                </p>
            </Panel>

            <SetupFooter
                prev=prev_step_href("controllers")
                next=next_step_href("controllers")
            />
        </div>
    }
}
