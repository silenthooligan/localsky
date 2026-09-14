// Cycle-and-soak runtime splitter. When the sprinkler precipitation rate
// exceeds the soil's infiltration capacity, applying the full computed
// runtime in one pass causes runoff. The fix is to split the runtime
// into N shorter cycles separated by soak gaps that let water move into
// the soil before the next pass.
//
// References:
//   * USDA NRCS National Irrigation Guide, Part 652, ch. 11.
//   * Snyder, R.L. et al., "Irrigation Scheduling Tools" UC ANR.
//
// Algorithm:
//   1. If precip_rate <= infiltration_rate, no splitting needed.
//   2. Otherwise, compute max_cycle_minutes such that each cycle applies
//      no more depth than the soil can absorb in that time. Roughly:
//      max_cycle_minutes = (infiltration / precip) * 60.
//   3. Divide the total runtime into ceil(total / max_cycle) equal
//      segments, each rounded up to the controller's quantum.
//   4. After every segment but the last, soak for as long as the water
//      that cycle left standing takes to infiltrate, never less than the
//      texture's floor or the operator's minimum.
//
// The soak used to be one global number, 30 minutes by default, inserted
// between every cycle regardless of texture or of how much water was
// actually standing. That is the right figure for clay under a spray
// head and roughly six times too long for loamy sand under a rotor, and
// the difference is the whole pre-dawn window on a large yard. The soak
// is physics: ponded depth over infiltration rate. It is derived here
// and the operator's number is a floor on it.
//
// The splitter returns a Vec<CycleSegment> the controller adapter runs
// back-to-back (with the controller's own scheduler observing soak gaps).

use crate::config::schema::SoilTexture;
use crate::engine::soil_catalog::infiltration_mm_hr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleSegment {
    pub run_seconds: u32,
    /// Soak gap immediately after this segment. 0 for the last segment.
    pub soak_seconds: u32,
}

/// Split total_seconds into infiltration-respecting cycles.
///
/// `slope_pct` selects between flat / moderate / steep infiltration rates
/// from the soil catalog. `min_soak_minutes` is the operator's floor on
/// the derived soak. `quantum_s` is the smallest run the controller can
/// be told (60 for the cloud adapters that take whole minutes); every
/// segment is rounded up to it so the plan says what will actually run.
///
/// Because of that rounding the segments can sum to MORE than
/// `total_seconds` on a quantized controller. That is the truth of the
/// morning, not an error: a controller that runs whole minutes will run
/// the extra seconds whether or not the plan admits it.
pub fn split(
    total_seconds: u32,
    precip_rate_mm_hr: f64,
    soil: SoilTexture,
    slope_pct: f64,
    min_soak_minutes: u32,
    quantum_s: u32,
) -> Vec<CycleSegment> {
    if total_seconds == 0 || precip_rate_mm_hr <= 0.01 {
        return Vec::new();
    }

    let infiltration = infiltration_mm_hr(soil, slope_pct).max(0.5);

    if precip_rate_mm_hr <= infiltration {
        // Soil absorbs as fast as we apply; no splitting required.
        return vec![CycleSegment {
            run_seconds: round_up(total_seconds, quantum_s),
            soak_seconds: 0,
        }];
    }

    // Max cycle minutes that won't overrun infiltration. Clamp so a
    // pathological infiltration of 0 doesn't drive cycles to 1 second.
    let max_cycle_minutes = ((infiltration / precip_rate_mm_hr) * 60.0).max(3.0);
    let max_cycle_seconds = (max_cycle_minutes * 60.0).round() as u32;

    if total_seconds <= max_cycle_seconds {
        return vec![CycleSegment {
            run_seconds: round_up(total_seconds, quantum_s),
            soak_seconds: 0,
        }];
    }

    let cycle_count = ((total_seconds as f64) / (max_cycle_seconds as f64)).ceil() as u32;
    let per_cycle = total_seconds / cycle_count;
    let remainder = total_seconds - per_cycle * cycle_count;
    let floor_s = soak_floor_s(soil).max(min_soak_minutes.saturating_mul(60));

    let mut out = Vec::with_capacity(cycle_count as usize);
    for i in 0..cycle_count {
        // Distribute the remainder across the first few cycles so the
        // sum equals total_seconds before quantization.
        let run = round_up(
            if i < remainder {
                per_cycle + 1
            } else {
                per_cycle
            },
            quantum_s,
        );
        let soak = if i + 1 < cycle_count {
            soak_to_drain_s(run, precip_rate_mm_hr, infiltration).max(floor_s)
        } else {
            0
        };
        out.push(CycleSegment {
            run_seconds: run,
            soak_seconds: soak,
        });
    }
    out
}

/// How long the water a cycle leaves standing takes to soak in.
///
/// A cycle of `run_s` at `precip_rate_mm_hr` onto soil taking
/// `infiltration_mm_hr` leaves `(precip - infiltration) * run` standing
/// at the end, and that depth drains at the infiltration rate. Zero when
/// the soil keeps up with the head.
pub fn soak_to_drain_s(run_s: u32, precip_rate_mm_hr: f64, infiltration_mm_hr: f64) -> u32 {
    if precip_rate_mm_hr <= infiltration_mm_hr || infiltration_mm_hr <= 0.0 {
        return 0;
    }
    let ponded_mm = (precip_rate_mm_hr - infiltration_mm_hr) * run_s as f64 / 3600.0;
    (ponded_mm / infiltration_mm_hr * 3600.0).ceil() as u32
}

/// The shortest soak worth inserting for a texture, in seconds.
///
/// The drain arithmetic assumes a steady basic intake rate. Real
/// surfaces start faster and slow down, water redistributes sideways
/// under the canopy, and a head's pattern is never uniform, so a soak
/// computed at a few seconds is not one the soil would recognize. The
/// floor follows the intake family: coarse soils recover in minutes,
/// fine ones do not.
pub fn soak_floor_s(soil: SoilTexture) -> u32 {
    use SoilTexture::*;
    let minutes = match soil {
        Sand | LoamySand => 5,
        SandyLoam | Loam | SiltLoam => 10,
        ClayLoam | Clay => 15,
    };
    minutes * 60
}

/// Round a run up to what the controller can actually be told.
///
/// B-hyve and Rain Bird take whole minutes and round up themselves. The
/// plan has to say the same number they will run, or the executor waits
/// for a valve that is still open.
pub fn round_up(seconds: u32, quantum_s: u32) -> u32 {
    let q = quantum_s.max(1);
    seconds.div_ceil(q).saturating_mul(q)
}

/// Total elapsed wall-clock time (seconds) for a cycle plan, including soaks.
pub fn total_elapsed_seconds(segments: &[CycleSegment]) -> u64 {
    segments
        .iter()
        .map(|s| s.run_seconds as u64 + s.soak_seconds as u64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_split_when_precip_under_infiltration() {
        // Sandy loam flat: 25 mm/hr infiltration. Drip at 4 mm/hr.
        let plan = split(1800, 4.0, SoilTexture::SandyLoam, 0.0, 5, 1);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].run_seconds, 1800);
        assert_eq!(plan[0].soak_seconds, 0);
    }

    #[test]
    fn no_split_when_runtime_fits_one_cycle() {
        // Clay flat: 5 mm/hr infiltration. Spray at 15 mm/hr -> max cycle
        // ~20 min. 10 min runtime fits in one cycle.
        let plan = split(600, 15.0, SoilTexture::Clay, 0.0, 5, 1);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].run_seconds, 600);
    }

    #[test]
    fn split_clay_high_precip_spray() {
        // Clay flat: 5 mm/hr. Spray at 15 mm/hr -> max cycle = 5/15*60=20 min.
        // 45 min runtime -> 3 cycles of 15 min. Each cycle puts down
        // 3.75 mm and the clay takes 1.25 mm of it while the head runs,
        // so 2.5 mm is standing at the end, which at 5 mm/hr is a 30
        // minute soak. The old fixed default was this one case's answer
        // applied to every soil; here it is derived, with the operator
        // minimum set low enough not to bind.
        let plan = split(45 * 60, 15.0, SoilTexture::Clay, 0.0, 5, 1);
        assert_eq!(plan.len(), 3);
        // Sum of runs == total.
        let sum: u32 = plan.iter().map(|s| s.run_seconds).sum();
        assert_eq!(sum, 45 * 60);
        // First two have soak, last has 0.
        assert_eq!(plan[0].soak_seconds, 30 * 60);
        assert_eq!(plan[1].soak_seconds, 30 * 60);
        assert_eq!(plan[2].soak_seconds, 0);
    }

    /// The property the derivation exists for: at the end of every soak
    /// there is no water standing, for every texture the catalog knows
    /// under every head it knows on every slope band.
    #[test]
    fn ponded_depth_is_zero_at_the_end_of_every_soak() {
        use SoilTexture::*;
        let textures = [Sand, LoamySand, SandyLoam, Loam, SiltLoam, ClayLoam, Clay];
        let heads = ["rotor", "spray", "mp_rotator", "drip", "bubbler", "other"];
        for soil in textures {
            for head in heads {
                let precip = crate::agronomy::sprinkler_precip_mm_hr(head);
                for slope in [0.0, 4.0, 8.0] {
                    let infil = infiltration_mm_hr(soil, slope).max(0.5);
                    let plan = split(3600, precip, soil, slope, 5, 1);
                    let sum: u32 = plan.iter().map(|s| s.run_seconds).sum();
                    assert_eq!(
                        sum, 3600,
                        "{soil:?}/{head}/{slope}: runs must sum to the total"
                    );
                    for (i, seg) in plan.iter().enumerate() {
                        if seg.soak_seconds == 0 {
                            continue;
                        }
                        let ponded = (precip - infil) * seg.run_seconds as f64 / 3600.0;
                        let drained = infil * seg.soak_seconds as f64 / 3600.0;
                        assert!(
                            ponded - drained <= 1e-6,
                            "{soil:?}/{head}/{slope} segment {i}: {ponded:.2} mm standing, \
                             {drained:.2} mm drained in the soak"
                        );
                    }
                }
            }
        }
    }

    /// Sand under a bubbler soaks for minutes; clay under a spray head
    /// soaks for most of an hour. One number could not be right for both.
    #[test]
    fn sand_soaks_briefly_and_clay_soaks_long() {
        let sand = split(3600, 50.0, SoilTexture::LoamySand, 0.0, 5, 1);
        let clay = split(3600, 38.0, SoilTexture::Clay, 0.0, 5, 1);
        let sand_soak = sand.iter().map(|s| s.soak_seconds).max().unwrap();
        let clay_soak = clay.iter().map(|s| s.soak_seconds).max().unwrap();
        assert!(
            sand_soak > 0 && clay_soak > sand_soak * 3,
            "sand {sand_soak}s, clay {clay_soak}s"
        );
    }

    /// The operator's minimum raises a soak; it never lowers one.
    #[test]
    fn the_operator_minimum_is_a_floor() {
        let derived = split(45 * 60, 15.0, SoilTexture::Clay, 0.0, 5, 1);
        let raised = split(45 * 60, 15.0, SoilTexture::Clay, 0.0, 45, 1);
        assert_eq!(derived[0].soak_seconds, 30 * 60);
        assert_eq!(raised[0].soak_seconds, 45 * 60);
    }

    /// A controller that runs whole minutes gets whole minutes, and the
    /// plan admits it: 200 s becomes 240 s, and an odd split rounds
    /// every segment up rather than the total.
    #[test]
    fn a_quantized_controller_gets_whole_minutes() {
        let one = split(200, 4.0, SoilTexture::SandyLoam, 0.0, 5, 60);
        assert_eq!(
            one,
            vec![CycleSegment {
                run_seconds: 240,
                soak_seconds: 0
            }]
        );
        // 47 min on clay under spray: three cycles of 940 s become 960 s.
        let plan = split(47 * 60, 15.0, SoilTexture::Clay, 0.0, 5, 60);
        assert!(plan.iter().all(|s| s.run_seconds % 60 == 0), "{plan:?}");
        assert!(plan.iter().all(|s| s.run_seconds == 960), "{plan:?}");
        // And the soak drains what the ROUNDED run leaves standing.
        let infil = infiltration_mm_hr(SoilTexture::Clay, 0.0);
        assert_eq!(plan[0].soak_seconds, soak_to_drain_s(960, 15.0, infil));
    }

    #[test]
    fn split_remainder_distributed() {
        // Force a non-divisible split: 47 min with max cycle 20 min.
        let plan = split(47 * 60, 15.0, SoilTexture::Clay, 0.0, 5, 1);
        let sum: u32 = plan.iter().map(|s| s.run_seconds).sum();
        assert_eq!(sum, 47 * 60, "split must preserve total runtime");
    }

    #[test]
    fn steep_slope_reduces_cycle_length() {
        // Same soil, steeper slope -> shorter cycles -> more segments.
        let flat = split(60 * 60, 15.0, SoilTexture::SandyLoam, 0.0, 5, 1);
        let steep = split(60 * 60, 15.0, SoilTexture::SandyLoam, 8.0, 5, 1);
        assert!(steep.len() >= flat.len(), "steep should not split fewer");
    }

    #[test]
    fn elapsed_includes_soaks() {
        let plan = vec![
            CycleSegment {
                run_seconds: 900,
                soak_seconds: 1800,
            },
            CycleSegment {
                run_seconds: 900,
                soak_seconds: 0,
            },
        ];
        assert_eq!(total_elapsed_seconds(&plan), 900 + 1800 + 900);
    }

    #[test]
    fn empty_for_zero_runtime() {
        let plan = split(0, 15.0, SoilTexture::Clay, 0.0, 5, 1);
        assert!(plan.is_empty());
    }
}
