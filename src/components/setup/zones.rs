// ZonesStep. Walks the operator through what a "zone" is in LocalSky's
// model, then shows the full grass-species catalog as a visual gallery so
// they know what they're picking (and have a fighting chance of picking
// right). Image files live under public/grass-species/<slug>.jpg; cards
// with no image present degrade to the description-only layout.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::Panel;

#[component]
pub fn ZonesStep() -> impl IntoView {
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
