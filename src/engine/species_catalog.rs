// Grass species catalog. Monthly Kc curves keyed by GrassSpecies.
//
// Kc values are dimensionless crop coefficients applied to reference ET0
// to get crop ET (ETc = ET0 * Kc). The 12-element array stores monthly
// midpoint values; kc_at_doy interpolates linearly across months so the
// transition from dormant-winter to active-summer is smooth.
//
// Citations:
//   * FAO Irrigation and Drainage Paper No. 56, Table 12 (Allen et al., 1998)
//   * UF/IFAS Extension publications:
//       ENH6   - Bahiagrass for Florida Lawns
//       ENH8   - Centipedegrass for Florida Lawns
//       ENH11  - Zoysiagrass for Florida Lawns
//       ENH19  - Bermudagrass for Florida Lawns
//       ENH62  - St. Augustinegrass for Florida Lawns
//       ENH1115 - Florida-Friendly Landscaping (ornamentals)
//
// Numbers represent typical mature stand Kc in a humid subtropical climate
// and transfer to comparable climates worldwide (US Gulf South, coastal
// Australia, southern Brazil, East Asia); curves auto-shift six months for
// the Southern Hemisphere via kc_at_doy_lat. Cool-temperate users should
// see the KBG/TTTF/PRG entries. Beyond the hemisphere shift, curves are not
// adjusted for latitude or stress; operators with measured local data
// should override per-zone via ZoneConfig.mad_pct_override /
// precip_rate_mm_hr.

use crate::config::schema::GrassSpecies;

#[derive(Debug, Clone, Copy)]
pub struct SpeciesProfile {
    /// Monthly Kc, 1 = Jan ... 12 = Dec.
    pub kc_monthly: [f64; 12],
    /// Typical effective root zone depth (mm). Per-zone override available.
    pub root_depth_mm: f64,
    /// Management Allowed Depletion. Trigger irrigation when soil
    /// depletion >= TAW * mad_pct. Typical turf = 0.50; xeriscape = 0.30.
    pub mad_pct: f64,
    /// Optional ECe tolerance (dS/m) at 50% yield reduction. None for
    /// species without published values.
    pub salinity_tolerance_ds_m: Option<f64>,
    /// Recommended mow height (inches). None = N/A (shrubs, garden).
    pub mow_height_in: Option<f64>,
    /// One-line operator note. Surfaced in the advisor tile.
    pub notes: &'static str,
    pub citation: &'static str,
}

pub fn lookup(species: GrassSpecies) -> SpeciesProfile {
    use GrassSpecies::*;
    match species {
        // ----- Warm-season turfgrasses -----
        StAugustine => SpeciesProfile {
            // Active growth Apr-Oct (NH anchor); semi-dormant in the cool season.
            kc_monthly: [0.55, 0.60, 0.70, 0.85, 0.95, 1.00, 1.00, 1.00, 0.95, 0.85, 0.70, 0.55],
            root_depth_mm: 150.0, // 4-6"; 5" mid -> ~125mm; aerated lawns up to 6" / 150mm.
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(6.0),
            mow_height_in: Some(3.5),
            notes: "Warm-season turf common across the US Southeast, Mediterranean climates, and Australia/NZ (sold there as Buffalo). Shallow-rooted; prefers deeper, less frequent watering.",
            citation: "FAO-56 Table 12; UF/IFAS ENH62",
        },
        Bermuda => SpeciesProfile {
            kc_monthly: [0.50, 0.55, 0.65, 0.80, 0.90, 0.95, 0.95, 0.95, 0.90, 0.80, 0.65, 0.50],
            root_depth_mm: 200.0, // 4-8"; deep rooter on sand
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(8.0),
            mow_height_in: Some(1.5),
            notes: "Deepest-rooted common turf. Drought-tolerant; can go semi-dormant in heat.",
            citation: "FAO-56 Table 12; UF/IFAS ENH19",
        },
        Zoysia => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.65, 0.75, 0.85, 0.90, 0.90, 0.90, 0.85, 0.75, 0.65, 0.55],
            root_depth_mm: 150.0,
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(7.0),
            mow_height_in: Some(2.0),
            notes: "Slow but dense; tolerates moderate shade; recovers slowly from drought.",
            citation: "FAO-56 Table 12; UF/IFAS ENH11",
        },
        Bahia => SpeciesProfile {
            kc_monthly: [0.55, 0.60, 0.65, 0.75, 0.80, 0.85, 0.85, 0.85, 0.80, 0.75, 0.65, 0.55],
            root_depth_mm: 200.0,
            mad_pct: 0.55,
            salinity_tolerance_ds_m: Some(4.0),
            mow_height_in: Some(3.5),
            notes: "Drought-tolerant pasture-and-lawn grass widespread across the subtropical Americas; tolerates low fertility.",
            citation: "FAO-56 Table 12; UF/IFAS ENH6",
        },
        Centipede => SpeciesProfile {
            kc_monthly: [0.50, 0.55, 0.60, 0.70, 0.80, 0.85, 0.85, 0.85, 0.80, 0.70, 0.60, 0.50],
            root_depth_mm: 100.0, // 3-5"
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(3.0),
            mow_height_in: Some(2.0),
            notes: "Low-maintenance; shallow-rooted; iron-chlorotic on high-pH soils.",
            citation: "FAO-56 Table 12; UF/IFAS ENH8",
        },

        GrassSpecies::Kikuyu => SpeciesProfile {
            kc_monthly: [
                0.55, 0.60, 0.70, 0.85, 0.95, 1.00, 1.00, 1.00, 0.95, 0.85, 0.70, 0.55,
            ],
            root_depth_mm: 300.0, // deep, aggressive rhizomes; 10-14" typical
            mad_pct: 0.5,
            salinity_tolerance_ds_m: Some(4.0),
            mow_height_in: Some(1.5),
            notes: "Southern-hemisphere staple (Australia, NZ, South Africa). \
                    Vigorous warm-season runner; curve anchors shift \
                    automatically below the equator.",
            citation: "FAO-56 Table 12 (kikuyu grass)",
        },
        // ----- Cool-season turfgrasses (transitional + northern users) -----
        KentuckyBluegrass => SpeciesProfile {
            // Peak ET in spring/fall; summer heat stress dips Kc.
            kc_monthly: [0.55, 0.60, 0.75, 0.85, 0.85, 0.80, 0.78, 0.80, 0.85, 0.80, 0.65, 0.55],
            root_depth_mm: 150.0, // 4-8"
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(3.0),
            mow_height_in: Some(2.5),
            notes: "Self-repairs via rhizomes; dormant in summer drought without irrigation.",
            citation: "FAO-56 Table 12 (cool-season turf)",
        },
        TallFescue => SpeciesProfile {
            kc_monthly: [0.55, 0.65, 0.78, 0.85, 0.85, 0.80, 0.78, 0.80, 0.85, 0.80, 0.65, 0.55],
            root_depth_mm: 250.0, // 6-12"; deepest of cool-season
            mad_pct: 0.55,
            salinity_tolerance_ds_m: Some(5.0),
            mow_height_in: Some(3.5),
            notes: "Deep-rooted; most heat- and drought-tolerant cool-season grass.",
            citation: "FAO-56 Table 12 (cool-season turf)",
        },
        PerennialRyegrass => SpeciesProfile {
            kc_monthly: [0.55, 0.65, 0.78, 0.85, 0.85, 0.80, 0.78, 0.80, 0.85, 0.80, 0.65, 0.55],
            root_depth_mm: 125.0, // 4-6"
            mad_pct: 0.50,
            salinity_tolerance_ds_m: Some(5.0),
            mow_height_in: Some(2.5),
            notes: "Quick germination; often overseeded into dormant warm-season lawns for winter color.",
            citation: "FAO-56 Table 12 (cool-season turf)",
        },

        // ----- Non-turf zones -----
        OrnamentalShrubs => SpeciesProfile {
            // Established shrubs use ~half the ET0 of turf year-round.
            kc_monthly: [0.45, 0.45, 0.50, 0.55, 0.55, 0.55, 0.55, 0.55, 0.55, 0.55, 0.50, 0.45],
            root_depth_mm: 250.0, // 8-12"
            mad_pct: 0.40,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Established shrubs; water deeply + infrequently. Drip preferred.",
            citation: "FAO-56 Table 12; UF/IFAS ENH1115",
        },
        VegetableGarden => SpeciesProfile {
            // Wide swing; mid-season (fruit set) is the high.
            kc_monthly: [0.55, 0.65, 0.75, 0.90, 1.10, 1.15, 1.15, 1.05, 0.90, 0.75, 0.65, 0.55],
            root_depth_mm: 400.0, // 12-24"
            mad_pct: 0.45,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Critical at germination + fruit set. Mulch heavily to cut ET.",
            citation: "FAO-56 Table 12 (vegetables mid-season)",
        },
        DripXeriscape => SpeciesProfile {
            kc_monthly: [0.25, 0.25, 0.28, 0.30, 0.32, 0.35, 0.35, 0.35, 0.32, 0.30, 0.28, 0.25],
            root_depth_mm: 300.0,
            mad_pct: 0.30,
            salinity_tolerance_ds_m: None,
            mow_height_in: None,
            notes: "Established native plantings on drip. Water only during establishment / drought stress.",
            citation: "Operator convention; FAO-56 Kc_late for drought-tolerant ornamentals",
        },
        Other => SpeciesProfile {
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

/// Interpolate the monthly Kc curve for a given day-of-year (1..=366).
///
/// Assumes a Northern-Hemisphere calendar. Southern-Hemisphere users
/// should call `kc_at_doy_lat` instead so December reads as peak summer
/// rather than peak winter dormancy.
///
/// We treat each month's Kc as its mid-month value (e.g., Jan = day 15)
/// and linearly interpolate. Outside [Jan 15, Dec 15] wraps via Dec/Jan.
pub fn kc_at_doy(species: GrassSpecies, doy: u16) -> f64 {
    let profile = lookup(species);
    kc_at_doy_curve(&profile.kc_monthly, doy)
}

/// Hemisphere-aware Kc lookup. The seasonal Kc curves in this catalog
/// are anchored to the Northern-Hemisphere calendar (December = winter
/// dormancy for warm-season turfgrass; July = peak ET demand). For
/// `latitude_deg < 0` we shift the day-of-year by half a year so the
/// same curve reads correctly south of the equator.
///
/// At the equator (`latitude_deg == 0`) and small tropical latitudes the
/// seasonal swing in Kc is small enough that either anchor is fine; we
/// pick Northern as the default. Operators in those zones who want a
/// flatter curve can use `GrassSpecies::DripXeriscape` or override Kc
/// with `mad_pct_override` and per-zone fine-tuning.
pub fn kc_at_doy_lat(species: GrassSpecies, doy: u16, latitude_deg: f64) -> f64 {
    let profile = lookup(species);
    let effective_doy = shift_doy_for_hemisphere(doy, latitude_deg);
    kc_at_doy_curve(&profile.kc_monthly, effective_doy)
}

/// Shift DOY by ~6 months for Southern-Hemisphere latitudes. Wraps in
/// the range 1..=365. Exposed for tests + the verdict-strip projection.
pub fn shift_doy_for_hemisphere(doy: u16, latitude_deg: f64) -> u16 {
    if latitude_deg >= 0.0 {
        return doy;
    }
    let shifted = (doy as i32 + 182).rem_euclid(365);
    let shifted = if shifted == 0 { 365 } else { shifted };
    shifted as u16
}

fn kc_at_doy_curve(curve: &[f64; 12], doy: u16) -> f64 {
    // Mid-month anchor day-of-year (non-leap; good enough for Kc smoothing).
    const MID_MONTH_DOY: [f64; 12] = [
        15.0, 46.0, 74.0, 105.0, 135.0, 166.0, // Jan-Jun
        196.0, 227.0, 258.0, 288.0, 319.0, 349.0, // Jul-Dec
    ];
    let d = doy.clamp(1, 366) as f64;
    // Locate the bracket. Wraps Dec 15 <-> Jan 15.
    if d <= MID_MONTH_DOY[0] {
        return interp(
            d + 365.0,
            MID_MONTH_DOY[11],
            curve[11],
            MID_MONTH_DOY[0] + 365.0,
            curve[0],
        );
    }
    if d >= MID_MONTH_DOY[11] {
        return interp(
            d,
            MID_MONTH_DOY[11],
            curve[11],
            MID_MONTH_DOY[0] + 365.0,
            curve[0],
        );
    }
    for i in 0..11 {
        if d >= MID_MONTH_DOY[i] && d <= MID_MONTH_DOY[i + 1] {
            return interp(
                d,
                MID_MONTH_DOY[i],
                curve[i],
                MID_MONTH_DOY[i + 1],
                curve[i + 1],
            );
        }
    }
    curve[5] // unreachable; defensive
}

fn interp(x: f64, x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
    let t = (x - x0) / (x1 - x0);
    y0 + (y1 - y0) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every species enum variant must have a matching photo at
    /// public/grass-species/<slug>.jpg. The wizard's onerror handler
    /// hides missing photos silently, so visual drift is invisible at
    /// runtime; this test catches it at build time instead.
    #[test]
    fn every_species_has_a_photo() {
        use std::path::Path;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("public/grass-species");
        let expected = [
            ("st_augustine", GrassSpecies::StAugustine),
            ("bermuda", GrassSpecies::Bermuda),
            ("zoysia", GrassSpecies::Zoysia),
            ("bahia", GrassSpecies::Bahia),
            ("centipede", GrassSpecies::Centipede),
            ("kentucky_bluegrass", GrassSpecies::KentuckyBluegrass),
            ("tall_fescue", GrassSpecies::TallFescue),
            ("perennial_ryegrass", GrassSpecies::PerennialRyegrass),
            ("ornamental_shrubs", GrassSpecies::OrnamentalShrubs),
            ("vegetable_garden", GrassSpecies::VegetableGarden),
            ("drip_xeriscape", GrassSpecies::DripXeriscape),
            ("other", GrassSpecies::Other),
        ];
        let mut missing = Vec::new();
        for (slug, _sp) in expected {
            if !dir.join(format!("{slug}.jpg")).exists() {
                missing.push(slug);
            }
        }
        assert!(
            missing.is_empty(),
            "missing grass-species photos: {missing:?}"
        );
    }

    #[test]
    fn st_augustine_peaks_in_summer() {
        let jan = kc_at_doy(GrassSpecies::StAugustine, 15);
        let jul = kc_at_doy(GrassSpecies::StAugustine, 196);
        assert!(
            jul > jan,
            "expected summer Kc > winter Kc, got {jul} vs {jan}"
        );
        assert!(
            (jul - 1.00).abs() < 0.01,
            "St. Aug Jul peak ~1.00, got {jul}"
        );
        assert!(
            (jan - 0.55).abs() < 0.01,
            "St. Aug Jan low ~0.55, got {jan}"
        );
    }

    #[test]
    fn xeriscape_stays_low_year_round() {
        for doy in (15..=349).step_by(30) {
            let kc = kc_at_doy(GrassSpecies::DripXeriscape, doy);
            assert!(
                kc < 0.40,
                "drip xeriscape Kc should stay <0.40, got {kc} at doy {doy}"
            );
        }
    }

    #[test]
    fn interp_wraps_dec_to_jan_smoothly() {
        let dec_15 = kc_at_doy(GrassSpecies::Bermuda, 349);
        let jan_15 = kc_at_doy(GrassSpecies::Bermuda, 15);
        let dec_31 = kc_at_doy(GrassSpecies::Bermuda, 365);
        // Dec 31 should land between Dec 15 and Jan 15 values.
        let lo = dec_15.min(jan_15);
        let hi = dec_15.max(jan_15);
        assert!(
            dec_31 >= lo - 0.01 && dec_31 <= hi + 0.01,
            "dec 31 Kc {dec_31} not between dec_15 {dec_15} and jan_15 {jan_15}"
        );
    }

    #[test]
    fn cool_season_dips_in_summer() {
        let apr = kc_at_doy(GrassSpecies::KentuckyBluegrass, 105);
        let jul = kc_at_doy(GrassSpecies::KentuckyBluegrass, 196);
        assert!(
            apr > jul,
            "cool-season should dip in summer heat ({apr} vs {jul})"
        );
    }

    #[test]
    fn southern_hemisphere_kc_shifted_six_months() {
        // North-of-equator caller: Jan 15 reads dormancy, Jul 15 reads peak.
        let jan_n = kc_at_doy_lat(GrassSpecies::StAugustine, 15, 28.5);
        let jul_n = kc_at_doy_lat(GrassSpecies::StAugustine, 196, 28.5);
        // South-of-equator caller at the same DOYs should see them inverted:
        // Jan 15 = peak (Southern summer), Jul 15 = dormancy (Southern winter).
        let jan_s = kc_at_doy_lat(GrassSpecies::StAugustine, 15, -28.5);
        let jul_s = kc_at_doy_lat(GrassSpecies::StAugustine, 196, -28.5);
        assert!(jul_n > jan_n, "N hemi: Jul peak > Jan dormancy");
        assert!(jan_s > jul_s, "S hemi: Jan peak > Jul dormancy (flipped)");
        // And the Southern-Jan reading should equal the Northern-Jul reading
        // (within interpolation noise), since the DOY shift of 182 lines up
        // the two anchors.
        assert!(
            (jan_s - jul_n).abs() < 0.05,
            "S Jan {jan_s} should ~= N Jul {jul_n}"
        );
    }

    #[test]
    fn equator_treated_as_northern() {
        // At lat 0, no shift applied. Same number as the bare kc_at_doy call.
        let direct = kc_at_doy(GrassSpecies::Bermuda, 100);
        let via_lat = kc_at_doy_lat(GrassSpecies::Bermuda, 100, 0.0);
        assert!((direct - via_lat).abs() < 1e-9);
    }

    #[test]
    fn doy_shift_wraps() {
        assert_eq!(shift_doy_for_hemisphere(1, -30.0), 183);
        assert_eq!(shift_doy_for_hemisphere(183, -30.0), 365);
        assert_eq!(shift_doy_for_hemisphere(184, -30.0), 1);
        assert_eq!(shift_doy_for_hemisphere(365, -30.0), 182);
        // Northern is identity
        assert_eq!(shift_doy_for_hemisphere(1, 30.0), 1);
        assert_eq!(shift_doy_for_hemisphere(365, 30.0), 365);
    }
}
