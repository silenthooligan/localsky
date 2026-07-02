//! Shared agronomy catalogs: species FAO-56 profiles + sprinkler precipitation
//! rates, keyed by the snake_case config slug. Plain data with NO ssr-only deps
//! (the same pattern as `gates_catalog`), so both the engine (ssr) and the
//! wizard / settings UI (wasm) compile it.
//!
//! This is the SINGLE source of truth: `engine::species_catalog::lookup` and
//! `engine::sprinkler_catalog::catalog_precip_rate_mm_hr` delegate here (keyed by
//! the enum's serde slug, pinned by tests), and the per-zone form reads it
//! directly to show its FAO-56 params + precip estimate inline (P2-4) without an
//! ssr round-trip.

/// FAO-56 species profile (crop-coefficient curve + root depth + management
/// allowed depletion, plus operator-facing notes).
#[derive(Debug, Clone, Copy)]
pub struct SpeciesProfile {
    /// Monthly Kc, 1 = Jan ... 12 = Dec.
    pub kc_monthly: [f64; 12],
    /// Typical effective root zone depth (mm). Per-zone override available.
    pub root_depth_mm: f64,
    /// Management Allowed Depletion. Trigger irrigation when soil depletion
    /// >= TAW * mad_pct. Typical turf = 0.50; xeriscape = 0.30.
    pub mad_pct: f64,
    /// Optional ECe tolerance (dS/m) at 50% yield reduction. None for species
    /// without published values.
    pub salinity_tolerance_ds_m: Option<f64>,
    /// Recommended mow height (inches). None = N/A (shrubs, garden).
    pub mow_height_in: Option<f64>,
    /// One-line operator note. Surfaced in the advisor tile.
    pub notes: &'static str,
    pub citation: &'static str,
}

/// Species FAO-56 profile by config slug (snake_case). Total: an unknown slug
/// falls back to the generic "other" profile.
pub fn species_profile_by_slug(slug: &str) -> SpeciesProfile {
    match slug {
        // ----- Warm-season turfgrasses -----
        "st_augustine" => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.70, 0.85, 0.95, 1.00, 1.00, 1.00, 0.95, 0.85, 0.70, 0.55],
            root_depth_mm: 150.0,
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(6.0),
            mow_height_in: Some(3.5),
            notes: "Warm-season turf common across the US Southeast, Mediterranean climates, and Australia/NZ (sold there as Buffalo). Shallow-rooted; prefers deeper, less frequent watering.",
            citation: "FAO-56 Table 12; UF/IFAS ENH62",
        },
        "bermuda" => SpeciesProfile {
            kc_monthly: [0.50, 0.55, 0.65, 0.80, 0.90, 0.95, 0.95, 0.95, 0.90, 0.80, 0.65, 0.50],
            root_depth_mm: 200.0,
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(8.0),
            mow_height_in: Some(1.5),
            notes: "Deepest-rooted common turf. Drought-tolerant; can go semi-dormant in heat.",
            citation: "FAO-56 Table 12; UF/IFAS ENH19",
        },
        "zoysia" => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.65, 0.75, 0.85, 0.90, 0.90, 0.90, 0.85, 0.75, 0.65, 0.55],
            root_depth_mm: 150.0,
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(7.0),
            mow_height_in: Some(2.0),
            notes: "Slow but dense; tolerates moderate shade; recovers slowly from drought.",
            citation: "FAO-56 Table 12; UF/IFAS ENH11",
        },
        "bahia" => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.65, 0.75, 0.80, 0.85, 0.85, 0.85, 0.80, 0.75, 0.65, 0.55],
            root_depth_mm: 200.0,
            mad_pct: 0.55,
            salinity_tolerance_ds_m: Some(4.0),
            mow_height_in: Some(3.5),
            notes: "Drought-tolerant pasture-and-lawn grass widespread across the subtropical Americas; tolerates low fertility.",
            citation: "FAO-56 Table 12; UF/IFAS ENH6",
        },
        "centipede" => SpeciesProfile {
            kc_monthly: [0.50, 0.55, 0.60, 0.70, 0.80, 0.85, 0.85, 0.85, 0.80, 0.70, 0.60, 0.50],
            root_depth_mm: 100.0,
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(3.0),
            mow_height_in: Some(2.0),
            notes: "Low-maintenance; shallow-rooted; iron-chlorotic on high-pH soils.",
            citation: "FAO-56 Table 12; UF/IFAS ENH8",
        },
        "kikuyu" => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.70, 0.85, 0.95, 1.00, 1.00, 1.00, 0.95, 0.85, 0.70, 0.55],
            root_depth_mm: 300.0,
            mad_pct: 0.5,
            salinity_tolerance_ds_m: Some(4.0),
            mow_height_in: Some(1.5),
            notes: "Southern-hemisphere staple (Australia, NZ, South Africa). Vigorous warm-season runner; curve anchors shift automatically below the equator.",
            citation: "FAO-56 Table 12 (kikuyu grass)",
        },
        // ----- Cool-season turfgrasses -----
        "kentucky_bluegrass" => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.75, 0.85, 0.85, 0.80, 0.78, 0.80, 0.85, 0.80, 0.65, 0.55],
            root_depth_mm: 150.0,
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(3.0),
            mow_height_in: Some(2.5),
            notes: "Self-repairs via rhizomes; dormant in summer drought without irrigation.",
            citation: "FAO-56 Table 12 (cool-season turf)",
        },
        "tall_fescue" => SpeciesProfile {
            kc_monthly: [0.55, 0.65, 0.78, 0.85, 0.85, 0.80, 0.78, 0.80, 0.85, 0.80, 0.65, 0.55],
            root_depth_mm: 250.0,
            mad_pct: 0.55,
            salinity_tolerance_ds_m: Some(5.0),
            mow_height_in: Some(3.5),
            notes: "Deep-rooted; most heat- and drought-tolerant cool-season grass.",
            citation: "FAO-56 Table 12 (cool-season turf)",
        },
        "perennial_ryegrass" => SpeciesProfile {
            kc_monthly: [0.55, 0.65, 0.78, 0.85, 0.85, 0.80, 0.78, 0.80, 0.85, 0.80, 0.65, 0.55],
            root_depth_mm: 125.0,
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(5.0),
            mow_height_in: Some(2.5),
            notes: "Quick germination; often overseeded into dormant warm-season lawns for winter color.",
            citation: "FAO-56 Table 12 (cool-season turf)",
        },
        // ----- Non-turf zones -----
        "ornamental_shrubs" => SpeciesProfile {
            kc_monthly: [0.45, 0.45, 0.50, 0.55, 0.55, 0.55, 0.55, 0.55, 0.55, 0.55, 0.50, 0.45],
            root_depth_mm: 250.0,
            mad_pct: 0.40,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Established shrubs; water deeply + infrequently. Drip preferred.",
            citation: "FAO-56 Table 12; UF/IFAS ENH1115",
        },
        "vegetable_garden" => SpeciesProfile {
            kc_monthly: [0.55, 0.65, 0.75, 0.90, 1.10, 1.15, 1.15, 1.05, 0.90, 0.75, 0.65, 0.55],
            root_depth_mm: 400.0,
            mad_pct: 0.45,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Critical at germination + fruit set. Mulch heavily to cut ET.",
            citation: "FAO-56 Table 12 (vegetables mid-season)",
        },
        "drip_xeriscape" => SpeciesProfile {
            kc_monthly: [0.25, 0.25, 0.28, 0.30, 0.32, 0.35, 0.35, 0.35, 0.32, 0.30, 0.28, 0.25],
            root_depth_mm: 300.0,
            mad_pct: 0.30,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Established native plantings on drip. Water only during establishment / drought stress.",
            citation: "Operator convention; FAO-56 Kc_late for drought-tolerant ornamentals",
        },
        // "other" + any unknown slug.
        _ => SpeciesProfile {
            kc_monthly: [0.70; 12],
            root_depth_mm: 150.0,
            mad_pct: 0.50,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Generic placeholder. Override per zone with measured values.",
            citation: "Operator-supplied",
        },
    }
}

/// Catalog default precipitation rate (mm/hr) by sprinkler-type slug, used when
/// the operator has not measured it. Total: unknown slugs use the generic rate.
pub fn sprinkler_precip_mm_hr(slug: &str) -> f64 {
    match slug {
        "rotor" => 10.0,
        "spray" => 38.0,
        "mp_rotator" => 14.0,
        "drip" => 6.0,
        "bubbler" => 50.0,
        // "other" + any unknown slug.
        _ => 25.0,
    }
}

/// `[min, max]` of the monthly Kc curve, for a one-line "Kc x-y" summary.
pub fn kc_range(p: &SpeciesProfile) -> (f64, f64) {
    let min = p.kc_monthly.iter().copied().fold(f64::INFINITY, f64::min);
    let max = p
        .kc_monthly
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    (min, max)
}
