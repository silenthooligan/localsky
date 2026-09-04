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
    /// How many mornings a week the weekly target is split across when
    /// the operator has not set one. Turf takes two soakings; plantings
    /// the profile notes as deep-and-infrequent take one.
    pub default_sessions_per_week: u32,
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
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: Some(6.0),
            mow_height_in: Some(3.5),
            notes: "Warm-season turf common across the US Southeast, Mediterranean climates, and Australia/NZ (sold there as Buffalo). Shallow-rooted; prefers deeper, less frequent watering.",
            citation: "FAO-56 Table 12; UF/IFAS ENH62",
        },
        "bermuda" => SpeciesProfile {
            kc_monthly: [0.50, 0.55, 0.65, 0.80, 0.90, 0.95, 0.95, 0.95, 0.90, 0.80, 0.65, 0.50],
            root_depth_mm: 200.0,
            mad_pct: 0.50,
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: Some(8.0),
            mow_height_in: Some(1.5),
            notes: "Deepest-rooted common turf. Drought-tolerant; can go semi-dormant in heat.",
            citation: "FAO-56 Table 12; UF/IFAS ENH19",
        },
        "zoysia" => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.65, 0.75, 0.85, 0.90, 0.90, 0.90, 0.85, 0.75, 0.65, 0.55],
            root_depth_mm: 150.0,
            mad_pct: 0.50,
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: Some(7.0),
            mow_height_in: Some(2.0),
            notes: "Slow but dense; tolerates moderate shade; recovers slowly from drought.",
            citation: "FAO-56 Table 12; UF/IFAS ENH11",
        },
        "bahia" => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.65, 0.75, 0.80, 0.85, 0.85, 0.85, 0.80, 0.75, 0.65, 0.55],
            root_depth_mm: 200.0,
            mad_pct: 0.55,
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: Some(4.0),
            mow_height_in: Some(3.5),
            notes: "Drought-tolerant pasture-and-lawn grass widespread across the subtropical Americas; tolerates low fertility.",
            citation: "FAO-56 Table 12; UF/IFAS ENH6",
        },
        "centipede" => SpeciesProfile {
            kc_monthly: [0.50, 0.55, 0.60, 0.70, 0.80, 0.85, 0.85, 0.85, 0.80, 0.70, 0.60, 0.50],
            root_depth_mm: 100.0,
            mad_pct: 0.50,
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: Some(3.0),
            mow_height_in: Some(2.0),
            notes: "Low-maintenance; shallow-rooted; iron-chlorotic on high-pH soils.",
            citation: "FAO-56 Table 12; UF/IFAS ENH8",
        },
        "kikuyu" => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.70, 0.85, 0.95, 1.00, 1.00, 1.00, 0.95, 0.85, 0.70, 0.55],
            root_depth_mm: 300.0,
            mad_pct: 0.5,
            default_sessions_per_week: 2,
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
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: Some(3.0),
            mow_height_in: Some(2.5),
            notes: "Self-repairs via rhizomes; dormant in summer drought without irrigation.",
            citation: "FAO-56 Table 12 (cool-season turf)",
        },
        "tall_fescue" => SpeciesProfile {
            kc_monthly: [0.55, 0.65, 0.78, 0.85, 0.85, 0.80, 0.78, 0.80, 0.85, 0.80, 0.65, 0.55],
            root_depth_mm: 250.0,
            mad_pct: 0.55,
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: Some(5.0),
            mow_height_in: Some(3.5),
            notes: "Deep-rooted; most heat- and drought-tolerant cool-season grass.",
            citation: "FAO-56 Table 12 (cool-season turf)",
        },
        "perennial_ryegrass" => SpeciesProfile {
            kc_monthly: [0.55, 0.65, 0.78, 0.85, 0.85, 0.80, 0.78, 0.80, 0.85, 0.80, 0.65, 0.55],
            root_depth_mm: 125.0,
            mad_pct: 0.50,
            default_sessions_per_week: 2,
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
            default_sessions_per_week: 1,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Established shrubs; water deeply + infrequently. Drip preferred.",
            citation: "FAO-56 Table 12; UF/IFAS ENH1115",
        },
        "vegetable_garden" => SpeciesProfile {
            kc_monthly: [0.55, 0.65, 0.75, 0.90, 1.10, 1.15, 1.15, 1.05, 0.90, 0.75, 0.65, 0.55],
            root_depth_mm: 400.0,
            mad_pct: 0.45,
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Critical at germination + fruit set. Mulch heavily to cut ET.",
            citation: "FAO-56 Table 12 (vegetables mid-season)",
        },
        "drip_xeriscape" => SpeciesProfile {
            kc_monthly: [0.25, 0.25, 0.28, 0.30, 0.32, 0.35, 0.35, 0.35, 0.32, 0.30, 0.28, 0.25],
            root_depth_mm: 300.0,
            mad_pct: 0.30,
            default_sessions_per_week: 1,
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
            default_sessions_per_week: 2,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Generic placeholder. Override per zone with measured values.",
            citation: "Operator-supplied",
        },
    }
}

/// Latitude at which a warm-season default stops being the sensible one.
/// Beyond it, summers are too short and winters too cold for the
/// subtropical turfs, so a cool-season grass is the better starting pick.
const COOL_SEASON_LATITUDE_DEG: f64 = 35.0;

/// The species a form should PRE-SELECT for a yard at this latitude,
/// given the species it currently shows. Only ever moves the warm-season
/// default to the cool-season one; a species the operator actually chose
/// is returned untouched.
///
/// Both the setup wizard and the zone editor seed their picker this way,
/// and each carried its own copy of the rule with the latitude and both
/// species names written out, so a change had to be made twice to hold.
pub fn climate_default_species(current_slug: &str, latitude_deg: f64) -> &str {
    if latitude_deg.abs() >= COOL_SEASON_LATITUDE_DEG && current_slug == "st_augustine" {
        "tall_fescue"
    } else {
        current_slug
    }
}

/// Peak-season crop coefficient of the reference planting the weekly
/// starting target is anchored on: warm-season turf at full canopy.
const REFERENCE_PEAK_KC: f64 = 1.00;

/// The weekly starting target (inches) and how many mornings it splits
/// across, for a zone whose operator has not set one.
///
/// Anchored on the extension-service recommendation every turf guide
/// carries, about an inch of water a week in two soakings, and scaled to
/// the species by its OWN peak crop coefficient against that reference
/// turf. A planting that transpires half as hard starts at half the
/// water. So vegetables (peak Kc 1.15) start above turf rather than
/// below it, and established xeriscape (0.35) starts far below, which is
/// the direction the agronomy has always pointed even when the starting
/// number did not.
///
/// This replaced a guess made from the zone's NAME: any slug containing
/// shrub, garden or bed took half an inch and everything else took a
/// full inch. A vegetable bed therefore started on half the water it
/// wants, a xeriscape zone named for its street corner started on triple,
/// and a lawn named "back_bed" was watered like a shrub. The species is
/// declared by the operator in the zone editor; the name is not data.
///
/// It remains a STARTING point, flagged in the UI as inferred, that the
/// tuning report corrects from measured runs.
pub fn default_weekly_target_in(species_slug: &str) -> (f64, u32) {
    let p = species_profile_by_slug(species_slug);
    let (_, peak_kc) = kc_range(&p);
    let inches = (peak_kc / REFERENCE_PEAK_KC).max(0.0);
    // Two decimals: the field is inches of water and the UI prints it so.
    let inches = (inches * 100.0).round() / 100.0;
    (inches, p.default_sessions_per_week)
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

// ---------------------------------------------------------------------
// Soil textures
// ---------------------------------------------------------------------
//
// Citations:
//   * Water holding: FAO Irrigation and Drainage Paper No. 56, Table 19
//     "Typical soil water characteristics for different soil types"
//     (Allen et al., 1998).
//   * Infiltration: USDA NRCS Part 652 National Irrigation Guide,
//     Table 11-3.
//
// Field capacity (FC) and wilting point (WP) are volumetric water
// content (m3 water / m3 soil). Available water per metre of depth is
// (FC - WP) * 1000 mm, and the engine's TAW for a zone is that times the
// root depth.
//
// EVERY water-holding entry sits inside the published FAO-56 Table 19
// range, with the band for that row in a comment beside it, pinned by
// `catalog_entries_sit_inside_the_fao56_bands` in engine::soil_catalog.
// These numbers set both the per-day rain-credit cap and the soil
// model's bucket, so a value invented here would misrepresent real
// yards. Table 19 carries no clay-loam row; its "silt clay loam" is the
// nearest published analogue and is what clay_loam is held against.
//
// This lives here, beside the species profiles, because the zone editor
// shows the derived rain cap live and cannot reach the ssr-only engine.
// One copy, read by both sides.

#[derive(Debug, Clone, Copy)]
pub struct SoilProfile {
    /// Volumetric field capacity (m³/m³).
    pub field_capacity: f64,
    /// Volumetric wilting point (m³/m³).
    pub wilting_point: f64,
    /// Available water per metre depth (mm/m). Derived: (FC-WP) * 1000.
    pub aw_mm_per_m: f64,
    /// Basic infiltration rate (mm/hr) on flat, 3-5% slope, and >5% slope.
    pub infiltration_mm_hr: InfiltrationRates,
    pub citation: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct InfiltrationRates {
    pub flat: f64,
    pub moderate_slope: f64,
    pub steep_slope: f64,
}

/// Soil water + infiltration profile by config slug (snake_case). An
/// unknown slug takes sandy loam, the same middle-of-the-road default
/// the zone form loads for an unset texture.
pub fn soil_profile_by_slug(slug: &str) -> SoilProfile {
    match slug {
        // FAO-56 Table 19 Sand: FC 0.07-0.17, WP 0.02-0.07, AW 0.05-0.11.
        // Held near the coarse end: the yards this serves are the fast-
        // draining sands the per-day cap exists for.
        "sand" => SoilProfile {
            field_capacity: 0.09,
            wilting_point: 0.03,
            aw_mm_per_m: 60.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 50.0,
                moderate_slope: 35.0,
                steep_slope: 25.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        // FAO-56 Table 19 Loamy sand: FC 0.11-0.19, WP 0.03-0.10, AW 0.06-0.12.
        "loamy_sand" => SoilProfile {
            field_capacity: 0.14,
            wilting_point: 0.06,
            aw_mm_per_m: 80.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 35.0,
                moderate_slope: 25.0,
                steep_slope: 18.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        // FAO-56 Table 19 Sandy loam: FC 0.18-0.28, WP 0.06-0.16, AW 0.11-0.15.
        "sandy_loam" => SoilProfile {
            field_capacity: 0.23,
            wilting_point: 0.10,
            aw_mm_per_m: 130.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 25.0,
                moderate_slope: 18.0,
                steep_slope: 12.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        // FAO-56 Table 19 Loam: FC 0.20-0.30, WP 0.07-0.17, AW 0.13-0.18.
        "loam" => SoilProfile {
            field_capacity: 0.27,
            wilting_point: 0.12,
            aw_mm_per_m: 150.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 13.0,
                moderate_slope: 10.0,
                steep_slope: 7.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        // FAO-56 Table 19 Silt loam: FC 0.22-0.36, WP 0.09-0.21, AW 0.13-0.19.
        "silt_loam" => SoilProfile {
            field_capacity: 0.32,
            wilting_point: 0.15,
            aw_mm_per_m: 170.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 10.0,
                moderate_slope: 8.0,
                steep_slope: 5.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        // FAO-56 Table 19 Silt clay loam: FC 0.30-0.37, WP 0.17-0.24,
        // AW 0.13-0.18.
        "clay_loam" => SoilProfile {
            field_capacity: 0.36,
            wilting_point: 0.20,
            aw_mm_per_m: 160.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 8.0,
                moderate_slope: 6.0,
                steep_slope: 4.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        // FAO-56 Table 19 Clay: FC 0.32-0.40, WP 0.20-0.24, AW 0.12-0.20.
        "clay" => SoilProfile {
            field_capacity: 0.38,
            wilting_point: 0.24,
            aw_mm_per_m: 140.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 5.0,
                moderate_slope: 4.0,
                steep_slope: 3.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        // Unknown or unset: the middle-of-the-road texture the zone form
        // also loads by default. Recursing keeps one copy of the numbers.
        _ => soil_profile_by_slug("sandy_loam"),
    }
}
