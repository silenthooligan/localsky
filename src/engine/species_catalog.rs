// Grass species catalog. Monthly Kc curves keyed by GrassSpecies.
//
// The species DATA now lives in the shared, slug-keyed `crate::agronomy`
// catalog (plain data, no ssr-only deps), so the wizard / settings UI can read
// the same FAO-56 params the engine uses without an ssr round-trip. This module
// keeps the GrassSpecies-keyed API the engine depends on and the Kc-curve
// interpolation, delegating the lookup to agronomy via the enum's serde slug.
//
// Kc values are dimensionless crop coefficients applied to reference ET0 to get
// crop ET (ETc = ET0 * Kc). The 12-element array stores monthly midpoint values;
// kc_at_doy interpolates linearly across months so the transition from
// dormant-winter to active-summer is smooth. Curves auto-shift six months for
// the Southern Hemisphere via kc_at_doy_lat. See `crate::agronomy` for the data
// and citations.

use crate::config::schema::GrassSpecies;

pub use crate::agronomy::SpeciesProfile;

/// FAO-56 profile for a species. Delegates to the shared, slug-keyed `agronomy`
/// catalog (the single source of truth, also read by the wasm UI); `species_slug`
/// maps the enum to its serde slug and is pinned to serde by
/// `species_slug_matches_serde`.
pub fn lookup(species: GrassSpecies) -> SpeciesProfile {
    crate::agronomy::species_profile_by_slug(species_slug(species))
}

/// Enum -> snake_case slug used by the agronomy catalog + the config wire
/// format. Kept in lockstep with serde by a test.
fn species_slug(species: GrassSpecies) -> &'static str {
    use GrassSpecies::*;
    match species {
        StAugustine => "st_augustine",
        Bermuda => "bermuda",
        Zoysia => "zoysia",
        Bahia => "bahia",
        Centipede => "centipede",
        Kikuyu => "kikuyu",
        KentuckyBluegrass => "kentucky_bluegrass",
        TallFescue => "tall_fescue",
        PerennialRyegrass => "perennial_ryegrass",
        OrnamentalShrubs => "ornamental_shrubs",
        VegetableGarden => "vegetable_garden",
        DripXeriscape => "drip_xeriscape",
        Other => "other",
    }
}

/// Interpolate the monthly Kc curve for a given day-of-year (1..=366).
///
/// Assumes a Northern-Hemisphere calendar. Southern-Hemisphere users should call
/// `kc_at_doy_lat` instead so December reads as peak summer rather than peak
/// winter dormancy.
///
/// We treat each month's Kc as its mid-month value (e.g., Jan = day 15) and
/// linearly interpolate. Outside [Jan 15, Dec 15] wraps via Dec/Jan.
pub fn kc_at_doy(species: GrassSpecies, doy: u16) -> f64 {
    let profile = lookup(species);
    kc_at_doy_curve(&profile.kc_monthly, doy)
}

/// Hemisphere-aware Kc lookup. The seasonal Kc curves in this catalog are
/// anchored to the Northern-Hemisphere calendar (December = winter dormancy for
/// warm-season turfgrass; July = peak ET demand). For `latitude_deg < 0` we shift
/// the day-of-year by half a year so the same curve reads correctly south of the
/// equator.
///
/// At the equator (`latitude_deg == 0`) and small tropical latitudes the seasonal
/// swing in Kc is small enough that either anchor is fine; we pick Northern as the
/// default. Operators in those zones who want a flatter curve can use
/// `GrassSpecies::DripXeriscape` or override Kc with `mad_pct_override` and
/// per-zone fine-tuning.
pub fn kc_at_doy_lat(species: GrassSpecies, doy: u16, latitude_deg: f64) -> f64 {
    let profile = lookup(species);
    let effective_doy = shift_doy_for_hemisphere(doy, latitude_deg);
    kc_at_doy_curve(&profile.kc_monthly, effective_doy)
}

/// Shift DOY by ~6 months for Southern-Hemisphere latitudes. Wraps in the range
/// 1..=365. Exposed for tests + the verdict-strip projection.
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
    /// public/grass-species/<slug>.jpg. The wizard's onerror handler hides
    /// missing photos silently, so visual drift is invisible at runtime; this
    /// test catches it at build time instead.
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

    /// `species_slug` must equal each variant's serde wire name, since the
    /// agronomy catalog + the config file key off that exact slug. A renamed
    /// variant that updated serde but not this map would silently fall back to
    /// the generic "other" profile; this pins them together.
    #[test]
    fn species_slug_matches_serde() {
        let variants = [
            GrassSpecies::StAugustine,
            GrassSpecies::Bermuda,
            GrassSpecies::Zoysia,
            GrassSpecies::Bahia,
            GrassSpecies::Centipede,
            GrassSpecies::Kikuyu,
            GrassSpecies::KentuckyBluegrass,
            GrassSpecies::TallFescue,
            GrassSpecies::PerennialRyegrass,
            GrassSpecies::OrnamentalShrubs,
            GrassSpecies::VegetableGarden,
            GrassSpecies::DripXeriscape,
            GrassSpecies::Other,
        ];
        for v in variants {
            let serde_slug = serde_json::to_value(v).unwrap();
            assert_eq!(
                serde_json::Value::String(species_slug(v).to_string()),
                serde_slug,
                "species_slug drifted from serde for {v:?}"
            );
        }
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
        // (within interpolation noise), since the DOY shift of 182 lines up the
        // two anchors.
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
