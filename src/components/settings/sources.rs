// SettingsSources. List view of configured weather + sensor sources,
// add/remove + raw-JSON config editor for per-kind customization.
//
// The /settings/sources page is intentionally raw-JSON for the per-kind
// configs in v0.1; per-kind form widgets (e.g. an MQTT subscription
// editor with topic + field + json_path inputs) land in a follow-up
// once the catalog of supported kinds stabilizes. Today's UX:
//
//   List of configured sources (id, kind, enabled toggle, delete)
//   "Add source" button -> kind picker + JSON config textarea
//   Save -> PUT /api/config with the full Config block

use leptos::prelude::*;

use crate::components::ui::{FormField, Panel, SegmentedControl};

#[component]
pub fn SettingsSources() -> impl IntoView {
    // The raw config JSON we round-trip; mutable as the operator edits.
    let config_json = RwSignal::new(serde_json::Value::Null);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let add_open = RwSignal::new(false);
    let new_id = RwSignal::new(String::new());
    let new_kind = RwSignal::new("mqtt".to_string());
    let new_priority = RwSignal::new(50i32);
    let new_config_text = RwSignal::new(default_config_text("mqtt"));

    // Load on mount.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = fetch_config().await {
                    config_json.set(cfg);
                }
            });
        });
        // When kind changes, swap default JSON template.
        Effect::new(move |_| {
            let k = new_kind.get();
            new_config_text.set(default_config_text(&k));
        });
    }

    let sources_view = move || {
        let cfg = config_json.get();
        let arr = cfg.get("sources").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        arr.into_iter().enumerate().map(|(_i, src)| {
            let id = src.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let kind = src.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let priority = src.get("priority").and_then(|v| v.as_i64()).unwrap_or(50);
            let enabled = src.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let id_for_delete = id.clone();
            view! {
                <li class="settings-list__item settings-list__item--row">
                    <span class="settings-list__icon" aria-hidden="true">{kind_icon(&kind)}</span>
                    <span class="settings-list__text">
                        <span class="settings-list__label">{id.clone()}</span>
                        <span class="settings-list__helptext">
                            {format!("{kind} · priority {priority} · {}",
                                if enabled { "enabled" } else { "disabled" })}
                        </span>
                    </span>
                    <button
                        class="setup-footer__btn setup-footer__btn--ghost"
                        type="button"
                        aria-label=format!("Delete source {id}")
                        on:click=move |_| {
                            let id = id_for_delete.clone();
                            config_json.update(|cfg| {
                                if let Some(arr) = cfg.get_mut("sources").and_then(|v| v.as_array_mut()) {
                                    arr.retain(|s| s.get("id").and_then(|v| v.as_str()) != Some(&id));
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
            result_msg.set("Source id is required".into());
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
        let entry = serde_json::json!({
            "id": id,
            "priority": new_priority.get(),
            "enabled": true,
            "kind": new_kind.get(),
            "config": cfg_value,
        });
        config_json.update(|cfg| {
            let arr = cfg
                .as_object_mut()
                .and_then(|o| o.entry("sources").or_insert(serde_json::json!([])).as_array_mut());
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
                        result_msg.set("Saved. Source registry hot-reloads on next tick.".into());
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
                <h1 class="settings-page__title">"Weather sources"</h1>
                <p class="settings-page__subtitle">
                    "Each source contributes per-field observations the merge engine picks from by priority. "
                    "Add what you have; remove what you don't. Higher priority wins per field for the same observation type."
                </p>
            </header>

            <Panel title="Configured sources".to_string()>
                <ul class="settings-list">
                    {sources_view}
                </ul>

                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    style="margin-top: 1rem"
                    on:click=move |_| add_open.update(|v| *v = !*v)
                >
                    {move || if add_open.get() { "× Cancel add" } else { "+ Add source" }}
                </button>
            </Panel>

            <Show when=move || add_open.get()>
                <Panel title="New source".to_string()>
                    <FormField
                        label="ID".to_string()
                        helptext="snake_case, unique across sources (e.g. tempest_lan, mqtt_sensors).".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            placeholder="e.g. mqtt_sensors"
                            prop:value=move || new_id.get()
                            on:input=move |ev| new_id.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Kind".to_string()
                        helptext="What protocol or service this source uses.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=new_kind
                            options=vec![
                                ("tempest_udp".into(), "Tempest UDP".into()),
                                ("open_meteo".into(), "Open-Meteo".into()),
                                ("ecowitt_local".into(), "Ecowitt LAN".into()),
                                ("mqtt".into(), "MQTT".into()),
                                ("http_webhook".into(), "HTTP webhook".into()),
                                ("ha_passthrough".into(), "HA passthrough".into()),
                                ("demo_replay".into(), "Demo".into()),
                            ]
                            aria_label="Source kind".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Priority".to_string()
                        helptext="Higher wins per-field. Convention: 100=LAN station, 50=cloud forecast, 10=fallback.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="number"
                            class="ui-input"
                            min="-100"
                            max="200"
                            prop:value=move || new_priority.get().to_string()
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<i32>() {
                                    new_priority.set(v);
                                }
                            }
                        />
                    </FormField>

                    <FormField
                        label="Config (JSON)".to_string()
                        helptext="Kind-specific configuration. The template auto-fills when you change Kind above.".to_string()
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
        "tempest_udp" | "tempest_ws" => "🌬",
        "open_meteo" | "nws" | "openweather" | "pirate_weather" | "met_norway" => "☁",
        "ecowitt_local" => "📡",
        "mqtt" => "📬",
        "http_webhook" => "📥",
        "ha_passthrough" => "🏠",
        "ambient_weather" => "🌤",
        "demo_replay" => "🧪",
        _ => "🔌",
    }
}

fn default_config_text(kind: &str) -> String {
    match kind {
        "tempest_udp" => "{\n  \"bind_addr\": \"0.0.0.0:50222\"\n}".into(),
        "open_meteo" => "{\n  \"forecast_days\": 7,\n  \"forecast_hours\": 48,\n  \"past_days\": 1,\n  \"include_radar\": false\n}".into(),
        "ecowitt_local" => "{\n  \"path\": \"/ingest/ecowitt\",\n  \"shared_secret\": null\n}".into(),
        "mqtt" => "{\n  \"broker_host\": \"broker.local\",\n  \"broker_port\": 1883,\n  \"username\": null,\n  \"password\": null,\n  \"subscriptions\": [\n    {\n      \"topic\": \"sensors/+/soil\",\n      \"field\": \"rh_pct\",\n      \"json_path\": \"moisture\",\n      \"scale\": 1.0,\n      \"offset\": 0.0\n    }\n  ]\n}".into(),
        "http_webhook" => "{\n  \"path\": \"/ingest/webhook/myhook\",\n  \"token\": \"changeme\",\n  \"fields\": [\n    {\"field\": \"air_temp_f\", \"json_path\": \"temperature\", \"scale\": 1.0, \"offset\": 0.0}\n  ]\n}".into(),
        "ha_passthrough" => "{\n  \"base_url\": \"http://homeassistant.local:8123\",\n  \"bearer_token\": \"${HA_LONG_LIVED_TOKEN}\",\n  \"field_map\": {}\n}".into(),
        "demo_replay" => "{\n  \"rate\": 10.0\n}".into(),
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
