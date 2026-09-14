// SettingsNotifications. Edit cfg.notifications: the Web Push server
// switch plus this device's own subscription, MQTT broker host, ntfy
// URL and Slack URL.

use leptos::prelude::*;

use crate::components::settings_ui::{SettingsResult, StatusHero};
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

    let web_push_enabled = RwSignal::new(false);

    // This device's push subscription. Read from the browser on mount;
    // Subscribe asks for permission, subscribes against the server's
    // VAPID key and registers the endpoint; Unsubscribe tears both down.
    let device_state = RwSignal::new(DeviceState::Unknown);
    let device_busy = RwSignal::new(false);
    let device_msg = RwSignal::new(String::new());
    #[cfg(feature = "hydrate")]
    {
        leptos::task::spawn_local(async move {
            device_state.set(read_device_state().await);
        });
    }
    let on_subscribe = move |_| {
        device_busy.set(true);
        device_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match crate::push_client::subscribe().await {
                Ok(()) => device_msg.set("This device will get alerts.".into()),
                Err(e) => device_msg.set(e),
            }
            device_state.set(read_device_state().await);
            device_busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        device_busy.set(false);
    };
    let on_unsubscribe = move |_| {
        device_busy.set(true);
        device_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match crate::push_client::unsubscribe().await {
                Ok(()) => device_msg.set("This device stopped getting alerts.".into()),
                Err(e) => device_msg.set(e),
            }
            device_state.set(read_device_state().await);
            device_busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        device_busy.set(false);
    };

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

    // Status hero: how many channels are actually wired up. Shares the HA page's
    // hero look so the integration pages read as one family.
    let active_count = move || {
        let mut n = 0;
        if !mqtt_host.get().trim().is_empty() && mqtt_publish_enabled.get() {
            n += 1;
        }
        if !ntfy_base_url.get().trim().is_empty() && !ntfy_topic.get().trim().is_empty() {
            n += 1;
        }
        if !slack_webhook.get().trim().is_empty() {
            n += 1;
        }
        if web_push_enabled.get() {
            n += 1;
        }
        n
    };
    let hero_chip = move || {
        let n = active_count();
        if n == 0 {
            "Off".to_string()
        } else {
            format!("{n} active")
        }
    };
    let hero_meaning = move || {
        let n = active_count();
        if n == 0 {
            "No channels set up yet, so run/skip alerts go nowhere. Turn on a channel below."
                .to_string()
        } else {
            format!(
                "{n} channel{} will receive run/skip + verdict alerts. Each is independent.",
                if n == 1 { "" } else { "s" }
            )
        }
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Notifications"</h1>
                <p class="settings-page__subtitle">
                    "Where alerts go: zone start and stop, the daily verdict, "
                    "and anomalies. Each channel is independent."
                </p>
            </header>

            <StatusHero
                icon="bell"
                title="Notifications"
                ok=Signal::derive(move || active_count() > 0)
                chip=Signal::derive(hero_chip)
                meaning=Signal::derive(hero_meaning)
            />

            <Panel title="Web Push".to_string() help_topic="notifications">
                <Toggle
                    checked=web_push_enabled
                    label="Send push alerts".to_string()
                    helptext="Needs a VAPID keypair (env vars or /data/keys/). Then each device subscribes below.".to_string()
                />
                <div class="push-device">
                    <div class="push-device__status">
                        <span class="push-device__label">"This device"</span>
                        <span class="push-device__state">{move || device_state.get().label()}</span>
                    </div>
                    <div class="push-device__actions">
                        {move || match device_state.get() {
                            DeviceState::Subscribed => view! {
                                <Button
                                    variant="secondary"
                                    size="sm"
                                    disabled=Signal::derive(move || device_busy.get())
                                    on_click=Callback::new(on_unsubscribe)
                                >
                                    "Unsubscribe"
                                </Button>
                            }.into_any(),
                            DeviceState::Unsupported | DeviceState::Blocked => ().into_any(),
                            _ => view! {
                                <Button
                                    variant="primary"
                                    size="sm"
                                    disabled=Signal::derive(move || device_busy.get())
                                    on_click=Callback::new(on_subscribe)
                                >
                                    "Subscribe this device"
                                </Button>
                            }.into_any(),
                        }}
                    </div>
                    {move || {
                        let m = device_msg.get();
                        (!m.is_empty()).then(|| view! { <p class="push-device__msg">{m}</p> })
                    }}
                </div>
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
                    disabled=Signal::derive(move || saving.get())
                    on_click=Callback::new(on_save)
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </Button>
            </div>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>
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
}

#[cfg(feature = "hydrate")]
async fn fetch_notifications() -> Result<NotificationsDraft, String> {
    let val = crate::components::config_client::get_config().await?;
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
    });
    cfg["notifications"] = notifications;

    crate::components::config_client::put_config(&cfg)
        .await
        .map(|_| ())
}

/// What the browser says about this device's subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
enum DeviceState {
    /// Not read yet (server render, or the query is in flight).
    Unknown,
    /// The browser has no push support or no service worker (plain HTTP).
    Unsupported,
    /// Notifications are blocked in the browser; only the browser can lift it.
    Blocked,
    NotSubscribed,
    Subscribed,
}

impl DeviceState {
    fn label(self) -> &'static str {
        match self {
            DeviceState::Unknown => "Checking",
            DeviceState::Unsupported => "Not available here (push needs HTTPS)",
            DeviceState::Blocked => "Blocked in the browser",
            DeviceState::NotSubscribed => "Not subscribed",
            DeviceState::Subscribed => "Subscribed",
        }
    }
}

#[cfg(feature = "hydrate")]
async fn read_device_state() -> DeviceState {
    match crate::push_client::permission_state() {
        Err(_) => DeviceState::Unsupported,
        Ok(p) if p == "denied" => DeviceState::Blocked,
        Ok(_) => {
            if crate::push_client::is_subscribed().await {
                DeviceState::Subscribed
            } else {
                DeviceState::NotSubscribed
            }
        }
    }
}
