// SettingsHelp. Documentation and support links, one click from any
// settings session. External docs open in a new tab; the wizard link
// stays in-app.

use leptos::prelude::*;

use crate::components::ui::{ChartKey, Icon, Panel};
use crate::docs::{doc_url, ISSUES_URL, REPO_URL};

#[component]
pub fn SettingsHelp() -> impl IntoView {
    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Help & documentation"</h1>
                <p class="settings-page__subtitle">
                    "Guides for every stage, from first install to deep tuning."
                </p>
            </header>
            <Panel title="Reading your dashboard">
                <p>"The key beside each plot names its measurement and units. Trend charts keep the same colors across screens."</p>
                <div class="chart-key">
                    <ChartKey label="Watering" color="var(--chart-water)"/>
                    <ChartKey label="Rain" color="var(--chart-rain)"/>
                    <ChartKey label="Temperature" color="var(--chart-temperature)"/>
                    <ChartKey label="Soil moisture" color="var(--chart-soil)"/>
                    <ChartKey label="Wind speed" color="var(--chart-wind)"/>
                    <ChartKey label="Pressure" color="var(--chart-pressure)"/>
                    <ChartKey label="Plant water use (ET)" color="var(--chart-et)"/>
                </div>
                <p>"Rain is teal before the threshold and amber beyond it. Rain beyond the ET goal is green. The watering status explains the final decision."</p>
                <p>"Blue: watering. Green: completed. Amber: skipped or paused. Gray: off or unknown. Red: a device fault. Card header colors identify sections."</p>
                <p>"Forecasts and projections are estimates. A missing reading is not zero. Radar and UV use their own labeled weather scales."</p>
                <p>"Explore History charts with a pointer or focus the plot and use the arrow keys. Home and End jump to the first and last day."</p>
            </Panel>
            <div class="about-links">
                <a class="about-link" href=doc_url("getting-started") target="_blank" rel="noopener">
                    <Icon name="download" size=18/>
                    <strong>"Installation guide"</strong>
                    <span>"Docker, first boot, the wizard"</span>
                </a>
                <a class="about-link" href=doc_url("faq") target="_blank" rel="noopener">
                    <Icon name="info" size=18/>
                    <strong>"FAQ and glossary"</strong>
                    <span>"Quick answers and the vocabulary"</span>
                </a>
                <a class="about-link" href=doc_url("irrigation-engine") target="_blank" rel="noopener">
                    <Icon name="gauge" size=18/>
                    <strong>"How watering decisions work"</strong>
                    <span>"ET, the weekly water balance, rules, scheduling"</span>
                </a>
                <a class="about-link" href=doc_url("migrating-from-ha") target="_blank" rel="noopener">
                    <Icon name="home" size=18/>
                    <strong>"Migrating from Home Assistant"</strong>
                    <span>"Move the watering brain here safely"</span>
                </a>
                <a class="about-link" href=doc_url("api") target="_blank" rel="noopener">
                    <Icon name="advanced" size=18/>
                    <strong>"API reference"</strong>
                    <span>"REST + SSE for builders"</span>
                </a>
                // Bundled, same-origin manual: offline-safe and matched to
                // the running build (see docs.rs).
                <a class="about-link" href=doc_url("index") target="_blank" rel="noopener">
                    <Icon name="external" size=18/>
                    <strong>"All documentation"</strong>
                    <span>"The full manual, bundled with this install"</span>
                </a>
                <a class="about-link" href=ISSUES_URL target="_blank" rel="noopener">
                    <Icon name="alert-triangle" size=18/>
                    <strong>"Report a problem"</strong>
                    <span>"Bugs and feature requests on GitHub"</span>
                </a>
                <a class="about-link" href=REPO_URL target="_blank" rel="noopener">
                    <Icon name="info" size=18/>
                    <strong>"Source code"</strong>
                    <span>"Apache-2.0 on GitHub"</span>
                </a>
            </div>
        </div>
    }
}
