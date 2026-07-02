// SettingsHelp. Documentation and support links, one click from any
// settings session. External docs open in a new tab; the wizard link
// stays in-app.

use leptos::prelude::*;

use crate::components::ui::Icon;
use crate::docs::{doc_url, ISSUES_URL, REPO_URL, SITE_BASE};

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
                    <span>"ET, soil buckets, rules, scheduling"</span>
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
                <a class="about-link" href=SITE_BASE target="_blank" rel="noopener">
                    <Icon name="external" size=18/>
                    <strong>"All documentation"</strong>
                    <span>"localsky.io"</span>
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
