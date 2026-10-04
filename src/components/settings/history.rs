use leptos::prelude::*;

use crate::components::settings_ui::{SettingsLoadError, SettingsResult};
use crate::components::ui::{Button, ConfirmSheet, FormField, Panel, SkeletonRows};

fn days(value: &str) -> Result<u32, String> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|_| "Enter a whole number of days, or 0 to keep forever.".to_string())
}

fn shortens(previous: u32, next: u32) -> bool {
    next > 0 && (previous == 0 || next < previous)
}

#[cfg(feature = "hydrate")]
fn limits(config: &serde_json::Value) -> Result<(u32, u32), String> {
    let section = &config["persistence"];
    let parse = |name: &str| {
        section[name]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| "History settings could not be read. Reload and try again.".to_string())
    };
    Ok((parse("retention_days")?, parse("runs_retention_days")?))
}

#[component]
pub fn SettingsHistory() -> impl IntoView {
    let sensor_days = RwSignal::new(String::new());
    let run_days = RwSignal::new(String::new());
    let original = RwSignal::new(None::<(u32, u32)>);
    let load_error = RwSignal::new(None::<String>);
    let retry = RwSignal::new(0u32);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let confirm_open = RwSignal::new(false);
    let pending = RwSignal::new(None::<(u32, u32)>);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let _ = retry.get();
        leptos::task::spawn_local(async move {
            let result = crate::components::config_client::get_config()
                .await
                .and_then(|v| limits(&v));
            match result {
                Ok((sensor, runs)) => {
                    sensor_days.set(sensor.to_string());
                    run_days.set(runs.to_string());
                    original.set(Some((sensor, runs)));
                    load_error.set(None);
                }
                Err(e) => {
                    original.set(None);
                    load_error.set(Some(e));
                }
            }
        });
    });

    let do_save = Callback::new(move |()| {
        let (Some(next), Some(previous)) = (pending.get(), original.get()) else {
            return;
        };
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let result = async {
                let mut config = crate::components::config_client::get_config().await?;
                if limits(&config)? != previous {
                    return Err(
                        "Retention changed in another session. Reload before saving.".to_string(),
                    );
                }
                config["persistence"]["retention_days"] = serde_json::json!(next.0);
                config["persistence"]["runs_retention_days"] = serde_json::json!(next.1);
                crate::components::config_client::put_config(&config).await
            }
            .await;
            match result {
                Ok(outcome) => {
                    original.set(Some(next));
                    result_ok.set(true);
                    result_msg.set(outcome.save_confirmation().to_string());
                }
                Err(e) => {
                    result_ok.set(false);
                    result_msg.set(e);
                }
            }
            saving.set(false);
            pending.set(None);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = (next, previous);
            saving.set(false);
        }
    });

    let save = move |_| {
        if saving.get() {
            return;
        }
        let Some(previous) = original.get() else {
            return;
        };
        match days(&sensor_days.get()).and_then(|s| days(&run_days.get()).map(|r| (s, r))) {
            Ok(next) => {
                pending.set(Some(next));
                if shortens(previous.0, next.0) || shortens(previous.1, next.1) {
                    confirm_open.set(true);
                } else {
                    do_save.run(());
                }
            }
            Err(e) => {
                result_ok.set(false);
                result_msg.set(e);
            }
        }
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"History retention"</h1>
                <p class="settings-page__subtitle">"Choose how long LocalSky keeps your data. These limits apply to this instance."</p>
            </header>
            <Show when=move || load_error.get().is_none()
                fallback=move || view! { <SettingsLoadError error=load_error retry=retry/> }>
                <Show when=move || original.get().is_some() fallback=|| view! { <SkeletonRows count=3/> }>
                    <Panel title="Keep history".to_string()>
                        <p class="settings-page__subtitle">"Use 0 to keep forever. Shorter limits permanently remove older records during cleanup."</p>
                        <FormField label="Sensor readings (days)" helptext="Temperature, rain, wind, and other raw readings. Default: 90 days.">
                            <input class="ui-input" type="number" min="0" max="4294967295" step="1" required
                                prop:value=move || sensor_days.get() on:input=move |ev| sensor_days.set(event_target_value(&ev))/>
                        </FormField>
                        <FormField label="Watering history (days)" helptext="Run and skip records and decision details. Default: forever. Keep at least 365 days for yearly comparisons.">
                            <input class="ui-input" type="number" min="0" max="4294967295" step="1" required
                                prop:value=move || run_days.get() on:input=move |ev| run_days.set(event_target_value(&ev))/>
                        </FormField>
                        <details class="retention-details">
                            <summary>"When does cleanup happen?"</summary>
                            <p>"Sensor cleanup runs hourly as readings arrive; watering cleanup runs daily. Changes need no restart. Deleted records cannot be recovered without a backup."</p>
                        </details>
                        <a href="/api/v1/backup" download>"Download a backup"</a>
                    </Panel>
                    <div class="settings-actions">
                        <Button variant="primary" disabled=Signal::derive(move || saving.get())
                            on_click=Callback::new(save)>{move || if saving.get() { "Saving…" } else { "Save retention" }}</Button>
                    </div>
                </Show>
            </Show>
            <SettingsResult result_msg result_ok/>
            <ConfirmSheet visible=confirm_open title="Shorten history retention?"
                body=Signal::derive(move || {
                    let Some((sensor, runs)) = pending.get() else { return String::new(); };
                    let describe = |v| if v == 0 { "forever".to_string() } else { format!("{v} days") };
                    format!("Keep sensor readings for {} and watering history for {}. Older records will be permanently deleted during cleanup. Download a backup first if you need them.", describe(sensor), describe(runs))
                }) confirm_label=Signal::derive(|| "Save shorter limits".to_string()) danger=true on_confirm=do_save/>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_shorter_finite_limits_need_confirmation() {
        assert!(shortens(0, 90));
        assert!(shortens(365, 90));
        assert!(!shortens(90, 365));
        assert!(!shortens(90, 0));
        assert!(!shortens(0, 0));
        for invalid in ["", "-1", "1.5", "4294967296", "NaN"] {
            assert!(days(invalid).is_err());
        }
        assert_eq!(days("0"), Ok(0));
    }
}
