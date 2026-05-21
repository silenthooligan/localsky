// NotificationsStep. Optional outbound channels. None enabled is fine;
// the dashboard works without push.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{FormField, Panel, Toggle};

#[component]
pub fn NotificationsStep() -> impl IntoView {
    let push_enabled = RwSignal::new(false);
    let mqtt_enabled = RwSignal::new(false);
    let mqtt_host = RwSignal::new(String::new());
    let ntfy_url = RwSignal::new(String::new());
    let slack_url = RwSignal::new(String::new());

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Notifications (optional)"</h2>
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
