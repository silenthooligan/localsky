// SettingsZones. Per-zone editor with structured fields (not raw JSON):
// slug + display_name + species + soil_texture + area + sprinkler type
// + measured precip rate + max run time + weekly target + sessions per week
// + controller mapping. Save round-trips through the full Config PUT like
// the Sources/Controllers pages.
//
// List view uses the SettingsCard UI kit so each zone is an
// expandable card with status badges and a read-only details panel;
// the Edit button still opens the structured form.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
use leptos_router::hooks::{use_location, use_navigate};

use crate::components::controllers_form::can_scan_zones;
use crate::components::settings::{form_state_url, FormState};
use crate::components::settings_ui::{
    BadgeTone, EntityKind, SettingsBadge, SettingsCard, SettingsKv, SettingsLoadError,
    SettingsResult,
};
use crate::components::ui::{
    Button, ConfirmSheet, FormField, HelpHint, Panel, PhotoField, SegmentedControl, Sheet,
};
use crate::components::units_fmt::{
    area_unit, depth_unit, depth_value_mm, fmt_area_sqft, fmt_rain_amount, fmt_rain_rate_mm,
    use_unit_prefs, UnitPrefs,
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
    // Persistent, dismissible restart-required banner (the controllers-page
    // pattern). Zone add/remove and station remaps are boot-wired, so their
    // saves return restart_reasons; scalar edits (including the run limit)
    // hot-reload and leave it empty.
    let restart_reasons: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    let restart_dismissed = RwSignal::new(false);
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
                    Ok(reasons) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Engine picks up changes on next tick.",
                        );
                        // A zone-set or station-binding change is boot-wired:
                        // surface the server's restart reasons; an empty list
                        // (hot-reloaded change) clears the banner.
                        restart_dismissed.set(false);
                        restart_reasons.set(reasons);
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
    let new_max_run = RwSignal::new(String::new()); // empty = 60 minute default
                                                    // Weekly target (inches) and sessions per week, the two numbers that size
                                                    // every run. Empty = the default inferred from the slug, which the form
                                                    // shows as the placeholder so the owner can see what the zone waters on.
    let new_weekly_budget = RwSignal::new(String::new());
    let new_sessions = RwSignal::new(String::new());
    // Per-day rain-credit cap (inches). Empty = derived from this zone's
    // soil texture and root depth; the form shows the derived value as
    // the placeholder so the owner can see what the zone clips at.
    let new_rain_cap = RwSignal::new(String::new());
    // Per-zone scheduling-model pin: "" = the engine default, else
    // "weekly" | "soil". Same blank-follows-the-default pattern as the
    // optional overrides above.
    let new_sched_model = RwSignal::new(String::new());
    let new_controller = RwSignal::new(String::new());
    let new_station = RwSignal::new(String::new());
    let new_photo_url = RwSignal::new(String::new()); // optional zone photo
                                                      // Soil-moisture sensor assignment (the flexible per-zone wiring).
                                                      // "" = none (no soil gate). Otherwise an `ha:<entity>` or
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
                            new_max_run,
                            new_weekly_budget,
                            new_sessions,
                            new_rain_cap,
                            new_sched_model,
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
                    new_max_run.set(
                        z.get("max_run_minutes")
                            .and_then(|v| v.as_u64())
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    );
                    new_weekly_budget.set(
                        z.get("weekly_budget_in")
                            .and_then(|v| v.as_f64())
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    );
                    new_sessions.set(
                        z.get("sessions_per_week")
                            .and_then(|v| v.as_u64())
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    );
                    new_rain_cap.set(
                        z.get("rain_credit_cap_in")
                            .and_then(|v| v.as_f64())
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    );
                    new_sched_model.set(gs("scheduling_model"));
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
            let current = new_species.get_untracked();
            let picked = crate::agronomy::climate_default_species(&current, lat);
            if picked != current {
                new_species.set(picked.to_string());
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

    // The ADD form is an overlay, not a panel at the end of the list.
    // Appending it meant clicking "+ Add zone" scrolled nothing and opened
    // the form below the fold: on a four-zone yard it landed 69px past the
    // viewport, and on a fourteen-zone yard it is nowhere near the screen.
    // An overlay is unmissable and costs nothing at any zone count.
    //
    // Two effects keep it in step with the URL state that owns form
    // open-ness. In: the sheet opens when the form is open and no zone is
    // being edited. Out: dismissing the sheet (its X, the backdrop, Escape)
    // routes back through `close_form`, so the URL and the draft reset the
    // same way Cancel does.
    let add_sheet_open = RwSignal::new(false);
    Effect::new(move |_| {
        add_sheet_open.set(add_open.get() && editing_slug.get().is_none());
    });
    Effect::new(move |_| {
        if !add_sheet_open.get()
            && add_open.get_untracked()
            && editing_slug.get_untracked().is_none()
        {
            close_form.run(());
        }
    });

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
                // The editor for THIS zone renders directly beneath its own
                // row, so clicking Edit opens something the operator can see
                // without scrolling past the rest of the yard.
                let editing_this = {
                    let slug = slug.clone();
                    move || editing_slug.get().as_deref() == Some(slug.as_str())
                };
                let zone_label = zone
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.is_empty())
                    .unwrap_or(slug.as_str())
                    .to_string();
                view! {
                    <ZoneCard
                        slug=slug
                        zone=zone
                        config_json=config_json
                        nav_form=nav_form
                        persist=persist
                        prefs=p
                    />
                    <Show when=editing_this.clone()>
                        <li class="settings-card-list__item zone-edit-inline">
                            <ZoneForm
                                panel_title=format!("Editing {zone_label}")
                                config_json=config_json
                                new_slug=new_slug
                                new_display_name=new_display_name
                                new_species=new_species
                                new_soil=new_soil
                                new_area=new_area
                                new_sprinkler=new_sprinkler
                                new_precip=new_precip
                                new_max_run=new_max_run
                                new_weekly_budget=new_weekly_budget
                                new_sessions=new_sessions
                                new_rain_cap=new_rain_cap
                                new_sched_model=new_sched_model
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
                        </li>
                    </Show>
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
                    "One zone = one chunk of yard tied to one valve. The weekly water balance decides when it waters and how much; the measured precip rate turns that depth into minutes, and the soil texture sets cycle-and-soak. Species and soil also feed the ET math shown on the zone page. "
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

            <crate::components::settings::RestartBanner reasons=restart_reasons dismissed=restart_dismissed/>

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
                        if add_open.get() && editing_slug.get().is_none() {
                            "× Cancel add"
                        } else {
                            "+ Add zone"
                        }
                    }}
                </Button>
                </div>
            </Panel>

            // ADD only. An EDIT renders inline, directly under the zone it
            // belongs to (see `zones_view`): a form that opened below every
            // zone left the operator scrolling past the whole yard to find
            // the editor they just asked for, and on a fourteen-zone yard it
            // was off screen entirely.
            // Mounted only when nothing is being edited. The sheet keeps its
            // children in the DOM while it animates closed, so leaving it
            // mounted during an edit would put two #zone-form-panel ids on
            // the page and the scroll-into-view would find the hidden one.
            <Show when=move || editing_slug.get().is_none()>
            <Sheet open=add_sheet_open title="Add a zone" aria_label="Add a zone">
                <ZoneForm
                    panel_title=String::new()
                    config_json=config_json
                    new_slug=new_slug
                    new_display_name=new_display_name
                    new_species=new_species
                    new_soil=new_soil
                    new_area=new_area
                    new_sprinkler=new_sprinkler
                    new_precip=new_precip
                    new_max_run=new_max_run
                    new_weekly_budget=new_weekly_budget
                    new_sessions=new_sessions
                    new_rain_cap=new_rain_cap
                    new_sched_model=new_sched_model
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
            </Sheet>
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
    new_max_run: RwSignal<String>,
    new_weekly_budget: RwSignal<String>,
    new_sessions: RwSignal<String>,
    new_rain_cap: RwSignal<String>,
    new_sched_model: RwSignal<String>,
    new_controller: RwSignal<String>,
    new_station: RwSignal<String>,
    new_photo_url: RwSignal<String>,
    new_soil_sensor: RwSignal<String>,
    new_soil_min: RwSignal<f64>,
    new_soil_sat: RwSignal<f64>,
    soil_sensor_opts: RwSignal<Vec<(String, String, Option<f64>, String)>>,
    /// Heading for the panel. An inline edit names the zone it belongs to,
    /// so an open editor sitting between two zone rows is unmistakably
    /// attached to one of them.
    panel_title: String,
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
    // The station picker's zone list for the CURRENTLY SELECTED controller.
    // Local to the form on purpose: it is ephemeral UI state, not part of
    // the zone draft the page seeds, and scoping it to one open of the form
    // is exactly the "cache for the editing session" the vendor request
    // budget wants.
    let station_scan: RwSignal<StationScan> = RwSignal::new(StationScan::default());
    // The selected controller's kind, tracked REACTIVELY. controller_options()
    // is called non-reactively at mount, so switching controllers has to be
    // observed here or the picker would keep showing the previous
    // controller's zones.
    let selected_kind = Signal::derive(move || {
        let id = new_controller.get();
        config_json.with(|cfg| {
            cfg.get("controllers")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                })
                .and_then(|c| c.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
    });
    // True when a real zone list is in hand for the selected controller.
    // Gates the select, and (via station_to_persist) whether a blank draft
    // is allowed to clear an existing binding.
    let enumerated =
        Signal::derive(move || station_scan.with(|s| s.enumerated(&new_controller.get())));

    // Run-limit confirmation state: on_add stashes the fully built entry
    // here and opens the sheet when the save raises the limit past 60;
    // Confirm commits the pending entry, Cancel just closes.
    let confirm_open = RwSignal::new(false);
    let pending_cap_min: RwSignal<u32> = RwSignal::new(60);
    let pending_zone_name: RwSignal<String> = RwSignal::new(String::new());
    let pending_commit: RwSignal<Option<(String, serde_json::Value)>> = RwSignal::new(None);
    // Insert the finished entry, reset the draft, close, persist. Shared
    // by the direct path and the sheet's Confirm.
    let commit_zone = move |slug: String, entry: serde_json::Value| {
        config_json.update(|cfg| {
            let zones = cfg.as_object_mut().and_then(|o| {
                o.entry("zones")
                    .or_insert(serde_json::json!({}))
                    .as_object_mut()
            });
            if let Some(zones) = zones {
                zones.insert(slug, entry);
            }
        });
        reset_zone_draft(
            editing_slug,
            new_slug,
            new_display_name,
            new_area,
            new_precip,
            new_max_run,
            new_weekly_budget,
            new_sessions,
            new_rain_cap,
            new_sched_model,
            new_station,
            new_photo_url,
            new_soil_sensor,
            new_soil_min,
            new_soil_sat,
        );
        close();
        // Commit immediately -- persist this change now instead of staging it
        // for a separate "Save" the user might never click.
        persist.run(());
    };
    let on_add = move |_| {
        let slug = new_slug.get().trim().to_lowercase().replace(' ', "_");
        if slug.is_empty() {
            result_ok.set(false);
            result_msg.set("Zone slug is required".into());
            return;
        }
        // An add is a CREATE, and `commit_zone` inserts by key, which
        // REPLACES. Without this a name that slugifies onto an existing zone
        // silently overwrites that zone's binding, its vendor label, and
        // every agronomic field this form does not carry, and the zone-key
        // rename guard on the server never fires because the key SET did not
        // change. The station-preserving rule in `station_to_persist` cannot
        // help either: it keys off the zone being edited, and this path is
        // not editing anything.
        if editing_slug.get().is_none() {
            let existing = config_json.with_untracked(|cfg| zone_key_taken(cfg, &slug));
            if let Some(taken) = existing {
                result_ok.set(false);
                result_msg.set(format!(
                    "A zone already uses the id \"{taken}\". Edit that zone instead of adding \
                     a second one with the same name; adding would replace it."
                ));
                return;
            }
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
        let max_run_value = new_max_run.get();
        let max_run: Option<u32> = if max_run_value.trim().is_empty() {
            None
        } else {
            match max_run_value.trim().parse::<u32>() {
                Ok(v) if (5..=360).contains(&v) => Some(v),
                _ => {
                    result_ok.set(false);
                    result_msg.set(
                        "Max run time must be whole minutes between 5 and 360 (or blank for \
                         the default 60)"
                            .into(),
                    );
                    return;
                }
            }
        };
        let max_run_json = max_run
            .map(|v| serde_json::json!(v))
            .unwrap_or(serde_json::Value::Null);
        // Weekly target and sessions per week. Blank means the default
        // inferred from the slug, which is what the zone watered on before
        // this form carried the fields. The server holds sessions_per_week
        // to 1..=7 (zone_sessions_per_week_range), so the same bound is
        // enforced here rather than discovered as a 422 at save.
        let weekly_budget_value = new_weekly_budget.get();
        let weekly_budget_json = if weekly_budget_value.trim().is_empty() {
            serde_json::Value::Null
        } else {
            match weekly_budget_value.trim().parse::<f64>() {
                Ok(v) if v > 0.0 && v <= 10.0 => serde_json::json!(v),
                _ => {
                    result_ok.set(false);
                    result_msg.set(
                        "Weekly target must be a number above 0 and at most 10 inches (or \
                         blank for the default)"
                            .into(),
                    );
                    return;
                }
            }
        };
        let sessions_value = new_sessions.get();
        let sessions_json = if sessions_value.trim().is_empty() {
            serde_json::Value::Null
        } else {
            match sessions_value.trim().parse::<u32>() {
                Ok(v) if (1..=7).contains(&v) => serde_json::json!(v),
                _ => {
                    result_ok.set(false);
                    result_msg.set(
                        "Sessions per week must be a whole number from 1 to 7 (or blank for \
                         the default)"
                            .into(),
                    );
                    return;
                }
            }
        };
        // Per-day rain-credit cap. Blank round-trips to null (the cap
        // derived from soil texture and root depth), like the weekly
        // target above. The server holds the value to 0.05..=5.0
        // (zone_rain_credit_cap_range), so the same bound is enforced
        // here rather than discovered as a 422 at save.
        let rain_cap_value = new_rain_cap.get();
        let rain_cap_json = if rain_cap_value.trim().is_empty() {
            serde_json::Value::Null
        } else {
            match rain_cap_value.trim().parse::<f64>() {
                Ok(v) if (0.05..=5.0).contains(&v) => serde_json::json!(v),
                _ => {
                    result_ok.set(false);
                    result_msg.set(
                        "Rain the soil can bank per day must be between 0.05 and 5 inches \
                         (or blank for the value derived from the soil)"
                            .into(),
                    );
                    return;
                }
            }
        };
        // Per-zone scheduling-model pin. Blank round-trips to null (the
        // engine default governs); the segmented control only offers the
        // two enum variants, so no free-text validation is needed here
        // and the server's enum parse still gates a raw PUT.
        let sched_model_json = {
            let s = new_sched_model.get();
            if s.trim().is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(s.trim().to_string())
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
        // For the run-limit confirmation copy, which names the zone.
        let confirm_name = display_name.clone();
        let photo_url_json = {
            let s = new_photo_url.get();
            if s.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(s)
            }
        };
        // Soil-sensor assignment: "" -> null (no soil gate), else the
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
        // THE BINDING, and the one field on this form that can take a
        // working zone dark. Every save writes it, including a save that
        // only changed the area, so a blank draft is only allowed to clear
        // an existing station when the picker actually enumerated the
        // controller's zones and the user chose "(not bound)". See
        // station_to_persist.
        let (stored_station, stored_name, stored_ctrl) = editing_now
            .as_ref()
            .map(|existing_slug| {
                config_json.with_untracked(|cfg| {
                    let z = cfg.get("zones").and_then(|z| z.get(existing_slug));
                    let gs = |k: &str| {
                        z.and_then(|z| z.get(k))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    (
                        gs("controller_station"),
                        z.and_then(|z| z.get("controller_zone_name"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        gs("controller_id"),
                    )
                })
            })
            .unwrap_or_default();
        let station = station_to_persist(&new_station.get(), &stored_station, enumerated.get());
        // The label describes ONE controller's zone. Read the scan cache only
        // when it belongs to the controller now selected (the same
        // `enumerated` guard every other consumer uses), and drop a stored
        // label the moment the controller changes, or a station id that
        // happens to exist on both (station "1" on two OpenSprinklers) would
        // carry the old controller's zone name onto the new binding.
        let discovered = if enumerated.get() {
            station_scan.with(|s| s.zones.clone())
        } else {
            Vec::new()
        };
        let stored_name = stored_name.filter(|_| stored_ctrl == new_controller.get());
        let vendor_name = vendor_name_to_persist(
            &station,
            &stored_station,
            stored_name.as_deref(),
            &discovered,
        );
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
                    obj.insert("max_run_minutes".into(), max_run_json.clone());
                    obj.insert("weekly_budget_in".into(), weekly_budget_json.clone());
                    obj.insert("sessions_per_week".into(), sessions_json.clone());
                    obj.insert("rain_credit_cap_in".into(), rain_cap_json.clone());
                    obj.insert("scheduling_model".into(), sched_model_json.clone());
                    obj.insert(
                        "controller_id".into(),
                        serde_json::json!(new_controller.get()),
                    );
                    obj.insert("controller_station".into(), serde_json::json!(station));
                    obj.insert(
                        "controller_zone_name".into(),
                        match vendor_name.clone() {
                            Some(n) => serde_json::Value::String(n),
                            None => serde_json::Value::Null,
                        },
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
                "max_run_minutes": max_run_json,
                "weekly_budget_in": weekly_budget_json,
                "sessions_per_week": sessions_json,
                "rain_credit_cap_in": rain_cap_json,
                "scheduling_model": sched_model_json,
                "controller_id": new_controller.get(),
                "controller_station": station,
                "controller_zone_name": match vendor_name.clone() {
                    Some(n) => serde_json::Value::String(n),
                    None => serde_json::Value::Null,
                },
                "soil_sensor_id": soil_sensor_json,
                "target_min_pct_soil": soil_min,
                "saturation_pct_soil": soil_sat,
                "photo_url": photo_url_json,
            }),
        };
        // The run limit the zone had BEFORE this save (unset = 60). The
        // confirmation fires only when this save raises the limit past 60,
        // so re-saving an already-raised zone stays quiet.
        let prior_max_run: Option<u32> = editing_now.as_ref().and_then(|existing_slug| {
            config_json.with_untracked(|cfg| {
                cfg.get("zones")
                    .and_then(|z| z.get(existing_slug))
                    .and_then(|z| z.get("max_run_minutes"))
                    .and_then(|v| v.as_u64())
                    .and_then(|v| u32::try_from(v).ok())
            })
        });
        if cap_raise_needs_confirm(max_run, prior_max_run) {
            pending_cap_min.set(max_run.unwrap_or(crate::config::schema::DEFAULT_MAX_RUN_MINUTES));
            pending_zone_name.set(confirm_name);
            pending_commit.set(Some((slug, entry)));
            confirm_open.set(true);
            return;
        }
        commit_zone(slug, entry);
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

    // Ask the selected controller for its own zone list.
    //
    // No new endpoint: POST /api/v1/wizard/scan_zones takes a whole
    // controller entry and restores its redacted secrets server-side BY ENTRY
    // ID, so the entry is posted exactly as GET /api/config served it,
    // sentinel and all, and no token ever passes through the zone editor.
    // Transport fields (base_url/host/port) must go over untouched or the
    // probe is refused: the server pins them to the stored entry.
    //
    // `force` distinguishes the explicit Rescan control from the lazy first
    // touch, which is a no-op once results are cached for this controller.
    let run_station_scan = move |force: bool| {
        let ctrl_id = new_controller.get_untracked();
        if ctrl_id.is_empty() {
            return;
        }
        let kind = selected_kind.get_untracked();
        if !can_scan_zones(&kind) {
            return;
        }
        // A FAILED scan counts as cached too. The lazy trigger is focus on
        // the station field, so without this a rate-limited or offline
        // controller is probed again every time the user tabs back to copy
        // an id, which is the one state that most needs backoff: the whole
        // reason the scan is lazy is that a Rachio account has roughly 1700
        // requests a day and live polling already spends most of them. The
        // explicit Rescan button passes force and stays a one-click retry,
        // and selecting a different controller rescans on its own.
        let already = station_scan.with_untracked(|sc| {
            sc.controller_id == ctrl_id
                && matches!(
                    sc.state,
                    StationScanState::Scanning
                        | StationScanState::Ready
                        | StationScanState::Unavailable(_)
                )
        });
        if already && !force {
            return;
        }
        let Some(entry) = config_json.with_untracked(|cfg| {
            cfg.get("controllers")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(ctrl_id.as_str()))
                })
                .cloned()
        }) else {
            return;
        };
        station_scan.set(StationScan {
            controller_id: ctrl_id.clone(),
            zones: Vec::new(),
            state: StationScanState::Scanning,
        });
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            let body = serde_json::json!({ "controller": entry });
            let result = async {
                let req = gloo_net::http::Request::post("/api/v1/wizard/scan_zones")
                    .json(&body)
                    .map_err(|e| e.to_string())?;
                let resp = req.send().await.map_err(|e| e.to_string())?;
                let v = resp
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or(serde_json::Value::Null);
                match v.get("zones").and_then(|z| z.as_array()) {
                    Some(arr) => Ok(arr
                        .iter()
                        .filter_map(|z| {
                            Some((
                                z.get("station_id")?.as_str()?.to_string(),
                                z.get("name")?.as_str()?.to_string(),
                            ))
                        })
                        .collect::<Vec<_>>()),
                    // Every failure mode lands here with the server's own
                    // trimmed category: offline, auth failed, rate limited,
                    // unsupported, demo instance, sentinel unresolvable.
                    None => Err(v
                        .get("detail")
                        .or_else(|| v.get("error"))
                        .and_then(|d| d.as_str())
                        .unwrap_or("the controller did not answer")
                        .to_string()),
                }
            }
            .await;
            // Detached continuation: the panel can be disposed mid-scan and a
            // bare set then panics (wasm release is panic=abort).
            let next = match result {
                Ok(zones) if !zones.is_empty() => StationScan {
                    controller_id: ctrl_id,
                    zones,
                    state: StationScanState::Ready,
                },
                Ok(_) => StationScan {
                    controller_id: ctrl_id,
                    zones: Vec::new(),
                    state: StationScanState::Unavailable(
                        "This controller reported no zones. Enter the id by hand below.".into(),
                    ),
                },
                Err(detail) => StationScan {
                    controller_id: ctrl_id,
                    zones: Vec::new(),
                    state: StationScanState::Unavailable(format!(
                        "Could not list this controller's zones ({detail}). Enter the id by \
                         hand below."
                    )),
                },
            };
            // A Some() back means the signal was disposed: the form went away
            // while the scan was in flight, and there is nothing to update.
            let _ = station_scan.try_set(next);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = entry;
            station_scan.set(StationScan {
                controller_id: ctrl_id,
                zones: Vec::new(),
                state: StationScanState::Idle,
            });
        }
    };

    let on_cancel = move |_| {
        reset_zone_draft(
            editing_slug,
            new_slug,
            new_display_name,
            new_area,
            new_precip,
            new_max_run,
            new_weekly_budget,
            new_sessions,
            new_rain_cap,
            new_sched_model,
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
            "Kc {kc_min:.2}-{kc_max:.2} · root {}{} · MAD {:.0}% (agronomy reference)",
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

    // The target the zone waters on while the two budget fields are
    // blank, resolved from the species picker exactly as the engine
    // resolves it, so the placeholder shows the number in effect and
    // follows the picker live.
    let inferred_target = move || inferred_weekly_target(&new_species.get());

    // The per-day rain cap in effect while the cap field is blank,
    // derived exactly as the engine derives it (the texture's FC-WP
    // spread times the root depth): the stored root override when this
    // zone carries one, else the species default from the shared
    // agronomy profile. Tracks the form's live texture and species picks
    // so the placeholder moves with them.
    let derived_rain_cap = move || {
        let root_override = editing_slug.get().and_then(|slug| {
            config_json.with(|cfg| {
                cfg.get("zones")
                    .and_then(|z| z.get(&slug))
                    .and_then(|z| z.get("root_depth_mm"))
                    .and_then(|v| v.as_f64())
            })
        });
        let root = root_override.unwrap_or_else(|| {
            crate::agronomy::species_profile_by_slug(&new_species.get()).root_depth_mm
        });
        derived_rain_cap_in(&new_soil.get(), root)
    };

    view! {
        <div id="zone-form-panel"><Panel title=panel_title>
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
                    // A station id only means anything to the controller it
                    // came from. Carried across a switch it persists an id
                    // the new controller has never heard of, and because the
                    // string is non-empty both the Unbound badge and the
                    // zone_unbound warning read it as bound, so the zone goes
                    // dark with nothing reporting it (or, between two
                    // numeric-station kinds, fires the wrong valve). Fires
                    // only on real interaction, so seeding the form and
                    // reopening it for another zone are unaffected. Dropping
                    // the cached scan with it keeps `enumerated` from briefly
                    // gating the picker on the previous controller's list.
                    on_change=Callback::new(move |_| {
                        new_station.set(String::new());
                        station_scan.set(StationScan::default());
                    })
                />
            </FormField>

            <FormField
                label="Controller station".to_string()
                helptext="Which of the controller's own zones this one fires. Pick it from the list where the controller can be asked; otherwise enter the id it uses.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                // The picker, only once a real list is in hand. It shows the
                // controller's NAME for each zone and stores the id, so the
                // names on either side are free to differ.
                <Show when=move || enumerated.get()>
                    <select
                        class="ui-input"
                        on:change=move |ev| new_station.set(event_target_value(&ev))
                    >
                        <option value="" selected=move || new_station.get().trim().is_empty()>
                            "(not bound)"
                        </option>
                        {move || {
                            let cur = new_station.get();
                            station_scan.with(|sc| {
                                sc.zones
                                    .iter()
                                    .map(|(id, name)| {
                                        let sel = cur.trim() == id.trim();
                                        let label = format!("{name} ({id})");
                                        view! {
                                            <option value=id.clone() selected=sel>{label}</option>
                                        }
                                    })
                                    .collect_view()
                            })
                        }}
                        // A stored id the controller did not report (a zone
                        // that was removed on the vendor's side, or a value
                        // typed before the picker existed) keeps its own
                        // option, so opening the form can never silently
                        // blank a working binding.
                        {move || {
                            let cur = new_station.get();
                            let known = station_scan
                                .with(|sc| sc.zones.iter().any(|(id, _)| id.trim() == cur.trim()));
                            (!cur.trim().is_empty() && !known).then(|| {
                                let label = format!("{cur} (not in this controller's list)");
                                view! { <option value=cur.clone() selected=true>{label}</option> }
                            })
                        }}
                    </select>
                </Show>
                // The free-text field, unchanged and always present. It is the
                // escape hatch for the six kinds that cannot enumerate, for a
                // controller that is offline right now, and for an id the scan
                // did not report. Nothing about the picker gates the save.
                <input
                    type="text"
                    class="ui-input"
                    style=move || if enumerated.get() { "margin-top: 0.4rem" } else { "" }
                    placeholder=move || station_placeholder(&selected_kind.get()).to_string()
                    prop:value=move || new_station.get()
                    on:input=move |ev| new_station.set(event_target_value(&ev))
                    // Lazy first touch: a scan is a live vendor request, so it
                    // waits for the user to actually reach for this field
                    // instead of firing on every editor open.
                    on:focus=move |_| run_station_scan(false)
                />
                <p class="ui-form-field__helptext">
                    {move || station_help(&selected_kind.get())}
                </p>
                {move || {
                    let kind = selected_kind.get();
                    can_scan_zones(&kind).then(|| {
                        let scanning = station_scan
                            .with(|sc| sc.state == StationScanState::Scanning);
                        view! {
                            <Button
                                variant="ghost"
                                disabled=Signal::derive(move || {
                                    station_scan
                                        .with(|sc| sc.state == StationScanState::Scanning)
                                })
                                aria_label="List this controller's zones and their ids".to_string()
                                on_click=Callback::new(move |_| run_station_scan(true))
                            >
                                {if scanning {
                                    "Listing zones\u{2026}"
                                } else if enumerated.get() {
                                    "Rescan zones"
                                } else {
                                    "List the controller's zones"
                                }}
                            </Button>
                        }
                    })
                }}
                // Why there is no list, in plain words. Never blocks anything.
                {move || {
                    station_scan.with(|sc| match &sc.state {
                        StationScanState::Unavailable(why) if sc.controller_id == new_controller.get() => {
                            Some(view! { <p class="zone-form__facts">{why.clone()}</p> })
                        }
                        _ => None,
                    })
                }}
                // The binding as an identity fact: which of the controller's
                // zones this one is wired to.
                {move || {
                    let ctrl = new_controller.get();
                    let station = new_station.get();
                    // Same controller guard as the save path: a cached list
                    // from a DIFFERENT controller must never name this zone.
                    let name = enumerated.get().then(|| {
                        station_scan.with(|sc| {
                            sc.zones
                                .iter()
                                .find(|(id, _)| id.trim() == station.trim())
                                .map(|(_, n)| n.clone())
                        })
                    }).flatten();
                    (!ctrl.is_empty() && !station.trim().is_empty()).then(|| {
                        let text = binding_display(&ctrl, &station, name.as_deref());
                        view! { <p class="zone-form__facts">"Fires " {text}</p> }
                    })
                }}
                // The unbound warning: no station here AND no entry under this
                // zone's slug in the controller's own zone map, which means
                // every run answers zone_unknown and the zone never waters.
                {move || {
                    let ctrl = new_controller.get();
                    if ctrl.is_empty() {
                        return None;
                    }
                    let slug = new_slug.get();
                    let controllers = config_json.with(|cfg| {
                        cfg.get("controllers")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default()
                    });
                    let probe = serde_json::json!({
                        "controller_id": ctrl,
                        "controller_station": new_station.get(),
                    });
                    (!slug.is_empty() && !zone_is_bound(&probe, &slug, &controllers)).then(|| {
                        // Two reasons a zone reads unbound, and they need
                        // different words: nothing entered, or something
                        // entered that this controller cannot use.
                        let station = new_station.get();
                        let kind = selected_kind.get();
                        let bad_shape = !station.trim().is_empty()
                            && crate::station_id::station_is_dispatchable(&kind, station.trim())
                                == Some(false);
                        let text = if bad_shape {
                            let expects = crate::station_id::station_expectation(&kind)
                                .unwrap_or("an id of its own");
                            format!(
                                "This controller cannot use \"{}\". It expects {expects}, so \
                                 nothing will water this zone until you change it.",
                                station.trim()
                            )
                        } else {
                            "This zone is not bound to anything on that controller yet, so \
                             nothing will water it. Pick or enter the controller's id for it \
                             above."
                                .to_string()
                        };
                        view! { <p class="zone-form__facts">{text}</p> }
                    })
                }}
            </FormField>

            // The model pin leads the three fields whose meaning it
            // changes (Weekly target, Sessions per week, the rain cap),
            // so their helptexts read as refinements of a visible choice
            // instead of three conditionals about a knob two fields
            // further down. The "Engine default" option resolves what it
            // means TODAY from the already-loaded config, so the choice
            // reads as a fact instead of a pointer to another page.
            <FormField
                label="Scheduling model".to_string()
                helptext="Engine default follows the setting on the Engine page. Weekly waters toward the weekly target in sessions. Soil waters when this zone's soil deficit crosses its trigger and refills it; cadence follows soil texture and roots, and a set weekly target acts as a ceiling.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {move || {
                    let engine_model = config_json.with(|cfg| {
                        if cfg.is_null() {
                            return None;
                        }
                        Some(
                            cfg.get("engine")
                                .and_then(|e| e.get("scheduling_model"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("weekly")
                                .to_string(),
                        )
                    });
                    let default_label = match engine_model.as_deref() {
                        Some("soil") => "Engine default (Soil)",
                        Some(_) => "Engine default (Weekly)",
                        // Before the config loads, the plain label; the
                        // resolved one takes over on load.
                        None => "Engine default",
                    };
                    view! {
                        <SegmentedControl
                            value=new_sched_model
                            options=vec![
                                ("".into(), default_label.into()),
                                ("weekly".into(), "Weekly".into()),
                                ("soil".into(), "Soil".into()),
                            ]
                            aria_label="Scheduling model".to_string()
                        />
                    }
                }}
            </FormField>

            <FormField
                label="Weekly target (inches a week)".to_string()
                helptext="Gross weekly depth this zone should receive. Weekly model: sizes every run, and rain counts toward the target. Soil model: a value set here becomes a ceiling on sprinkler water delivered over the trailing 7 days; rain does not count against the ceiling, and a blank or inferred target caps nothing. Blank = the starting value taken from this zone's species, shown in the box.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    class="ui-input"
                    min="0.05"
                    max="10"
                    step="0.05"
                    placeholder=move || format!("(blank for the default {:.2})", inferred_target().0)
                    prop:value=move || new_weekly_budget.get()
                    on:input=move |ev| new_weekly_budget.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Sessions per week".to_string()
                helptext="How many mornings the weekly target is split across, 1 to 7. Sessions space at floor(7 / sessions) days apart. Weekly model only: a soil-governed zone sets its own cadence from soil texture and root depth. Blank = the starting value taken from this zone's species, shown in the box.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    class="ui-input"
                    min="1"
                    max="7"
                    step="1"
                    placeholder=move || format!("(blank for the default {})", inferred_target().1)
                    prop:value=move || new_sessions.get()
                    on:input=move |ev| new_sessions.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Rain the soil can bank per day (inches)".to_string()
                helptext="The most rain one day can count against the weekly target. Rain beyond it in one day drains past the roots and does not count. Under the soil model the same cap limits how much of one day's rain the soil deficit credits. Blank = derived from this zone's soil texture and root depth, shown in the box; sandy soils want it low.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    class="ui-input"
                    min="0.05"
                    max="5"
                    step="0.05"
                    placeholder=move || format!("(blank for the derived {:.2})", derived_rain_cap())
                    prop:value=move || new_rain_cap.get()
                    on:input=move |ev| new_rain_cap.set(event_target_value(&ev))
                />
            </FormField>

            // Everything below is fine-tuning with a sensible default; a
            // beginner can add a working zone with just the fields above.
            <details class="zone-form-advanced">
                <summary class="zone-form-advanced__summary">"Advanced options"</summary>

                <FormField
                    label="Internal id (slug)".to_string()
                    helptext="Auto-generated from the name and permanent. Run history, soil sensor bindings, Home Assistant entity ids and this zone's links are all stored under it, so it never changes. Rename the zone by editing Name above; the slug stays.".to_string()
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
                    label="Max run time (minutes)".to_string()
                    helptext="Longest single watering this zone may run. Blank = 60. Values above 60 ask for confirmation when you save; long runs are still cycle-and-soaked against runoff.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="number"
                        class="ui-input"
                        min="5"
                        max="360"
                        step="5"
                        placeholder="(blank for the default 60)"
                        prop:value=move || new_max_run.get()
                        on:input=move |ev| new_max_run.set(event_target_value(&ev))
                    />
                </FormField>

            <FormField
                label="Photo (optional)".to_string()
                helptext="Drop or browse for an image to upload; it lands under /site/photos. You can also paste an off-site URL.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <PhotoField value=new_photo_url/>
            </FormField>

            <FormField
                label="Soil moisture sensor (optional)".to_string()
                helptext="Assign a sensor to drive this zone's skip decision. The dropdown lists every discovered soil channel, both Home Assistant entities and LocalSky native sources (incl. a zone-bound MQTT probe's channel). Or type an id below. Blank = no measured soil gate; the zone waters on the weekly water balance alone.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <select
                    class="ui-input"
                    on:change=move |ev| new_soil_sensor.set(event_target_value(&ev))
                >
                    <option value="" selected=move || new_soil_sensor.get().is_empty()>
                        "(none, no soil gate)"
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

            // Override-style confirmation for a run limit raised past 60:
            // the save is staged in pending_commit and lands only on
            // Confirm. Never a hard block; Cancel returns to the form
            // with the draft intact.
            <ConfirmSheet
                visible=confirm_open
                title="Raise run limit?"
                body=Signal::derive(move || {
                    format!(
                        "This allows {} to run up to {} minutes in a single watering. \
                         Cycle and soak still splits long runs to limit runoff.",
                        pending_zone_name.get(),
                        pending_cap_min.get()
                    )
                })
                confirm_label=Signal::derive(move || format!("Allow {} min", pending_cap_min.get()))
                on_confirm=Callback::new(move |()| {
                    if let Some((slug, entry)) = pending_commit.get_untracked() {
                        pending_commit.set(None);
                        commit_zone(slug, entry);
                    }
                })
            />
        </Panel></div>
    }
}

/// The one line under the station field, per controller kind. The picker
/// carries the explanation now, so this says only what the kind's id LOOKS
/// like and, where it matters, that this field is not the binding at all.
pub(crate) fn station_help(kind: &str) -> &'static str {
    match kind {
        "opensprinkler_direct" => {
            "OpenSprinkler numbers its stations from 1. The list above comes \
             straight off the box."
        }
        "http_generic" => {
            "Your board's own zone id, whatever string it uses. The list above \
             comes from the board's /zones response."
        }
        "rachio" => {
            "Rachio addresses zones by UUID, never by station number. Pick from \
             the list rather than typing one; a number here is ignored."
        }
        "hydrawise" => {
            "Hydrawise addresses zones by relay id, a number. This controller \
             cannot list its zones, so read the relay id off Hydrawise and \
             enter it."
        }
        "bhyve" | "rainbird" => {
            "This controller addresses zones by station number, counting from \
             1. It cannot list its zones, so enter the number yourself."
        }
        "ha_service_call" => {
            "The entity_id of the valve in Home Assistant, for example \
             switch.back_yard_zone. Home Assistant cannot be scanned from \
             here, so copy it from your HA entity list."
        }
        "mqtt_command" => {
            "MQTT zones do not bind here. Each one needs a command topic and \
             its payloads, which live in the controller's zone_command_map \
             under Settings, then Devices, then Advanced."
        }
        "esphome_native" => {
            "The ESPHome native adapter is not built yet, so this controller \
             fires nothing whatever you put here. ESPHome hardware runs \
             through the MQTT or DIY board controller kinds today."
        }
        "dry_run" => "Simulated hardware accepts any zone, bound or not.",
        _ => "The id the controller uses for this zone.",
    }
}

/// Placeholder for the free-text station input, shaped like the kind's own
/// ids so an empty field still teaches the format.
pub(crate) fn station_placeholder(kind: &str) -> &'static str {
    match kind {
        "rachio" => "zone UUID",
        "http_generic" => "board zone id",
        "ha_service_call" => "switch.back_yard_zone",
        "mqtt_command" => "(bound in the controller's zone_command_map)",
        _ => "1",
    }
}

/// Where the station picker's zone list stands for the controller currently
/// selected in the form. Cached per controller id for the editing session:
/// a scan is a LIVE vendor request (Rachio's daily budget is roughly 1700
/// and live polling already spends most of it), so it fires lazily on first
/// interaction with the field and on an explicit rescan, never on open.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct StationScan {
    /// The controller id these results belong to. A different selection in
    /// the Controller picker invalidates them.
    pub controller_id: String,
    /// (station id, vendor zone name) in the order the controller reported.
    pub zones: Vec<(String, String)>,
    pub state: StationScanState,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum StationScanState {
    /// Not asked yet.
    #[default]
    Idle,
    /// A request is in flight.
    Scanning,
    /// The controller listed its zones. `zones` is non-empty.
    Ready,
    /// No list is available and the plain reason why. The free-text field
    /// stays exactly as it was; this never blocks the form. Only the
    /// hydrate build ever runs a scan, so ssr sees this variant read but
    /// never constructed.
    #[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
    Unavailable(String),
}

impl StationScan {
    /// True only when a real list is in hand for THIS controller. Every
    /// "may the picker touch the binding" decision hangs off this.
    pub(crate) fn enumerated(&self, controller_id: &str) -> bool {
        self.controller_id == controller_id
            && self.state == StationScanState::Ready
            && !self.zones.is_empty()
    }
}

/// The existing zone key an ADD would replace, or `None` when the slug is
/// free.
///
/// Hyphens normalize to underscores because that is how dispatch resolves a
/// slug: "back-yard" and "back_yard" water the same valve, so letting the
/// two coexist would be its own defect. Returns the REAL stored key so the
/// refusal can name what the user already has.
pub(crate) fn zone_key_taken(cfg: &serde_json::Value, slug: &str) -> Option<String> {
    let want = slug.replace('-', "_");
    cfg.get("zones")
        .and_then(|z| z.as_object())
        .and_then(|m| m.keys().find(|k| k.replace('-', "_") == want).cloned())
}

/// The `controller_station` value a zone save should persist.
///
/// The form writes this field on EVERY save, including a save that only
/// changed the zone's area. So a blank draft must not be allowed to clear a
/// working binding just because the picker could not enumerate the
/// controller's zones (scan failed, rate limited, demo instance, or a kind
/// that cannot scan at all). When the picker DID enumerate, a blank is a
/// deliberate "(not bound)" choice and is honored.
///
/// A non-empty draft always wins: that is the user typing or picking.
pub(crate) fn station_to_persist(draft: &str, stored: &str, enumerated: bool) -> String {
    if !draft.trim().is_empty() {
        return draft.to_string();
    }
    if !enumerated && !stored.trim().is_empty() {
        return stored.to_string();
    }
    String::new()
}

/// The `controller_zone_name` label a zone save should persist: the
/// controller's own name for the bound zone, or `None`.
///
/// A pure label. Nothing dispatches on it and nothing keys on it, so the
/// only risk it carries is showing a stale name, which this rules out:
/// - the station matches something the scan just reported -> that name,
///   which also refreshes a name the vendor changed since the last bind;
/// - the station is unchanged from what was stored -> keep the stored label;
/// - anything else (a hand-typed station, a cleared binding) -> `None`,
///   because a label that no longer describes the bound zone is worse than
///   no label.
pub(crate) fn vendor_name_to_persist(
    station: &str,
    stored_station: &str,
    stored_name: Option<&str>,
    discovered: &[(String, String)],
) -> Option<String> {
    let s = station.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((_, name)) = discovered.iter().find(|(id, _)| id.trim() == s) {
        let name = name.trim();
        return (!name.is_empty()).then(|| name.to_string());
    }
    if s == stored_station.trim() {
        return stored_name.map(str::to_string).filter(|n| !n.is_empty());
    }
    None
}

/// The controller-config key holding a kind's per-zone map, mirroring
/// `config::validate::controller_zone_map_covers` on the server. `None` for
/// the kinds that bind by the zone's station field alone.
pub(crate) fn controller_zone_map_key(kind: &str) -> Option<&'static str> {
    match kind {
        "rachio" => Some("zone_uuid_map"),
        "hydrawise" => Some("zone_relay_map"),
        "bhyve" | "rainbird" => Some("zone_station_map"),
        "ha_service_call" => Some("zone_entity_map"),
        "mqtt_command" => Some("zone_command_map"),
        // Deliberately not esphome_native: it has a zone_entity_map in its
        // config, but the adapter is never built, so an entry in it cannot
        // certify a binding. `zone_is_bound` handles that kind before it
        // reaches this lookup.
        _ => None,
    }
}

/// Whether a zone is bound to anything that will actually fire it: a
/// non-empty station on the zone entry, or an entry under this zone's slug
/// in the controller's own zone map (the fallback that keeps pre-picker
/// configs watering). Hyphens normalize to underscores on both sides, the
/// way dispatch resolves them.
///
/// The station only counts when the kind can dispatch its SHAPE, judged by
/// the shared `crate::station_id` helpers that dispatch itself binds with, so
/// a value the controller will never accept does not read as a binding.
///
/// Three kinds do not follow that rule and are answered before it, matching
/// `config::validate` on the server: `dry_run` is always bound (it accepts
/// any slug), `esphome_native` never is (its adapter is not built), and
/// `mqtt_command` ignores the station field entirely (its per-zone value is
/// a command struct), so only its map counts.
///
/// An unbound zone looks completely healthy today and only reveals itself
/// when a run answers zone_unknown. This is what the card badge and the
/// editor line read.
pub(crate) fn zone_is_bound(
    zone: &serde_json::Value,
    slug: &str,
    controllers: &[serde_json::Value],
) -> bool {
    let ctrl_id = zone
        .get("controller_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if ctrl_id.is_empty() {
        // No controller at all already has its own badge; do not double up.
        return true;
    }
    let Some(entry) = controllers
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(ctrl_id))
    else {
        // References a controller that does not exist: a validation error in
        // its own right, not an unbound zone.
        return true;
    };
    let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    // Simulated hardware accepts any zone, bound or not, so there is nothing
    // to be unbound from. Matches station_help("dry_run") and the server's
    // controller_zone_map_covers.
    if kind == "dry_run" {
        return true;
    }
    // ESPHome native is never constructed, so neither binding fires and a
    // map entry proves nothing. The editor's per-kind line says why.
    if kind == "esphome_native" {
        return false;
    }
    let station = zone
        .get("controller_station")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    // A non-empty station is evidence of a binding only when the kind can
    // dispatch that SHAPE. This is the same `station_id` code the server's
    // config check and `runtime::build_controllers` use, so the badge, the
    // warning and the valve can never disagree. It answers None for the
    // kinds that ignore the field (MQTT, whose per-zone value is a command
    // struct), which leaves the controller's own map as the only test.
    //
    // A value the kind's parser rejects is the issue #8 shape one step on: a
    // Rachio UUID left on a zone moved to Hydrawise reads as bound, dispatch
    // ignores it, and the zone silently never waters.
    if !station.is_empty()
        && crate::station_id::station_is_dispatchable(kind, station) == Some(true)
    {
        return true;
    }
    let Some(map_key) = controller_zone_map_key(kind) else {
        return false;
    };
    let want = slug.replace('-', "_");
    entry
        .get("config")
        .and_then(|c| c.get(map_key))
        .and_then(|m| m.as_object())
        .is_some_and(|m| m.keys().any(|k| k.replace('-', "_") == want))
}

/// How a zone's controller binding reads on a card or in the editor: an
/// identity fact, never a status. The controller id always leads; the
/// controller's own name for the zone follows when a bind captured it,
/// falling back to the raw station id and then to the controller alone.
pub(crate) fn binding_display(ctrl_id: &str, station: &str, vendor_name: Option<&str>) -> String {
    let station = station.trim();
    match vendor_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("{ctrl_id} \u{00b7} {name}"),
        None if !station.is_empty() => format!("{ctrl_id} \u{00b7} station {station}"),
        None => ctrl_id.to_string(),
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
/// The weekly target a zone waters on when neither budget field is set,
/// from the SPECIES it is planted with. The engine's own resolution,
/// called directly, so the placeholder cannot promise a target the
/// engine does not water on.
pub fn inferred_weekly_target(species_slug: &str) -> (f64, u32) {
    crate::agronomy::default_weekly_target_in(species_slug)
}

/// The per-day rain-credit cap (inches) the engine derives while the
/// zone's `rain_credit_cap_in` is blank: TAW = (field capacity - wilting
/// point) x root depth, converted to inches. Reads the same shared
/// `agronomy` catalog `engine::soil_catalog::taw_mm` reads, so the
/// placeholder and the cap in effect come from one set of numbers rather
/// than two that can drift. An unknown slug takes the sandy_loam spread,
/// matching this form's own load default for an unset texture.
pub fn derived_rain_cap_in(soil_slug: &str, root_depth_mm: f64) -> f64 {
    // The engine's own function, called directly. Not a mirror of it.
    let texture = crate::config::schema::SoilTexture::from_slug(soil_slug);
    crate::engine::taw_mm(texture, root_depth_mm) / 25.4
}

#[allow(clippy::too_many_arguments)]
fn reset_zone_draft(
    editing_slug: RwSignal<Option<String>>,
    new_slug: RwSignal<String>,
    new_display_name: RwSignal<String>,
    new_area: RwSignal<f64>,
    new_precip: RwSignal<String>,
    new_max_run: RwSignal<String>,
    new_weekly_budget: RwSignal<String>,
    new_sessions: RwSignal<String>,
    new_rain_cap: RwSignal<String>,
    new_sched_model: RwSignal<String>,
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
    new_max_run.set(String::new());
    new_weekly_budget.set(String::new());
    new_sessions.set(String::new());
    new_rain_cap.set(String::new());
    new_sched_model.set(String::new());
    new_station.set(String::new());
    new_photo_url.set(String::new());
    new_soil_sensor.set(String::new());
    new_soil_min.set(30.0);
    new_soil_sat.set(70.0);
}

/// The run-limit confirmation predicate: a save needs the override-style
/// confirm exactly when the NEW effective limit crosses 60 minutes AND is
/// an increase over the prior effective limit (unset reads as the 60
/// default on both sides). Lowering, keeping, or re-saving an
/// already-raised value stays quiet; only crossing (or raising further
/// past) the line asks.
pub(crate) fn cap_raise_needs_confirm(
    new_minutes: Option<u32>,
    prior_minutes: Option<u32>,
) -> bool {
    let new_eff = new_minutes.unwrap_or(crate::config::schema::DEFAULT_MAX_RUN_MINUTES);
    let prior_eff = prior_minutes.unwrap_or(crate::config::schema::DEFAULT_MAX_RUN_MINUTES);
    new_eff > 60 && new_eff > prior_eff
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the picker must never clear a binding it could not enumerate ----

    /// THE UPGRADE-SAFETY TEST for the form. The zone editor writes
    /// controller_station on EVERY save, so a picker that came up empty
    /// (scan failed, rate limited, demo instance, or one of the six kinds
    /// that cannot enumerate) would silently unbind a working zone the
    /// moment its owner edited its area.
    #[test]
    fn a_picker_that_could_not_enumerate_never_clears_a_binding() {
        // The reporter's zone after the load-time backfill: a uuid on the
        // entry. He opens it to change the sprinkler type; the scan fails.
        assert_eq!(
            station_to_persist("", "1f00aa00-0000-4000-8000-0000000000a1", false),
            "1f00aa00-0000-4000-8000-0000000000a1"
        );
        // Same for a plain station number on a kind that cannot scan.
        assert_eq!(station_to_persist("", "3", false), "3");
        assert_eq!(station_to_persist("   ", "3", false), "3");
    }

    #[test]
    fn an_enumerated_picker_honors_an_explicit_unbind_and_a_typed_value_always_wins() {
        // The picker listed the controller's zones and the user chose
        // "(not bound)". That is a deliberate choice, so it is honored.
        assert_eq!(station_to_persist("", "3", true), "");
        // A non-empty draft always wins, enumerated or not: it is the user
        // typing or picking.
        assert_eq!(station_to_persist("7", "3", true), "7");
        assert_eq!(station_to_persist("7", "3", false), "7");
        // Nothing stored and nothing drafted stays empty either way.
        assert_eq!(station_to_persist("", "", true), "");
        assert_eq!(station_to_persist("", "", false), "");
    }

    // ---- the vendor label ----

    #[test]
    fn the_vendor_name_follows_the_binding_and_never_goes_stale() {
        let scan = vec![
            ("uuid-a".to_string(), "Front Lawn".to_string()),
            ("uuid-b".to_string(), "Back Lawn".to_string()),
        ];
        // Bound to something the scan reported: that name, which also
        // refreshes a label the vendor renamed since the last bind.
        assert_eq!(
            vendor_name_to_persist("uuid-a", "uuid-a", Some("Front Yard"), &scan),
            Some("Front Lawn".to_string())
        );
        // Station unchanged and not in the scan (or no scan at all): keep
        // the stored label rather than dropping it.
        assert_eq!(
            vendor_name_to_persist("uuid-z", "uuid-z", Some("Orchard"), &[]),
            Some("Orchard".to_string())
        );
        // Station hand-typed to something new: the old label no longer
        // describes it, so it goes rather than misleading.
        assert_eq!(
            vendor_name_to_persist("uuid-c", "uuid-a", Some("Front Lawn"), &scan),
            None
        );
        // Unbound carries no label.
        assert_eq!(
            vendor_name_to_persist("", "uuid-a", Some("Front Lawn"), &scan),
            None
        );
    }

    #[test]
    fn the_binding_reads_as_an_identity_fact() {
        assert_eq!(
            binding_display("rachio_main", "uuid-a", Some("Front Lawn")),
            "rachio_main \u{00b7} Front Lawn"
        );
        assert_eq!(
            binding_display("os_main", "3", None),
            "os_main \u{00b7} station 3"
        );
        assert_eq!(binding_display("os_main", "", None), "os_main");
        // A blank stored label does not produce a trailing separator.
        assert_eq!(
            binding_display("os_main", "3", Some("  ")),
            "os_main \u{00b7} station 3"
        );
    }

    // ---- unbound detection ----

    fn rachio(map: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "rachio_main",
            "kind": "rachio",
            "config": { "api_token": "t", "device_id": "d", "zone_uuid_map": map },
        })
    }

    #[test]
    fn a_zone_with_a_station_or_a_map_entry_is_bound() {
        const UUID_A: &str = "1f00aa00-0000-4000-8000-0000000000a1";
        const UUID_B: &str = "1f00aa00-0000-4000-8000-0000000000a2";
        let controllers = vec![rachio(serde_json::json!({ "back-yard": UUID_B }))];
        // Station on the entry. It has to be a shape Rachio can dispatch:
        // the station only counts when the kind can actually use it.
        let z = serde_json::json!({ "controller_id": "rachio_main", "controller_station": UUID_A });
        assert!(zone_is_bound(&z, "front_yard", &controllers));
        // No station, but the controller's own map covers the slug. This is
        // the issue #8 reporter's shape and it must NOT read as unbound.
        let z = serde_json::json!({ "controller_id": "rachio_main", "controller_station": "" });
        assert!(
            zone_is_bound(&z, "back_yard", &controllers),
            "hyphens normalize on both sides, exactly like dispatch"
        );
    }

    #[test]
    fn a_zone_bound_by_neither_path_reads_as_unbound() {
        let controllers = vec![rachio(serde_json::json!({}))];
        let z = serde_json::json!({ "controller_id": "rachio_main", "controller_station": "  " });
        assert!(!zone_is_bound(&z, "front_yard", &controllers));
        // A map-less kind has only the station field, so an empty one is
        // unbound with no fallback to check.
        let os = vec![serde_json::json!({
            "id": "os_main", "kind": "opensprinkler_direct", "config": { "host": "192.0.2.10" }
        })];
        let z = serde_json::json!({ "controller_id": "os_main", "controller_station": "" });
        assert!(!zone_is_bound(&z, "front_yard", &os));
    }

    #[test]
    fn no_controller_and_a_missing_controller_are_not_reported_as_unbound() {
        let controllers = vec![rachio(serde_json::json!({}))];
        // No controller at all already has its own badge and its own save
        // gate; a second warning on top would be noise.
        let z = serde_json::json!({ "controller_id": "", "controller_station": "" });
        assert!(zone_is_bound(&z, "front_yard", &controllers));
        // A controller that does not exist is a validation error in its own
        // right, not an unbound zone.
        let z = serde_json::json!({ "controller_id": "gone", "controller_station": "" });
        assert!(zone_is_bound(&z, "front_yard", &controllers));
    }

    // ---- adding must never replace ----

    /// `commit_zone` inserts by key, which REPLACES. Without the collision
    /// check an add whose name slugifies onto an existing zone silently
    /// wipes that zone's binding, its vendor label, and every agronomic
    /// field this form does not carry, and the server's zone-key rename
    /// guard never fires because the key SET did not change.
    #[test]
    fn an_add_whose_name_collides_with_an_existing_zone_is_caught() {
        let cfg = serde_json::json!({
            "zones": {
                "back_yard": { "display_name": "Back Yard", "controller_station": "uuid-b" },
                "side-yard": { "display_name": "Side Yard", "controller_station": "2" }
            }
        });
        // "Back Yard" slugifies to back_yard, which is taken.
        assert_eq!(
            zone_key_taken(&cfg, "back_yard"),
            Some("back_yard".to_string())
        );
        // Hyphens normalize the way dispatch resolves them, so a hyphenated
        // stored key and an underscored new one are the same zone.
        assert_eq!(
            zone_key_taken(&cfg, "side_yard"),
            Some("side-yard".to_string()),
            "the REAL stored key comes back so the refusal can name it"
        );
        // A genuinely new zone is free.
        assert_eq!(zone_key_taken(&cfg, "orchard"), None);
        // No zones at all, and a config that never loaded.
        assert_eq!(zone_key_taken(&serde_json::json!({}), "orchard"), None);
        assert_eq!(zone_key_taken(&serde_json::Value::Null, "orchard"), None);
    }

    // ---- per-kind honesty ----

    fn ctrl(id: &str, kind: &str, config: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "id": id, "kind": kind, "config": config })
    }

    /// MQTT never reads the station field, so whatever is typed there is not
    /// evidence of a binding. Certifying it hid the exact silent-never-
    /// waters state the badge exists to surface.
    #[test]
    fn an_mqtt_zone_is_bound_only_by_its_command_map() {
        let empty = vec![ctrl(
            "mqtt_main",
            "mqtt_command",
            serde_json::json!({ "broker_host": "b", "zone_command_map": {} }),
        )];
        let z = serde_json::json!({ "controller_id": "mqtt_main", "controller_station": "1" });
        assert!(
            !zone_is_bound(&z, "front_yard", &empty),
            "a station on an MQTT zone binds nothing"
        );
        let mapped = vec![ctrl(
            "mqtt_main",
            "mqtt_command",
            serde_json::json!({
                "broker_host": "b",
                "zone_command_map": { "front-yard": { "topic": "t" } }
            }),
        )];
        let z = serde_json::json!({ "controller_id": "mqtt_main", "controller_station": "" });
        assert!(
            zone_is_bound(&z, "front_yard", &mapped),
            "the map is the binding, with hyphens normalized"
        );
    }

    #[test]
    fn a_dry_run_zone_is_always_bound_and_an_esphome_zone_never_is() {
        let dry = vec![ctrl(
            "demo_controller",
            "dry_run",
            serde_json::json!({ "simulate_runs": false }),
        )];
        let z = serde_json::json!({ "controller_id": "demo_controller", "controller_station": "" });
        assert!(
            zone_is_bound(&z, "front_yard", &dry),
            "simulated hardware accepts any zone, matching station_help(\"dry_run\")"
        );
        // ESPHome's adapter is never built, so even a map entry binds nothing.
        let esp = vec![ctrl(
            "esphome_main",
            "esphome_native",
            serde_json::json!({
                "host": "192.0.2.60",
                "zone_entity_map": { "front_yard": "switch.front_yard" }
            }),
        )];
        let z = serde_json::json!({
            "controller_id": "esphome_main",
            "controller_station": "switch.front_yard"
        });
        assert!(!zone_is_bound(&z, "front_yard", &esp));
    }

    // ---- the vendor label follows the controller, not just the station ----

    /// Station ids collide across controllers of the same kind (station "1"
    /// on two OpenSprinklers), so a label kept on the station string alone
    /// would state a binding that does not exist.
    #[test]
    fn the_vendor_label_is_dropped_when_the_controller_changes() {
        // The save path filters `stored_name` on the stored controller id
        // before calling this, so a changed controller arrives as None.
        assert_eq!(
            vendor_name_to_persist("1", "1", None, &[]),
            None,
            "no label survives a controller change with no scan to re-resolve it"
        );
        // And with the controller unchanged the label is kept.
        assert_eq!(
            vendor_name_to_persist("1", "1", Some("Front Lawn"), &[]),
            Some("Front Lawn".to_string())
        );
    }

    /// The same guard on the scan cache: `enumerated` is false once the
    /// selected controller differs from the one that was scanned, and the
    /// save path passes an empty list in that case rather than matching a
    /// station id against another controller's zones.
    #[test]
    fn a_cached_scan_from_another_controller_cannot_name_this_zone() {
        let other = StationScan {
            controller_id: "os_main".into(),
            zones: vec![("1".into(), "Front Lawn".into())],
            state: StationScanState::Ready,
        };
        assert!(!other.enumerated("os_shed"));
        // What the save path hands the resolver once the guard says no.
        assert_eq!(vendor_name_to_persist("1", "1", None, &[]), None);
        // With the matching controller the same station does resolve.
        assert!(other.enumerated("os_main"));
        assert_eq!(
            vendor_name_to_persist("1", "", None, &other.zones),
            Some("Front Lawn".to_string())
        );
    }

    /// The client twin of `zone_station_unparseable`: the card badge and the
    /// server's config check must agree, or a user is told two different
    /// things about the same zone. Both call `station_id`, which is what
    /// dispatch binds with.
    #[test]
    fn a_station_the_kind_cannot_use_does_not_read_as_bound() {
        const UUID: &str = "1f00aa00-0000-4000-8000-0000000000a1";
        let hydrawise = vec![ctrl(
            "hydrawise_main",
            "hydrawise",
            serde_json::json!({ "api_key": "k", "controller_id": 7, "zone_relay_map": {} }),
        )];
        // A Rachio UUID left behind on a zone moved to Hydrawise.
        let z = serde_json::json!({
            "controller_id": "hydrawise_main",
            "controller_station": UUID
        });
        assert!(!zone_is_bound(&z, "front_yard", &hydrawise));
        // The relay id it should have been.
        let z = serde_json::json!({
            "controller_id": "hydrawise_main",
            "controller_station": "42"
        });
        assert!(zone_is_bound(&z, "front_yard", &hydrawise));
        // A station number on a Rachio zone: the original defect.
        let rachio_ctrl = vec![ctrl(
            "rachio_main",
            "rachio",
            serde_json::json!({ "api_token": "t", "device_id": "d", "zone_uuid_map": {} }),
        )];
        let z = serde_json::json!({
            "controller_id": "rachio_main",
            "controller_station": "3"
        });
        assert!(!zone_is_bound(&z, "front_yard", &rachio_ctrl));
    }

    /// The fallback rule is unchanged: whatever is in the station field, a
    /// zone the controller's own map covers still waters, so it is bound.
    #[test]
    fn an_unusable_station_still_reads_as_bound_when_the_map_covers_the_zone() {
        let mapped = vec![ctrl(
            "rachio_main",
            "rachio",
            serde_json::json!({
                "api_token": "t", "device_id": "d",
                "zone_uuid_map": { "front-yard": "1f00aa00-0000-4000-8000-0000000000a1" }
            }),
        )];
        let z = serde_json::json!({
            "controller_id": "rachio_main",
            "controller_station": "3"
        });
        assert!(zone_is_bound(&z, "front_yard", &mapped));
    }

    #[test]
    fn every_map_bearing_kind_has_a_map_key_and_the_rest_have_none() {
        for kind in [
            "rachio",
            "hydrawise",
            "bhyve",
            "rainbird",
            "ha_service_call",
            "mqtt_command",
        ] {
            assert!(
                controller_zone_map_key(kind).is_some(),
                "{kind} holds a per-zone map that dispatch reads"
            );
        }
        for kind in ["opensprinkler_direct", "http_generic", "dry_run", ""] {
            assert_eq!(
                controller_zone_map_key(kind),
                None,
                "{kind} binds by the zone's station field alone"
            );
        }
        // ESPHome native DOES hold a zone_entity_map, but its adapter is
        // never constructed, so an entry in it must not certify a binding.
        // zone_is_bound answers that kind before it reaches this lookup.
        assert_eq!(controller_zone_map_key("esphome_native"), None);
    }

    #[test]
    fn every_controller_kind_has_its_own_station_help_and_placeholder() {
        use crate::components::controllers_form::controller_kind_options;
        let generic = station_help("");
        for (kind, _) in controller_kind_options() {
            assert_ne!(
                station_help(&kind),
                generic,
                "{kind} needs its own line; the generic fallback is for an \
                 unselected controller"
            );
            assert!(!station_placeholder(&kind).is_empty());
        }
    }

    #[test]
    fn a_scan_only_counts_as_enumerated_for_the_controller_it_came_from() {
        let ready = StationScan {
            controller_id: "rachio_main".into(),
            zones: vec![("uuid-a".into(), "Front Lawn".into())],
            state: StationScanState::Ready,
        };
        assert!(ready.enumerated("rachio_main"));
        // Switching the Controller picker invalidates the list, so a stale
        // one can never authorize clearing the new controller's binding.
        assert!(!ready.enumerated("os_main"));
        // In flight, failed, or empty is never enumerated.
        assert!(!StationScan {
            controller_id: "rachio_main".into(),
            zones: Vec::new(),
            state: StationScanState::Ready,
        }
        .enumerated("rachio_main"));
        assert!(!StationScan {
            controller_id: "rachio_main".into(),
            zones: vec![("uuid-a".into(), "Front Lawn".into())],
            state: StationScanState::Scanning,
        }
        .enumerated("rachio_main"));
        assert!(!StationScan::default().enumerated(""));
    }

    #[test]
    fn cap_raise_confirm_predicate_matrix() {
        // Crossing the 60 minute line asks.
        assert!(cap_raise_needs_confirm(Some(90), None));
        assert!(cap_raise_needs_confirm(Some(61), Some(60)));
        // Raising an already-raised limit further asks again.
        assert!(cap_raise_needs_confirm(Some(120), Some(90)));
        // At or under 60 never asks.
        assert!(!cap_raise_needs_confirm(None, None));
        assert!(!cap_raise_needs_confirm(Some(60), None));
        assert!(!cap_raise_needs_confirm(Some(45), Some(90)));
        // Unchanged or lowered stays quiet, even while still above 60.
        assert!(!cap_raise_needs_confirm(Some(90), Some(90)));
        assert!(!cap_raise_needs_confirm(Some(90), Some(120)));
        // Clearing back to the default stays quiet.
        assert!(!cap_raise_needs_confirm(None, Some(120)));
    }
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
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::components::settings_ui::load_error_message(
            resp.status(),
            &body,
        ));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())
}

/// PUT the candidate config. Returns the restart_reasons the PUT response
/// carried (empty when the change hot-reloaded), the controllers.rs
/// pattern: this page previously discarded the response body, so a zone
/// add/remove never surfaced its restart requirement. A missing/old field
/// reads as "no restart", the safe default.
#[cfg(feature = "hydrate")]
async fn save_config(cfg: serde_json::Value) -> Result<Vec<String>, String> {
    use gloo_net::http::Request;
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::components::settings_ui::save_error_message(
            resp.status(),
            &body,
        ));
    }
    let reasons = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .filter(|v| {
            v.get("restart_required")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
        })
        .and_then(|v| {
            v.get("restart_reasons")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
        })
        .unwrap_or_default();
    Ok(reasons)
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
    // The controller's own name for the bound zone, captured when the
    // binding was made. A label only; nothing dispatches on it.
    let vendor_name = zone
        .get("controller_zone_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|n| !n.is_empty());
    // Bound by neither the zone's station nor the controller's own zone map
    // means every run answers zone_unknown and the zone silently never
    // waters. Computed against the config the page already holds, so it
    // costs no extra request.
    let bound = zone_is_bound(
        &zone,
        &slug,
        &config_json.with_untracked(|cfg| {
            cfg.get("controllers")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        }),
    );
    let sprinkler = zone
        .get("sprinkler_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let precip = zone.get("precip_rate_mm_hr").and_then(|v| v.as_f64());
    // The weekly target the allocator sizes this zone's week from, and
    // whether the operator set it or it was inferred from the name. This is
    // the row that lets the list answer "which zones still run on a guess".
    let weekly_target_display = {
        let set_budget = zone.get("weekly_budget_in").and_then(|v| v.as_f64());
        let set_sessions = zone
            .get("sessions_per_week")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        let (def_budget, def_sessions) = inferred_weekly_target(&species_slug);
        let budget = set_budget.unwrap_or(def_budget);
        let sessions = set_sessions.unwrap_or(def_sessions);
        let sessions_word = if sessions == 1 { "session" } else { "sessions" };
        let origin = if set_budget.is_some() && set_sessions.is_some() {
            ""
        } else {
            " (inferred from the species; set it in the editor)"
        };
        format!(
            "{} a week over {sessions} {sessions_word}{origin}",
            fmt_rain_amount(budget, prefs)
        )
    };
    // The 0.8.0 knobs, on the page whose job is scanning zone config: a
    // zone pinned to a model was indistinguishable from its neighbors
    // without opening Edit, and the rain cap had no read-only surface at
    // all.
    let sched_model_display = match zone.get("scheduling_model").and_then(|v| v.as_str()) {
        Some("soil") => "Soil (pinned)".to_string(),
        Some("weekly") => "Weekly (pinned)".to_string(),
        _ => "Engine default".to_string(),
    };
    let rain_cap_display = match zone.get("rain_credit_cap_in").and_then(|v| v.as_f64()) {
        Some(v) => format!("{} a day", fmt_rain_amount(v, prefs)),
        None => "(derived from soil and roots)".to_string(),
    };
    let subtitle = format!(
        "{slug} \u{00b7} {} \u{00b7} {} \u{00b7} {}",
        pretty_species(&species_slug),
        pretty_soil(&soil_slug),
        fmt_area_sqft(area, prefs)
    );
    let ctrl_display = if bound {
        binding_display(&ctrl_id, &station, vendor_name.as_deref())
    } else {
        format!("{ctrl_id} \u{00b7} not bound to a zone on this controller")
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
        _ => "(none, no soil gate)".to_string(),
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
    // Delete is destructive and used to fire with no confirmation at all;
    // it now stages behind the shared ConfirmSheet (danger variant, the
    // same two-step idiom as the run-limit raise).
    let delete_open = RwSignal::new(false);
    let display_for_delete = display.clone();
    let on_delete = move |_| delete_open.set(true);
    let do_delete = Callback::new(move |()| {
        let s = slug_for_delete.clone();
        config_json.update(|cfg| {
            if let Some(zones) = cfg.get_mut("zones").and_then(|v| v.as_object_mut()) {
                zones.remove(&s);
            }
        });
        // Commit immediately so the deletion can't be silently lost.
        persist.run(());
    });

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
                    // A zone with a controller but no binding to any of its
                    // zones. Warm, not Danger: nothing is broken, the wiring
                    // is simply not finished. Danger stays reserved for
                    // auth-failed and offline.
                    {(!bound).then(|| view! {
                        <SettingsBadge label="Unbound".into() tone=BadgeTone::Warm/>
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
                    <SettingsKv label="Weekly target" value=weekly_target_display/>
                    <SettingsKv label="Scheduling model" value=sched_model_display/>
                    <SettingsKv label="Rain cap / day" value=rain_cap_display/>
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
            <ConfirmSheet
                visible=delete_open
                title="Delete zone?"
                body=Signal::derive(move || {
                    format!(
                        "This removes {display_for_delete} and its zone settings from the \
                         configuration. Its run history is kept."
                    )
                })
                confirm_label=Signal::derive(|| "Delete zone".to_string())
                danger=true
                on_confirm=do_delete
            />
        </li>
    }
}
