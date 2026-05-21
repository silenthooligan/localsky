// SettingsHome. Section list. Each item routes to a sub-page that
// edits its slice of config (or per-device prefs in localStorage).

use leptos::prelude::*;

use crate::components::ui::Panel;

struct SectionLink {
    href: &'static str,
    label: &'static str,
    helptext: &'static str,
    icon: &'static str,
}

const SECTIONS: &[SectionLink] = &[
    SectionLink {
        href: "/settings/theme",
        label: "Theme",
        helptext: "Dark, light, auto, or high-contrast",
        icon: "🎨",
    },
    SectionLink {
        href: "/settings/units",
        label: "Units",
        helptext: "Imperial, metric, or per-field overrides",
        icon: "📏",
    },
    SectionLink {
        href: "/settings/location",
        label: "Location",
        helptext: "Latitude, longitude, elevation, timezone",
        icon: "📍",
    },
    SectionLink {
        href: "/settings/sources",
        label: "Weather sources",
        helptext: "Tempest, Open-Meteo, Ecowitt, NWS, others",
        icon: "🌤️",
    },
    SectionLink {
        href: "/settings/controllers",
        label: "Irrigation controllers",
        helptext: "OpenSprinkler, Home Assistant, Rachio, ESPHome",
        icon: "💧",
    },
    SectionLink {
        href: "/settings/zones",
        label: "Zones",
        helptext: "Grass species, soil texture, area, sprinkler PR",
        icon: "🌱",
    },
    SectionLink {
        href: "/settings/llm",
        label: "LLM advisor",
        helptext: "Ollama, llama.cpp, OpenAI-compatible",
        icon: "🤖",
    },
    SectionLink {
        href: "/settings/notifications",
        label: "Notifications",
        helptext: "Web Push, MQTT discovery, ntfy, Slack",
        icon: "🔔",
    },
    SectionLink {
        href: "/settings/skip-rules",
        label: "Skip rules",
        helptext: "Rain, wind, freeze, heat-advisory thresholds",
        icon: "🎯",
    },
    SectionLink {
        href: "/settings/advanced",
        label: "Advanced",
        helptext: "Nerd mode, raw snapshots, rollback",
        icon: "⚙️",
    },
];

#[component]
pub fn SettingsHome() -> impl IntoView {
    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <h1 class="settings-page__title">"Settings"</h1>
                <p class="settings-page__subtitle">
                    "Per-deployment configuration lives here. Per-device "
                    "preferences (theme, units, nerd mode) are saved on this "
                    "browser only."
                </p>
            </header>

            <Panel title="".to_string()>
                <ul class="settings-list">
                    {SECTIONS.iter().map(|s| {
                        view! {
                            <li>
                                <a class="settings-list__item" href=s.href>
                                    <span class="settings-list__icon" aria-hidden="true">{s.icon}</span>
                                    <span class="settings-list__text">
                                        <span class="settings-list__label">{s.label}</span>
                                        <span class="settings-list__helptext">{s.helptext}</span>
                                    </span>
                                    <span class="settings-list__chevron" aria-hidden="true">"›"</span>
                                </a>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            </Panel>
        </main>
    }
}
