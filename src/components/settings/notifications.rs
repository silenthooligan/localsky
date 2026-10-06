// SettingsNotifications. Edit cfg.notifications: the Web Push server
// switch plus this device's own subscription, MQTT broker host, ntfy
// URL and Slack URL.

use leptos::prelude::*;

use crate::components::settings_ui::SettingsResult;
use crate::components::ui::{Button, FormField, Panel, SecretInput, Toggle};

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

    let web_push_enabled = RwSignal::new(true);
    let channels_loaded = RwSignal::new(false);
    let server_revision = RwSignal::new(0u32);
    let daily_outlook_enabled = RwSignal::new(false);
    let daily_outlook_time = RwSignal::new("09:00".to_string());

    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_notifications().await {
                    Ok(d) => {
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
                        daily_outlook_enabled.set(d.daily_outlook_enabled);
                        daily_outlook_time.set(d.daily_outlook_time);
                        channels_loaded.set(true);
                    }
                    Err(e) => {
                        result_msg.set(e);
                        result_ok.set(false);
                    }
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
            daily_outlook_enabled: daily_outlook_enabled.get(),
            daily_outlook_time: daily_outlook_time.get(),
        };
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match save_notifications(payload).await {
                    Ok(()) => {
                        server_revision.update(|n| *n += 1);
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            crate::voice::SAVED_LIVE,
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
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Notifications"</h1>
                <p class="settings-page__subtitle">
                    "Choose what reaches this device and when."
                </p>
            </header>

            <super::pwa_notifications::PwaNotifications server_revision=server_revision/>
            <details class="pwa-shared-channels">
                <summary>"Server and shared channels"</summary>
                <p class="settings-page__subtitle">"These settings apply to the whole LocalSky installation. Device choices above apply only to Web Push."</p>
                <fieldset class="pwa-preferences__fields" disabled=move || !channels_loaded.get() || saving.get()>
                <legend class="sr-only">"Shared channel settings"</legend>
            <Panel title="Shared channel outlook".to_string() help_topic="notifications">
                <Toggle
                    checked=daily_outlook_enabled
                    label="Daily outlook for ntfy and Slack".to_string()
                    helptext="Optional. One summary per day; forecast changes stay in the app.".to_string()
                />
                <div class="grid settings-field-grid">
                    <FormField
                        label="Summary time".to_string()
                        helptext="Uses the timezone in Settings > Location. Missed summaries are skipped.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input type="time" class="ui-input" aria-label="Summary time"
                            prop:value=move || daily_outlook_time.get()
                            disabled=move || !daily_outlook_enabled.get()
                            on:input=move |ev| daily_outlook_time.set(event_target_value(&ev))
                        />
                    </FormField>
                </div>
                <p class="settings-page__subtitle">"Applies to ntfy and Slack. Each PWA device chooses its own outlook above."</p>
            </Panel>

            <Panel title="Web Push".to_string() help_topic="notifications">
                <Toggle
                    checked=web_push_enabled
                    label="Send push alerts".to_string()
                    helptext="Enable or pause delivery to all PWA devices. Your server keys and device choices are kept.".to_string()
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
                        <SecretInput
                            value=mqtt_password
                            on_input=Callback::new(move |v: String| mqtt_password.set(v))
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
                <Button
                    variant="primary"
                    disabled=Signal::derive(move || saving.get() || !channels_loaded.get())
                    on_click=Callback::new(on_save)
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </Button>
            </div>

            </fieldset>
            <SettingsResult result_msg=result_msg result_ok=result_ok/>
            </details>
        </div>
    }
}

#[derive(Clone, Default)]
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
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
    daily_outlook_enabled: bool,
    daily_outlook_time: String,
}

#[cfg(feature = "hydrate")]
async fn fetch_notifications() -> Result<NotificationsDraft, String> {
    let val = crate::components::config_client::get_config().await?;
    let n = val
        .get("notifications")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let mqtt = n.get("mqtt").cloned().unwrap_or(serde_json::Value::Null);
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
        web_push_enabled: n
            .get("web_push_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        daily_outlook_enabled: n
            .get("daily_outlook")
            .and_then(|d| d.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        daily_outlook_time: n
            .get("daily_outlook")
            .and_then(|d| d.get("time"))
            .and_then(|v| v.as_str())
            .unwrap_or("09:00")
            .to_string(),
    })
}

#[cfg(feature = "hydrate")]
fn get_str<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

#[cfg(feature = "hydrate")]
async fn save_notifications(d: NotificationsDraft) -> Result<(), String> {
    let outlook = crate::config::schema::DailyOutlook {
        enabled: d.daily_outlook_enabled,
        time: d.daily_outlook_time.clone(),
    };
    if outlook.minute_of_day().is_none() {
        return Err("Choose a valid summary time.".into());
    }
    let mut cfg = crate::components::config_client::get_config().await?;

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
            "subscribe_enabled": cfg.pointer("/notifications/mqtt/subscribe_enabled").cloned().unwrap_or(serde_json::json!(false)),
        })
    };

    let ntfy = if d.ntfy_base_url.is_empty() || d.ntfy_topic.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({
            "base_url": d.ntfy_base_url,
            "topic": d.ntfy_topic,
            "auth_token": cfg.pointer("/notifications/ntfy/auth_token").cloned().unwrap_or(serde_json::Value::Null),
        })
    };

    let slack = if d.slack_webhook.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "webhook_url": d.slack_webhook })
    };

    let notifications = serde_json::json!({
        "daily_outlook": outlook,
        "web_push_enabled": d.web_push_enabled,
        "mqtt": mqtt,
        "ntfy": ntfy,
        "slack": slack,
        // web_push retained from existing config if present; otherwise null.
        // VAPID keypair config is operator-side env/file, not editable here.
        "web_push": cfg.get("notifications").and_then(|n| n.get("web_push")).cloned().unwrap_or(serde_json::Value::Null),
    });
    cfg["notifications"] = notifications;

    crate::components::config_client::put_config(&cfg)
        .await
        .map(|_| ())
}
