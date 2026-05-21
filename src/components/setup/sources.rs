// SourcesStep. Lightweight info pane describing the source landscape
// + showing what env_compat synthesized. Full list editor lands in a
// follow-up under /settings/sources.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::Panel;

#[component]
pub fn SourcesStep() -> impl IntoView {
    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Weather sources"</h2>
            <p class="setup-step__body">
                "LocalSky merges across any number of weather sources. A "
                "live LAN station (Tempest, Ecowitt) is the strongest "
                "signal; a forecast model (Open-Meteo, NWS) fills in "
                "horizon. The engine picks per-field winners by priority "
                "and reachability. You can configure the full list under "
                "Settings after the wizard completes."
            </p>

            <Panel title="Adapters this build ships".to_string()>
                <ul class="setup-source-list">
                    <li>
                        <strong>"Tempest UDP"</strong>
                        " (LAN). Listens for the hub's broadcast on UDP 50222."
                    </li>
                    <li>
                        <strong>"Open-Meteo"</strong>
                        " (cloud). Free, no API key. 7-day forecast + radar."
                    </li>
                    <li>
                        <strong>"DemoReplay"</strong>
                        " (synthetic). Used by LOCALSKY_DEMO=1 for demos and CI."
                    </li>
                    <li class="setup-source-list__planned">
                        <em>"Coming soon: "</em>
                        "Tempest WS cloud, Ecowitt LAN, NWS, OpenWeather, "
                        "Pirate Weather, MET Norway, HA passthrough."
                    </li>
                </ul>
            </Panel>

            <Panel title="What happens if I skip this".to_string()>
                <p class="setup-step__body" style="margin-bottom: 0">
                    "If you leave this step, LocalSky will synthesize two "
                    "default sources based on your location: Tempest UDP "
                    "listening on the LAN (no effect if you don't have a "
                    "hub), and Open-Meteo for forecast data. Both are "
                    "edit-able under "
                    <a href="/settings/sources" style="color: var(--accent)">"/settings/sources"</a>
                    " after the wizard."
                </p>
            </Panel>

            <SetupFooter
                prev=prev_step_href("sources")
                next=next_step_href("sources")
            />
        </div>
    }
}
