// SettingsNotifications. Edit cfg.notifications. Web Push toggles +
// MQTT broker host + ntfy URL + Slack URL. Web Push subscription
// itself is a per-device action handled elsewhere; this page only
// edits server-side enablement.

use leptos::prelude::*;

use crate::components::settings_ui::SettingsResult;
use crate::components::ui::{FormField, Panel, Toggle};

#[component]
pub fn SettingsNotifications() -> impl IntoView {
    let mqtt_host = RwSignal::new(String::new());
    let mqtt_port = RwSignal::new(1883u16);
    let mqtt_username = RwSignal::new(String::new());
    let mqtt_password = RwSignal::new(String::new());
    let mqtt_discovery_prefix = RwSignal::new("homeassistant".to_string());
    let mqtt_publish_enabled = RwSignal::new(true);

    let ntfy_base_url = RwSignal::new(String::new());
    let ntfy_topic = RwSignal::new(String::new());

    let slack_webhook = RwSignal::new(String::new());

    let web_push_enabled = RwSignal::new(false);

    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(d) = fetch_notifications().await {
                    mqtt_host.set(d.mqtt_host);
                    mqtt_port.set(d.mqtt_port);
                    mqtt_username.set(d.mqtt_username);
                    mqtt_password.set(d.mqtt_password);
                    mqtt_discovery_prefix.set(d.mqtt_discovery_prefix);
                    mqtt_publish_enabled.set(d.mqtt_publish_enabled);
                    ntfy_base_url.set(d.ntfy_base_url);
                    ntfy_topic.set(d.ntfy_topic);
                    slack_webhook.set(d.slack_webhook);
                    web_push_enabled.set(d.web_push_enabled);
                }
            });
        });
    }

    let on_save = move |_| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let payload = NotificationsDraft {
            mqtt_host: mqtt_host.get(),
            mqtt_port: mqtt_port.get(),
            mqtt_username: mqtt_username.get(),
            mqtt_password: mqtt_password.get(),
            mqtt_discovery_prefix: mqtt_discovery_prefix.get(),
            mqtt_publish_enabled: mqtt_publish_enabled.get(),
            ntfy_base_url: ntfy_base_url.get(),
            ntfy_topic: ntfy_topic.get(),
            slack_webhook: slack_webhook.get(),
            web_push_enabled: web_push_enabled.get(),
        };
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match save_notifications(payload).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. New channels engage on next event.",
                        );
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
            let _ = payload;
        }
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Notifications"</h1>
                <p class="settings-page__subtitle">
                    "Outbound channels for zone-start, zone-stop, daily verdict, "
                    "and anomaly events. Each channel is independent. Web Push "
                    "subscription is per-device and handled from the dashboard."
                </p>
            </header>

            <Panel title="Web Push (per device)".to_string() help_topic="notifications">
                <Toggle
                    checked=web_push_enabled
                    label="Server-side push enabled".to_string()
                    helptext="Requires a VAPID keypair set via env vars or /data/keys/. Each device subscribes from the dashboard.".to_string()
                />
            </Panel>

            <Panel title="MQTT (HA discovery)".to_string() help_topic="notifications">
                <div class="grid settings-field-grid">
                    <FormField
                        label="Broker host".to_string()
                        helptext="Leave blank to disable MQTT entirely.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            placeholder="broker.local"
                            prop:value=move || mqtt_host.get()
                            on:input=move |ev| mqtt_host.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Port".to_string()
                        helptext="Default 1883 (unencrypted). 8883 for TLS.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="number"
                            class="ui-input"
                            min="1"
                            max="65535"
                            prop:value=move || mqtt_port.get().to_string()
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<u16>() {
                                    mqtt_port.set(v);
                                }
                            }
                        />
                    </FormField>

                    <FormField
                        label="Username".to_string()
                        helptext="Optional. Required if your broker authenticates.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            prop:value=move || mqtt_username.get()
                            on:input=move |ev| mqtt_username.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Password".to_string()
                        helptext="Optional.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="password"
                            class="ui-input"
                            prop:value=move || mqtt_password.get()
                            on:input=move |ev| mqtt_password.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Discovery prefix".to_string()
                        helptext="HA discovery topic prefix. Default 'homeassistant'.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            prop:value=move || mqtt_discovery_prefix.get()
                            on:input=move |ev| mqtt_discovery_prefix.set(event_target_value(&ev))
                        />
                    </FormField>
                </div>

                <Toggle
                    checked=mqtt_publish_enabled
                    label="Publish discovery + state".to_string()
                    helptext="Off disables sensor publishes without removing the broker config.".to_string()
                />
            </Panel>

            <Panel title="ntfy".to_string() help_topic="notifications">
                <div class="grid settings-field-grid">
                    <FormField
                        label="Base URL".to_string()
                        helptext="e.g. https://ntfy.sh, or your self-hosted instance.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="url"
                            class="ui-input"
                            placeholder="https://ntfy.sh"
                            prop:value=move || ntfy_base_url.get()
                            on:input=move |ev| ntfy_base_url.set(event_target_value(&ev))
                        />
                    </FormField>
                    <FormField
                        label="Topic".to_string()
                        helptext="Pick something unique; ntfy topics are public by default.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            prop:value=move || ntfy_topic.get()
                            on:input=move |ev| ntfy_topic.set(event_target_value(&ev))
                        />
                    </FormField>
                </div>
            </Panel>

            <Panel title="Slack".to_string() help_topic="notifications">
                <div class="grid settings-field-grid">
                    <FormField
                        label="Incoming webhook URL".to_string()
                        helptext="Generate from Slack > Apps > Incoming Webhooks. Leave blank to disable.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="url"
                            class="ui-input"
                            placeholder="https://hooks.slack.com/services/..."
                            prop:value=move || slack_webhook.get()
                            on:input=move |ev| slack_webhook.set(event_target_value(&ev))
                        />
                    </FormField>
                </div>
            </Panel>

            <div class="settings-actions">
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    disabled=move || saving.get()
                    on:click=on_save
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </button>
            </div>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>
        </main>
    }
}

#[derive(Clone, Default)]
#[allow(dead_code)]
struct NotificationsDraft {
    mqtt_host: String,
    mqtt_port: u16,
    mqtt_username: String,
    mqtt_password: String,
    mqtt_discovery_prefix: String,
    mqtt_publish_enabled: bool,
    ntfy_base_url: String,
    ntfy_topic: String,
    slack_webhook: String,
    web_push_enabled: bool,
}

#[cfg(feature = "hydrate")]
async fn fetch_notifications() -> Result<NotificationsDraft, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let n = val
        .get("notifications")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let mqtt = n.get("mqtt").cloned().unwrap_or(serde_json::Value::Null);
    let web_push = n
        .get("web_push")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ntfy = n.get("ntfy").cloned().unwrap_or(serde_json::Value::Null);
    let slack = n.get("slack").cloned().unwrap_or(serde_json::Value::Null);

    Ok(NotificationsDraft {
        mqtt_host: get_str(&mqtt, "host").to_string(),
        mqtt_port: mqtt.get("port").and_then(|v| v.as_u64()).unwrap_or(1883) as u16,
        mqtt_username: get_str(&mqtt, "username").to_string(),
        mqtt_password: get_str(&mqtt, "password").to_string(),
        mqtt_discovery_prefix: if mqtt.get("discovery_prefix").is_some() {
            get_str(&mqtt, "discovery_prefix").to_string()
        } else {
            "homeassistant".to_string()
        },
        mqtt_publish_enabled: mqtt
            .get("publish_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        ntfy_base_url: get_str(&ntfy, "base_url").to_string(),
        ntfy_topic: get_str(&ntfy, "topic").to_string(),
        slack_webhook: get_str(&slack, "webhook_url").to_string(),
        web_push_enabled: !web_push.is_null(),
    })
}

#[cfg(feature = "hydrate")]
fn get_str<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

#[cfg(feature = "hydrate")]
async fn save_notifications(d: NotificationsDraft) -> Result<(), String> {
    use gloo_net::http::Request;
    let cur = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = cur.json().await.map_err(|e| e.to_string())?;

    let mqtt = if d.mqtt_host.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({
            "host": d.mqtt_host,
            "port": d.mqtt_port,
            "username": if d.mqtt_username.is_empty() { serde_json::Value::Null } else { serde_json::json!(d.mqtt_username) },
            "password": if d.mqtt_password.is_empty() { serde_json::Value::Null } else { serde_json::json!(d.mqtt_password) },
            "discovery_prefix": d.mqtt_discovery_prefix,
            "publish_enabled": d.mqtt_publish_enabled,
            "subscribe_enabled": false,
        })
    };

    let ntfy = if d.ntfy_base_url.is_empty() || d.ntfy_topic.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({
            "base_url": d.ntfy_base_url,
            "topic": d.ntfy_topic,
            "auth_token": serde_json::Value::Null,
        })
    };

    let slack = if d.slack_webhook.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "webhook_url": d.slack_webhook })
    };

    let notifications = serde_json::json!({
        "mqtt": mqtt,
        "ntfy": ntfy,
        "slack": slack,
        // web_push retained from existing config if present; otherwise null.
        // VAPID keypair config is operator-side env/file, not editable here.
        "web_push": cfg.get("notifications").and_then(|n| n.get("web_push")).cloned().unwrap_or(serde_json::Value::Null),
        "email": cfg.get("notifications").and_then(|n| n.get("email")).cloned().unwrap_or(serde_json::Value::Null),
    });
    cfg["notifications"] = notifications;

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
