// NotificationsStep. Optional outbound channels. None enabled is fine;
// the dashboard works without push. Every field is persisted into the
// wizard draft (load on mount, save on change) so the choices survive
// step navigation AND reach the server-side apply; without that wiring
// the inputs were bare local signals that silently reset on remount, so
// the Review step always read "None".

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{FormField, HelpHint, Panel, Toggle};

#[cfg(feature = "hydrate")]
async fn fetch_draft() -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get("/api/wizard/draft")
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

#[cfg(feature = "hydrate")]
async fn save_draft(draft: serde_json::Value) -> Result<(), String> {
    let resp = gloo_net::http::Request::put("/api/wizard/draft")
        .json(&draft)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

/// Reconstruct the single ntfy topic URL the wizard collects from the
/// stored (base_url, topic) pair, e.g. ("https://ntfy.sh", "my-topic")
/// -> "https://ntfy.sh/my-topic". Used only by the hydrate-side load.
#[cfg(feature = "hydrate")]
fn join_ntfy_url(base_url: &str, topic: &str) -> String {
    if base_url.is_empty() || topic.is_empty() {
        return String::new();
    }
    format!("{}/{}", base_url.trim_end_matches('/'), topic)
}

/// Split a full ntfy topic URL into the (base_url, topic) pair the schema
/// stores. The last path segment is the topic; everything before it is the
/// base. Returns None when either side would be empty (no usable channel).
fn split_ntfy_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let trimmed = url.trim_end_matches('/');
    let (base, topic) = trimmed.rsplit_once('/')?;
    let topic = topic.trim();
    if base.is_empty() || topic.is_empty() {
        return None;
    }
    Some((base.to_string(), topic.to_string()))
}

#[component]
pub fn NotificationsStep() -> impl IntoView {
    let push_enabled = RwSignal::new(false);
    let mqtt_enabled = RwSignal::new(false);
    let mqtt_host = RwSignal::new(String::new());
    let ntfy_url = RwSignal::new(String::new());
    let slack_url = RwSignal::new(String::new());

    let draft = RwSignal::new(serde_json::Value::Null);
    let loaded = RwSignal::new(false);

    // Load the draft on mount and hydrate the five inputs from
    // config.notifications.* so a returning user sees their prior choices.
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                let notif = d
                    .get("config")
                    .and_then(|c| c.get("notifications"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                push_enabled.set(notif.get("web_push").map(|v| !v.is_null()).unwrap_or(false));

                let mqtt = notif
                    .get("mqtt")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                mqtt_enabled.set(!mqtt.is_null());
                mqtt_host.set(
                    mqtt.get("host")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                );

                let ntfy = notif
                    .get("ntfy")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let ntfy_base = ntfy.get("base_url").and_then(|v| v.as_str()).unwrap_or("");
                let ntfy_topic = ntfy.get("topic").and_then(|v| v.as_str()).unwrap_or("");
                ntfy_url.set(join_ntfy_url(ntfy_base, ntfy_topic));

                slack_url.set(
                    notif
                        .get("slack")
                        .and_then(|s| s.get("webhook_url"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                );

                draft.set(d);
                loaded.set(true);
            }
        });
    });

    // Persist the five fields into config.notifications whenever any of them
    // changes after load. Each channel is an Option<...> in the schema, so a
    // disabled/blank channel is written as null and an enabled one as a
    // fully-formed object (the PUT deserializes the draft into a typed Config,
    // so a partial object would 422 and lose the whole save). web_push VAPID
    // keys are server-side (env/boot); the wizard only records intent, so an
    // enabled web_push is a placeholder object that env_compat overwrites with
    // the real keypair at boot. The changed-guard makes the post-hydration
    // re-run a no-op (the draft already holds the loaded values), so only a
    // real user edit triggers a save.
    Effect::new(move |_| {
        // Track all five inputs so any edit re-runs this effect.
        let push = push_enabled.get();
        let mqtt_on = mqtt_enabled.get();
        let host = mqtt_host.get().trim().to_string();
        let ntfy_raw = ntfy_url.get();
        let slack_v = slack_url.get().trim().to_string();
        if !loaded.get_untracked() {
            return;
        }
        let web_push = if push {
            serde_json::json!({
                "vapid_public": "",
                "vapid_private_path": "",
                "vapid_subject": "",
            })
        } else {
            serde_json::Value::Null
        };
        let mqtt = if mqtt_on && !host.is_empty() {
            serde_json::json!({
                "host": host,
                "port": 1883,
                "username": serde_json::Value::Null,
                "password": serde_json::Value::Null,
                "discovery_prefix": "homeassistant",
                "publish_enabled": true,
                "subscribe_enabled": false,
            })
        } else {
            serde_json::Value::Null
        };
        let ntfy = match split_ntfy_url(&ntfy_raw) {
            Some((base_url, topic)) => serde_json::json!({
                "base_url": base_url,
                "topic": topic,
                "auth_token": serde_json::Value::Null,
            }),
            None => serde_json::Value::Null,
        };
        let slack = if slack_v.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "webhook_url": slack_v })
        };

        let mut changed = false;
        draft.update(|d| {
            let Some(notif) = d
                .get_mut("config")
                .and_then(|c| c.get_mut("notifications"))
                .and_then(|n| n.as_object_mut())
            else {
                return;
            };
            for (key, next) in [
                ("web_push", web_push),
                ("mqtt", mqtt),
                ("ntfy", ntfy),
                ("slack", slack),
            ] {
                if notif.get(key) != Some(&next) {
                    notif.insert(key.into(), next);
                    changed = true;
                }
            }
        });
        if !changed {
            return;
        }
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    });

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Notifications "<span class="setup-step__optional">"optional"</span><HelpHint topic="notifications"/></h2>
            <p class="setup-step__body">
                "Where should LocalSky send zone-start, zone-stop, daily "
                "verdict, and anomaly events? Every channel is independent; "
                "you can enable any combination, or none. Web Push lives on "
                "the device (subscribe per browser); the other channels are "
                "deployment-wide."
            </p>

            <Panel title="Web Push (per device)".to_string()>
                <Toggle
                    checked=push_enabled
                    label="Enable browser push notifications".to_string()
                    helptext="Requires a VAPID keypair on the server. Subscribe per device after the dashboard mounts.".to_string()
                />
            </Panel>

            <Panel title="MQTT (Home Assistant discovery)".to_string()>
                <Toggle
                    checked=mqtt_enabled
                    label="Publish sensors to MQTT".to_string()
                    helptext="Creates sensor.localsky_* entities in HA automatically (no manual YAML).".to_string()
                />
                <Show when=move || mqtt_enabled.get()>
                    <FormField
                        label="MQTT broker host".to_string()
                        helptext="e.g. 10.0.0.5".to_string()
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
                </Show>
            </Panel>

            <Panel title="ntfy (free, self-hostable)".to_string()>
                <FormField
                    label="ntfy topic URL".to_string()
                    helptext="e.g. https://ntfy.sh/your-private-topic. Leave blank to disable.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="url"
                        class="ui-input"
                        placeholder="https://ntfy.sh/..."
                        prop:value=move || ntfy_url.get()
                        on:input=move |ev| ntfy_url.set(event_target_value(&ev))
                    />
                </FormField>
            </Panel>

            <Panel title="Slack".to_string()>
                <FormField
                    label="Incoming webhook URL".to_string()
                    helptext="Slack incoming webhook target. Leave blank to disable.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="url"
                        class="ui-input"
                        placeholder="https://hooks.slack.com/services/..."
                        prop:value=move || slack_url.get()
                        on:input=move |ev| slack_url.set(event_target_value(&ev))
                    />
                </FormField>
            </Panel>

            <SetupFooter
                prev=prev_step_href("notifications")
                next=next_step_href("notifications")
            />
        </div>
    }
}
