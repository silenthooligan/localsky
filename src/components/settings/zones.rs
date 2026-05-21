// SettingsZones. Per-zone editor with structured fields (not raw JSON):
// slug + display_name + species + soil_texture + area + sprinkler type
// + measured precip rate + controller mapping. Save round-trips through
// the full Config PUT like the Sources/Controllers pages.

use leptos::prelude::*;

use crate::components::ui::{FormField, Panel, SegmentedControl};

#[component]
pub fn SettingsZones() -> impl IntoView {
    let config_json = RwSignal::new(serde_json::Value::Null);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    // "Add zone" form state.
    let add_open = RwSignal::new(false);
    let new_slug = RwSignal::new(String::new());
    let new_display_name = RwSignal::new(String::new());
    let new_species = RwSignal::new("st_augustine".to_string());
    let new_soil = RwSignal::new("sandy_loam".to_string());
    let new_area = RwSignal::new(1000.0f64);
    let new_sprinkler = RwSignal::new("rotor".to_string());
    let new_precip = RwSignal::new(String::new()); // empty = use catalog default
    let new_controller = RwSignal::new(String::new());
    let new_station = RwSignal::new(String::new());
    let new_photo_url = RwSignal::new(String::new()); // optional zone photo

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = fetch_config().await {
                    // Pre-select first available controller for new zones.
                    if let Some(ctrl) = cfg
                        .get("controllers")
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.first())
                    {
                        if let Some(id) = ctrl.get("id").and_then(|v| v.as_str()) {
                            new_controller.set(id.to_string());
                        }
                    }
                    config_json.set(cfg);
                }
            });
        });
    }

    let zones_view = move || {
        let cfg = config_json.get();
        let zones_obj = cfg.get("zones").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let mut keys: Vec<String> = zones_obj.keys().cloned().collect();
        keys.sort();
        keys.into_iter().map(|slug| {
            let zone = zones_obj.get(&slug).cloned().unwrap_or(serde_json::Value::Null);
            let display = zone.get("display_name").and_then(|v| v.as_str()).unwrap_or(&slug).to_string();
            let species = zone.get("species").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let soil = zone.get("soil_texture").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let area = zone.get("area_sqft").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let ctrl_id = zone.get("controller_id").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let slug_for_delete = slug.clone();
            view! {
                <li class="settings-list__item settings-list__item--row">
                    <span class="settings-list__icon" aria-hidden="true">"🌱"</span>
                    <span class="settings-list__text">
                        <span class="settings-list__label">{display}</span>
                        <span class="settings-list__helptext">
                            {format!("{slug} · {species} · {soil} · {area:.0} ft² · controller {ctrl_id}")}
                        </span>
                    </span>
                    <button
                        class="setup-footer__btn setup-footer__btn--ghost"
                        type="button"
                        aria-label=format!("Delete zone {slug}")
                        on:click=move |_| {
                            let s = slug_for_delete.clone();
                            config_json.update(|cfg| {
                                if let Some(zones) = cfg.get_mut("zones").and_then(|v| v.as_object_mut()) {
                                    zones.remove(&s);
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
        let slug = new_slug.get().trim().to_lowercase().replace(' ', "_");
        if slug.is_empty() {
            result_ok.set(false);
            result_msg.set("Zone slug is required".into());
            return;
        }
        if new_controller.get().is_empty() {
            result_ok.set(false);
            result_msg.set("Controller is required; configure one under /settings/controllers first".into());
            return;
        }
        let area = new_area.get();
        if area <= 0.0 {
            result_ok.set(false);
            result_msg.set("Area must be > 0".into());
            return;
        }
        let precip_value = new_precip.get();
        let precip = if precip_value.trim().is_empty() {
            serde_json::Value::Null
        } else {
            match precip_value.parse::<f64>() {
                Ok(v) if v > 0.0 && v < 200.0 => serde_json::json!(v),
                _ => {
                    result_ok.set(false);
                    result_msg.set("Precip rate must be a number 0..200 mm/hr (or blank)".into());
                    return;
                }
            }
        };
        let precip_source = if precip.is_null() { "catalog" } else { "measured" };
        let display_name = if new_display_name.get().is_empty() {
            slug.replace('_', " ")
        } else {
            new_display_name.get()
        };
        let photo_url_json = {
            let s = new_photo_url.get();
            if s.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(s)
            }
        };
        let entry = serde_json::json!({
            "display_name": display_name,
            "area_sqft": area,
            "species": new_species.get(),
            "soil_texture": new_soil.get(),
            "slope_pct": 0.0,
            "sun_exposure": "full",
            "sprinkler_type": new_sprinkler.get(),
            "precip_rate_mm_hr": precip,
            "precip_rate_source": precip_source,
            "root_depth_mm": serde_json::Value::Null,
            "mad_pct_override": serde_json::Value::Null,
            "controller_id": new_controller.get(),
            "controller_station": new_station.get(),
            "soil_sensor_id": serde_json::Value::Null,
            "target_min_pct_soil": 30.0,
            "saturation_pct_soil": 70.0,
            "photo_url": photo_url_json,
        });
        config_json.update(|cfg| {
            let zones = cfg
                .as_object_mut()
                .and_then(|o| o.entry("zones").or_insert(serde_json::json!({})).as_object_mut());
            if let Some(zones) = zones {
                zones.insert(slug.clone(), entry);
            }
        });
        new_slug.set(String::new());
        new_display_name.set(String::new());
        new_area.set(1000.0);
        new_precip.set(String::new());
        new_station.set(String::new());
        add_open.set(false);
        result_ok.set(true);
        result_msg.set(format!("Added zone '{slug}'. Click Save to apply."));
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
                        result_msg.set("Saved. Engine picks up new zones on next tick.".into());
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

    // Pull configured controller ids for the picker.
    let controller_options = move || {
        let cfg = config_json.get();
        let arr = cfg.get("controllers").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        arr.into_iter()
            .filter_map(|c| {
                c.get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| (s.to_string(), s.to_string()))
            })
            .collect::<Vec<_>>()
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Zones"</h1>
                <p class="settings-page__subtitle">
                    "One zone = one chunk of yard tied to one valve. Pick grass species + soil texture + measured precip rate; the engine computes ETc, soil bucket, and runtime from there. "
                    "See "
                    <a href="https://github.com/silenthooligan/localsky/blob/main/docs/grass-species.md"
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"the species catalog"</a>
                    " and "
                    <a href="https://github.com/silenthooligan/localsky/blob/main/docs/soil-textures.md"
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"soil textures"</a>
                    " for reference."
                </p>
            </header>

            <Panel title="Configured zones".to_string()>
                <ul class="settings-list">
                    {zones_view}
                </ul>

                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    style="margin-top: 1rem"
                    on:click=move |_| add_open.update(|v| *v = !*v)
                >
                    {move || if add_open.get() { "× Cancel add" } else { "+ Add zone" }}
                </button>
            </Panel>

            <Show when=move || add_open.get()>
                <Panel title="New zone".to_string()>
                    <FormField
                        label="Slug".to_string()
                        helptext="snake_case identifier; URL-safe; used by skip-check + history. e.g. back_yard.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            placeholder="back_yard"
                            prop:value=move || new_slug.get()
                            on:input=move |ev| new_slug.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Display name".to_string()
                        helptext="Human-readable label. Defaults to the slug with underscores -> spaces.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            placeholder="Back Yard"
                            prop:value=move || new_display_name.get()
                            on:input=move |ev| new_display_name.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Grass species".to_string()
                        helptext="Picks the Kc seasonal curve, root depth, and MAD threshold.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=new_species
                            options=vec![
                                ("st_augustine".into(), "St. Augustine".into()),
                                ("bermuda".into(), "Bermuda".into()),
                                ("zoysia".into(), "Zoysia".into()),
                                ("bahia".into(), "Bahia".into()),
                                ("centipede".into(), "Centipede".into()),
                                ("kentucky_bluegrass".into(), "KBG".into()),
                                ("tall_fescue".into(), "Tall Fescue".into()),
                                ("perennial_ryegrass".into(), "PRG".into()),
                                ("ornamental_shrubs".into(), "Shrubs".into()),
                                ("vegetable_garden".into(), "Vegetables".into()),
                                ("drip_xeriscape".into(), "Drip / xeri".into()),
                                ("other".into(), "Other".into()),
                            ]
                            aria_label="Grass species".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Soil texture".to_string()
                        helptext="USDA class. Drives field capacity, wilting point, and infiltration rate.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=new_soil
                            options=vec![
                                ("sand".into(), "Sand".into()),
                                ("loamy_sand".into(), "Loamy sand".into()),
                                ("sandy_loam".into(), "Sandy loam".into()),
                                ("loam".into(), "Loam".into()),
                                ("silt_loam".into(), "Silt loam".into()),
                                ("clay_loam".into(), "Clay loam".into()),
                                ("clay".into(), "Clay".into()),
                            ]
                            aria_label="Soil texture".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Area (sqft)".to_string()
                        helptext="Approximate; doesn't have to be exact. Used by leak detection + flow validation when a flow meter is configured.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="number"
                            class="ui-input"
                            min="1"
                            step="50"
                            prop:value=move || format!("{:.0}", new_area.get())
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                                    new_area.set(v);
                                }
                            }
                        />
                    </FormField>

                    <FormField
                        label="Sprinkler type".to_string()
                        helptext="Drives the default precip rate when measured value is blank.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=new_sprinkler
                            options=vec![
                                ("rotor".into(), "Rotor".into()),
                                ("spray".into(), "Spray".into()),
                                ("mp_rotator".into(), "MP rotator".into()),
                                ("drip".into(), "Drip".into()),
                                ("bubbler".into(), "Bubbler".into()),
                                ("other".into(), "Other".into()),
                            ]
                            aria_label="Sprinkler type".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Measured precip rate (mm/hr)".to_string()
                        helptext="Catch-cup measurement; leave blank for catalog default per sprinkler type. Calibration improves runtime accuracy substantially.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="number"
                            class="ui-input"
                            min="0"
                            step="0.5"
                            placeholder="(blank for catalog default)"
                            prop:value=move || new_precip.get()
                            on:input=move |ev| new_precip.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Controller".to_string()
                        helptext="Which controller fires this zone. Configure controllers under /settings/controllers first.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=new_controller
                            options=controller_options()
                            aria_label="Controller id".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Controller station".to_string()
                        helptext="Station identifier on the chosen controller. For OpenSprinkler: 1-based number (e.g. 1, 2, 3). For HA service call: entity_id (e.g. switch.back_yard_zone). For ESPHome: switch entity_id.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            placeholder="1"
                            prop:value=move || new_station.get()
                            on:input=move |ev| new_station.set(event_target_value(&ev))
                        />
                    </FormField>

                    <FormField
                        label="Photo URL (optional)".to_string()
                        helptext="Renders on the zone card. Any relative path the server can serve (e.g. /site/photos/back_yard.jpg) or an off-site URL.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="text"
                            class="ui-input"
                            placeholder="/site/photos/back_yard.jpg"
                            prop:value=move || new_photo_url.get()
                            on:input=move |ev| new_photo_url.set(event_target_value(&ev))
                        />
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
