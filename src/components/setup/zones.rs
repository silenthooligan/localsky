// ZonesStep. Walks the operator through what a "zone" is in LocalSky's
// model, then shows the full grass-species catalog as a visual gallery so
// they know what they're picking (and have a fighting chance of picking
// right). Image files live under public/grass-species/<slug>.jpg; cards
// with no image present degrade to the description-only layout.

use leptos::prelude::*;

use crate::components::settings::zones::ZoneForm;
use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{Button, Panel};

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

#[component]
pub fn ZonesStep() -> impl IntoView {
    // P2-1: create real zones in the wizard by reusing the settings ZoneForm,
    // wired to the wizard draft instead of the live config. `config_json` is the
    // draft's `config` object (the form mutates config["zones"] and reads
    // config["controllers"] for its picker); `persist` folds it back into the
    // full draft and PUTs it.
    let draft = RwSignal::new(serde_json::Value::Null);
    let config_json = RwSignal::new(serde_json::Value::Null);
    let loaded = RwSignal::new(false);

    let add_open = RwSignal::new(false);
    let editing_slug: RwSignal<Option<String>> = RwSignal::new(None);
    let new_slug = RwSignal::new(String::new());
    let new_display_name = RwSignal::new(String::new());
    // Preset-seeded with sensible warm-season defaults; re-seeded cool-season
    // from the configured latitude on load (matches the settings form).
    let new_species = RwSignal::new("st_augustine".to_string());
    let new_soil = RwSignal::new("sandy_loam".to_string());
    let new_area = RwSignal::new(1000.0f64);
    let new_sprinkler = RwSignal::new("rotor".to_string());
    let new_precip = RwSignal::new(String::new());
    let new_controller = RwSignal::new(String::new());
    let new_station = RwSignal::new(String::new());
    let new_photo_url = RwSignal::new(String::new());
    let new_soil_sensor = RwSignal::new(String::new());
    let new_soil_min = RwSignal::new(30.0f64);
    let new_soil_sat = RwSignal::new(70.0f64);
    // Sensors are configured AFTER zones in the wizard, so the soil-sensor
    // picker is empty here (modeled bucket); the user assigns probes later.
    let soil_sensor_opts = RwSignal::new(Vec::<(String, String, Option<f64>, String)>::new());
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                let cfg = d
                    .get("config")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                // Cool-season default outside |lat| < 35, so a northern user does
                // not open the form to a Florida lawn.
                let lat = cfg
                    .get("deployment")
                    .and_then(|dep| dep.get("location"))
                    .and_then(|l| l.get("lat"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                if lat.abs() >= 35.0 && new_species.get_untracked() == "st_augustine" {
                    new_species.set("tall_fescue".to_string());
                }
                // Default the (required) controller to the first one configured in
                // the prior step, so a beginner never faces an empty required field.
                if let Some(first) = cfg
                    .get("controllers")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("id"))
                    .and_then(|v| v.as_str())
                {
                    new_controller.set(first.to_string());
                }
                config_json.set(cfg);
                draft.set(d);
                loaded.set(true);
            }
        });
    });

    // Commit-immediately: fold the edited config back into the draft and save.
    let persist = Callback::new(move |()| {
        if !loaded.get_untracked() {
            return;
        }
        draft.update(|d| {
            if let Some(obj) = d.as_object_mut() {
                obj.insert("config".into(), config_json.get_untracked());
            }
        });
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    });

    // (slug, display name, species) for the added-zones list.
    let zone_list = move || {
        config_json
            .get()
            .get("zones")
            .and_then(|z| z.as_object())
            .map(|m| {
                m.iter()
                    .map(|(slug, z)| {
                        let name = z
                            .get("display_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(slug)
                            .to_string();
                        let species = z
                            .get("species")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .replace('_', " ");
                        (slug.clone(), name, species)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Tell us about your yard"</h2>
            <p class="setup-step__body">
                "A zone is one watering district: a chunk of yard tied to "
                "one valve. LocalSky asks for grass species, soil texture, "
                "area, sprinkler type and measured precipitation rate, then "
                "computes ETc from local weather, tracks soil depletion "
                "with the FAO-56 single-bucket model, and only fires when "
                "depletion crosses the species-specific MAD threshold."
            </p>

            // P2-1: the actual zone-creation surface (reuses the settings form).
            <Panel title="Your zones".to_string()>
                {move || {
                    let zones = zone_list();
                    if zones.is_empty() {
                        view! {
                            <p class="setup-step__body">
                                "No zones yet. Add your first watering zone to get a real "
                                "schedule; you can add more or edit them anytime under "
                                "/settings/zones."
                            </p>
                        }
                        .into_any()
                    } else {
                        view! {
                            <ul class="setup-zone-list">
                                {zones
                                    .into_iter()
                                    .map(|(slug, name, species)| {
                                        view! {
                                            <li>
                                                <strong>{name}</strong>
                                                " - "
                                                {species}
                                                " ("<code>{slug}</code>")"
                                            </li>
                                        }
                                            .into_any()
                                    })
                                    .collect::<Vec<_>>()}
                            </ul>
                        }
                        .into_any()
                    }
                }}
                <Show when=move || !add_open.get()>
                    <Button
                        variant="primary"
                        on_click=Callback::new(move |_| add_open.set(true))
                    >
                        {move || {
                            if zone_list().is_empty() {
                                "+ Add your first zone"
                            } else {
                                "+ Add another zone"
                            }
                        }}
                    </Button>
                </Show>
                {move || {
                    let msg = result_msg.get();
                    if msg.is_empty() {
                        ().into_any()
                    } else {
                        let color = if result_ok.get() {
                            "var(--verdict-run)"
                        } else {
                            "var(--accent-warm)"
                        };
                        view! {
                            <p class="setup-step__body" style=format!("color: {color}")>
                                {msg}
                            </p>
                        }
                        .into_any()
                    }
                }}
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
                    result_msg=result_msg
                    result_ok=result_ok
                    persist=persist
                />
            </Show>

            <Panel title="Grass species catalog".to_string()>
                <p class="setup-step__body" style="margin-bottom: 0.85rem">
                    "Each species has its own seasonal Kc curve, root depth, "
                    "and management allowable depletion. Pick the closest "
                    "match; per-zone overrides for root depth and MAD are "
                    "available under "
                    <a href="/settings/zones" style="color: var(--accent)">"/settings/zones"</a>
                    "."
                </p>
                {species_groups().into_iter().map(|(title, hint, cards)| view! {
                    <section>
                        <h4 style="margin: 1.1rem 0 0; color: var(--text); font-family: var(--font-display); font-size: 0.95rem">{title}</h4>
                        <p class="species-card__meta" style="margin: 0.15rem 0 0">{hint}</p>
                        <div class="species-gallery">
                            {cards.into_iter().map(|s| view! {
                                <SpeciesCard species=s/>
                            }.into_any()).collect::<Vec<_>>()}
                        </div>
                    </section>
                }.into_any()).collect::<Vec<_>>()}
            </Panel>

            <Panel title="What goes into a good zone definition".to_string()>
                <ul class="setup-source-list">
                    <li>
                        <strong>"Soil texture"</strong>
                        " - USDA texture class (the internationally standard "
                        "soil-texture taxonomy) drives field capacity, wilting "
                        "point, available water per metre, and infiltration. Sand, "
                        "Loamy Sand, Sandy Loam, Loam, Silt Loam, Clay Loam, Clay."
                    </li>
                    <li>
                        <strong>"Precipitation rate"</strong>
                        " - measured via catch-cup (preferred) or estimated "
                        "from sprinkler type. Drives runtime-to-depth math."
                    </li>
                    <li>
                        <strong>"Controller station"</strong>
                        " - which numbered valve on your controller this zone is. "
                        "For OpenSprinkler: 1-based station index. For HA: the "
                        "entity_id of the switch."
                    </li>
                    <li>
                        <strong>"Photo (optional)"</strong>
                        " - drop an image URL under "
                        <a href="/settings/zones" style="color: var(--accent)">"/settings/zones"</a>
                        " and the zone card renders it. Useful when you have "
                        "more than a handful of zones."
                    </li>
                </ul>
            </Panel>

            <Panel title="What happens if I skip this".to_string()>
                <p class="setup-step__body" style="margin-bottom: 0">
                    "Zones can be added after the wizard via "
                    <a href="/settings/zones" style="color: var(--accent)">"/settings/zones"</a>
                    ". The dashboard renders empty until at least one zone is "
                    "configured."
                </p>
            </Panel>

            <SetupFooter
                prev=prev_step_href("zones")
                next=next_step_href("zones")
            />
        </div>
    }
}

#[component]
fn SpeciesCard(species: SpeciesCardData) -> impl IntoView {
    let img = format!("/grass-species/{}.jpg", species.slug);
    view! {
        <article class="species-card">
            <div class="species-card__photo-wrap">
                <img
                    class="species-card__photo"
                    src=img
                    alt=species.name
                    loading="lazy"
                    onerror="this.style.display='none'"
                />
            </div>
            <div class="species-card__body">
                <h4 class="species-card__name">{species.name}</h4>
                <p class="species-card__meta">
                    <span>{format!("Kc {:.2}-{:.2}", species.kc_low, species.kc_high)}</span>
                    " · "
                    <span>{format!("root {}", species.root_depth)}</span>
                    " · "
                    <span>{format!("MAD {}%", species.mad_pct)}</span>
                </p>
                <p class="species-card__desc">{species.note}</p>
            </div>
        </article>
    }
}

#[derive(Clone)]
struct SpeciesCardData {
    slug: &'static str,
    name: &'static str,
    kc_low: f64,
    kc_high: f64,
    root_depth: &'static str,
    mad_pct: u32,
    note: &'static str,
}

/// Gallery sections: (title, climate hint, cards). Grouped by climate
/// band rather than region so no single locale's turf reads as the
/// default; species are alphabetical within each group.
fn species_groups() -> Vec<(&'static str, &'static str, Vec<SpeciesCardData>)> {
    vec![
        (
            "Warm-season turf",
            "Hot summers, mild winters: subtropical and Mediterranean climates.",
            vec![
                SpeciesCardData {
                    slug: "bahia",
                    name: "Bahia",
                    kc_low: 0.70,
                    kc_high: 0.85,
                    root_depth: "6-8\" (15-20 cm)",
                    mad_pct: 50,
                    note: "Low-input warm-season turfgrass. Tolerates poor sandy soils.",
                },
                SpeciesCardData {
                    slug: "bermuda",
                    name: "Bermuda",
                    kc_low: 0.70,
                    kc_high: 0.95,
                    root_depth: "4-8\" (10-20 cm)",
                    mad_pct: 50,
                    note: "Aggressive warm-season turfgrass (Couch in AU/NZ). High wear tolerance, full sun.",
                },
                SpeciesCardData {
                    slug: "centipede",
                    name: "Centipede",
                    kc_low: 0.60,
                    kc_high: 0.85,
                    root_depth: "3-5\" (8-13 cm)",
                    mad_pct: 50,
                    note: "Acidic-soil warm-season turfgrass. Low fertilizer requirement.",
                },
                SpeciesCardData {
                    slug: "kikuyu",
                    name: "Kikuyu",
                    kc_low: 0.55,
                    kc_high: 1.00,
                    root_depth: "10-14\" (25-36 cm)",
                    mad_pct: 50,
                    note: "Vigorous warm-season runner; a staple in AU / NZ / ZA.",
                },
                SpeciesCardData {
                    slug: "st_augustine",
                    name: "St. Augustine",
                    kc_low: 0.80,
                    kc_high: 1.00,
                    root_depth: "4-6\" (10-15 cm)",
                    mad_pct: 50,
                    note: "Dominant warm-season turfgrass of the US Southeast; sold as Buffalo grass in Australia/NZ. Coarse-textured, good shade tolerance.",
                },
                SpeciesCardData {
                    slug: "zoysia",
                    name: "Zoysia",
                    kc_low: 0.70,
                    kc_high: 0.90,
                    root_depth: "4-6\" (10-15 cm)",
                    mad_pct: 50,
                    note: "Fine-textured warm-season turfgrass. Slow to establish, drought tolerant.",
                },
            ],
        ),
        (
            "Cool-season turf",
            "Cool-temperate climates: cold winters, moderate summers.",
            vec![
                SpeciesCardData {
                    slug: "kentucky_bluegrass",
                    name: "Kentucky Bluegrass",
                    kc_low: 0.75,
                    kc_high: 0.95,
                    root_depth: "4-8\" (10-20 cm)",
                    mad_pct: 50,
                    note: "Cool-season turfgrass. Best fit for cool-temperate climates (northern US and Canada, northern Europe, NZ South Island).",
                },
                SpeciesCardData {
                    slug: "perennial_ryegrass",
                    name: "Perennial Ryegrass",
                    kc_low: 0.75,
                    kc_high: 0.95,
                    root_depth: "4-6\" (10-15 cm)",
                    mad_pct: 50,
                    note: "Cool-season turfgrass. Fast-establishing; often overseeded into warm-season for winter color.",
                },
                SpeciesCardData {
                    slug: "tall_fescue",
                    name: "Tall Fescue",
                    kc_low: 0.70,
                    kc_high: 0.95,
                    root_depth: "6-12\" (15-30 cm)",
                    mad_pct: 50,
                    note: "Cool-season turfgrass. Deep-rooted, drought-tolerant relative to other cool-season.",
                },
            ],
        ),
        (
            "Non-turf zones",
            "Beds, gardens, and low-water plantings in any climate.",
            vec![
                SpeciesCardData {
                    slug: "ornamental_shrubs",
                    name: "Ornamental Shrubs",
                    kc_low: 0.40,
                    kc_high: 0.60,
                    root_depth: "8-12\" (20-30 cm)",
                    mad_pct: 40,
                    note: "Mixed shrub bed. Drip or low-precip rotor preferred; less frequent, deeper waterings.",
                },
                SpeciesCardData {
                    slug: "vegetable_garden",
                    name: "Vegetable Garden",
                    kc_low: 0.65,
                    kc_high: 1.15,
                    root_depth: "12-24\" (30-60 cm)",
                    mad_pct: 50,
                    note: "Mixed vegetable bed. High demand at fruiting; consider seasonal Kc override.",
                },
                SpeciesCardData {
                    slug: "drip_xeriscape",
                    name: "Drip / Xeriscape",
                    kc_low: 0.30,
                    kc_high: 0.30,
                    root_depth: "n/a",
                    mad_pct: 30,
                    note: "Native + xeriscape plantings. Minimal supplemental irrigation; long soak cycles.",
                },
                SpeciesCardData {
                    slug: "other",
                    name: "Other (custom Kc)",
                    kc_low: 0.40,
                    kc_high: 0.80,
                    root_depth: "varies",
                    mad_pct: 50,
                    note: "Generic placeholder. Override per zone with measured values.",
                },
            ],
        ),
    ]
}
