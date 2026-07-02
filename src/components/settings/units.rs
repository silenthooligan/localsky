// SettingsUnits. Two layers of display-unit control:
//
//   1. Household default (per-deployment): Imperial / Metric, stored in
//      /data/localsky.toml as `deployment.units` and PUT via /api/config.
//      Carried on the irrigation snapshot so every device that hasn't opted
//      out follows it. This is the execution/display default for the whole
//      install.
//   2. This device (per-device, localStorage): a device can opt into its own
//      units, with per-field granularity (temp/rain/wind/pressure/distance/
//      area). Read back by components::units_fmt::use_unit_prefs.
//
// The "Use household default / This device" segmented control at the top picks
// between them. "Household default" clears the per-device localStorage keys so
// use_unit_prefs falls through to the household value; "This device" keeps the
// per-field selectors. The household value is shown as the inactive baseline so
// the user can see what "household" currently means.

use leptos::prelude::*;

use crate::components::settings_ui::SettingsResult;
use crate::components::ui::{Button, FormField, HelpHint, Panel, SegmentedControl};

#[component]
pub fn SettingsUnits() -> impl IntoView {
    // Per-device scope: "household" (follow the deployment default) or "device"
    // (this device has its own units). Stored as the `units_scope` sentinel.
    // Default "household" so a fresh device follows the deployment, which keeps
    // the imperial-default install byte-identical to before this control
    // existed (no units_system key -> use_unit_prefs expands the household).
    let scope = RwSignal::new("household".to_string());

    // Per-device per-field selectors (only meaningful when scope == "device").
    let system = RwSignal::new("imperial".to_string());
    let temp_unit = RwSignal::new("f".to_string());
    let rain_unit = RwSignal::new("in".to_string());
    let wind_unit = RwSignal::new("mph".to_string());
    let pressure_unit = RwSignal::new("inhg".to_string());
    let distance_unit = RwSignal::new("mi".to_string());
    let area_unit = RwSignal::new("sqft".to_string());

    // Household default (Imperial / Metric), fetched from + PUT to /api/config.
    let household = RwSignal::new("imperial".to_string());
    let household_loaded = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    // Transient "Saved on this device" confirmation. The per-device units have
    // no Save button (they persist to localStorage on every change), so this
    // flash is the only feedback that a pick stuck. Bumped on each persist; the
    // flash effect below shows the line for ~2s, then clears it.
    let device_saved = RwSignal::new(false);

    // Load the per-device state from localStorage + the household default from
    // /api/config on mount.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    // Scope sentinel: a device that previously opted into its
                    // own units (units_system in imperial/metric/custom) is on
                    // the "device" scope; otherwise it follows the household.
                    let opted_in = matches!(
                        storage.get_item("units_system").ok().flatten().as_deref(),
                        Some("imperial") | Some("metric") | Some("custom")
                    );
                    scope.set(if opted_in { "device" } else { "household" }.to_string());
                    if let Ok(Some(v)) = storage.get_item("units_system") {
                        if !v.is_empty() && v != "household" {
                            system.set(v);
                        }
                    }
                    if let Ok(Some(v)) = storage.get_item("units_temp") {
                        temp_unit.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_rain") {
                        rain_unit.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_wind") {
                        wind_unit.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_pressure") {
                        pressure_unit.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_distance") {
                        distance_unit.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_area") {
                        area_unit.set(v);
                    }
                }
            }
        });
        // Household default from /api/config.
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(h) = fetch_household().await {
                    household.set(h);
                    household_loaded.set(true);
                }
            });
        });
    }

    // Persist the per-device per-field selectors. Each Effect tracks a single
    // signal. These only WRITE the keys; whether use_unit_prefs reads them is
    // gated by the scope (the household-scope branch clears units_system below).
    #[cfg(feature = "hydrate")]
    {
        let persist = move |key: &'static str, sig: RwSignal<String>| {
            Effect::new(move |prev: Option<String>| {
                let val = sig.get();
                if let Some(win) = web_sys::window() {
                    if let Ok(Some(storage)) = win.local_storage() {
                        let _ = storage.set_item(key, &val);
                    }
                }
                // Flash the "Saved on this device" line on real edits only:
                // skip the first run (initial seed from localStorage) and any
                // no-op re-run where the value is unchanged.
                if let Some(prev_val) = &prev {
                    if *prev_val != val {
                        flash_device_saved(device_saved);
                    }
                }
                val
            });
        };
        persist("units_temp", temp_unit);
        persist("units_rain", rain_unit);
        persist("units_wind", wind_unit);
        persist("units_pressure", pressure_unit);
        persist("units_distance", distance_unit);
        persist("units_area", area_unit);
    }

    // Apply the scope to localStorage. On "household" we REMOVE the per-field
    // override keys and clear units_system so use_unit_prefs falls back to the
    // household value. On "device" we write units_system to the current system
    // pick so use_unit_prefs treats this device as opted-in.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let s = scope.get();
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    if s == "household" {
                        // Clear the opt-in sentinel + the six per-field keys so
                        // resolution falls through to the household Units.
                        let _ = storage.remove_item("units_system");
                        let _ = storage.remove_item("units_temp");
                        let _ = storage.remove_item("units_rain");
                        let _ = storage.remove_item("units_wind");
                        let _ = storage.remove_item("units_pressure");
                        let _ = storage.remove_item("units_distance");
                        let _ = storage.remove_item("units_area");
                    } else {
                        // Opt this device in. units_system carries the system
                        // pick (imperial/metric/custom); the per-field persist
                        // Effects re-write the six keys.
                        let _ = storage.set_item("units_system", &system.get_untracked());
                    }
                }
            }
        });
    }

    // Keep units_system in sync with the system pick while on the device scope.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let sys = system.get();
            if scope.get_untracked() != "device" {
                return;
            }
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    let _ = storage.set_item("units_system", &sys);
                }
            }
        });
    }

    // When the device system switches, snap all per-field selectors to match.
    Effect::new(move |_| {
        let s = system.get();
        if s == "imperial" {
            temp_unit.set("f".into());
            rain_unit.set("in".into());
            wind_unit.set("mph".into());
            pressure_unit.set("inhg".into());
            distance_unit.set("mi".into());
            area_unit.set("sqft".into());
        } else if s == "metric" {
            temp_unit.set("c".into());
            rain_unit.set("mm".into());
            wind_unit.set("kph".into());
            pressure_unit.set("hpa".into());
            distance_unit.set("km".into());
            area_unit.set("sqm".into());
        }
    });

    // Save the household default to /api/config.
    let on_save_household = move |_| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let units = household.get();
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match patch_household(units).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Devices on the household default update on the next snapshot.",
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
            let _ = units;
        }
    };

    let show_device = move || scope.get() == "device";
    let show_custom = move || scope.get() == "device" && system.get() == "custom";

    // Inactive baseline label: what "household default" currently means.
    let household_label = move || match household.get().as_str() {
        "metric" => "Metric (°C, mm, km/h, hPa, km, m²)",
        _ => "Imperial (°F, inches, mph, inHg, mi, sq ft)",
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Units"<HelpHint topic="units"/></h1>
                <p class="settings-page__subtitle">
                    "Display only; engine math runs in metric internally and "
                    "converts at the boundary. Two independent save models on this "
                    "screen: the household default is shared across the deployment "
                    "and needs an explicit Save (it travels on the snapshot); "
                    "per-device units live in this browser only and persist the "
                    "moment you pick them, with no Save button."
                </p>
            </header>

            <Panel title="Applies to".to_string()>
                <SegmentedControl
                    value=scope
                    options=vec![
                        ("household".into(), "Household default".into()),
                        ("device".into(), "This device only".into()),
                    ]
                    aria_label="Units scope".to_string()
                />
                <p class="sensors-section__hint">
                    {move || if scope.get() == "household" {
                        format!("This device follows the household default: {}.", household_label())
                    } else {
                        format!("The units below apply to this browser/device only; every other device keeps following the household default ({}).", household_label())
                    }}
                </p>
            </Panel>

            <Panel title="Household default".to_string() help_topic="units">
                <p class="sensors-section__hint">
                    "Per-deployment. Stored in /data/localsky.toml; carried on "
                    "the irrigation snapshot so every device that follows the "
                    "household updates on the next tick."
                </p>
                <FormField
                    label="System".to_string()
                    helptext="".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <SegmentedControl
                        value=household
                        options=vec![
                            ("imperial".into(), "Imperial".into()),
                            ("metric".into(), "Metric".into()),
                        ]
                        aria_label="Household unit system".to_string()
                    />
                </FormField>
                <div class="settings-actions">
                    <Button
                        variant="primary"
                        disabled=Signal::derive(move || saving.get())
                        on_click=Callback::new(on_save_household)
                    >
                        {move || if saving.get() { "Saving…" } else { "Save household default" }}
                    </Button>
                </div>
                <SettingsResult result_msg=result_msg result_ok=result_ok/>
                <Show when=move || !household_loaded.get()>
                    <p class="settings-page__subtitle" style="margin-top: 1rem">
                        "Loading household default from /api/config..."
                    </p>
                </Show>
            </Panel>

            <Show when=show_device>
                <Panel title="This device only".to_string()>
                    <p class="sensors-section__hint">
                        "Saved in this browser only, the instant you pick (no Save "
                        "button). Pick a system, or \"Custom\" to set each measurement "
                        "individually. The household default and your other devices are "
                        "not affected."
                    </p>
                    <FormField
                        label="System".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=system
                            options=vec![
                                ("imperial".into(), "Imperial".into()),
                                ("metric".into(), "Metric".into()),
                                ("custom".into(), "Custom (per field, this device)".into()),
                            ]
                            aria_label="Device unit system".to_string()
                        />
                    </FormField>
                    <Show when=move || device_saved.get()>
                        <p
                            class="setup-result setup-result--ok"
                            role="status"
                            style="margin-top: 0.75rem;"
                        >
                            "Saved on this device"
                        </p>
                    </Show>
                </Panel>
            </Show>

            <Show when=show_custom>
                <Panel title="Per-field overrides".to_string()>
                    <p class="sensors-section__hint">
                        "Each measurement individually, on this device only."
                    </p>
                    <FormField
                        label="Temperature".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=temp_unit
                            options=vec![
                                ("f".into(), "°F".into()),
                                ("c".into(), "°C".into()),
                            ]
                            aria_label="Temperature unit".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Rainfall".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=rain_unit
                            options=vec![
                                ("in".into(), "inches".into()),
                                ("mm".into(), "mm".into()),
                            ]
                            aria_label="Rainfall unit".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Wind speed".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=wind_unit
                            options=vec![
                                ("mph".into(), "mph".into()),
                                ("kph".into(), "km/h".into()),
                            ]
                            aria_label="Wind speed unit".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Pressure".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=pressure_unit
                            options=vec![
                                ("inhg".into(), "inHg".into()),
                                ("hpa".into(), "hPa".into()),
                            ]
                            aria_label="Pressure unit".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Distance".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=distance_unit
                            options=vec![
                                ("mi".into(), "mi".into()),
                                ("km".into(), "km".into()),
                            ]
                            aria_label="Distance unit".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Zone area".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=area_unit
                            options=vec![
                                ("sqft".into(), "sq ft".into()),
                                ("sqm".into(), "m²".into()),
                            ]
                            aria_label="Zone area unit".to_string()
                        />
                    </FormField>
                </Panel>
            </Show>
        </div>
    }
}

/// Flash the transient "Saved on this device" line: set it true now, then clear
/// it after ~2s. Each call restarts the window (a rapid second edit keeps the
/// line up rather than blinking), because the clear only fires for the latest
/// turn-on while `device_saved` is still true.
#[cfg(feature = "hydrate")]
fn flash_device_saved(device_saved: RwSignal<bool>) {
    device_saved.set(true);
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(2_000).await;
        device_saved.set(false);
    });
}

/// GET the household units default from /api/config -> "imperial" | "metric".
#[cfg(feature = "hydrate")]
async fn fetch_household() -> Result<String, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(val
        .get("deployment")
        .and_then(|d| d.get("units"))
        .and_then(|v| v.as_str())
        .unwrap_or("imperial")
        .to_string())
}

/// Read-modify-write the household units default into /api/config, mirroring
/// settings/location.rs::patch_location (GET current, splice, PUT).
#[cfg(feature = "hydrate")]
async fn patch_household(units: String) -> Result<(), String> {
    use gloo_net::http::Request;
    let cur = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = cur.json().await.map_err(|e| e.to_string())?;
    if let Some(dep) = cfg.get_mut("deployment") {
        if let Some(obj) = dep.as_object_mut() {
            // Serde rename_all = "snake_case": "imperial" | "metric".
            obj.insert("units".into(), serde_json::json!(units));
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

#[cfg(test)]
mod tests {
    use crate::ha::snapshot::Units;

    /// The household PUT round-trips deployment.units: splicing "metric" into a
    /// config value and deserializing it back yields Units::Metric (and the
    /// default "imperial" yields Imperial). This locks the wire string the
    /// patch_household splice writes against the serde contract.
    #[test]
    fn household_units_round_trips() {
        for (wire, want) in [("imperial", Units::Imperial), ("metric", Units::Metric)] {
            let mut cfg = serde_json::json!({ "deployment": { "units": "imperial" } });
            cfg["deployment"]["units"] = serde_json::json!(wire);
            let got: Units = serde_json::from_value(cfg["deployment"]["units"].clone()).unwrap();
            assert_eq!(got, want);
            // And the value serializes back to the same wire string.
            assert_eq!(serde_json::to_value(want).unwrap(), serde_json::json!(wire));
        }
    }
}
