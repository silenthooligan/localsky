// Soil texture catalog. Lookup tables for water-holding capacity and
// infiltration rate keyed by USDA SoilTexture.
//
// Citations:
//   * FAO Irrigation and Drainage Paper No. 56, Table 19 (Allen et al., 1998)
//   * USDA NRCS Part 652 National Irrigation Guide, Table 11-3
//
// Field capacity (FC) and wilting point (WP) are reported as volumetric
// water content (m³ water / m³ soil). Available water (AW) per metre of
// soil depth equals (FC - WP) * 1000 mm. The engine computes TAW (total
// available water in the root zone) as AW * root_depth_mm / 1000.

use crate::config::schema::SoilTexture;

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

pub fn lookup(texture: SoilTexture) -> SoilProfile {
    use SoilTexture::*;
    match texture {
        Sand => SoilProfile {
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
        LoamySand => SoilProfile {
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
        SandyLoam => SoilProfile {
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
        Loam => SoilProfile {
            field_capacity: 0.34,
            wilting_point: 0.12,
            aw_mm_per_m: 220.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 13.0,
                moderate_slope: 10.0,
                steep_slope: 7.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        SiltLoam => SoilProfile {
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
        ClayLoam => SoilProfile {
            field_capacity: 0.39,
            wilting_point: 0.20,
            aw_mm_per_m: 190.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 8.0,
                moderate_slope: 6.0,
                steep_slope: 4.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
        Clay => SoilProfile {
            field_capacity: 0.42,
            wilting_point: 0.25,
            aw_mm_per_m: 170.0,
            infiltration_mm_hr: InfiltrationRates {
                flat: 5.0,
                moderate_slope: 4.0,
                steep_slope: 3.0,
            },
            citation: "FAO-56 Table 19; USDA NRCS Part 652 Table 11-3",
        },
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
