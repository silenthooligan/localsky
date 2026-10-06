use crate::components::settings_ui::StatusHero;
use crate::components::ui::{Button, Panel, Toggle};
use crate::notification_preferences::{EventKind, PushPreferences};
use leptos::prelude::*;

#[component]
fn PreferenceSwitch(
    value: Signal<bool>,
    label: &'static str,
    help: &'static str,
    on_change: Callback<bool>,
) -> impl IntoView {
    let checked = RwSignal::new(value.get_untracked());
    Effect::new(move |_| checked.set(value.get()));
    let id = format!(
        "pwa-device-{}",
        label.to_ascii_lowercase().replace(' ', "-")
    );
    view! { <Toggle id=id checked=checked label=label.to_string() helptext=help.to_string() on_change=on_change/> }
}

#[component]
pub fn PwaNotifications(server_revision: RwSignal<u32>) -> impl IntoView {
    #[cfg(not(feature = "hydrate"))]
    let _ = server_revision;
    let prefs = RwSignal::new(PushPreferences::default());
    let loaded = RwSignal::new(false);
    let subscribed = RwSignal::new(false);
    let available = RwSignal::new(false);
    let server_ready = RwSignal::new(false);
    let server_enabled = RwSignal::new(false);
    let timezone = RwSignal::new(String::new());
    let state = RwSignal::new("Checking this device…".to_string());
    let busy = RwSignal::new(false);
    let dirty = RwSignal::new(false);
    let message = RwSignal::new(String::new());
    let error = RwSignal::new(String::new());
    let retry = RwSignal::new(0u32);

    // Refresh delivery status after a shared-channel save without discarding
    // this device's unsaved choices.
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        if server_revision.get() == 0 {
            return;
        }
        leptos::task::spawn_local(async move {
            if let Ok(s) = crate::push_client::status().await {
                server_ready.set(s.ready);
                server_enabled.set(s.enabled);
                timezone.set(s.timezone);
            }
        });
    });

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let _ = retry.get();
        loaded.set(false);
        available.set(false);
        error.set(String::new());
        leptos::task::spawn_local(async move {
            match crate::push_client::status().await {
                Ok(s) => {
                    server_ready.set(s.ready);
                    server_enabled.set(s.enabled);
                    timezone.set(s.timezone);
                }
                Err(e) => {
                    state.set("Connection unavailable".into());
                    error.set(e);
                    return;
                }
            }
            match crate::push_client::permission_state() {
                Err(_) => {
                    state.set("Use the installed PWA or an HTTPS browser".into());
                    return;
                }
                Ok(p) if p == "denied" => {
                    state.set("Notifications blocked in this browser".into());
                    return;
                }
                _ => available.set(true),
            }
            match crate::push_client::preferences(None).await {
                Ok(Some(p)) => {
                    prefs.set(p);
                    subscribed.set(true);
                    loaded.set(true);
                    dirty.set(false);
                    state.set("Connected".into());
                }
                Ok(None) => {
                    subscribed.set(false);
                    state.set("This device is not connected".into());
                }
                Err(e) => {
                    state.set("Reconnect or retry to load this device".into());
                    error.set(e);
                }
            }
        });
    });

    let connect = move |_| {
        busy.set(true);
        error.set(String::new());
        message.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match crate::push_client::subscribe().await {
                Ok(()) => retry.update(|n| *n += 1),
                Err(e) => error.set(e),
            }
            busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        busy.set(false);
    };
    let disconnect = move |_| {
        busy.set(true);
        error.set(String::new());
        message.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match crate::push_client::unsubscribe().await {
                Ok(()) => {
                    subscribed.set(false);
                    loaded.set(false);
                    retry.update(|n| *n += 1);
                }
                Err(e) => error.set(e),
            }
            busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        busy.set(false);
    };
    let save = move |_| {
        let next = prefs.get_untracked();
        if let Err(e) = next.validate() {
            error.set(e.into());
            return;
        }
        busy.set(true);
        error.set(String::new());
        message.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            match crate::push_client::preferences(Some(&next)).await {
                Ok(Some(saved)) => {
                    prefs.set(saved);
                    dirty.set(false);
                    message.set("Saved for this device.".into());
                }
                Ok(None) => error.set("Reconnect this device before saving.".into()),
                Err(e) => error.set(e),
            }
            busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        busy.set(false);
    };

    view! {
        <section class="pwa-preferences" aria-label="This device's notifications">
            <StatusHero icon="bell" title="This device"
                ok=Signal::derive(move || loaded.get() && prefs.get().enabled && server_ready.get() && server_enabled.get())
                chip=Signal::derive(move || if !loaded.get() { state.get() } else if !server_ready.get() { "Setup needed".into() } else if !server_enabled.get() { "Server delivery off".into() } else if !prefs.get().enabled { "Paused".into() } else { "Connected".into() })
                meaning=Signal::derive(move || if !loaded.get() { "Connect this phone or browser to choose its alerts.".to_string() } else { "Your choices apply to this device, even with LocalSky closed.".to_string() })
            />
            <div class="pwa-preferences__connection">
                <Show when=move || available.get() && !loaded.get()>
                    <Button disabled=Signal::derive(move || busy.get() || !server_ready.get() || !server_enabled.get()) on_click=Callback::new(connect)>"Connect this device"</Button>
                </Show>
                <Show when=move || subscribed.get()>
                    <Button variant="secondary" disabled=Signal::derive(move || busy.get()) on_click=Callback::new(disconnect)>"Disconnect"</Button>
                </Show>
                <Button variant="ghost" disabled=Signal::derive(move || busy.get() || dirty.get()) on_click=Callback::new(move |_| retry.update(|n| *n += 1))>"Check again"</Button>
            </div>
            <Show when=move || !server_ready.get() && !timezone.get().is_empty()>
                <p class="pwa-preferences__note">"Web Push needs a server key. Enable Web Push in "<a href=crate::base::url("/setup")>"Setup"</a>" to create it."</p>
            </Show>
            <Show when=move || server_ready.get() && !server_enabled.get()>
                <p class="pwa-preferences__note">"Server delivery is off. Turn on Send push alerts under Server and shared channels below."</p>
            </Show>
            <Show when=move || loaded.get()>
                <fieldset class="pwa-preferences__fields" disabled=move || busy.get()>
                    <legend class="sr-only">"Device preferences"</legend>
                    <Panel title="Delivery".to_string()>
                        <PreferenceSwitch value=Signal::derive(move || prefs.get().enabled) label="Notifications on this device" help="Pause all alerts here without changing your other devices."
                            on_change=Callback::new(move |v| { prefs.update(|p| p.enabled = v); dirty.set(true); })/>
                        <PreferenceSwitch value=Signal::derive(move || prefs.get().quiet_hours.enabled) label="Quiet hours" help="Skip routine alerts during these hours. They will not arrive later."
                            on_change=Callback::new(move |v| { prefs.update(|p| p.quiet_hours.enabled = v); dirty.set(true); })/>
                        <div class="pwa-preferences__times">
                            <label>"From"<input class="ui-input" type="time" aria-label="Quiet hours start" disabled=move || !prefs.get().quiet_hours.enabled
                                prop:value=move || prefs.get().quiet_hours.start on:input=move |ev| { prefs.update(|p| p.quiet_hours.start = event_target_value(&ev)); dirty.set(true); }/></label>
                            <label>"Until"<input class="ui-input" type="time" aria-label="Quiet hours end" disabled=move || !prefs.get().quiet_hours.enabled
                                prop:value=move || prefs.get().quiet_hours.end on:input=move |ev| { prefs.update(|p| p.quiet_hours.end = event_target_value(&ev)); dirty.set(true); }/></label>
                        </div>
                        <p class="settings-page__subtitle">{move || format!("Times use the LocalSky location timezone ({}).", timezone.get())}</p>
                        <PreferenceSwitch value=Signal::derive(move || prefs.get().quiet_hours.allow_urgent) label="Urgent alerts during quiet hours" help="Let enabled urgent equipment alerts through. Pausing this device still silences everything."
                            on_change=Callback::new(move |v| { prefs.update(|p| p.quiet_hours.allow_urgent = v); dirty.set(true); })/>
                    </Panel>
                    { ["Irrigation", "Equipment and sensors", "Weather", "Updates"].into_iter().map(move |group| view! {
                        <Panel title=group.to_string()>
                            {(group == "Weather").then(|| view! { <p class="pwa-preferences__note">"Optional alerts from fresh station measurements. These are not official weather warnings."</p> })}
                            {EventKind::ALL.into_iter().filter(move |kind| kind.group() == group).map(move |kind| view! {
                                <PreferenceSwitch value=Signal::derive(move || prefs.get().events.contains(&kind)) label=kind.label() help=kind.trigger()
                                    on_change=Callback::new(move |v| { prefs.update(|p| { if v { p.events.insert(kind); } else { p.events.remove(&kind); } }); dirty.set(true); })/>
                            }).collect_view()}
                            {(group == "Weather").then(|| view! {
                                <details class="pwa-preferences__details"><summary>"When weather alerts repeat"</summary>
                                    <p>"An alert fires when a measured condition crosses its threshold. It must clear before another alert, with at least an hour between alerts of the same kind. Existing conditions at first setup establish a baseline; nearby lightning can alert immediately."</p>
                                </details>
                            })}
                        </Panel>
                    }).collect_view() }
                    <Panel title="Daily outlook".to_string()>
                        <PreferenceSwitch value=Signal::derive(move || prefs.get().daily_outlook.enabled) label="Daily watering outlook on this device" help="One optional summary per day. Forecast updates stay in the app."
                            on_change=Callback::new(move |v| { prefs.update(|p| p.daily_outlook.enabled = v); dirty.set(true); })/>
                        <label class="pwa-preferences__summary-time">"Delivery time"<input class="ui-input" type="time" aria-label="Device outlook time" disabled=move || !prefs.get().daily_outlook.enabled
                            prop:value=move || prefs.get().daily_outlook.time on:input=move |ev| { prefs.update(|p| p.daily_outlook.time = event_target_value(&ev)); dirty.set(true); }/></label>
                        <p class="settings-page__subtitle">"Choose a time outside quiet hours. Missed outlooks are skipped."</p>
                    </Panel>
                    <div class="settings-actions">
                        <Button disabled=Signal::derive(move || busy.get() || !dirty.get()) on_click=Callback::new(save)>{move || if busy.get() { "Saving…" } else { "Save device settings" }}</Button>
                        <span role="status">{move || if dirty.get() { "Unsaved changes".to_string() } else { message.get() }}</span>
                    </div>
                    <p class="settings-page__subtitle">"Offline devices do not receive a backlog. Your browser controls notification sound and delivery."</p>
                </fieldset>
            </Show>
            <Show when=move || !error.get().is_empty()><p class="pwa-preferences__error" role="alert">{move || error.get()}</p></Show>
        </section>
    }
}
