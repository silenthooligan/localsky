// Soil texture catalog. Lookup tables for water-holding capacity and
// infiltration rate keyed by USDA SoilTexture.
//
// Citations:
//   * Water holding: FAO Irrigation and Drainage Paper No. 56, Table 19
//     "Typical soil water characteristics for different soil types"
//     (Allen et al., 1998).
//   * Infiltration: USDA NRCS Part 652 National Irrigation Guide,
//     Table 11-3.
//
// Field capacity (FC) and wilting point (WP) are reported as volumetric
// water content (m³ water / m³ soil). Available water (AW) per metre of
// soil depth equals (FC - WP) * 1000 mm. The engine computes TAW (total
// available water in the root zone) as AW * root_depth_mm / 1000.
//
// The DATA lives in the shared, slug-keyed `crate::agronomy` catalog
// (plain data, no ssr-only deps), so the zone editor can show the same
// derived rain cap the engine clips at without an ssr round-trip. This
// module keeps the SoilTexture-keyed API the engine depends on, the TAW
// and RAW arithmetic, and the FAO-56 band test that guards the numbers.

use crate::config::schema::SoilTexture;

pub use crate::agronomy::{InfiltrationRates, SoilProfile};

/// Water-holding + infiltration profile for a texture. Delegates to the
/// shared, slug-keyed `agronomy` catalog, the single source of truth the
/// wasm zone editor reads too, so the cap the editor shows and the cap
/// the balance clips at can never drift apart. `soil_slug` maps the enum
/// to its serde slug and is pinned to serde by `soil_slug_matches_serde`.
pub fn lookup(texture: SoilTexture) -> SoilProfile {
    crate::agronomy::soil_profile_by_slug(soil_slug(texture))
}

/// Enum -> snake_case slug used by the agronomy catalog + the config wire
/// format. Kept in lockstep with serde by a test.
pub fn soil_slug(texture: SoilTexture) -> &'static str {
    use SoilTexture::*;
    match texture {
        Sand => "sand",
        LoamySand => "loamy_sand",
        SandyLoam => "sandy_loam",
        Loam => "loam",
        SiltLoam => "silt_loam",
        ClayLoam => "clay_loam",
        Clay => "clay",
    }
}

/// Total Available Water (mm) in the root zone. = (FC - WP) * root_depth.
pub fn taw_mm(texture: SoilTexture, root_depth_mm: f64) -> f64 {
    let p = lookup(texture);
    (p.field_capacity - p.wilting_point) * root_depth_mm
}

/// Readily Available Water (mm). = TAW * MAD. Above this depletion the
/// crop starts to suffer water stress.
pub fn raw_mm(texture: SoilTexture, root_depth_mm: f64, mad_pct: f64) -> f64 {
    taw_mm(texture, root_depth_mm) * mad_pct.clamp(0.0, 1.0)
}

/// Infiltration rate (mm/hr) for the texture + slope. Used by the
/// cycle-and-soak splitter to keep applied water from running off.
pub fn infiltration_mm_hr(texture: SoilTexture, slope_pct: f64) -> f64 {
    let p = lookup(texture).infiltration_mm_hr;
    if slope_pct <= 3.0 {
        p.flat
    } else if slope_pct <= 5.0 {
        p.moderate_slope
    } else {
        p.steep_slope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog cites FAO-56 Table 19, so every entry has to sit
    /// inside it. These bands are the table verbatim (Allen et al.,
    /// 1998); Table 19 has no clay-loam row, so ClayLoam is held against
    /// its "silt clay loam", the nearest published analogue. The three
    /// figures are checked separately because the table publishes
    /// (FC - WP) as its own range rather than as the difference of the
    /// other two. A texture that drifts outside its band changes how
    /// much storm rain a real yard banks and how big its bucket is, so
    /// it fails here rather than shipping.
    #[test]
    fn catalog_entries_sit_inside_the_fao56_bands() {
        // texture, FC lo/hi, WP lo/hi, (FC-WP) lo/hi
        let bands = [
            (SoilTexture::Sand, 0.07, 0.17, 0.02, 0.07, 0.05, 0.11),
            (SoilTexture::LoamySand, 0.11, 0.19, 0.03, 0.10, 0.06, 0.12),
            (SoilTexture::SandyLoam, 0.18, 0.28, 0.06, 0.16, 0.11, 0.15),
            (SoilTexture::Loam, 0.20, 0.30, 0.07, 0.17, 0.13, 0.18),
            (SoilTexture::SiltLoam, 0.22, 0.36, 0.09, 0.21, 0.13, 0.19),
            (SoilTexture::ClayLoam, 0.30, 0.37, 0.17, 0.24, 0.13, 0.18),
            (SoilTexture::Clay, 0.32, 0.40, 0.20, 0.24, 0.12, 0.20),
        ];
        for (t, fc_lo, fc_hi, wp_lo, wp_hi, aw_lo, aw_hi) in bands {
            let p = lookup(t);
            assert!(
                p.field_capacity >= fc_lo && p.field_capacity <= fc_hi,
                "{t:?} field capacity {} outside FAO-56 {fc_lo}..{fc_hi}",
                p.field_capacity
            );
            assert!(
                p.wilting_point >= wp_lo && p.wilting_point <= wp_hi,
                "{t:?} wilting point {} outside FAO-56 {wp_lo}..{wp_hi}",
                p.wilting_point
            );
            let aw = p.field_capacity - p.wilting_point;
            assert!(
                aw >= aw_lo - 1e-9 && aw <= aw_hi + 1e-9,
                "{t:?} available water {aw} outside FAO-56 {aw_lo}..{aw_hi}"
            );
            // The published mm/m field has to agree with the two it is
            // derived from, or TAW and the displayed cap disagree.
            assert!(
                (p.aw_mm_per_m - aw * 1000.0).abs() < 1e-6,
                "{t:?} aw_mm_per_m {} does not match (FC - WP) * 1000 = {}",
                p.aw_mm_per_m,
                aw * 1000.0
            );
        }
    }

    /// Water held rises with fineness up to the silt loams and eases off
    /// on the heavy clays, the ordering every published table carries. A
    /// catalog that put loam above silt loam once gave silt-loam yards a
    /// tighter rain cap than loam, which is backwards.
    #[test]
    fn water_held_follows_the_published_texture_ordering() {
        let aw = |t| lookup(t).aw_mm_per_m;
        assert!(aw(SoilTexture::Sand) < aw(SoilTexture::LoamySand));
        assert!(aw(SoilTexture::LoamySand) < aw(SoilTexture::SandyLoam));
        assert!(aw(SoilTexture::SandyLoam) < aw(SoilTexture::Loam));
        assert!(aw(SoilTexture::Loam) <= aw(SoilTexture::SiltLoam));
        assert!(aw(SoilTexture::ClayLoam) <= aw(SoilTexture::SiltLoam));
        assert!(aw(SoilTexture::Clay) <= aw(SoilTexture::SiltLoam));
    }

    /// The slug this module hands the shared catalog has to be the slug
    /// serde writes into the config, or a texture would silently read
    /// another texture's profile (or the sandy-loam fallback).
    /// The slug conversion round-trips, both directions. A form holds its
    /// texture picker as a string and hands `from_slug` to the engine, so
    /// a mismatch here would silently water a zone on another texture's
    /// numbers.
    #[test]
    fn soil_texture_slug_round_trips() {
        for t in [
            SoilTexture::Sand,
            SoilTexture::LoamySand,
            SoilTexture::SandyLoam,
            SoilTexture::Loam,
            SoilTexture::SiltLoam,
            SoilTexture::ClayLoam,
            SoilTexture::Clay,
        ] {
            assert_eq!(SoilTexture::from_slug(soil_slug(t)), t, "{t:?}");
        }
        // An unknown slug takes the form's own load default.
        assert_eq!(
            SoilTexture::from_slug("mystery"),
            SoilTexture::SandyLoam,
            "unknown slugs fall back to sandy loam"
        );
    }

    #[test]
    fn soil_slug_matches_serde() {
        for t in [
            SoilTexture::Sand,
            SoilTexture::LoamySand,
            SoilTexture::SandyLoam,
            SoilTexture::Loam,
            SoilTexture::SiltLoam,
            SoilTexture::ClayLoam,
            SoilTexture::Clay,
        ] {
            let serde_slug = serde_json::to_string(&t).unwrap();
            let serde_slug = serde_slug.trim_matches('"');
            assert_eq!(soil_slug(t), serde_slug, "{t:?}");
        }
    }

    #[test]
    fn sandy_loam_holds_more_than_sand() {
        let sand = lookup(SoilTexture::Sand).aw_mm_per_m;
        let sandy_loam = lookup(SoilTexture::SandyLoam).aw_mm_per_m;
        assert!(sandy_loam > sand);
    }

    #[test]
    fn taw_scales_with_root_depth() {
        let shallow = taw_mm(SoilTexture::SandyLoam, 100.0);
        let deep = taw_mm(SoilTexture::SandyLoam, 300.0);
        assert!((deep / shallow - 3.0).abs() < 0.01);
    }

    #[test]
    fn raw_is_mad_fraction_of_taw() {
        let taw = taw_mm(SoilTexture::Loam, 200.0);
        let raw = raw_mm(SoilTexture::Loam, 200.0, 0.5);
        assert!((raw - taw * 0.5).abs() < 0.001);
    }

    #[test]
    fn clay_infiltration_lowest_steep_slope_lower_still() {
        let flat = infiltration_mm_hr(SoilTexture::Clay, 0.0);
        let mid = infiltration_mm_hr(SoilTexture::Clay, 4.0);
        let steep = infiltration_mm_hr(SoilTexture::Clay, 8.0);
        assert!(flat > mid && mid > steep);
        assert!((flat - 5.0).abs() < 0.01);
        assert!((steep - 3.0).abs() < 0.01);
    }

    #[test]
    fn sand_infiltrates_fastest() {
        let sand = infiltration_mm_hr(SoilTexture::Sand, 0.0);
        let clay = infiltration_mm_hr(SoilTexture::Clay, 0.0);
        assert!(sand > clay * 5.0);
    }
}
