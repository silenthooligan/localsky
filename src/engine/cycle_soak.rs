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
//      segments. Insert soak_minutes between each, except after the last.
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
/// from the soil catalog. `soak_minutes` defaults to 30 in EngineParams.
pub fn split(
    total_seconds: u32,
    precip_rate_mm_hr: f64,
    soil: SoilTexture,
    slope_pct: f64,
    soak_minutes: u32,
) -> Vec<CycleSegment> {
    if total_seconds == 0 || precip_rate_mm_hr <= 0.01 {
        return Vec::new();
    }

    let infiltration = infiltration_mm_hr(soil, slope_pct).max(0.5);

    if precip_rate_mm_hr <= infiltration {
        // Soil absorbs as fast as we apply; no splitting required.
        return vec![CycleSegment {
            run_seconds: total_seconds,
            soak_seconds: 0,
        }];
    }

    // Max cycle minutes that won't overrun infiltration. Clamp so a
    // pathological infiltration of 0 doesn't drive cycles to 1 second.
    let max_cycle_minutes = ((infiltration / precip_rate_mm_hr) * 60.0).max(3.0);
    let max_cycle_seconds = (max_cycle_minutes * 60.0).round() as u32;

    if total_seconds <= max_cycle_seconds {
        return vec![CycleSegment {
            run_seconds: total_seconds,
            soak_seconds: 0,
        }];
    }

    let cycle_count = ((total_seconds as f64) / (max_cycle_seconds as f64)).ceil() as u32;
    let per_cycle = total_seconds / cycle_count;
    let remainder = total_seconds - per_cycle * cycle_count;
    let soak_s = soak_minutes.saturating_mul(60);

    let mut out = Vec::with_capacity(cycle_count as usize);
    for i in 0..cycle_count {
        // Distribute the remainder across the first few cycles so the
        // sum exactly equals total_seconds.
        let run = if i < remainder {
            per_cycle + 1
        } else {
            per_cycle
        };
        let soak = if i + 1 < cycle_count { soak_s } else { 0 };
        out.push(CycleSegment {
            run_seconds: run,
            soak_seconds: soak,
        });
    }
    out
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
        let plan = split(1800, 4.0, SoilTexture::SandyLoam, 0.0, 30);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].run_seconds, 1800);
        assert_eq!(plan[0].soak_seconds, 0);
    }

    #[test]
    fn no_split_when_runtime_fits_one_cycle() {
        // Clay flat: 5 mm/hr infiltration. Spray at 15 mm/hr -> max cycle
        // ~20 min. 10 min runtime fits in one cycle.
        let plan = split(600, 15.0, SoilTexture::Clay, 0.0, 30);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].run_seconds, 600);
    }

    #[test]
    fn split_clay_high_precip_spray() {
        // Clay flat: 5 mm/hr. Spray at 15 mm/hr -> max cycle = 5/15*60=20 min.
        // 45 min runtime -> 3 cycles of 15 min with 30 min soaks between.
        let plan = split(45 * 60, 15.0, SoilTexture::Clay, 0.0, 30);
        assert_eq!(plan.len(), 3);
        // Sum of runs == total.
        let sum: u32 = plan.iter().map(|s| s.run_seconds).sum();
        assert_eq!(sum, 45 * 60);
        // First two have soak, last has 0.
        assert_eq!(plan[0].soak_seconds, 30 * 60);
        assert_eq!(plan[1].soak_seconds, 30 * 60);
        assert_eq!(plan[2].soak_seconds, 0);
    }

    #[test]
    fn split_remainder_distributed() {
        // Force a non-divisible split: 47 min with max cycle 20 min.
        let plan = split(47 * 60, 15.0, SoilTexture::Clay, 0.0, 30);
        let sum: u32 = plan.iter().map(|s| s.run_seconds).sum();
        assert_eq!(sum, 47 * 60, "split must preserve total runtime");
    }

    #[test]
    fn steep_slope_reduces_cycle_length() {
        // Same soil, steeper slope -> shorter cycles -> more segments.
        let flat = split(60 * 60, 15.0, SoilTexture::SandyLoam, 0.0, 30);
        let steep = split(60 * 60, 15.0, SoilTexture::SandyLoam, 8.0, 30);
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
        let plan = split(0, 15.0, SoilTexture::Clay, 0.0, 30);
        assert!(plan.is_empty());
    }
}
