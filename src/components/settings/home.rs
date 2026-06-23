// SettingsShell, master-detail settings. The section list lives on the
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

use super::help::SettingsHelp;
use super::{
    SettingsAccount, SettingsAdvanced, SettingsControllers, SettingsDevices, SettingsHomeAssistant,
    SettingsLlm, SettingsLocation, SettingsNotifications, SettingsRadar, SettingsRestrictions,
    SettingsSchedules, SettingsSensors, SettingsSkipRules, SettingsSources, SettingsTheme,
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
        subtitle: "Wire up the physical world, set once, mostly.",
        links: &[
            SectionLink {
                key: "devices",
                label: "Devices",
                helptext: "Every gateway, controller, and service, with what it provides",
                icon: "controllers",
            },
            SectionLink {
                key: "sensors",
                label: "Sensors",
                helptext: "Soil probes and flow meters: live readings, battery, zone binding",
                icon: "gauge",
            },
            SectionLink {
                key: "home-assistant",
                label: "Home Assistant",
                helptext: "The bidirectional link: what flows in, what HA consumes",
                icon: "home",
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
                helptext: "Tempest, Ecowitt, Open-Meteo, and 18 more",
                icon: "sources",
            },
            SectionLink {
                key: "controllers",
                label: "Controllers",
                helptext: "OpenSprinkler, DIY/ESP32 (HTTP or MQTT), Rachio, Hydrawise, B-hyve, Home Assistant",
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
                key: "account",
                label: "Account",
                helptext: "Owner login + API tokens for integrations",
                icon: "settings",
            },
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
                key: "radar",
                label: "Radar",
                helptext: "Imagery providers + default map layers",
                icon: "sources",
            },
            SectionLink {
                key: "theme",
                label: "Theme",
                helptext: "Dark, light, auto, high-contrast",
                icon: "theme",
            },
            SectionLink {
                key: "help",
                label: "Help & documentation",
                helptext: "Installation guide, manual, migration, API",
                icon: "info",
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
                    None => view! { <SettingsOverview selected=selected/> }.into_any(),
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

/// Default detail pane: a live system overview instead of dead space.
/// Polls /api/v1/health once on mount (hydrate-only; SSR + first frame
/// render the skeleton, so the DOM matches) and shows version, uptime,
/// counts with per-source freshness dots, plus quick links into the
/// relevant sections.
#[component]
fn SettingsOverview(selected: RwSignal<Option<&'static str>>) -> impl IntoView {
    let health: RwSignal<serde_json::Value> = RwSignal::new(serde_json::Value::Null);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/v1/health").send().await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    health.set(v);
                }
            }
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = health;

    move || {
        let h = health.get();
        if h.is_null() {
            return view! {
                <div class="settings-overview">
                    <crate::components::ui::SkeletonRows count=4/>
                </div>
            }
            .into_any();
        }
        let version = h
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let status = h
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let uptime_s = h.get("uptime_s").and_then(|v| v.as_u64()).unwrap_or(0);
        let uptime = if uptime_s >= 86_400 {
            format!("{}d {}h", uptime_s / 86_400, (uptime_s % 86_400) / 3_600)
        } else if uptime_s >= 3_600 {
            format!("{}h {}m", uptime_s / 3_600, (uptime_s % 3_600) / 60)
        } else {
            format!("{}m", uptime_s / 60)
        };
        let status_tone = match status.as_str() {
            "ok" => "settings-overview__status is-ok",
            "degraded" => "settings-overview__status is-warn",
            _ => "settings-overview__status",
        };

        let sources = h
            .get("sources")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let controllers = h
            .get("controllers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let source_rows = sources
            .iter()
            .map(|s| {
                let id = s
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let st = s.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                let dot = match st {
                    "fresh" => "settings-overview__dot is-fresh",
                    "stale" => "settings-overview__dot is-stale",
                    _ => "settings-overview__dot is-offline",
                };
                view! {
                    <li class="settings-overview__row">
                        <span class=dot aria-hidden="true"></span>
                        <span class="settings-overview__row-name">{id}</span>
                        <span class="settings-overview__row-meta">{st.to_string()}</span>
                    </li>
                }
            })
            .collect_view();
        let controller_rows = controllers
            .iter()
            .map(|c| {
                let id = c
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let kind = c
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_default = c.get("default").and_then(|v| v.as_bool()) == Some(true);
                view! {
                    <li class="settings-overview__row">
                        <span class="settings-overview__dot is-fresh" aria-hidden="true"></span>
                        <span class="settings-overview__row-name">{id}</span>
                        <span class="settings-overview__row-meta">
                            {kind}
                            {if is_default { " · default" } else { "" }}
                        </span>
                    </li>
                }
            })
            .collect_view();

        view! {
            <div class="settings-overview">
                <div class="settings-overview__stats">
                    <div class="settings-overview__stat">
                        <span class="settings-overview__stat-label">"Status"</span>
                        <span class=status_tone>{status}</span>
                    </div>
                    <div class="settings-overview__stat">
                        <span class="settings-overview__stat-label">"Version"</span>
                        <span class="settings-overview__stat-value">{version}</span>
                    </div>
                    <div class="settings-overview__stat">
                        <span class="settings-overview__stat-label">"Uptime"</span>
                        <span class="settings-overview__stat-value">{uptime}</span>
                    </div>
                </div>

                <div class="settings-overview__section">
                    <div class="settings-overview__section-head">
                        <h3>"Weather sources"</h3>
                        <button type="button" class="settings-overview__jump"
                            on:click=move |_| selected.set(Some("sources"))>"Edit"</button>
                    </div>
                    <ul class="settings-overview__list">{source_rows}</ul>
                </div>

                <div class="settings-overview__section">
                    <div class="settings-overview__section-head">
                        <h3>"Controllers"</h3>
                        <button type="button" class="settings-overview__jump"
                            on:click=move |_| selected.set(Some("controllers"))>"Edit"</button>
                    </div>
                    <ul class="settings-overview__list">{controller_rows}</ul>
                </div>

                <p class="settings-overview__hint">
                    "Pick a section on the left to edit it here. The Sensors hub shows live "
                    "per-field readings; the Devices section maps everything that feeds this box."
                </p>
            </div>
        }
        .into_any()
    }
}

/// Render the editor component for a section key into the detail pane.
fn section_view(key: &str) -> leptos::prelude::AnyView {
    match key {
        "devices" => view! { <SettingsDevices/> }.into_any(),
        "sensors" => view! { <SettingsSensors/> }.into_any(),
        "home-assistant" => view! { <SettingsHomeAssistant/> }.into_any(),
        "help" => view! { <SettingsHelp/> }.into_any(),
        "zones" => view! { <SettingsZones/> }.into_any(),
        "sources" => view! { <SettingsSources/> }.into_any(),
        "controllers" => view! { <SettingsControllers/> }.into_any(),
        "location" => view! { <SettingsLocation/> }.into_any(),
        "skip-rules" => view! { <SettingsSkipRules/> }.into_any(),
        "restrictions" => view! { <SettingsRestrictions/> }.into_any(),
        "schedules" => view! { <SettingsSchedules/> }.into_any(),
        "llm" => view! { <SettingsLlm/> }.into_any(),
        "account" => view! { <SettingsAccount/> }.into_any(),
        "notifications" => view! { <SettingsNotifications/> }.into_any(),
        "units" => view! { <SettingsUnits/> }.into_any(),
        "radar" => view! { <SettingsRadar/> }.into_any(),
        "theme" => view! { <SettingsTheme/> }.into_any(),
        "advanced" => view! { <SettingsAdvanced/> }.into_any(),
        _ => view! { <div/> }.into_any(),
    }
}
