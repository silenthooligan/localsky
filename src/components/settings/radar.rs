// SettingsRadar. Operator surface for ui.radar in /data/localsky.toml:
// which imagery providers the Live Radar map offers, and which layers
// start visible for browsers with no stored per-browser preference.
// Round-trips through /api/config like every other settings page; the
// map rebuilds its overlay menu from the new lists on the next load.
//
// Provider selection is Auto-vs-Custom:
//   Auto   = ui.radar.providers stays empty and the catalog's
//            recommended() set for the station lat/lon applies (global
//            composites always, regional sources when in coverage).
//   Custom = ui.radar.providers holds exactly the chosen menu. Any
//            provider is allowed anywhere; that freedom is the point
//            (a station in Europe can keep a US reflectivity layer
//            around to compare renderings).
//
// The catalog itself (descriptors + region logic) lives in
// src/radar_catalog.rs and is shared with the radar map's data
// attributes; this page only reads identity + display fields from it.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::settings_ui::{BadgeTone, SettingsBadge, SettingsResult, StatusHero};
use crate::components::ui::{Button, HelpHint, Panel};

#[component]
pub fn SettingsRadar() -> impl IntoView {
    // false = Auto (providers list empty), true = Custom (explicit menu).
    let custom = RwSignal::new(false);
    // Custom-mode provider menu. Seeded from the config list, or from
    // the recommended set when the config says Auto, so flipping the
    // pill to Custom starts from what Auto resolves to instead of zero.
    let enabled: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    // Default-visible layer ids (providers + feature overlays).
    let defaults: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    // Station location, for showing what Auto resolves to. The
    // fallbacks match the radar map's no-config coordinates.
    let lat = RwSignal::new(40.0f64);
    let lon = RwSignal::new(-75.0f64);
    // Whether an enabled source supplies radar map tiles (Open-Meteo with
    // include_radar). Drives the "no radar source" banner. Defaults true so the
    // banner never flashes before the config loads.
    let has_radar_source = RwSignal::new(true);

    let loaded = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(d) = fetch_radar_ui().await {
                    lat.set(d.lat);
                    lon.set(d.lon);
                    has_radar_source.set(d.has_radar_source);
                    custom.set(!d.providers.is_empty());
                    enabled.set(if d.providers.is_empty() {
                        recommended_ids(d.lat, d.lon)
                    } else {
                        d.providers
                    });
                    defaults.set(d.default_layers);
                    loaded.set(true);
                }
            });
        });
    }

    let flip_mode = move |_| {
        let to_custom = !custom.get();
        custom.set(to_custom);
        // Entering Custom with an empty menu seeds from the Auto set so
        // the user edits the recommendation instead of a blank list.
        if to_custom && enabled.get().is_empty() {
            enabled.set(recommended_ids(lat.get(), lon.get()));
        }
    };

    // An empty Custom menu would round-trip as Auto (empty = Auto in
    // the schema), silently undoing the user's mode choice. Gate Save.
    let can_save = move || !custom.get() || !enabled.get().is_empty();

    let on_save = move |_| {
        if saving.get() || !can_save() || !loaded.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        // Stored lists keep catalog order regardless of click order so
        // the resulting config diff is stable across edits.
        let providers_out: Vec<String> = if custom.get() {
            let chosen = enabled.get();
            provider_rows()
                .into_iter()
                .map(|(id, ..)| id)
                .filter(|id| chosen.contains(id))
                .collect()
        } else {
            Vec::new()
        };
        let chosen_defaults = defaults.get();
        let layers_out: Vec<String> = provider_rows()
            .into_iter()
            .map(|(id, ..)| id)
            .chain(
                feature_rows(lat.get(), lon.get())
                    .into_iter()
                    .map(|(id, _)| id),
            )
            .filter(|id| chosen_defaults.contains(id))
            .collect();
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match patch_radar_ui(providers_out, layers_out).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. The radar menu rebuilds on the next dashboard load.",
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
            let _ = (providers_out, layers_out);
        }
    };

    // Read-only rows for Auto mode: what the region resolves to.
    let auto_rows = move || {
        let rec = recommended_ids(lat.get(), lon.get());
        provider_rows()
            .into_iter()
            .filter(|(id, ..)| rec.contains(id))
            .map(|(_, label, coverage, attribution)| {
                view! {
                    <li class="radar-settings__row">
                        <div class="radar-settings__row-text">
                            <span class="radar-settings__row-label">{label}</span>
                            <span class="radar-settings__row-meta">{attribution}</span>
                        </div>
                        <div class="radar-settings__row-aside">
                            <SettingsBadge label=coverage tone=BadgeTone::Accent/>
                        </div>
                    </li>
                }
            })
            .collect_view()
    };

    // Editable rows for Custom mode: every catalog provider with an
    // On/Off pill, plus a Recommended badge on the Auto picks so the
    // user can see what they are diverging from.
    let custom_rows = move || {
        let rec = recommended_ids(lat.get(), lon.get());
        provider_rows()
            .into_iter()
            .map(|(id, label, coverage, attribution)| {
                let recommended = rec.contains(&id);
                let on_id = id.clone();
                let is_on = Signal::derive(move || enabled.get().contains(&on_id));
                let toggle = move |_| {
                    enabled.update(|v| {
                        if let Some(pos) = v.iter().position(|e| *e == id) {
                            v.remove(pos);
                        } else {
                            v.push(id.clone());
                        }
                    });
                };
                view! {
                    <li class="radar-settings__row">
                        <div class="radar-settings__row-text">
                            <span class="radar-settings__row-label">{label}</span>
                            <span class="radar-settings__row-meta">{attribution}</span>
                        </div>
                        <div class="radar-settings__row-aside">
                            {recommended.then(|| view! {
                                <SettingsBadge label="Recommended".to_string() tone=BadgeTone::Good/>
                            })}
                            <SettingsBadge label=coverage tone=BadgeTone::Muted/>
                            <button
                                type="button"
                                class="toggle-pill"
                                role="switch"
                                aria-checked=move || if is_on.get() { "true" } else { "false" }
                                on:click=toggle
                            >
                                <span class="toggle-pill__opt toggle-pill__opt--on" class:is-active=move || is_on.get()>"On"</span>
                                <span class="toggle-pill__opt toggle-pill__opt--off" class:is-active=move || !is_on.get()>"Off"</span>
                            </button>
                        </div>
                    </li>
                }
            })
            .collect_view()
    };

    // Status hero (Auto vs Custom), matching the other integration pages.
    let hero_chip = move || {
        if custom.get() {
            "Custom".to_string()
        } else {
            "Auto".to_string()
        }
    };
    let hero_meaning = move || {
        if custom.get() {
            let n = enabled.get().len();
            format!(
                "Custom menu: {n} provider{} you picked.",
                if n == 1 { "" } else { "s" }
            )
        } else {
            "Auto: imagery providers are chosen for your region. Switch to Custom below to pick your own.".to_string()
        }
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Radar"<HelpHint topic="radar"/></h1>
                <p class="settings-page__subtitle">
                    "Choose which imagery providers the Live Radar map offers "
                    "and which layers start visible. Stored in /data/localsky.toml "
                    "under ui.radar. Per-browser layer toggles made on the map "
                    "itself still win over the defaults set here."
                </p>
            </header>

            <StatusHero
                icon="sources"
                title="Radar"
                ok=Signal::derive(|| true)
                chip=Signal::derive(hero_chip)
                meaning=Signal::derive(hero_meaning)
            />

            // Radar imagery comes from a radar-capable weather source: an
            // enabled Open-Meteo source with "Provide radar" on, which is the
            // default for a fresh install. This banner shows ONLY when that is
            // missing (the user turned radar off on their Open-Meteo source, or
            // has no Open-Meteo source at all), because then the precipitation
            // overlay has nothing to fetch. The radar-tile layers below
            // (RainViewer, NEXRAD, and the rest) are public services that still
            // render, so the map is not blank, only its precipitation overlay.
            <Show when=move || !has_radar_source.get()>
                <p class="setup-result setup-result--err" role="alert" style="margin-bottom: 1rem">
                    "The radar precipitation overlay needs an Open-Meteo source "
                    "with radar turned on. Open Settings -> Devices, add or edit your "
                    "Open-Meteo source, and switch \"Provide radar\" on. It is on by "
                    "default for new installs, so you only land here if it was turned "
                    "off. The other radar layers below still work without it."
                </p>
            </Show>

            <p class="settings-page__subtitle">
                "The radar imagery comes from a radar-capable weather source: "
                "Open-Meteo with radar turned on, which is the default. The "
                "imagery providers menu below picks which radar overlay layers "
                "the map offers (such as RainViewer and regional reflectivity); "
                "the default layers picker chooses which start visible."
            </p>

            <Panel title="Imagery providers".to_string() help_topic="radar">
                <div class="radar-settings__mode">
                    <span class="radar-settings__mode-label">"Provider menu"</span>
                    <button
                        type="button"
                        class="toggle-pill"
                        role="switch"
                        aria-label="Use a custom provider menu instead of the regional recommendation"
                        aria-checked=move || if custom.get() { "true" } else { "false" }
                        on:click=flip_mode
                    >
                        <span class="toggle-pill__opt" class:is-active=move || !custom.get()>"Auto"</span>
                        <span class="toggle-pill__opt" class:is-active=move || custom.get()>"Custom"</span>
                    </button>
                </div>

                {move || if custom.get() {
                    view! {
                        <p class="radar-settings__hint">
                            "Exactly these providers, anywhere on Earth. The "
                            "coverage label says where a source actually renders "
                            "tiles, but nothing stops you from enabling an "
                            "out-of-region provider to compare."
                        </p>
                        <ul class="radar-settings__rows">{custom_rows()}</ul>
                    }
                    .into_any()
                } else {
                    view! {
                        <p class="radar-settings__hint">
                            {move || format!(
                                "Auto follows the station location ({:.2}, {:.2}): \
                                 global composites always, regional sources when \
                                 the station sits inside their coverage. Today \
                                 that resolves to:",
                                lat.get(),
                                lon.get(),
                            )}
                        </p>
                        <ul class="radar-settings__rows">{auto_rows()}</ul>
                    }
                    .into_any()
                }}

                <Show when=move || custom.get() && enabled.get().is_empty()>
                    <p class="setup-result setup-result--err" role="alert">
                        "Enable at least one provider to save a Custom menu."
                    </p>
                </Show>
            </Panel>

            <Panel title="Default layers".to_string()>
                <p class="radar-settings__hint">
                    "Layers that start visible for browsers with no stored map "
                    "preference. A default for a provider missing from the menu "
                    "above is ignored, so leaving extras lit is harmless."
                </p>
                <div class="radar-settings__group">
                    <h3 class="radar-settings__group-title">"Providers"</h3>
                    <div class="radar-settings__chips">
                        {provider_rows()
                            .into_iter()
                            .map(|(id, label, ..)| layer_chip(defaults, id, label))
                            .collect_view()}
                    </div>
                </div>
                <div class="radar-settings__group">
                    <h3 class="radar-settings__group-title">"Feature overlays"</h3>
                    // Reactive on lat/lon: the tropical chip's label
                    // localizes once the configured station loads.
                    <div class="radar-settings__chips">
                        {move || feature_rows(lat.get(), lon.get())
                            .into_iter()
                            .map(|(id, label)| layer_chip(defaults, id, label))
                            .collect_view()}
                    </div>
                </div>
            </Panel>

            <div class="settings-actions">
                <Button
                    variant="primary"
                    on_click=Callback::new(on_save)
                    disabled=Signal::derive(move || saving.get() || !loaded.get() || !can_save())
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </Button>
            </div>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>

            <Show when=move || !loaded.get()>
                <p class="settings-page__subtitle" style="margin-top: 1rem">
                    "Loading current values from /api/config..."
                </p>
            </Show>
        </div>
    }
}

/// One default-visibility chip. Toggles `id` in and out of the
/// default-layers draft; lit means the layer starts visible.
fn layer_chip(defaults: RwSignal<Vec<String>>, id: String, label: String) -> impl IntoView {
    let on_id = id.clone();
    let is_on = Signal::derive(move || defaults.get().contains(&on_id));
    view! {
        <button
            type="button"
            class="radar-settings__chip"
            class:is-on=move || is_on.get()
            aria-pressed=move || if is_on.get() { "true" } else { "false" }
            on:click=move |_| {
                defaults.update(|v| {
                    if let Some(pos) = v.iter().position(|e| *e == id) {
                        v.remove(pos);
                    } else {
                        v.push(id.clone());
                    }
                });
            }
        >
            {label}
        </button>
    }
}

// ---- Catalog access -----------------------------------------------------
// Thin adapters over crate::radar_catalog so this page touches the catalog
// API in exactly one place. The settings UI only needs identity + display
// fields; kind/url/wms-layer plumbing stays radar.rs + radar.js business.

/// (id, label, coverage label, attribution) per provider, catalog order.
fn provider_rows() -> Vec<(String, String, String, String)> {
    crate::radar_catalog::providers()
        .iter()
        .map(|p| {
            (
                p.id.to_string(),
                p.label.to_string(),
                p.coverage_label.to_string(),
                p.attribution.to_string(),
            )
        })
        .collect()
}

/// (id, label) per feature overlay, catalog order. Takes the station
/// coordinates because the tropical entry's label localizes to the
/// home basin ("Hurricanes (NOAA NHC)" / "Typhoons (JMA / RSMC
/// Tokyo)" / "Cyclones (BOM)"); ids never vary.
fn feature_rows(lat: f64, lon: f64) -> Vec<(String, String)> {
    crate::radar_catalog::features(lat, lon)
        .into_iter()
        .map(|f| (f.id.to_string(), f.label))
        .collect()
}

/// Provider ids the catalog recommends for a station location (the
/// Auto set).
fn recommended_ids(lat: f64, lon: f64) -> Vec<String> {
    crate::radar_catalog::recommended(lat, lon)
        .iter()
        .map(|p| p.id.to_string())
        .collect()
}

// ---- /api/config round-trip ----------------------------------------------

#[cfg(feature = "hydrate")]
#[derive(Clone, Debug)]
struct RadarUiDraft {
    lat: f64,
    lon: f64,
    providers: Vec<String>,
    default_layers: Vec<String>,
    /// True when an enabled source supplies radar map tiles (an Open-Meteo
    /// source with include_radar). False -> the "no radar source" banner shows.
    has_radar_source: bool,
}

#[cfg(feature = "hydrate")]
async fn fetch_radar_ui() -> Result<RadarUiDraft, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let radar = val.get("ui").and_then(|u| u.get("radar"));
    let str_list = |key: &str| -> Vec<String> {
        radar
            .and_then(|r| r.get(key))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    // Canonicalize legacy default-layer ids (precip / nexrad /
    // lightning) and drop ones the catalog does not know, deduping in
    // case a hand-edited config lists both the legacy and the catalog
    // spelling of the same layer.
    let mut default_layers: Vec<String> = Vec::new();
    for raw in str_list("default_layers") {
        if let Some(id) = crate::radar_catalog::canonical_layer_id(&raw) {
            if !default_layers.iter().any(|e| e == id) {
                default_layers.push(id.to_string());
            }
        }
    }
    let loc = val.get("deployment").and_then(|d| d.get("location"));
    // A radar-capable source = an enabled Open-Meteo source with
    // include_radar=true (it powers the precipitation nowcast tiles). Detected
    // straight off the config JSON so this page needs no extra round-trip.
    let has_radar_source = val
        .get("sources")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter().any(|s| {
                s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)
                    && s.get("kind").and_then(|v| v.as_str()) == Some("open_meteo")
                    && s.get("config")
                        .and_then(|c| c.get("include_radar"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    Ok(RadarUiDraft {
        lat: loc
            .and_then(|l| l.get("lat"))
            .and_then(|v| v.as_f64())
            .unwrap_or(40.0),
        lon: loc
            .and_then(|l| l.get("lon"))
            .and_then(|v| v.as_f64())
            .unwrap_or(-75.0),
        providers: str_list("providers"),
        default_layers,
        has_radar_source,
    })
}

#[cfg(feature = "hydrate")]
async fn patch_radar_ui(providers: Vec<String>, default_layers: Vec<String>) -> Result<(), String> {
    use gloo_net::http::Request;
    let cur = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = cur.json().await.map_err(|e| e.to_string())?;
    let root = cfg
        .as_object_mut()
        .ok_or_else(|| "config is not a table".to_string())?;
    // ui (and ui.radar) may be absent in configs written before the
    // radar block existed; create the tables on the way down.
    let ui = root.entry("ui").or_insert_with(|| serde_json::json!({}));
    let ui_obj = ui
        .as_object_mut()
        .ok_or_else(|| "ui is not a table".to_string())?;
    let radar = ui_obj
        .entry("radar")
        .or_insert_with(|| serde_json::json!({}));
    let radar_obj = radar
        .as_object_mut()
        .ok_or_else(|| "ui.radar is not a table".to_string())?;
    radar_obj.insert("providers".into(), serde_json::json!(providers));
    radar_obj.insert("default_layers".into(), serde_json::json!(default_layers));
    crate::components::rules::conditions::save_config(cfg).await
}
