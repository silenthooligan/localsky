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
use leptos_router::hooks::{use_location, use_navigate};

use super::help::SettingsHelp;
use super::{
    SettingsAccount, SettingsAdvanced, SettingsControllers, SettingsDataSources, SettingsDevices,
    SettingsHomeAssistant, SettingsLlm, SettingsLocation, SettingsNotifications, SettingsRadar,
    SettingsRestrictions, SettingsSchedules, SettingsSensors, SettingsSkipRules, SettingsTheme,
    SettingsUnits, SettingsZones,
};
use crate::components::ui::Icon;

struct SectionLink {
    key: &'static str,
    label: &'static str,
    helptext: &'static str,
    icon: &'static str,
    /// Entity identity ("source"/"sensor"/"controller"/"zone") -> a left
    /// color-stripe so the four hardware concepts are visually distinct.
    entity: Option<&'static str>,
}

struct SectionGroup {
    title: &'static str,
    subtitle: &'static str,
    links: &'static [SectionLink],
}

const GROUPS: &[SectionGroup] = &[
    SectionGroup {
        title: "Hardware",
        // One front door (Devices) for everything LocalSky talks to; Zones is
        // the yard it waters. Sources, sensors and controllers are no longer
        // separate doors -- they live inside Devices (a source carries its
        // sensors; a controller runs its zones).
        subtitle: "Everything LocalSky talks to, and the yard it waters.",
        links: &[
            SectionLink {
                key: "devices",
                label: "Devices",
                helptext: "Add and edit your weather sources + controllers, see the sensors each source carries, and pick which source provides each reading",
                icon: "controllers",
                entity: None,
            },
            // "Data sources" is no longer its own rail door: the per-field source
            // picker is folded into the Devices hub (design #4), so there is one
            // home for it. The /settings/data-sources route + ?section=data-sources
            // still resolve (redirect / shell pane) for any stale deep-link.
            SectionLink {
                key: "zones",
                label: "Zones",
                helptext: "An AREA of yard: grass species, soil texture, area, sprinkler rate",
                icon: "zones",
                entity: Some("zone"),
            },
            SectionLink {
                key: "home-assistant",
                label: "Home Assistant",
                helptext: "The bidirectional link: what flows in, what HA consumes",
                icon: "home",
                entity: None,
            },
            SectionLink {
                key: "location",
                label: "Location",
                helptext: "Lat / lon / elevation / timezone",
                icon: "location",
                entity: None,
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
                entity: None,
            },
            SectionLink {
                key: "restrictions",
                label: "Restrictions",
                helptext: "Day-of-week limits, time windows, blackout dates",
                icon: "ban",
                entity: None,
            },
            SectionLink {
                key: "schedules",
                label: "Schedules",
                helptext: "Manual programs that override the engine",
                icon: "calendar",
                entity: None,
            },
            SectionLink {
                key: "llm",
                label: "LLM advisor",
                helptext: "Ollama, llama.cpp, OpenAI-compatible",
                icon: "llm",
                entity: None,
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
                entity: None,
            },
            SectionLink {
                key: "notifications",
                label: "Notifications",
                helptext: "Web Push, MQTT discovery, ntfy, Slack",
                icon: "bell",
                entity: None,
            },
            SectionLink {
                key: "units",
                label: "Units",
                helptext: "Household default (imperial or metric), with optional per-device overrides",
                icon: "units",
                entity: None,
            },
            SectionLink {
                key: "radar",
                label: "Radar",
                helptext: "Imagery providers + default map layers",
                icon: "sources",
                entity: None,
            },
            SectionLink {
                key: "theme",
                label: "Theme",
                helptext: "Dark, light, auto, high-contrast",
                icon: "theme",
                entity: None,
            },
            SectionLink {
                key: "help",
                label: "Help & documentation",
                helptext: "Installation guide, manual, migration, API",
                icon: "info",
                entity: None,
            },
            SectionLink {
                key: "advanced",
                label: "Advanced",
                helptext: "Nerd mode, raw snapshots, rollback",
                icon: "advanced",
                entity: None,
            },
        ],
    },
];

/// Map a `?section=` query value to its canonical static key, so a bogus query
/// can't open a phantom pane. None = the hub.
fn section_key(s: &str) -> Option<&'static str> {
    const KEYS: &[&str] = &[
        "devices",
        "data-sources",
        "sensors",
        "home-assistant",
        "zones",
        "controllers",
        "location",
        "skip-rules",
        "restrictions",
        "schedules",
        "llm",
        "account",
        "notifications",
        "units",
        "radar",
        "theme",
        "help",
        "advanced",
    ];
    KEYS.iter().copied().find(|&k| k == s)
}

#[component]
pub fn SettingsHome() -> impl IntoView {
    // The open section is URL state (?section=KEY), not a bare signal: pushing a
    // real history entry per section means the phone's back gesture returns to
    // the settings menu instead of leaving settings entirely, and a section
    // deep-links. SSR + hydrate both derive it from the same URL, so no
    // mismatch. None = the hub.
    let loc = use_location();
    let selected = Signal::derive(move || {
        loc.search
            .get()
            .trim_start_matches('?')
            .split('&')
            .find_map(|kv| kv.strip_prefix("section=").map(str::to_string))
            .and_then(|v| section_key(&v))
    });
    let nav_go = use_navigate();
    let go: Callback<&'static str> = Callback::new(move |key: &'static str| {
        nav_go(&format!("/settings?section={key}"), Default::default());
    });
    let nav_back = use_navigate();
    let go_home: Callback<()> = Callback::new(move |_| {
        nav_back("/settings", Default::default());
    });

    // Selecting a section swaps the detail pane in place (URL state, no full
    // navigation), so move focus into the pane so SR/keyboard users follow.
    // Skip the first run so the initial render (deep-link or hub) doesn't
    // steal focus on load.
    let detail_pane: NodeRef<leptos::html::Div> = NodeRef::new();
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |prev: Option<Option<&'static str>>| {
            let cur = selected.get();
            if let Some(prev_sel) = prev {
                if prev_sel != cur && cur.is_some() {
                    if let Some(el) = detail_pane.get() {
                        let _ = el.focus();
                    }
                }
            }
            cur
        });
    }

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
                            <div class="settings-nav-card">
                                {g.links.iter().map(|s| {
                                    let key = s.key;
                                    let cls = match s.entity {
                                        Some(e) => format!(
                                            "ui-list-item ui-list-item--link settings-row entity-stripe entity-stripe--{e}"
                                        ),
                                        None => "ui-list-item ui-list-item--link settings-row".to_string(),
                                    };
                                    view! {
                                        <button
                                            type="button"
                                            class=cls
                                            class:is-selected=move || selected.get() == Some(key)
                                            on:click=move |_| go.run(key)
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

            <div
                class="settings-shell__detail"
                node_ref=detail_pane
                tabindex="-1"
                aria-live="polite"
            >
                // Header mirrors the left column's group heads so the pane
                // card drops down and top-aligns with the left section cards.
                <div class="settings-group__head">
                    <h2 class="settings-group__title">"Configuration"</h2>
                    <p class="settings-group__sub">
                        "Edit the selected section here. Changes save to your LocalSky config (and deploy on push)."
                    </p>
                </div>

                {move || match selected.get() {
                    None => view! { <SettingsOverview go=go/> }.into_any(),
                    Some(key) => view! {
                        <div class="settings-shell__pane">
                            <button
                                type="button"
                                class="settings-shell__back"
                                on:click=move |_| go_home.run(())
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
fn SettingsOverview(go: Callback<&'static str>) -> impl IntoView {
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

        // First-run guidance: a short "what's left" checklist driven by real
        // state, shown until the hardware is configured, so a beginner who just
        // finished the wizard has a path instead of a flat wall of sections.
        let has_source = !sources.is_empty();
        let has_controller = !controllers.is_empty();
        let has_default = controllers
            .iter()
            .any(|c| c.get("default").and_then(|v| v.as_bool()) == Some(true));
        let setup_done = has_source && has_controller && has_default;
        let done_n = [has_source, has_controller, has_default]
            .iter()
            .filter(|b| **b)
            .count();

        view! {
            <div class="settings-overview">
                {(!setup_done).then(|| view! {
                    <div class="settings-checklist">
                        <div class="settings-checklist__head">
                            <h3 class="settings-checklist__title">"Finish setting up"</h3>
                            <span class="settings-checklist__count">{format!("{done_n} of 3")}</span>
                        </div>
                        <div class="settings-checklist__bar" aria-hidden="true">
                            <div
                                class="settings-checklist__bar-fill"
                                style=format!("width: {}%", done_n * 100 / 3)
                            ></div>
                        </div>
                        <ul class="settings-checklist__list">
                            <li class=if has_source { "settings-checklist__item is-done" } else { "settings-checklist__item" }>
                                <span class="settings-checklist__mark" aria-hidden="true">
                                    {if has_source { "\u{2713}" } else { "\u{25CB}" }}
                                </span>
                                <span class="settings-checklist__label">"Add a weather source"</span>
                                {(!has_source).then(|| view! {
                                    <button type="button" class="settings-overview__jump settings-checklist__go"
                                        on:click=move |_| go.run("devices")>"Set up"</button>
                                })}
                            </li>
                            <li class=if has_controller { "settings-checklist__item is-done" } else { "settings-checklist__item" }>
                                <span class="settings-checklist__mark" aria-hidden="true">
                                    {if has_controller { "\u{2713}" } else { "\u{25CB}" }}
                                </span>
                                <span class="settings-checklist__label">"Add a controller"</span>
                                {(!has_controller).then(|| view! {
                                    <button type="button" class="settings-overview__jump settings-checklist__go"
                                        on:click=move |_| go.run("devices")>"Set up"</button>
                                })}
                            </li>
                            <li class=if has_default { "settings-checklist__item is-done" } else { "settings-checklist__item" }>
                                <span class="settings-checklist__mark" aria-hidden="true">
                                    {if has_default { "\u{2713}" } else { "\u{25CB}" }}
                                </span>
                                <span class="settings-checklist__label">"Pick a default controller"</span>
                                {(!has_default).then(|| view! {
                                    <button type="button" class="settings-overview__jump settings-checklist__go"
                                        on:click=move |_| go.run("devices")>"Set up"</button>
                                })}
                            </li>
                        </ul>
                    </div>
                })}
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
                        // Land in the unified Devices hub, not the orphaned
                        // raw-JSON sources editor (legacy /settings/sources now
                        // redirects here anyway), so every "Edit" stays in one
                        // place inside the settings shell.
                        <button type="button" class="settings-overview__jump"
                            on:click=move |_| go.run("devices")>"Edit"</button>
                    </div>
                    <ul class="settings-overview__list">{source_rows}</ul>
                </div>

                <div class="settings-overview__section">
                    <div class="settings-overview__section-head">
                        <h3>"Controllers"</h3>
                        // Same: the Devices hub owns controllers too (the legacy
                        // /settings/controllers route redirects here).
                        <button type="button" class="settings-overview__jump"
                            on:click=move |_| go.run("devices")>"Edit"</button>
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
        "data-sources" => view! { <SettingsDataSources/> }.into_any(),
        "sensors" => view! { <SettingsSensors/> }.into_any(),
        "home-assistant" => view! { <SettingsHomeAssistant/> }.into_any(),
        "help" => view! { <SettingsHelp/> }.into_any(),
        "zones" => view! { <SettingsZones/> }.into_any(),
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
