// SettingsHomeAssistant. The Home Assistant relationship as a product
// surface: a hero status card, then one card per capability written in
// user outcomes (what it does FOR you), each with a status chip and the
// action right there: switch the engine's data source, remove a dead
// bridge, jump into the matching editor, open the setup guide. Reads
// the `ha` block on /api/v1/health; actions write through the normal
// config PUT (snapshot + rollback machinery applies).

use leptos::prelude::*;

#[cfg(feature = "hydrate")]
use crate::components::config_client::{get_config, put_config};
use crate::components::settings_ui::SettingsResult;
#[cfg(feature = "hydrate")]
use crate::components::ui::use_toast;
use crate::components::ui::{Button, ConfirmSheet, FormField, Icon, SkeletonRows};
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

/// The legacy HA readback prefix is explicit configuration, never discovery.
#[component]
fn HaEntityPrefix() -> impl IntoView {
    let prefix = RwSignal::new(String::new());
    let saved_prefix = RwSignal::new(String::new());
    let example_zone = RwSignal::new("zone_slug".to_string());
    let loaded = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let restart_reasons = RwSignal::new(Vec::<String>::new());
    let dismissed = RwSignal::new(false);
    let invalid = Signal::derive(move || {
        let value = prefix.get();
        value.is_empty()
            || !value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    });

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            match get_config().await {
                Ok(cfg) => {
                    let value = cfg["deployment"]["ha_sprinkler_prefix"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("opensprinkler")
                        .to_string();
                    prefix.set(value.clone());
                    saved_prefix.set(value);
                    if let Some(zone) = cfg["zones"].as_object().and_then(|z| z.keys().next()) {
                        example_zone.set(zone.clone());
                    }
                    loaded.set(true);
                }
                Err(e) => result_msg.set(format!("Could not load the HA entity prefix: {e}")),
            }
        });
    });

    let save = Callback::new(move |_: leptos::ev::MouseEvent| {
        #[cfg(feature = "hydrate")]
        {
            if !loaded.get_untracked() || saving.get_untracked() || invalid.get_untracked() {
                return;
            }
            let value = prefix.get_untracked();
            saving.set(true);
            result_msg.set(String::new());
            leptos::task::spawn_local(async move {
                let result = async {
                    let mut cfg = get_config().await?;
                    cfg["deployment"]["ha_sprinkler_prefix"] = value.clone().into();
                    put_config(&cfg).await
                }
                .await;
                match result {
                    Ok(outcome) => {
                        saved_prefix.set(value);
                        result_ok.set(true);
                        result_msg.set(if outcome.restart_required() {
                            crate::voice::SAVED_NEEDS_RESTART.into()
                        } else {
                            crate::voice::SAVED_LIVE.into()
                        });
                        restart_reasons.set(outcome.restart_reasons);
                        dismissed.set(false);
                    }
                    Err(e) => {
                        result_ok.set(false);
                        result_msg.set(format!("Could not save the HA entity prefix: {e}"));
                    }
                }
                saving.set(false);
            });
        }
    });

    view! {
        <HaCard icon="controllers" title="HA controller entity names"
            meaning="Used when LocalSky reads irrigation state from Home Assistant. Match the controller's entity names in HA. The existing opensprinkler default is kept until you change it. Saving a different prefix holds watering until LocalSky restarts."
            chip="HA mode only" tone="off"
        >
            <div>
                <FormField label="HA controller entity prefix"
                    helptext="The part after the entity domain and before _enabled. Use lowercase letters, numbers, and underscores."
                    error=Signal::derive(move || (loaded.get() && invalid.get()).then(|| "Enter a prefix using lowercase letters, numbers, and underscores.".to_string()))
                >
                    <input type="text" class="settings-input" autocomplete="off"
                        prop:value=move || prefix.get()
                        prop:disabled=move || !loaded.get() || saving.get()
                        on:input=move |ev| prefix.set(event_target_value(&ev))/>
                </FormField>
                <Show when=move || loaded.get() && !invalid.get()>
                    <p class="settings-help">"Expected HA entities for this prefix:"</p>
                    <ul>
                        <li><code>{move || format!("switch.{}_enabled", prefix.get())}</code></li>
                        <li><code>{move || format!("sensor.{}_water_level", prefix.get())}</code></li>
                        <li><code>{move || format!("binary_sensor.{}_{}_station_running", prefix.get(), example_zone.get())}</code></li>
                    </ul>
                </Show>
                <Button disabled=Signal::derive(move || !loaded.get() || saving.get() || invalid.get() || prefix.get() == saved_prefix.get())
                    on_click=save>"Save HA entity prefix"</Button>
                <SettingsResult result_msg result_ok/>
                <super::data_sources::RestartBanner reasons=restart_reasons dismissed/>
            </div>
        </HaCard>
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

    // Two-step confirms. A click only asks; the work runs from the
    // sheet's on_confirm. The signals live here, in the page component,
    // because the handlers are built inside per-row closures while the
    // sheets mount once at the bottom of the page view.
    let pending_remove: RwSignal<Option<String>> = RwSignal::new(None);
    let remove_open = RwSignal::new(false);
    let switch_native_open = RwSignal::new(false);
    let switch_ha_open = RwSignal::new(false);

    // Remove a passthrough source by id (read-modify-write the config).
    let do_remove_source = move |id: String| {
        #[cfg(feature = "hydrate")]
        {
            if busy.get_untracked() {
                return;
            }
            busy.set(true);
            leptos::task::spawn_local(async move {
                let result = async {
                    let mut cfg = get_config().await?;
                    if let Some(arr) = cfg.get_mut("sources").and_then(|v| v.as_array_mut()) {
                        arr.retain(|s| s.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
                    }
                    put_config(&cfg).await.map(|_| ())
                }
                .await;
                match result {
                    Ok(()) => {
                        use_toast().success("Bridge removed. It takes effect on the next pass.");
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
    let confirm_remove = Callback::new(move |()| {
        if let Some(id) = pending_remove.get_untracked() {
            do_remove_source(id);
        }
        pending_remove.set(None);
    });

    // Switch the irrigation engine's data source (deployment.mode).
    let do_switch_mode = move |to_standalone: bool| {
        #[cfg(feature = "hydrate")]
        {
            if busy.get_untracked() {
                return;
            }
            busy.set(true);
            leptos::task::spawn_local(async move {
                let result = async {
                    let mut cfg = get_config().await?;
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
                    put_config(&cfg).await.map(|_| ())
                }
                .await;
                match result {
                    Ok(()) => {
                        use_toast().success(crate::voice::SAVED_NEEDS_RESTART);
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
    let confirm_switch_native = Callback::new(move |()| do_switch_mode(true));
    let confirm_switch_ha = Callback::new(move |()| do_switch_mode(false));

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
                            <crate::components::ui::Button variant="primary" size="sm"  class="ha-btn ha-btn--primary" href=doc_url("hacs") target="_blank" >"Open the setup guide"</crate::components::ui::Button>
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
                                meaning="This instance is mirroring watering logic that still lives in Home Assistant. That's a migration mode, not the destination: LocalSky's engine is the brain (ET model, the weekly water balance, rules, scheduling), and HA stays the dashboard."
                                chip="Mirroring HA (migration)".to_string()
                                tone="warn".to_string()
                            >
                                <crate::components::ui::Button variant="primary" size="sm"   class="ha-btn ha-btn--primary"
                                    disabled=Signal::derive(move || busy.get())
                                    on_click=Callback::new(move |_| switch_native_open.set(true))>"Switch to native engine"</crate::components::ui::Button>
                                <crate::components::ui::Button class="ha-btn" variant="secondary" size="sm" href=doc_url("migrating-from-ha") target="_blank">"Migration guide"</crate::components::ui::Button>
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
                        {(ha_mode || env_configured).then(|| view! { <HaEntityPrefix/> })}
                        {(!ha_mode && env_configured).then(|| view! {
                            <HaCard
                                icon="gauge"
                                title="Watering brain"
                                meaning="LocalSky computes everything itself (ET, the weekly water balance, rules, the morning schedule). HA mode exists only for mirroring an irrigation setup that still lives in Home Assistant."
                                chip="LocalSky engine".to_string()
                                tone="on".to_string()
                            >
                                <crate::components::ui::Button variant="secondary" size="sm"   class="ha-btn"
                                    disabled=Signal::derive(move || busy.get())
                                    on_click=Callback::new(move |_| switch_ha_open.set(true))>"Use Home Assistant data instead"</crate::components::ui::Button>
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
                                    <crate::components::ui::Button variant="secondary" size="sm"  class="ha-btn" href="/sensors?add=1">"Bring in HA sensors"</crate::components::ui::Button>
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
                                            format!("'{label}' feeds {feeds} HA reading{} in live (soil probes, rain gauges, anything HA can see), no rewiring needed.", if feeds == 1 { "" } else { "s" })
                                        } else {
                                            format!("'{label}' is connected but no HA entities are mapped yet. Pick the sensors it should bring in, and they'll flow in live.")
                                        }}
                                        chip={if feeds > 0 { format!("Feeding {feeds}") } else { "Nothing mapped yet".to_string() }}
                                        tone={if feeds > 0 { "on" } else { "warn" }}
                                    >
                                        <crate::components::ui::Button variant="primary" size="sm"  class="ha-btn ha-btn--primary" href=format!("/sensors?source={label}")>"Choose sensors"</crate::components::ui::Button>
                                        {(feeds == 0).then(|| view! {
                                            <crate::components::ui::Button variant="danger" size="sm"   class="ha-btn ha-btn--danger"
                                                disabled=Signal::derive(move || busy.get())
                                                on_click=Callback::new(move |_| {
                                                    pending_remove.set(Some(id_for_remove.clone()));
                                                    remove_open.set(true);
                                                })>"Remove"</crate::components::ui::Button>
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
                            <crate::components::ui::Button variant="secondary" size="sm"  class="ha-btn" href="/settings?section=devices">"Controllers"</crate::components::ui::Button>
                        </HaCard>

                        <HaCard
                            icon="bell"
                            title="MQTT discovery"
                            meaning="An alternative way to publish LocalSky entities into HA over an MQTT broker. Skip it when the LocalSky integration is installed; you'd get duplicates."
                            chip={if mqtt { "Publishing".to_string() } else { "Off".to_string() }}
                            tone={if mqtt { "warn" } else { "off" }}
                        >
                            <crate::components::ui::Button variant="secondary" size="sm"  class="ha-btn" href="/settings/notifications">"Configure"</crate::components::ui::Button>
                        </HaCard>
                    </div>
                    </details>
                }.into_any()
            }}

            // Confirms. Mounted unconditionally, outside the loaded
            // branch and the per-bridge loop; each one hides itself.
            <ConfirmSheet
                visible=remove_open
                title="Remove this bridge?"
                body=Signal::derive(move || match pending_remove.get() {
                    Some(id) => format!(
                        "'{id}' currently feeds nothing, so no data is lost. \
                         A config snapshot is kept for rollback."
                    ),
                    None => "This bridge currently feeds nothing, so no data is lost. \
                             A config snapshot is kept for rollback."
                        .to_string(),
                })
                confirm_label=Signal::derive(|| "Remove".to_string())
                danger=true
                on_confirm=confirm_remove
            />

            <ConfirmSheet
                visible=switch_native_open
                title="Switch watering decisions to LocalSky's native engine?"
                body=Signal::derive(|| {
                    "LocalSky will compute everything from its own sources (station, \
                     gateway, forecast). Home Assistant keeps receiving live data \
                     through the integration. You can switch back any time."
                        .to_string()
                })
                confirm_label=Signal::derive(|| "Switch to native engine".to_string())
                on_confirm=confirm_switch_native
            />

            <ConfirmSheet
                visible=switch_ha_open
                title="Read weather from Home Assistant again?"
                body=Signal::derive(|| {
                    "Watering decisions will be computed from the entities LocalSky \
                     reads out of HA."
                        .to_string()
                })
                confirm_label=Signal::derive(|| "Use Home Assistant data".to_string())
                on_confirm=confirm_switch_ha
            />
        </div>
    }
}
