// SettingsZones. Per-zone editor with structured fields (not raw JSON):
// slug + display_name + species + soil_texture + area + sprinkler type
// + measured precip rate + controller mapping. Save round-trips through
// the full Config PUT like the Sources/Controllers pages.
//
// List view uses the SettingsCard UI kit so each zone is an
// expandable card with status badges and a read-only details panel;
// the Edit button still opens the structured form.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
use leptos_router::hooks::{use_location, use_navigate};

use crate::components::settings::{form_state_url, FormState};
use crate::components::settings_ui::{
    BadgeTone, EntityKind, SettingsBadge, SettingsCard, SettingsKv, SettingsLoadError,
    SettingsResult,
};
use crate::components::ui::{Button, FormField, HelpHint, Panel, PhotoField, SegmentedControl};
use crate::components::units_fmt::{
    area_unit, depth_unit, depth_value_mm, fmt_area_sqft, fmt_rain_rate_mm, use_unit_prefs,
    UnitPrefs,
};
use crate::docs::doc_url;

/// Decode the zone form-state from a raw search string. Like the shared
/// [`parse_form_state`](crate::components::settings::parse_form_state) but also
/// honors the legacy `?zone=<slug>` deep link (zone-detail + sensor "edit zone"
/// links point at it) as an alias for `edit`. Priority: edit -> zone -> add ->
/// Closed. The slug is resolved to the real config key by the seeding Effect.
fn parse_zone_form_state(search: &str) -> FormState {
    let param = |key: &str| -> Option<String> {
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_string))
            .filter(|v| !v.is_empty())
    };
    if let Some(slug) = param("edit").or_else(|| param("zone")) {
        FormState::Edit(slug)
    } else if param("add").is_some() {
        FormState::Add
    } else {
        FormState::Closed
    }
}

#[component]
pub fn SettingsZones() -> impl IntoView {
    let config_json = RwSignal::new(serde_json::Value::Null);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    // Per-device display-unit prefs. Read in the (reactive) zones_view closure
    // and handed to each non-reactive ZoneCard as a plain prop, like VerdictCell.
    let prefs = use_unit_prefs();

    // Commit-immediately: every add / edit / delete persists on its own via this
    // shared callback, so nothing is silently lost by navigating away (the old
    // "Add to list -> Save all changes" two-step did exactly that). Passed to the
    // form and the per-zone cards.
    let persist = Callback::new(move |()| {
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
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Engine picks up changes on next tick.",
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
            let _ = candidate;
        }
    });
    // Initial-load state: Some(err) when the config GET failed. The
    // editor body is replaced by a Retry banner in that case.
    let load_error: RwSignal<Option<String>> = RwSignal::new(None);
    let load_retry = RwSignal::new(0u32);

    // Zone form open-state is URL state (?add=1 / ?edit=<slug>, plus the legacy
    // ?zone=<slug> deep link), so the phone back gesture closes the form instead
    // of leaving settings. The URL is the source of truth: the seeding Effect
    // below mirrors it into `add_open` / `editing_slug` (real RwSignals, because
    // ZoneForm is shared verbatim with the setup wizard, which drives them
    // directly with no URL). In settings nothing writes them except that Effect;
    // open/close go through `nav_form` (URL), so there is no feedback loop.
    let loc = use_location();
    // Consumed only by the hydrate-only seeding Effect below (the SSR frame
    // renders forms closed, then hydration opens them from the URL).
    let form_state = Signal::derive(move || parse_zone_form_state(&loc.search.get()));
    #[cfg(not(feature = "hydrate"))]
    let _ = form_state;
    let navigate = use_navigate();
    let nav_form: Callback<FormState> = Callback::new(move |next: FormState| {
        let url = form_state_url(
            &loc.pathname.get_untracked(),
            &loc.search.get_untracked(),
            &next,
        );
        navigate(&url, Default::default());
    });
    // Close callback handed to the shared form so its Cancel / post-save close
    // navigates (URL) instead of poking `add_open` directly. The wizard omits
    // it and keeps the direct-signal behavior.
    let close_form: Callback<()> = Callback::new(move |()| nav_form.run(FormState::Closed));
    let add_open = RwSignal::new(false);
    // The real config key being edited (resolved from the URL slug by the
    // seeding Effect; hyphen/underscore-normalized). The form reads this for
    // edit-mode UI; the URL slug may differ before resolution.
    let editing_slug: RwSignal<Option<String>> = RwSignal::new(None);
    let new_slug = RwSignal::new(String::new());
    let new_display_name = RwSignal::new(String::new());
    // Seeded "warm" and re-seeded from the configured latitude once the
    // config loads (see the climate-default Effect below): |lat| < 35
    // keeps a warm-season default, elsewhere cool-season. A Berlin user
    // should not open the form to a Florida lawn.
    let new_species = RwSignal::new("st_augustine".to_string());
    let new_soil = RwSignal::new("sandy_loam".to_string());
    let new_area = RwSignal::new(1000.0f64);
    let new_sprinkler = RwSignal::new("rotor".to_string());
    let new_precip = RwSignal::new(String::new()); // empty = use catalog default
    let new_controller = RwSignal::new(String::new());
    let new_station = RwSignal::new(String::new());
    let new_photo_url = RwSignal::new(String::new()); // optional zone photo
                                                      // Soil-moisture sensor assignment (the flexible per-zone wiring).
                                                      // "" = none (modeled bucket only). Otherwise an `ha:<entity>` or
                                                      // `source:<id>:<key>` address. Thresholds drive the per-zone skip.
    let new_soil_sensor = RwSignal::new(String::new());
    let new_soil_min = RwSignal::new(30.0f64);
    let new_soil_sat = RwSignal::new(70.0f64);
    // Soil channels from /api/v1/sensors/soil: (id, label, current_pct, source).
    // current_pct + source let the zone show the assigned sensor's live reading
    // and whether it's native or HA-bridged.
    let soil_sensor_opts = RwSignal::new(Vec::<(String, String, Option<f64>, String)>::new());

    // Seed the draft from URL form-state, REACTIVELY (this is the old one-shot
    // ?zone= deep-link Effect, rebuilt to track the URL so back / popstate close
    // and re-open the form correctly). ?edit=<slug> / ?zone=<slug> seeds from
    // that zone's config entry; ?add=1 resets to a blank draft. A per-open guard
    // seeds each open once, but re-attempts an Edit whose entry isn't loaded yet
    // (config arrives after a deep link), without clobbering in-progress edits.
    #[cfg(feature = "hydrate")]
    {
        let seeded_key: RwSignal<Option<FormState>> = RwSignal::new(None);
        Effect::new(move |_| {
            let state = form_state.get();
            let cfg = config_json.get();
            match &state {
                FormState::Closed => {
                    add_open.set(false);
                    if seeded_key.get_untracked().is_some() {
                        seeded_key.set(None);
                        editing_slug.set(None);
                    }
                }
                FormState::Add => {
                    add_open.set(true);
                    if seeded_key.get_untracked().as_ref() != Some(&state) {
                        reset_zone_draft(
                            editing_slug,
                            new_slug,
                            new_display_name,
                            new_area,
                            new_precip,
                            new_station,
                            new_photo_url,
                            new_soil_sensor,
                            new_soil_min,
                            new_soil_sat,
                        );
                        seeded_key.set(Some(state));
                    }
                }
                FormState::Edit(url_slug) => {
                    if seeded_key.get_untracked().as_ref() == Some(&state) {
                        return;
                    }
                    // Snapshot slugs are underscore-normalized ("back_yard")
                    // while config keys may use hyphens ("back-yard"); match on
                    // the normalized form and keep the REAL config key.
                    let zones_obj = cfg.get("zones").and_then(|m| m.as_object());
                    let Some(slug) = zones_obj.and_then(|m| {
                        m.keys()
                            .find(|k| k.replace('-', "_") == url_slug.replace('-', "_"))
                            .cloned()
                    }) else {
                        // Config not loaded (or slug unknown) yet; re-attempt
                        // when it arrives. Don't mark seeded.
                        return;
                    };
                    let Some(z) = zones_obj.and_then(|m| m.get(&slug)).cloned() else {
                        return;
                    };
                    let gs = |k: &str| z.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let gf = |k: &str, d: f64| z.get(k).and_then(|v| v.as_f64()).unwrap_or(d);
                    new_slug.set(slug.clone());
                    new_display_name.set(gs("display_name"));
                    new_species.set(if gs("species").is_empty() {
                        "st_augustine".into()
                    } else {
                        gs("species")
                    });
                    new_soil.set(if gs("soil_texture").is_empty() {
                        "sandy_loam".into()
                    } else {
                        gs("soil_texture")
                    });
                    new_area.set(gf("area_sqft", 1000.0));
                    new_sprinkler.set(if gs("sprinkler_type").is_empty() {
                        "rotor".into()
                    } else {
                        gs("sprinkler_type")
                    });
                    new_precip.set(
                        z.get("precip_rate_mm_hr")
                            .and_then(|v| v.as_f64())
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    );
                    new_controller.set(gs("controller_id"));
                    new_station.set(gs("controller_station"));
                    new_photo_url.set(gs("photo_url"));
                    new_soil_sensor.set(gs("soil_sensor_id"));
                    new_soil_min.set(gf("target_min_pct_soil", 30.0));
                    new_soil_sat.set(gf("saturation_pct_soil", 70.0));
                    editing_slug.set(Some(slug));
                    add_open.set(true);
                    seeded_key.set(Some(state));
                }
            }
        });
    }

    // Climate-aware Add-form default: re-seed the species once from the
    // configured latitude, only while the form is untouched (still on the
    // boot seed and the editor closed), so it never fights user input.
    #[cfg(feature = "hydrate")]
    {
        let seeded = RwSignal::new(false);
        Effect::new(move |_| {
            if seeded.get_untracked() || add_open.get_untracked() {
                return;
            }
            let lat = config_json
                .get()
                .pointer("/deployment/location/lat")
                .and_then(|v| v.as_f64());
            let Some(lat) = lat else { return };
            seeded.set(true);
            if lat.abs() >= 35.0 && new_species.get_untracked() == "st_augustine" {
                new_species.set("tall_fescue".to_string());
            }
        });
    }

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(resp) = gloo_net::http::Request::get("/api/v1/sensors/soil")
                    .send()
                    .await
                {
                    if let Ok(arr) = resp.json::<Vec<serde_json::Value>>().await {
                        let opts = arr
                            .into_iter()
                            .filter_map(|s| {
                                Some((
                                    s.get("id")?.as_str()?.to_string(),
                                    s.get("label")?.as_str()?.to_string(),
                                    s.get("current_pct").and_then(|v| v.as_f64()),
                                    s.get("source")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                ))
                            })
                            .collect();
                        soil_sensor_opts.set(opts);
                    }
                }
            });
        });
        Effect::new(move |_| {
            let _ = load_retry.get();
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_config().await {
                    Ok(cfg) => {
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
                        load_error.set(None);
                    }
                    Err(e) => load_error.set(Some(e)),
                }
            });
        });
        // Scroll the form panel into view whenever it opens, including
        // when the user clicks Edit on a card that's far down the page
        // (and tracks editing_slug so re-clicking Edit on a different
        // card also scrolls).
        Effect::new(move |_| {
            let open = add_open.get();
            let _ = editing_slug.get();
            if !open {
                return;
            }
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                if let Some(elt) = doc.get_element_by_id("zone-form-panel") {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                    opts.set_block(web_sys::ScrollLogicalPosition::Start);
                    elt.scroll_into_view_with_scroll_into_view_options(&opts);
                }
            }
        });
    }

    let zones_view = move || {
        let cfg = config_json.get();
        let zones_obj = cfg
            .get("zones")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let mut keys: Vec<String> = zones_obj.keys().cloned().collect();
        keys.sort();
        let p = prefs.get();
        keys.into_iter()
            .map(|slug| {
                let zone = zones_obj
                    .get(&slug)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                view! {
                    <ZoneCard
                        slug=slug
                        zone=zone
                        config_json=config_json
                        nav_form=nav_form
                        persist=persist
                        prefs=p
                    />
                }
            })
            .collect_view()
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Zones"<HelpHint topic="zones"/></h1>
                <p class="settings-page__subtitle">
                    "One zone = one chunk of yard tied to one valve. Pick grass species + soil texture + measured precip rate; the engine computes ETc, soil bucket, and runtime from there. "
                    "See "
                    <a href=doc_url("grass-species")
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"the species catalog"</a>
                    " and "
                    <a href=doc_url("soil-textures")
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"soil textures"</a>
                    " for reference."
                </p>
            </header>

            <Show
                when=move || load_error.get().is_none()
                fallback=move || view! { <SettingsLoadError error=load_error retry=load_retry/> }
            >
            <Panel title="Configured zones".to_string()>
                <ul class="settings-card-list">
                    {zones_view}
                </ul>

                <div class="settings-add-btn">
                <Button
                    variant="primary"
                    on_click=Callback::new(move |_| {
                        // Toggle: open the (blank) add form, or close what's
                        // open. The seeding Effect blanks the draft on ?add=1, so
                        // the next open is fresh even after an edit + cancel.
                        let next = if add_open.get() {
                            FormState::Closed
                        } else {
                            FormState::Add
                        };
                        nav_form.run(next);
                    })
                >
                    {move || {
                        if add_open.get() {
                            if editing_slug.get().is_some() {
                                "× Cancel edit"
                            } else {
                                "× Cancel add"
                            }
                        } else {
                            "+ Add zone"
                        }
                    }}
                </Button>
                </div>
            </Panel>

            <Show when=move || add_open.get()>
                <ZoneForm
                    config_json=config_json
                    new_slug=new_slug
                    new_display_name=new_display_name
                    new_species=new_species
                    new_soil=new_soil
                    new_area=new_area
                    new_sprinkler=new_sprinkler
                    new_precip=new_precip
                    new_controller=new_controller
                    new_station=new_station
                    new_photo_url=new_photo_url
                    new_soil_sensor=new_soil_sensor
                    new_soil_min=new_soil_min
                    new_soil_sat=new_soil_sat
                    soil_sensor_opts=soil_sensor_opts
                    editing_slug=editing_slug
                    add_open=add_open
                    on_close=close_form
                    result_msg=result_msg
                    result_ok=result_ok
                    persist=persist
                />
            </Show>
            </Show>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>
        </div>
    }
}

/// Add/edit form for a single zone, extracted out of the page component
/// so the page is a thin shell (header + list + save bar) and this whole
/// `<Panel>` view tree compiles inside its own monomorphization boundary
/// instead of nesting into the page. Owns the "add to in-memory config"
/// handler; the page still owns the load (Effect) and the persist (Save
/// all changes -> PUT).
/// Shared by the settings page and the first-run wizard (P2-1). The wizard
/// passes its draft `config` object as `config_json` and a draft-saving
/// `persist`, so the same form creates a zone in onboarding as in settings.
#[component]
pub fn ZoneForm(
    config_json: RwSignal<serde_json::Value>,
    new_slug: RwSignal<String>,
    new_display_name: RwSignal<String>,
    new_species: RwSignal<String>,
    new_soil: RwSignal<String>,
    new_area: RwSignal<f64>,
    new_sprinkler: RwSignal<String>,
    new_precip: RwSignal<String>,
    new_controller: RwSignal<String>,
    new_station: RwSignal<String>,
    new_photo_url: RwSignal<String>,
    new_soil_sensor: RwSignal<String>,
    new_soil_min: RwSignal<f64>,
    new_soil_sat: RwSignal<f64>,
    soil_sensor_opts: RwSignal<Vec<(String, String, Option<f64>, String)>>,
    editing_slug: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
    /// Optional close handler. The settings page passes one that navigates
    /// (URL form-state), so the form's Cancel / post-save close updates the URL
    /// and the back gesture works. The wizard omits it and the form falls back
    /// to setting `add_open` directly (its old behavior).
    #[prop(optional)]
    on_close: Option<Callback<()>>,
    result_msg: RwSignal<String>,
    result_ok: RwSignal<bool>,
    persist: Callback<()>,
) -> impl IntoView {
    // Per-device display-unit prefs for the live "facts" helper text
    // (root depth + estimated precip rate). Read inside the reactive
    // facts closures so a units toggle re-renders them.
    let prefs = use_unit_prefs();
    // Close the form: navigate if the caller gave us a handler (settings),
    // else just flip the local open signal (wizard).
    let close = move || match on_close {
        Some(cb) => cb.run(()),
        None => add_open.set(false),
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
            result_msg.set(
                "Controller is required; configure one under /settings/controllers first".into(),
            );
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
        let precip_source = if precip.is_null() {
            "catalog"
        } else {
            "measured"
        };
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
        // Soil-sensor assignment: "" -> null (modeled bucket), else the
        // chosen ha:/source: address. Thresholds drive the per-zone skip.
        let soil_sensor_json = {
            let s = new_soil_sensor.get();
            if s.trim().is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(s)
            }
        };
        let soil_min = new_soil_min.get();
        let soil_sat = new_soil_sat.get();

        // If editing an existing zone, start from its current JSON so
        // fields not present in this form (root_depth_mm, mad_pct_override,
        // slope_pct, sun_exposure) are preserved. For a new zone, build the
        // full struct with sensible defaults like before.
        let editing_now = editing_slug.get();
        let entry = match editing_now.as_ref() {
            Some(existing_slug) => {
                let mut existing = config_json.with_untracked(|cfg| {
                    cfg.get("zones")
                        .and_then(|z| z.get(existing_slug))
                        .cloned()
                        .unwrap_or(serde_json::json!({}))
                });
                if let Some(obj) = existing.as_object_mut() {
                    obj.insert("display_name".into(), serde_json::json!(display_name));
                    obj.insert("area_sqft".into(), serde_json::json!(area));
                    obj.insert("species".into(), serde_json::json!(new_species.get()));
                    obj.insert("soil_texture".into(), serde_json::json!(new_soil.get()));
                    obj.insert(
                        "sprinkler_type".into(),
                        serde_json::json!(new_sprinkler.get()),
                    );
                    obj.insert("precip_rate_mm_hr".into(), precip);
                    obj.insert(
                        "precip_rate_source".into(),
                        serde_json::json!(precip_source),
                    );
                    obj.insert(
                        "controller_id".into(),
                        serde_json::json!(new_controller.get()),
                    );
                    obj.insert(
                        "controller_station".into(),
                        serde_json::json!(new_station.get()),
                    );
                    obj.insert("photo_url".into(), photo_url_json);
                    obj.insert("soil_sensor_id".into(), soil_sensor_json);
                    obj.insert("target_min_pct_soil".into(), serde_json::json!(soil_min));
                    obj.insert("saturation_pct_soil".into(), serde_json::json!(soil_sat));
                }
                existing
            }
            None => serde_json::json!({
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
                "soil_sensor_id": soil_sensor_json,
                "target_min_pct_soil": soil_min,
                "saturation_pct_soil": soil_sat,
                "photo_url": photo_url_json,
            }),
        };
        config_json.update(|cfg| {
            let zones = cfg.as_object_mut().and_then(|o| {
                o.entry("zones")
                    .or_insert(serde_json::json!({}))
                    .as_object_mut()
            });
            if let Some(zones) = zones {
                zones.insert(slug.clone(), entry);
            }
        });
        let was_edit = editing_now.is_some();
        reset_zone_draft(
            editing_slug,
            new_slug,
            new_display_name,
            new_area,
            new_precip,
            new_station,
            new_photo_url,
            new_soil_sensor,
            new_soil_min,
            new_soil_sat,
        );
        close();
        let _ = was_edit;
        // Commit immediately -- persist this change now instead of staging it
        // for a separate "Save" the user might never click.
        persist.run(());
    };

    // Pull configured controller ids for the picker.
    let controller_options = move || {
        let cfg = config_json.get();
        let arr = cfg
            .get("controllers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        arr.into_iter()
            .filter_map(|c| {
                c.get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| (s.to_string(), s.to_string()))
            })
            .collect::<Vec<_>>()
    };

    let on_cancel = move |_| {
        reset_zone_draft(
            editing_slug,
            new_slug,
            new_display_name,
            new_area,
            new_precip,
            new_station,
            new_photo_url,
            new_soil_sensor,
            new_soil_min,
            new_soil_sat,
        );
        close();
    };

    // P2-4: presets show their work. The chosen species' FAO-56 params and the
    // sprinkler-derived precip estimate render inline, so the three expert knobs
    // (species / sprinkler / precip) become one confident click with the numbers
    // visible. Reads the shared, slug-keyed agronomy catalog (the same source the
    // engine uses), so it is a pure client-side lookup with no round-trip.
    let species_facts = move || {
        let up = prefs.get();
        let p = crate::agronomy::species_profile_by_slug(&new_species.get());
        let (kc_min, kc_max) = crate::agronomy::kc_range(&p);
        // root_depth_mm is a stored depth in mm; render in the viewer's
        // depth unit at the display boundary.
        format!(
            "Kc {kc_min:.2}-{kc_max:.2} · root {}{} · waters at {:.0}% soil depletion",
            depth_value_mm(p.root_depth_mm, up),
            depth_unit(up),
            p.mad_pct * 100.0
        )
    };
    let precip_estimate = move || {
        if !new_precip.get().trim().is_empty() {
            return None;
        }
        let up = prefs.get();
        // Catalog estimate is mm/hr; show it in the viewer's rate unit.
        let rate = crate::agronomy::sprinkler_precip_mm_hr(&new_sprinkler.get());
        Some(format!(
            "Using the catalog default ~{} for this sprinkler. Enter a catch-cup measurement above to override.",
            fmt_rain_rate_mm(rate, up)
        ))
    };

    view! {
        <div id="zone-form-panel"><Panel title="Zone form".to_string()>
            <Show when=move || editing_slug.get().is_some()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.75rem">
                    "Editing "
                    <code>{move || editing_slug.get().unwrap_or_default()}</code>
                    ". Save below applies to this slug; the slug field is read-only."
                </p>
            </Show>
            // Name leads and auto-derives the internal slug, so a beginner
            // never has to know what snake_case is. (When editing, the slug is
            // fixed; only the display name changes.)
            <FormField
                label="Name".to_string()
                helptext="What you call this zone, e.g. \"Back Yard\". Used everywhere in the app.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="Back Yard"
                    prop:value=move || new_display_name.get()
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        new_display_name.set(v.clone());
                        if editing_slug.get().is_none() {
                            new_slug.set(slugify(&v));
                        }
                    }
                />
            </FormField>

            <FormField
                label="Grass species".to_string()
                helptext="Picks the Kc seasonal curve, root depth, and MAD threshold.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <SegmentedControl
                    value=new_species
                    // Warm-season, then cool-season, then non-turf;
                    // alphabetical within each group so no single
                    // region's turf leads the control.
                    options=vec![
                        ("bahia".into(), "Bahia".into()),
                        ("bermuda".into(), "Bermuda".into()),
                        ("centipede".into(), "Centipede".into()),
                        ("kikuyu".into(), "Kikuyu".into()),
                        ("st_augustine".into(), "St. Augustine".into()),
                        ("zoysia".into(), "Zoysia".into()),
                        ("kentucky_bluegrass".into(), "KBG".into()),
                        ("perennial_ryegrass".into(), "PRG".into()),
                        ("tall_fescue".into(), "Tall Fescue".into()),
                        ("ornamental_shrubs".into(), "Shrubs".into()),
                        ("vegetable_garden".into(), "Vegetables".into()),
                        ("drip_xeriscape".into(), "Drip / xeri".into()),
                        ("other".into(), "Other".into()),
                    ]
                    aria_label="Grass species".to_string()
                />
            </FormField>
            <p class="zone-form__facts">{move || species_facts()}</p>

            <FormField
                label="Soil texture".to_string()
                helptext="USDA texture class (used internationally). Drives field capacity, wilting point, and infiltration rate.".to_string()
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
                // Editable input bound to the stored sq ft value (round-trips
                // into engine math as `area_sqft`), so the field stays imperial:
                // the value is NOT display-converted, hence the label is the
                // imperial unit sourced from the helper (always "sq ft"), not a
                // pref-reactive label that would desync from the stored value.
                label=format!("Area ({})", area_unit(UnitPrefs::default()))
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
                helptext="Station identifier on the chosen controller. For OpenSprinkler: 1-based number (e.g. 1, 2, 3). For DIY (HTTP): the board's zone id (e.g. 1 or back_yard). For HA service call: entity_id (e.g. switch.back_yard_zone).".to_string()
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

            // Everything below is fine-tuning with a sensible default; a
            // beginner can add a working zone with just the fields above.
            <details class="zone-form-advanced">
                <summary class="zone-form-advanced__summary">"Advanced options"</summary>

                <FormField
                    label="Internal id (slug)".to_string()
                    helptext="Auto-generated from the name; the stable key history + sensor bindings use. To change it, rename the zone.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="text"
                        class="ui-input field-readonly"
                        prop:value=move || new_slug.get()
                        prop:disabled=true
                        readonly=true
                    />
                </FormField>

                <FormField
                    label="Sprinkler type".to_string()
                    helptext="Drives the default precip rate when the measured value is blank.".to_string()
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
                {move || {
                    precip_estimate().map(|f| view! { <p class="zone-form__facts">{f}</p> })
                }}

            <FormField
                label="Photo (optional)".to_string()
                helptext="Drop or browse for an image to upload; it lands under /site/photos. You can also paste an off-site URL.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <PhotoField value=new_photo_url/>
            </FormField>

            <FormField
                label="Soil moisture sensor (optional)".to_string()
                helptext="Assign a sensor to drive this zone's skip decision. The dropdown lists every discovered soil channel, both Home Assistant entities and LocalSky native sources (incl. a zone-bound MQTT probe's channel). Or type an id below. Blank = modeled bucket only.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <select
                    class="ui-input"
                    on:change=move |ev| new_soil_sensor.set(event_target_value(&ev))
                >
                    <option value="" selected=move || new_soil_sensor.get().is_empty()>
                        "(none, modeled bucket)"
                    </option>
                    {move || soil_sensor_opts.get().into_iter().map(|(id, label, _, _)| {
                        let cur = new_soil_sensor.get();
                        let sel = cur.strip_prefix("ha:").unwrap_or(&cur) == id.strip_prefix("ha:").unwrap_or(&id);
                        view! { <option value=id.clone() selected=sel>{label}</option> }
                    }).collect_view()}
                </select>
                // Live reading + origin of the assigned sensor, the "full
                // data picture" right in the zone, with a jump to manage it.
                {move || {
                    let sel = new_soil_sensor.get();
                    if sel.is_empty() { return {
                        let _: () = view! {};
                        ().into_any()
                    }; }
                    // Zones store the bare entity (sensor.x) while the soil feed
                    // ids HA channels as ha:sensor.x, match on the bare id.
                    let bare = |s: &str| s.strip_prefix("ha:").unwrap_or(s).to_string();
                    let sel_bare = bare(&sel);
                    let opt = soil_sensor_opts.get().into_iter().find(|(id, ..)| bare(id) == sel_bare);
                    let (reading, origin) = match opt {
                        Some((_, _, pct, source)) => {
                            let r = pct.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "-".into());
                            let o = if source == "home_assistant" { "Home Assistant" } else if source.is_empty() { "manual / HA entity" } else { "LocalSky native" };
                            (r, o.to_string())
                        }
                        // Selected an id (e.g. a typed ha:entity) not in the list.
                        None => ("live".to_string(), "manual / HA entity".to_string()),
                    };
                    view! {
                        <div class="zone-soil-live">
                            <span class="zone-soil-live__pct">{reading}</span>
                            <span class="zone-soil-live__origin">{origin}</span>
                            <a class="zone-soil-live__manage" href="/settings?section=devices">"Manage in Devices →"</a>
                        </div>
                    }.into_any()
                }}
                // One picker: the select above already lists BOTH Home
                // Assistant soil entities (ha:*) and LocalSky native channels
                // (source:*) from /sensors/soil, so there is no separate HA
                // picker. This input is the escape hatch for an id not yet
                // discovered (e.g. an HA entity HA hasn't reported on yet).
                <input
                    type="text"
                    class="ui-input"
                    style="margin-top: 0.4rem"
                    placeholder="or type any id (e.g. ha:sensor.back_yard_soil_moisture)"
                    prop:value=move || new_soil_sensor.get()
                    on:input=move |ev| new_soil_sensor.set(event_target_value(&ev))
                />
                <a
                    class="setup-footer__btn setup-footer__btn--ghost"
                    href="/settings?section=devices&add=source"
                    target="_blank"
                    rel="noopener"
                    style="margin-top: 0.4rem; display: inline-flex"
                >
                    "+ Add a sensor"
                </a>
            </FormField>

            <FormField
                label="Healthy band low %".to_string()
                helptext="Below this, the zone reads 'dry' on the Sensors page.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    class="ui-input"
                    min="0"
                    max="100"
                    step="1"
                    prop:value=move || new_soil_min.get().to_string()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            new_soil_min.set(v);
                        }
                    }
                />
            </FormField>

            <FormField
                label="Saturation % (skip at/above)".to_string()
                helptext="When this zone's sensor reads at or above this, the zone skips watering.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    class="ui-input"
                    min="0"
                    max="100"
                    step="1"
                    prop:value=move || new_soil_sat.get().to_string()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            new_soil_sat.set(v);
                        }
                    }
                />
            </FormField>
            </details>

            <div class="settings-form-actions">
                <Button
                    variant="ghost"
                    on_click=Callback::new(on_cancel)
                >
                    "Cancel"
                </Button>
                <Button
                    variant="primary"
                    on_click=Callback::new(on_add)
                >
                    {move || if editing_slug.get().is_some() {
                        "Save zone changes"
                    } else {
                        "Add zone"
                    }}
                </Button>
            </div>
        </Panel></div>
    }
}

/// Turn a human zone name into a stable snake_case slug ("Back Yard" ->
/// "back_yard") so a beginner never has to type an identifier by hand.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

/// Reset the zone draft signals back to a blank "new zone" state.
/// Shared by the page's Cancel toggle and the form's post-add cleanup
/// so the two stay in sync. Mirrors the original inline reset: clears
/// edit-mode plus the free-text fields, and restores the default area;
/// the species/soil/sprinkler/controller pickers retain their prior
/// selection exactly as before.
#[allow(clippy::too_many_arguments)]
fn reset_zone_draft(
    editing_slug: RwSignal<Option<String>>,
    new_slug: RwSignal<String>,
    new_display_name: RwSignal<String>,
    new_area: RwSignal<f64>,
    new_precip: RwSignal<String>,
    new_station: RwSignal<String>,
    new_photo_url: RwSignal<String>,
    new_soil_sensor: RwSignal<String>,
    new_soil_min: RwSignal<f64>,
    new_soil_sat: RwSignal<f64>,
) {
    editing_slug.set(None);
    new_slug.set(String::new());
    new_display_name.set(String::new());
    new_area.set(1000.0);
    new_precip.set(String::new());
    new_station.set(String::new());
    new_photo_url.set(String::new());
    new_soil_sensor.set(String::new());
    new_soil_min.set(30.0);
    new_soil_sat.set(70.0);
}

#[cfg(feature = "hydrate")]
async fn fetch_config() -> Result<serde_json::Value, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    // A JSON error body must not be mistaken for the config.
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())
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

/// Lookup table that maps a species slug to its display label.
/// Mirrors the SegmentedControl options inside the edit form so the
/// read-only card view shows "St. Augustine" instead of "st_augustine".
fn pretty_species(slug: &str) -> &'static str {
    match slug {
        "st_augustine" => "St. Augustine",
        "bermuda" => "Bermuda",
        "zoysia" => "Zoysia",
        "bahia" => "Bahia",
        "centipede" => "Centipede",
        "kentucky_bluegrass" => "Kentucky Bluegrass",
        "tall_fescue" => "Tall Fescue",
        "perennial_ryegrass" => "Perennial Ryegrass",
        "kikuyu" => "Kikuyu",
        "ornamental_shrubs" => "Ornamental shrubs",
        "vegetable_garden" => "Vegetable garden",
        "drip_xeriscape" => "Drip / xeriscape",
        "other" => "Other",
        "" => "(unset)",
        _ => "Unknown",
    }
}

/// Lookup table for soil texture slugs.
fn pretty_soil(slug: &str) -> &'static str {
    match slug {
        "sand" => "Sand",
        "loamy_sand" => "Loamy sand",
        "sandy_loam" => "Sandy loam",
        "loam" => "Loam",
        "silt_loam" => "Silt loam",
        "clay_loam" => "Clay loam",
        "clay" => "Clay",
        "" => "(unset)",
        _ => "Unknown",
    }
}

/// Single zone row. Extracted into its own component so the
/// monomorphized type of the badges + 7 KV rows + edit/delete
/// closures stays inside one boundary instead of compounding
/// through the page's outer view.
#[component]
fn ZoneCard(
    slug: String,
    zone: serde_json::Value,
    config_json: RwSignal<serde_json::Value>,
    nav_form: Callback<FormState>,
    persist: Callback<()>,
    /// Display-unit prefs, passed by value (this card is built from a static
    /// serde_json zone, not reactive); mirrors VerdictCell's `prefs` prop.
    prefs: UnitPrefs,
) -> impl IntoView {
    let display = zone
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(&slug)
        .to_string();
    let species_slug = zone
        .get("species")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let soil_slug = zone
        .get("soil_texture")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let area = zone
        .get("area_sqft")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let ctrl_id = zone
        .get("controller_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let station = zone
        .get("controller_station")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sprinkler = zone
        .get("sprinkler_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let precip = zone.get("precip_rate_mm_hr").and_then(|v| v.as_f64());
    let subtitle = format!(
        "{slug} \u{00b7} {} \u{00b7} {} \u{00b7} {}",
        pretty_species(&species_slug),
        pretty_soil(&soil_slug),
        fmt_area_sqft(area, prefs)
    );
    let ctrl_display = if station.is_empty() {
        ctrl_id.clone()
    } else {
        format!("{ctrl_id} \u{00b7} station {station}")
    };
    let precip_display = match precip {
        // Stored mm/hr; render in the viewer's rate unit at the display boundary.
        Some(v) => format!("{} (measured)", fmt_rain_rate_mm(v, prefs)),
        None => "(catalog default)".to_string(),
    };
    let sprinkler_display = if sprinkler.is_empty() {
        "(unset)".to_string()
    } else {
        sprinkler.clone()
    };
    let species_display = pretty_species(&species_slug).to_string();
    let soil_display = pretty_soil(&soil_slug).to_string();
    let area_display = fmt_area_sqft(area, prefs);
    let soil_sensor_display = match zone.get("soil_sensor_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => {
            let sat = zone
                .get("saturation_pct_soil")
                .and_then(|v| v.as_f64())
                .unwrap_or(70.0);
            format!("{s} (skip ≥ {sat:.0}%)")
        }
        _ => "(none, modeled bucket)".to_string(),
    };
    let ctrl_id_for_badges = ctrl_id.clone();
    let slug_kv = slug.clone();
    let slug_for_edit = slug.clone();
    let slug_for_delete = slug.clone();
    let slug_for_delete_label = slug.clone();
    let slug_for_edit_label = slug.clone();
    let slug_for_test = slug.clone();

    // Test-run: fire this zone's valve for 30s so the user can confirm water
    // actually comes out before trusting the overnight engine. Reuses the
    // dashboard's action endpoint (POST /api/irrigation/action).
    let testing = RwSignal::new(false);
    let test_msg = RwSignal::new(String::new());
    let on_test = move |_| {
        if testing.get() {
            return;
        }
        testing.set(true);
        test_msg.set("Starting…".to_string());
        // The action endpoint keys zones by the underscore-normalized slug
        // (what the engine/snapshot uses), while config keys are hyphenated --
        // normalize so "front-yard" dispatches as "front_yard".
        let s = slug_for_test.replace('-', "_");
        let done = Callback::new(move |res: Result<(), String>| {
            testing.set(false);
            match res {
                Ok(()) => test_msg.set("Running 30s -- check the valve.".to_string()),
                Err(e) => test_msg.set(format!("Couldn't start: {e}")),
            }
        });
        crate::components::irrigation::controls::post_action_then(
            serde_json::json!({ "kind": "run", "zone": s, "seconds": 30 }),
            done,
        );
    };

    // Open the editor via URL state; the page's seeding Effect resolves this
    // slug to its config entry and populates the draft (so back / deep-links
    // seed it too). The config key is used directly as the edit slug.
    let on_edit = move |_| {
        nav_form.run(FormState::Edit(slug_for_edit.clone()));
    };
    let on_delete = move |_| {
        let s = slug_for_delete.clone();
        config_json.update(|cfg| {
            if let Some(zones) = cfg.get_mut("zones").and_then(|v| v.as_object_mut()) {
                zones.remove(&s);
            }
        });
        // Commit immediately so the deletion can't be silently lost.
        persist.run(());
    };

    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon="zones".into()
                title=display
                subtitle=subtitle
                entity=Some(EntityKind::Zone)
                badges=Box::new(move || view! {
                    {ctrl_id_for_badges.is_empty().then(|| view! {
                        <SettingsBadge label="No controller".into() tone=BadgeTone::Warm/>
                    })}
                    {match precip {
                        Some(_) => view! { <SettingsBadge label="Measured PR".into() tone=BadgeTone::Good/> }.into_any(),
                        None => view! { <SettingsBadge label="Catalog PR".into() tone=BadgeTone::Muted/> }.into_any(),
                    }}
                }.into_any())
                details=Box::new(move || view! {
                    <SettingsKv label="Slug" value=slug_kv/>
                    <SettingsKv label="Species" value=species_display/>
                    <SettingsKv label="Soil texture" value=soil_display/>
                    <SettingsKv label="Area" value=area_display/>
                    <SettingsKv label="Sprinkler" value=sprinkler_display/>
                    <SettingsKv label="Precip rate" value=precip_display/>
                    <SettingsKv label="Controller" value=ctrl_display/>
                    <SettingsKv label="Soil sensor" value=soil_sensor_display/>
                }.into_any())
                actions=Box::new(move || view! {
                    <Button
                        variant="primary"
                        aria_label="Run this zone for 30 seconds to confirm water comes out".to_string()
                        disabled=Signal::derive(move || testing.get())
                        on_click=Callback::new(on_test)
                    >
                        {move || if testing.get() { "Starting…" } else { "Test run" }}
                    </Button>
                    <Button
                        variant="ghost"
                        aria_label=format!("Edit zone {slug_for_edit_label}")
                        on_click=Callback::new(on_edit)
                    >
                        "Edit"
                    </Button>
                    <Button
                        variant="danger"
                        aria_label=format!("Delete zone {slug_for_delete_label}")
                        on_click=Callback::new(on_delete)
                    >
                        "Delete"
                    </Button>
                    {move || {
                        let m = test_msg.get();
                        (!m.is_empty()).then(|| view! { <span class="zone-test-msg">{m}</span> })
                    }}
                }.into_any())
            />
        </li>
    }
}
