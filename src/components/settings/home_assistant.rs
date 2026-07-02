// SettingsHomeAssistant. The Home Assistant relationship as a product
// surface: a hero status card, then one card per capability written in
// user outcomes (what it does FOR you), each with a status chip and the
// action right there: switch the engine's data source, remove a dead
// bridge, jump into the matching editor, open the setup guide. Reads
// the `ha` block on /api/v1/health; actions write through the normal
// config PUT (snapshot + rollback machinery applies).

use leptos::prelude::*;

#[cfg(feature = "hydrate")]
use crate::components::ui::use_toast;
use crate::components::ui::{Icon, SkeletonRows};
use crate::docs::doc_url;

/// One integration capability card: icon, name, plain-language meaning,
/// status chip, and the action buttons that belong to it.
#[component]
fn HaCard(
    icon: &'static str,
    title: &'static str,
    /// What this does for the user, in one plain sentence.
    #[prop(into)]
    meaning: String,
    /// Current state in one short phrase (chip text).
    #[prop(into)]
    chip: String,
    /// "on" (green) | "off" (muted) | "warn" (amber).
    #[prop(into)]
    tone: String,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="ha-card">
            <span class=format!("ha-card__icon ha-card__icon--{tone}")>
                <Icon name=icon size=18/>
            </span>
            <div class="ha-card__body">
                <div class="ha-card__top">
                    <span class="ha-card__title">{title}</span>
                    <span class=format!("ha-chip ha-chip--{tone}")>
                        <span class="ha-chip__dot" aria-hidden="true"></span>
                        {chip}
                    </span>
                </div>
                <p class="ha-card__meaning">{meaning}</p>
                <div class="ha-card__actions">{children()}</div>
            </div>
        </div>
    }
}

#[component]
pub fn SettingsHomeAssistant() -> impl IntoView {
    let ha: RwSignal<Option<serde_json::Value>> = RwSignal::new(None);
    let loaded = RwSignal::new(false);
    let reload = RwSignal::new(0u32);
    let busy = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let _ = reload.get();
        leptos::task::spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/v1/health").send().await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    ha.set(v.get("ha").cloned());
                }
            }
            loaded.set(true);
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = (ha, loaded, reload, busy);

    // Remove a passthrough source by id (read-modify-write the config).
    let remove_source = move |id: String| {
        #[cfg(feature = "hydrate")]
        {
            if busy.get_untracked() {
                return;
            }
            if let Some(win) = web_sys::window() {
                let ok = win
                    .confirm_with_message(&format!(
                        "Remove the '{id}' bridge? It currently feeds nothing, so no data is lost. A config snapshot is kept for rollback."
                    ))
                    .unwrap_or(false);
                if !ok {
                    return;
                }
            }
            busy.set(true);
            leptos::task::spawn_local(async move {
                let result = async {
                    let resp = gloo_net::http::Request::get("/api/config")
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    let mut cfg = resp
                        .json::<serde_json::Value>()
                        .await
                        .map_err(|e| e.to_string())?;
                    if let Some(arr) = cfg.get_mut("sources").and_then(|v| v.as_array_mut()) {
                        arr.retain(|s| s.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
                    }
                    let resp = gloo_net::http::Request::put("/api/config")
                        .json(&cfg)
                        .map_err(|e| e.to_string())?
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    if resp.ok() {
                        Ok(())
                    } else {
                        Err(format!("HTTP {}", resp.status()))
                    }
                }
                .await;
                match result {
                    Ok(()) => {
                        use_toast().success("Bridge removed. The engine reloads on the next tick.");
                        reload.update(|n| *n += 1);
                    }
                    Err(e) => use_toast().error(format!("Remove failed: {e}")),
                }
                busy.set(false);
            });
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = id;
    };

    // Switch the irrigation engine's data source (deployment.mode).
    let switch_mode = move |to_standalone: bool| {
        #[cfg(feature = "hydrate")]
        {
            if busy.get_untracked() {
                return;
            }
            if let Some(win) = web_sys::window() {
                let msg = if to_standalone {
                    "Switch watering decisions to LocalSky's native engine?\n\nLocalSky will compute everything from its own sources (station, gateway, forecast). Home Assistant keeps receiving live data through the integration. You can switch back any time."
                } else {
                    "Make Home Assistant the engine's data source again?\n\nWatering decisions will be computed from the entities LocalSky reads out of HA."
                };
                let ok = win.confirm_with_message(msg).unwrap_or(false);
                if !ok {
                    return;
                }
            }
            busy.set(true);
            leptos::task::spawn_local(async move {
                let result = async {
                    let resp = gloo_net::http::Request::get("/api/config")
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    let mut cfg = resp
                        .json::<serde_json::Value>()
                        .await
                        .map_err(|e| e.to_string())?;
                    if let Some(dep) = cfg.get_mut("deployment").and_then(|d| d.as_object_mut()) {
                        dep.insert(
                            "mode".into(),
                            if to_standalone {
                                "standalone".into()
                            } else {
                                "home_assistant".into()
                            },
                        );
                    }
                    let resp = gloo_net::http::Request::put("/api/config")
                        .json(&cfg)
                        .map_err(|e| e.to_string())?
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    if resp.ok() {
                        Ok(())
                    } else {
                        Err(format!("HTTP {}", resp.status()))
                    }
                }
                .await;
                match result {
                    Ok(()) => {
                        use_toast().success(
                            "Engine source updated. Takes effect after the next restart of LocalSky.",
                        );
                        reload.update(|n| *n += 1);
                    }
                    Err(e) => use_toast().error(format!("Switch failed: {e}")),
                }
                busy.set(false);
            });
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = to_standalone;
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Home Assistant"</h1>
                <p class="settings-page__subtitle">
                    "Don't use Home Assistant? Skip this page entirely; LocalSky "
                    "never needs it. If you do, one integration connects them."
                </p>
            </header>

            {move || {
                if !loaded.get() {
                    return view! { <SkeletonRows count=3/> }.into_any();
                }
                let h = ha.get().unwrap_or(serde_json::Value::Null);

                let reachable = h.get("reachable").and_then(|v| v.as_bool()).unwrap_or(false);
                let env_configured = h.get("env_configured").and_then(|v| v.as_bool()).unwrap_or(false);
                let snapshot_source = h
                    .get("snapshot_source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("standalone")
                    .to_string();
                let _hacs_epoch = h.get("hacs_last_seen_epoch").and_then(|v| v.as_i64()).unwrap_or(0);
                let hacs_streaming = h.get("hacs_streaming").and_then(|v| v.as_bool()).unwrap_or(false);
                let mqtt = h.get("mqtt_discovery").and_then(|v| v.as_bool()).unwrap_or(false);
                let passthrough: Vec<(String, usize)> = h
                    .get("passthrough_sources")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|p| {
                                let a = p.as_array()?;
                                Some((a.first()?.as_str()?.to_string(), a.get(1)?.as_u64()? as usize))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let service_controllers: Vec<String> = h
                    .get("service_call_controllers")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();

                let connected = reachable || hacs_streaming;
                let ha_mode = snapshot_source == "home_assistant";

                view! {
                    // Hero: identity + one sentence that tells the user
                    // where they stand.
                    <div class="ha-hero" class:ha-hero--ok=connected>
                        <span class="ha-hero__icon"><Icon name="home" size=24/></span>
                        <div class="ha-hero__text">
                            <div class="ha-hero__row">
                                <strong>"Home Assistant"</strong>
                                <span class=if connected { "ha-chip ha-chip--on" } else if env_configured { "ha-chip ha-chip--warn" } else { "ha-chip ha-chip--off" }>
                                    <span class="ha-chip__dot" aria-hidden="true"></span>
                                    {if connected { "Connected" } else if env_configured { "Not responding" } else { "Not linked" }}
                                </span>
                            </div>
                            <p>
                                {if hacs_streaming {
                                    "Home Assistant is receiving LocalSky's live data right now. Everything below is optional fine-tuning."
                                } else if connected {
                                    "The link is up. Install the LocalSky integration in HA to stream readings and controls into your dashboards."
                                } else if env_configured {
                                    "LocalSky is configured to reach Home Assistant but isn't getting answers. Check that HA is up and the token is valid."
                                } else {
                                    "Not paired. If you use Home Assistant, install the LocalSky integration there; it finds this device on your network by itself."
                                }}
                            </p>
                        </div>
                    </div>

                    {(!hacs_streaming).then(|| view! {
                        <div class="ha-connect">
                            <p class="ha-connect__title">"Connect in two steps"</p>
                            <ol class="ha-connect__steps">
                                <li>
                                    <strong>"Install the integration."</strong>
                                    " In Home Assistant, open HACS, search for LocalSky, install, restart HA."
                                </li>
                                <li>
                                    <strong>"Add it."</strong>
                                    " Settings > Devices & services: Home Assistant finds this LocalSky on your network by itself; click through and you're done."
                                </li>
                            </ol>
                            <a class="ha-btn ha-btn--primary" href=doc_url("hacs") target="_blank" rel="noopener">"Open the setup guide"</a>
                        </div>
                    })}

                    {hacs_streaming.then(|| view! {
                        <div class="ha-flows-simple">
                            <div class="ha-flow-line">
                                <Icon name="check" size=15/>
                                <span><strong>"Home Assistant gets"</strong>": live weather, every zone and its valve, forecasts, and run / stop / pause controls."</span>
                            </div>
                            <div class="ha-flow-line">
                                <Icon name="check" size=15/>
                                <span><strong>"LocalSky needs from HA"</strong>{format!(": {}.", if passthrough.iter().any(|(_, n)| *n > 0) {
                                    "the sensors you bridged below"
                                } else {
                                    "nothing; it runs entirely on its own"
                                })}</span>
                            </div>
                        </div>
                    })}

                    {ha_mode.then(|| view! {
                        <div class="ha-cards">
                            <HaCard
                                icon="gauge"
                                title="Watering brain"
                                meaning="This instance is mirroring watering logic that still lives in Home Assistant. That's a migration mode, not the destination: LocalSky's engine is the brain (ET model, soil buckets, rules, scheduling), and HA stays the dashboard."
                                chip="Mirroring HA (migration)".to_string()
                                tone="warn".to_string()
                            >
                                <button type="button" class="ha-btn ha-btn--primary"
                                    prop:disabled=move || busy.get()
                                    on:click=move |_| switch_mode(true)
                                >"Switch to native engine"</button>
                                <a class="ha-btn" href=doc_url("migrating-from-ha") target="_blank" rel="noopener">"Migration guide"</a>
                            </HaCard>
                        </div>
                    })}

                    <details class="ha-advanced">
                        <summary class="ha-advanced__summary">
                            <Icon name="advanced" size=15/>
                            "Advanced: bridges, valves through HA, MQTT"
                            <span class="ha-advanced__hint">"most people never need these"</span>
                        </summary>
                    <div class="ha-cards">
                        {(!ha_mode && env_configured).then(|| view! {
                            <HaCard
                                icon="gauge"
                                title="Watering brain"
                                meaning="LocalSky computes everything itself (ET, soil buckets, rules, the morning schedule). HA mode exists only for mirroring an irrigation setup that still lives in Home Assistant."
                                chip="LocalSky engine".to_string()
                                tone="on".to_string()
                            >
                                <button type="button" class="ha-btn"
                                    prop:disabled=move || busy.get()
                                    on:click=move |_| switch_mode(false)
                                >"Use Home Assistant data instead"</button>
                            </HaCard>
                        })}

                        {if passthrough.is_empty() {
                            view! {
                                <HaCard
                                    icon="sources"
                                    title="Use sensors you already have in HA"
                                    meaning="Anything Home Assistant can see (a Zigbee soil probe, a Z-Wave rain gauge, a weather station from another integration) can feed LocalSky's engine like a native sensor. Map the entities once and they flow in live."
                                    chip="Available".to_string()
                                    tone="off".to_string()
                                >
                                    <a class="ha-btn" href="/sensors?add=1">"Bring in HA sensors"</a>
                                </HaCard>
                            }.into_any()
                        } else {
                            passthrough.iter().map(|(id, n)| {
                                let id_for_remove = id.clone();
                                let label = id.clone();
                                let feeds = *n;
                                view! {
                                    <HaCard
                                        icon="sources"
                                        title="Use sensors you already have in HA"
                                        meaning={if feeds > 0 {
                                            format!("'{label}' feeds {feeds} HA reading{} into the engine live (soil probes, rain gauges, anything HA can see), no rewiring needed.", if feeds == 1 { "" } else { "s" })
                                        } else {
                                            format!("'{label}' is connected but no HA entities are mapped yet. Pick the sensors it should bring in, and they'll flow into the engine live.")
                                        }}
                                        chip={if feeds > 0 { format!("Feeding {feeds}") } else { "Nothing mapped yet".to_string() }}
                                        tone={if feeds > 0 { "on" } else { "warn" }}
                                    >
                                        <a class="ha-btn ha-btn--primary" href=format!("/sensors?source={label}")>"Choose sensors"</a>
                                        {(feeds == 0).then(|| view! {
                                            <button type="button" class="ha-btn ha-btn--danger"
                                                prop:disabled=move || busy.get()
                                                on:click=move |_| remove_source(id_for_remove.clone())
                                            >"Remove"</button>
                                        })}
                                    </HaCard>
                                }
                            }).collect_view().into_any()
                        }}

                        <HaCard
                            icon="controllers"
                            title="Valves through HA"
                            meaning={if service_controllers.is_empty() {
                                "LocalSky drives your controller directly, so watering works even if HA is down. Have a valve only HA can reach (Zigbee, Shelly, a smart plug)? Add an HA controller and LocalSky runs it as a zone.".to_string()
                            } else {
                                format!("LocalSky runs these valves by calling Home Assistant services: {}. Handy for Zigbee or WiFi valves only HA can reach; direct-attached controllers keep working even when HA is down.", service_controllers.join(", "))
                            }}
                            chip={if service_controllers.is_empty() { "Direct control".to_string() } else { format!("{} via HA", service_controllers.len()) }}
                            tone="on".to_string()
                        >
                            <a class="ha-btn" href="/settings/controllers">"Controllers"</a>
                        </HaCard>

                        <HaCard
                            icon="bell"
                            title="MQTT discovery"
                            meaning="An alternative way to publish LocalSky entities into HA over an MQTT broker. Skip it when the LocalSky integration is installed; you'd get duplicates."
                            chip={if mqtt { "Publishing".to_string() } else { "Off".to_string() }}
                            tone={if mqtt { "warn" } else { "off" }}
                        >
                            <a class="ha-btn" href="/settings/notifications">"Configure"</a>
                        </HaCard>
                    </div>
                    </details>
                }.into_any()
            }}
        </div>
    }
}
