// SettingsControllers. List + add/remove + raw-JSON config for
// irrigation controllers. Mirrors the Sources page pattern.

use leptos::prelude::*;

use crate::components::ui::{FormField, Panel, SegmentedControl};

#[component]
pub fn SettingsControllers() -> impl IntoView {
    let config_json = RwSignal::new(serde_json::Value::Null);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let add_open = RwSignal::new(false);
    let new_id = RwSignal::new(String::new());
    let new_kind = RwSignal::new("opensprinkler_direct".to_string());
    let new_default = RwSignal::new(false);
    let new_config_text = RwSignal::new(default_config_text("opensprinkler_direct"));

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = fetch_config().await {
                    config_json.set(cfg);
                }
            });
        });
        Effect::new(move |_| {
            let k = new_kind.get();
            new_config_text.set(default_config_text(&k));
        });
    }

    let controllers_view = move || {
        let cfg = config_json.get();
        let arr = cfg.get("controllers").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        arr.into_iter().map(|c| {
            let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let kind = c.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let default = c.get("default").and_then(|v| v.as_bool()).unwrap_or(false);
            let enabled = c.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let id_for_delete = id.clone();
            let id_for_default = id.clone();
            view! {
                <li class="settings-list__item settings-list__item--row">
                    <span class="settings-list__icon" aria-hidden="true">{kind_icon(&kind)}</span>
                    <span class="settings-list__text">
                        <span class="settings-list__label">
                            {id.clone()}
                            {if default { " (default)" } else { "" }}
                        </span>
                        <span class="settings-list__helptext">
                            {format!("{kind} · {}", if enabled { "enabled" } else { "disabled" })}
                        </span>
                    </span>
                    <button
                        class="setup-footer__btn setup-footer__btn--ghost"
                        type="button"
                        on:click=move |_| {
                            let id = id_for_default.clone();
                            config_json.update(|cfg| {
                                if let Some(arr) = cfg.get_mut("controllers").and_then(|v| v.as_array_mut()) {
                                    for c in arr.iter_mut() {
                                        let is_target = c.get("id").and_then(|v| v.as_str()) == Some(&id);
                                        if let Some(obj) = c.as_object_mut() {
                                            obj.insert("default".into(), serde_json::Value::Bool(is_target));
                                        }
                                    }
                                }
                            });
                        }
                    >
                        "Make default"
                    </button>
                    <button
                        class="setup-footer__btn setup-footer__btn--ghost"
                        type="button"
                        aria-label=format!("Delete controller {id}")
                        on:click=move |_| {
                            let id = id_for_delete.clone();
                            config_json.update(|cfg| {
                                if let Some(arr) = cfg.get_mut("controllers").and_then(|v| v.as_array_mut()) {
                                    arr.retain(|c| c.get("id").and_then(|v| v.as_str()) != Some(&id));
                                }
                            });
                        }
                    >
                        "Delete"
                    </button>
                </li>
            }
        }).collect_view()
    };

    let on_add = move |_| {
        let id = new_id.get().trim().to_string();
        if id.is_empty() {
            result_ok.set(false);
            result_msg.set("Controller id is required".into());
            return;
        }
        let cfg_value: serde_json::Value = match serde_json::from_str(&new_config_text.get()) {
            Ok(v) => v,
            Err(e) => {
                result_ok.set(false);
                result_msg.set(format!("Config JSON parse error: {e}"));
                return;
            }
        };
        let default_flag = new_default.get();
        let entry = serde_json::json!({
            "id": id,
            "default": default_flag,
            "enabled": true,
            "kind": new_kind.get(),
            "config": cfg_value,
        });
        config_json.update(|cfg| {
            // If this controller becomes default, clear existing defaults.
            if default_flag {
                if let Some(arr) = cfg.get_mut("controllers").and_then(|v| v.as_array_mut()) {
                    for c in arr.iter_mut() {
                        if let Some(obj) = c.as_object_mut() {
                            obj.insert("default".into(), serde_json::Value::Bool(false));
                        }
                    }
                }
            }
            let arr = cfg
                .as_object_mut()
                .and_then(|o| o.entry("controllers").or_insert(serde_json::json!([])).as_array_mut());
            if let Some(arr) = arr {
                arr.push(entry);
            }
        });
        new_id.set(String::new());
        add_open.set(false);
        result_ok.set(true);
        result_msg.set("Added. Click Save to apply.".into());
    };

    let on_save = move |_| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let candidate = config_json.get();
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match save_config(candidate).await {
                    Ok(()) => {
                        result_ok.set(true);
                        result_msg.set("Saved. Controller registry hot-reloads on next dispatch.".into());
                    }
                    Err(e) => {
                        result_ok.set(false);
                        result_msg.set(e);
                    }
                }
                saving.set(false);
            });
        }
        #[cfg(not(feature = "hydrate"))]
        {
            saving.set(false);
            let _ = candidate;
        }
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Irrigation controllers"</h1>
                <p class="settings-page__subtitle">
                    "Which hardware fires your valves. Exactly one must be default; new zones inherit that. "
                    "See "
                    <a href="https://github.com/silenthooligan/localsky/blob/main/docs/controllers.md"
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"the controllers doc"</a>
                    " for per-kind configuration details."
                </p>
            </header>

            <Panel title="Configured controllers".to_string()>
                <ul class="settings-list">
                    {controllers_view}
                </ul>

                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    style="margin-top: 1rem"
                    on:click=move |_| add_open.update(|v| *v = !*v)
                >
                    {move || if add_open.get() { "× Cancel add" } else { "+ Add controller" }}
                </button>
            </Panel>

            <Show when=move || add_open.get()>
                <Panel title="New controller".to_string()>
                    <FormField
                        label="ID".to_string()
                        helptext="snake_case (e.g. os_main, ha_backup). Used by zones to reference this controller.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            placeholder="e.g. os_main"
                            prop:value=move || new_id.get()
                            on:input=move |ev| new_id.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Kind".to_string()
                        helptext="See controllers.md for capabilities of each.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=new_kind
                            options=vec![
                                ("opensprinkler_direct".into(), "OpenSprinkler".into()),
                                ("ha_service_call".into(), "HA service call".into()),
                                ("esphome_native".into(), "ESPHome".into()),
                                ("rachio".into(), "Rachio".into()),
                                ("dry_run".into(), "DryRun".into()),
                            ]
                            aria_label="Controller kind".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Make this the default?".to_string()
                        helptext="If checked, other controllers lose default status. Zones without an explicit controller_id use the default.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                            <input
                                type="checkbox"
                                prop:checked=move || new_default.get()
                                on:input=move |ev| new_default.set(event_target_checked(&ev))
                            />
                            "Set as default controller"
                        </label>
                    </FormField>

                    <FormField
                        label="Config (JSON)".to_string()
                        helptext="Kind-specific configuration. Template auto-fills when Kind changes.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <textarea
                            class="ui-input"
                            style="min-height: 180px; font-family: var(--font-mono); font-size: 0.85rem;"
                            prop:value=move || new_config_text.get()
                            on:input=move |ev| new_config_text.set(event_target_value(&ev))
                        ></textarea>
                    </FormField>

                    <button
                        type="button"
                        class="setup-apply-btn"
                        on:click=on_add
                    >
                        "Add to list"
                    </button>
                </Panel>
            </Show>

            <button
                type="button"
                class="setup-apply-btn"
                style="margin-top: 1.5rem"
                disabled=move || saving.get()
                on:click=on_save
            >
                {move || if saving.get() { "Saving…" } else { "Save all changes" }}
            </button>

            <Show when=move || !result_msg.get().is_empty()>
                <p
                    class="setup-result"
                    class:setup-result--ok=move || result_ok.get()
                    class:setup-result--err=move || !result_ok.get()
                    role="status"
                >
                    {move || result_msg.get()}
                </p>
            </Show>
        </main>
    }
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "opensprinkler_direct" => "🚿",
        "ha_service_call" => "🏠",
        "esphome_native" => "⚡",
        "rachio" => "☁",
        "dry_run" => "🧪",
        _ => "💧",
    }
}

fn default_config_text(kind: &str) -> String {
    match kind {
        "opensprinkler_direct" => "{\n  \"host\": \"192.0.2.10\",\n  \"port\": 80,\n  \"password_md5\": \"\",\n  \"poll_interval_s\": 10\n}".into(),
        "ha_service_call" => "{\n  \"base_url\": \"http://homeassistant.local:8123\",\n  \"bearer_token\": \"${HA_LONG_LIVED_TOKEN}\",\n  \"start_service\": \"script.os_zone_toggle\",\n  \"stop_service\": \"opensprinkler.stop\",\n  \"zone_entity_map\": {}\n}".into(),
        "esphome_native" => "{\n  \"host\": \"esp-sprinkler.local\",\n  \"port\": 6053,\n  \"password\": null,\n  \"zone_entity_map\": {}\n}".into(),
        "rachio" => "{\n  \"api_token\": \"\",\n  \"device_id\": \"\",\n  \"zone_uuid_map\": {}\n}".into(),
        "dry_run" => "{\n  \"simulate_runs\": false\n}".into(),
        _ => "{}".into(),
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_config() -> Result<serde_json::Value, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json::<serde_json::Value>().await.map_err(|e| e.to_string())
}

#[cfg(feature = "hydrate")]
async fn save_config(cfg: serde_json::Value) -> Result<(), String> {
    use gloo_net::http::Request;
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {body}", resp.status()));
    }
    Ok(())
}
