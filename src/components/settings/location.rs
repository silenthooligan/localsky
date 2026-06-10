// SettingsLocation. Edit deployment.location in /data/localsky.toml.
// Reads + writes via /api/config; round-trips through PUT so the
// engine picks up new lat/lon on the next tick.
//
// Live validation matches the wizard's LocationStep. Save is gated
// when validation fails.

use leptos::prelude::*;

use crate::components::settings_ui::SettingsResult;
use crate::components::ui::{FormField, HelpHint, Panel};

#[component]
pub fn SettingsLocation() -> impl IntoView {
    let lat = RwSignal::new(0.0f64);
    let lon = RwSignal::new(0.0f64);
    let elevation = RwSignal::new(0.0f64);
    let tz = RwSignal::new(String::new());
    let display_name = RwSignal::new(String::new());

    let loaded = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    // Load current config on mount.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = fetch_config().await {
                    lat.set(cfg.lat);
                    lon.set(cfg.lon);
                    elevation.set(cfg.elevation);
                    tz.set(cfg.tz);
                    display_name.set(cfg.display_name);
                    loaded.set(true);
                }
            });
        });
    }

    let lat_err: Signal<Option<String>> = Signal::derive(move || {
        let v = lat.get();
        if !(-90.0..=90.0).contains(&v) {
            Some(format!("Latitude must be between -90 and 90 (got {v:.4})"))
        } else {
            None
        }
    });
    let lon_err: Signal<Option<String>> = Signal::derive(move || {
        let v = lon.get();
        if !(-180.0..=180.0).contains(&v) {
            Some(format!(
                "Longitude must be between -180 and 180 (got {v:.4})"
            ))
        } else {
            None
        }
    });

    let can_save = move || lat_err.get().is_none() && lon_err.get().is_none();

    let on_save = move |_| {
        if !can_save() || saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let payload = LocationDraft {
            lat: lat.get(),
            lon: lon.get(),
            elevation: elevation.get(),
            tz: tz.get(),
            display_name: display_name.get(),
        };
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match patch_location(payload).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Engine picks up on next tick.",
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
                <h1 class="settings-page__title">"Location"</h1>
                <p class="settings-page__subtitle">
                    "Per-deployment. Stored in /data/localsky.toml; engine "
                    "and forecast sources pick up on the next tick. A
                     snapshot of the previous config goes into the
                     rollback history before the write."
                </p>
            </header>

            <Panel title="Coordinates".to_string()>
                <HelpHint topic="location"/>
                <div class="grid settings-field-grid">
                <FormField
                    label="Latitude".to_string()
                    helptext="Decimal degrees (positive north).".to_string()
                    error=lat_err
                >
                    <input
                        type="number"
                        step="0.0001"
                        class="ui-input"
                        prop:value=move || lat.get()
                        on:input=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                lat.set(v);
                            }
                        }
                    />
                </FormField>

                <FormField
                    label="Longitude".to_string()
                    helptext="Decimal degrees (positive east).".to_string()
                    error=lon_err
                >
                    <input
                        type="number"
                        step="0.0001"
                        class="ui-input"
                        prop:value=move || lon.get()
                        on:input=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                lon.set(v);
                            }
                        }
                    />
                </FormField>

                <FormField
                    label="Elevation (m)".to_string()
                    helptext="Optional. Used by the FAO-56 Penman-Monteith net-radiation term.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="number"
                        step="1"
                        class="ui-input"
                        prop:value=move || elevation.get()
                        on:input=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                elevation.set(v);
                            }
                        }
                    />
                </FormField>
                </div>
            </Panel>

            <Panel title="Identity".to_string()>
                <HelpHint topic="location"/>
                <div class="grid settings-field-grid">
                <FormField
                    label="Deployment name".to_string()
                    helptext="Surfaces in the MQTT discovery node_id and the dashboard title. Slugified for topic safety.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="text"
                        class="ui-input"
                        placeholder="LocalSky"
                        prop:value=move || display_name.get()
                        on:input=move |ev| display_name.set(event_target_value(&ev))
                    />
                </FormField>

                <FormField
                    label="Timezone".to_string()
                    helptext="IANA name (e.g. America/New_York). Leave blank to derive from lat/lon at boot.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="text"
                        class="ui-input"
                        placeholder="America/New_York"
                        prop:value=move || tz.get()
                        on:input=move |ev| tz.set(event_target_value(&ev))
                    />
                </FormField>
                </div>
            </Panel>

            <div class="settings-actions">
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    disabled=move || !can_save() || saving.get()
                    on:click=on_save
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </button>
            </div>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>

            <Show when=move || !loaded.get()>
                <p class="settings-page__subtitle" style="margin-top: 1rem">
                    "Loading current location from /api/config..."
                </p>
            </Show>
        </main>
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct LocationDraft {
    lat: f64,
    lon: f64,
    elevation: f64,
    tz: String,
    display_name: String,
}

#[cfg(feature = "hydrate")]
async fn fetch_config() -> Result<LocationDraft, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let loc = val.get("deployment").and_then(|d| d.get("location"));
    Ok(LocationDraft {
        lat: loc
            .and_then(|l| l.get("lat"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        lon: loc
            .and_then(|l| l.get("lon"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        elevation: loc
            .and_then(|l| l.get("elevation_m"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        tz: val
            .get("deployment")
            .and_then(|d| d.get("timezone"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        display_name: val
            .get("deployment")
            .and_then(|d| d.get("display_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

#[cfg(feature = "hydrate")]
async fn patch_location(d: LocationDraft) -> Result<(), String> {
    use gloo_net::http::Request;
    let cur = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = cur.json().await.map_err(|e| e.to_string())?;
    if let Some(dep) = cfg.get_mut("deployment") {
        if let Some(loc) = dep.get_mut("location") {
            if let Some(obj) = loc.as_object_mut() {
                obj.insert("lat".into(), serde_json::json!(d.lat));
                obj.insert("lon".into(), serde_json::json!(d.lon));
                obj.insert("elevation_m".into(), serde_json::json!(d.elevation));
            }
        }
        if let Some(obj) = dep.as_object_mut() {
            obj.insert(
                "timezone".into(),
                if d.tz.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(d.tz)
                },
            );
            if !d.display_name.is_empty() {
                obj.insert("display_name".into(), serde_json::json!(d.display_name));
            }
        }
    }
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
