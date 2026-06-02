// SettingsShell — master-detail settings. The section list lives on the
// left; picking a section slides its editor into the detail pane on the
// right (filling the dead space on wide screens) with NO page navigation.
// On narrow screens it collapses to list -> detail -> back. Rebuilt on the
// v2 design (themeable Icon, elevation surfaces); emoji glyphs retired.
//
// The 12 section editors are the existing standalone components, reused
// verbatim inside the pane. Deep links to /settings/:section still render
// them standalone via their own routes.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use super::{
    SettingsAdvanced, SettingsControllers, SettingsLlm, SettingsLocation, SettingsNotifications,
    SettingsRestrictions, SettingsSchedules, SettingsSkipRules, SettingsSources, SettingsTheme,
    SettingsUnits, SettingsZones,
};
use crate::components::ui::Icon;

struct SectionLink {
    key: &'static str,
    label: &'static str,
    helptext: &'static str,
    icon: &'static str,
}

struct SectionGroup {
    title: &'static str,
    subtitle: &'static str,
    links: &'static [SectionLink],
}

const GROUPS: &[SectionGroup] = &[
    SectionGroup {
        title: "Hardware",
        subtitle: "Wire up the physical world — set once, mostly.",
        links: &[
            SectionLink {
                key: "devices",
                label: "Devices",
                helptext: "Every gateway, controller, and service, with what it provides",
                icon: "controllers",
            },
            SectionLink {
                key: "zones",
                label: "Zones",
                helptext: "Grass species, soil texture, area, sprinkler PR",
                icon: "zones",
            },
            SectionLink {
                key: "sources",
                label: "Weather sources",
                helptext: "Tempest, Ecowitt, NWS, and 18 more",
                icon: "sources",
            },
            SectionLink {
                key: "controllers",
                label: "Controllers",
                helptext: "OpenSprinkler, Rachio, Hydrawise, B-hyve, ESPHome, MQTT",
                icon: "controllers",
            },
            SectionLink {
                key: "location",
                label: "Location",
                helptext: "Lat / lon / elevation / timezone",
                icon: "location",
            },
        ],
    },
    SectionGroup {
        title: "Logic",
        subtitle: "How LocalSky decides what to do with that hardware.",
        links: &[
            SectionLink {
                key: "skip-rules",
                label: "Skip rules",
                helptext: "Rain, wind, freeze, heat-advisory thresholds",
                icon: "rules",
            },
            SectionLink {
                key: "restrictions",
                label: "Restrictions",
                helptext: "Day-of-week limits, time windows, blackout dates",
                icon: "ban",
            },
            SectionLink {
                key: "schedules",
                label: "Schedules",
                helptext: "Manual programs that override the engine",
                icon: "calendar",
            },
            SectionLink {
                key: "llm",
                label: "LLM advisor",
                helptext: "Ollama, llama.cpp, OpenAI-compatible",
                icon: "llm",
            },
        ],
    },
    SectionGroup {
        title: "App",
        subtitle: "How LocalSky talks to you + per-browser preferences.",
        links: &[
            SectionLink {
                key: "notifications",
                label: "Notifications",
                helptext: "Web Push, MQTT discovery, ntfy, Slack",
                icon: "bell",
            },
            SectionLink {
                key: "units",
                label: "Units",
                helptext: "Imperial, metric, or per-field overrides",
                icon: "units",
            },
            SectionLink {
                key: "theme",
                label: "Theme",
                helptext: "Dark, light, auto, high-contrast",
                icon: "theme",
            },
            SectionLink {
                key: "advanced",
                label: "Advanced",
                helptext: "Nerd mode, raw snapshots, rollback",
                icon: "advanced",
            },
        ],
    },
];

#[component]
pub fn SettingsHome() -> impl IntoView {
    // Selected section key. None = show the hub (and a placeholder pane on
    // desktop). SSR + hydrate's first frame both see None, so no mismatch.
    let selected: RwSignal<Option<&'static str>> = RwSignal::new(None);

    view! {
        <div class="settings-page-wrap">
            <header class="settings-hub__header">
                <p class="settings-hub__eyebrow">"Configure"</p>
                <h1 class="settings-hub__title">"Settings"</h1>
                <p class="settings-hub__sub">
                    "Per-deployment config lives in Hardware + Logic. Per-device "
                    "preferences (theme, units, nerd mode) are App-group items."
                </p>
            </header>

            <div class="settings-shell" class:has-detail=move || selected.get().is_some()>
                <div class="settings-shell__list">
                    {GROUPS.iter().map(|g| {
                    view! {
                        <section class="settings-group">
                            <div class="settings-group__head">
                                <h2 class="settings-group__title">{g.title}</h2>
                                <p class="settings-group__sub">{g.subtitle}</p>
                            </div>
                            <div class="settings-card">
                                {g.links.iter().map(|s| {
                                    let key = s.key;
                                    view! {
                                        <button
                                            type="button"
                                            class="ui-list-item ui-list-item--link settings-row"
                                            class:is-selected=move || selected.get() == Some(key)
                                            on:click=move |_| selected.set(Some(key))
                                        >
                                            <span class="ui-list-item__icon"><Icon name=s.icon size=18/></span>
                                            <span class="ui-list-item__text">
                                                <span class="ui-list-item__title">{s.label}</span>
                                                <span class="ui-list-item__subtitle">{s.helptext}</span>
                                            </span>
                                            <span class="ui-list-item__trail"><Icon name="chevron-right" size=18/></span>
                                        </button>
                                    }
                                }).collect_view()}
                            </div>
                        </section>
                    }
                }).collect_view()}
            </div>

            <div class="settings-shell__detail">
                // Header mirrors the left column's group heads so the pane
                // card drops down and top-aligns with the left section cards.
                <div class="settings-group__head">
                    <h2 class="settings-group__title">"Configuration"</h2>
                    <p class="settings-group__sub">
                        "Edit the selected section here. Changes save to your LocalSky config (and deploy on push)."
                    </p>
                </div>

                {move || match selected.get() {
                    None => view! {
                        <div class="settings-shell__placeholder">
                            <Icon name="settings" size=34/>
                            <p>"Pick a setting on the left to edit it here."</p>
                        </div>
                    }
                    .into_any(),
                    Some(key) => view! {
                        <div class="settings-shell__pane">
                            <button
                                type="button"
                                class="settings-shell__back"
                                on:click=move |_| selected.set(None)
                            >
                                <Icon name="chevron-right" size=16 class="settings-shell__back-icon".to_string()/>
                                "Back"
                            </button>
                            <div class="settings-shell__pane-body">
                                {section_view(key)}
                            </div>
                        </div>
                    }
                    .into_any(),
                }}
                </div>
            </div>
        </div>
    }
}

/// Render the editor component for a section key into the detail pane.
fn section_view(key: &str) -> leptos::prelude::AnyView {
    match key {
        "zones" => view! { <SettingsZones/> }.into_any(),
        "sources" => view! { <SettingsSources/> }.into_any(),
        "controllers" => view! { <SettingsControllers/> }.into_any(),
        "location" => view! { <SettingsLocation/> }.into_any(),
        "skip-rules" => view! { <SettingsSkipRules/> }.into_any(),
        "restrictions" => view! { <SettingsRestrictions/> }.into_any(),
        "schedules" => view! { <SettingsSchedules/> }.into_any(),
        "llm" => view! { <SettingsLlm/> }.into_any(),
        "notifications" => view! { <SettingsNotifications/> }.into_any(),
        "units" => view! { <SettingsUnits/> }.into_any(),
        "theme" => view! { <SettingsTheme/> }.into_any(),
        "advanced" => view! { <SettingsAdvanced/> }.into_any(),
        _ => view! { <div/> }.into_any(),
    }
}
