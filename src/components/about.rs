// /about page. Version + license + links. Tiny but expected.

use leptos::prelude::*;

use crate::components::ui::Panel;

#[component]
pub fn AboutPage() -> impl IntoView {
    let version = env!("CARGO_PKG_VERSION");
    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <h1 class="settings-page__title">"About LocalSky"</h1>
                <p class="settings-page__subtitle">
                    "Local-first weather and irrigation control."
                </p>
            </header>

            <Panel title="Build".to_string()>
                <ul class="setup-source-list">
                    <li>
                        <strong>"Version: "</strong>
                        {version}
                    </li>
                    <li>
                        <strong>"License: "</strong>
                        "Apache-2.0"
                    </li>
                    <li>
                        <strong>"Source: "</strong>
                        <a
                            href="https://github.com/silenthooligan/localsky"
                            target="_blank"
                            rel="noopener noreferrer"
                            style="color: var(--accent)"
                        >
                            "github.com/silenthooligan/localsky"
                        </a>
                    </li>
                </ul>
            </Panel>

            <Panel title="Acknowledgements".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.75rem">
                    "Built on decades of agronomy + meteorology + open-source software:"
                </p>
                <ul class="setup-source-list">
                    <li>
                        "FAO Irrigation and Drainage Paper No. 56 "
                        "(Allen et al., 1998) for the Penman-Monteith reference ET."
                    </li>
                    <li>
                        "ASCE-EWRI Standardized Reference Evapotranspiration Equation (2005)."
                    </li>
                    <li>
                        "UF/IFAS Extension publications on Florida turfgrass species "
                        "(ENH6, ENH8, ENH11, ENH19, ENH62, ENH1115)."
                    </li>
                    <li>
                        "USDA NRCS National Irrigation Guide (Part 652) for soil "
                        "infiltration + available-water tables."
                    </li>
                    <li>
                        "Home Assistant Smart Irrigation + Irrigation Unlimited "
                        "integrations as the prior art that informed this clean-room rewrite."
                    </li>
                    <li>
                        "Open-Meteo, RainViewer, Leaflet, Leptos, rumqttc, rusqlite, "
                        "tokio, reqwest, and the broader Rust + WASM ecosystem."
                    </li>
                </ul>
            </Panel>

            <Panel title="Links".to_string()>
                <ul class="setup-source-list">
                    <li>
                        <a href="/setup" style="color: var(--accent)">"Run the setup wizard"</a>
                        " (only mounts when no config file exists)"
                    </li>
                    <li>
                        <a href="/settings" style="color: var(--accent)">"Settings"</a>
                    </li>
                    <li>
                        <a
                            href="https://github.com/silenthooligan/localsky/blob/main/docs/getting-started.md"
                            target="_blank"
                            rel="noopener noreferrer"
                            style="color: var(--accent)"
                        >
                            "Getting started guide"
                        </a>
                    </li>
                    <li>
                        <a
                            href="https://github.com/silenthooligan/localsky/issues"
                            target="_blank"
                            rel="noopener noreferrer"
                            style="color: var(--accent)"
                        >
                            "Report a bug / request a feature"
                        </a>
                    </li>
                </ul>
            </Panel>
        </main>
    }
}
