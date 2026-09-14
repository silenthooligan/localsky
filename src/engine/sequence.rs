// How long a morning takes, and how each zone's run is broken up.
//
// This lived in the dispatcher, and the refresher reached into the
// dispatcher to ask it: once to decide when the sequence has to start,
// once to price the soil model's admission window, and once more from
// the demo seeder. The same shape as the sizing arithmetic, one level
// out. Sequence planning is irrigation logic, so it belongs with the
// rest of it, and the dispatcher becomes a caller like everyone else.
//
// Getting the length wrong has a direct cost. The legacy estimate summed
// only run seconds, so a cycle-and-soak morning overshot its finish
// target by the whole soak time, which on a yard that waters before
// dawn means finishing after sunrise, in the wind and the sun, which is
// the thing the pre-dawn window exists to avoid.

use std::collections::HashMap;

use crate::engine::agronomy_cfg::ZoneAgronomyCfg;
use crate::engine::{cycle_soak, effective_precip_rate_mm_hr, interleave, ZoneSlug};
use crate::model::ZoneState;

/// Dead time between zones: the valve close, the controller's own
/// settling, and the next open.
///
/// Small, but it accumulates across a fourteen zone yard and the window
/// it eats is the window before sunrise.
pub const INTER_ZONE_PREAMBLE_S: u64 = 2;

/// Break one zone's run into cycle-and-soak segments.
///
/// Falls back to a single unsplit run when the zone does not resolve in
/// the agronomy map, which happens in demo mode, on an unconfigured
/// install, and mid-cutover. A fallback that waters once is safer than
/// one that refuses to water.
pub fn cycle_plan(
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    slug: &str,
    duration_s: u32,
    soak_minutes: u32,
    quantum_s: u32,
) -> Vec<cycle_soak::CycleSegment> {
    // Even the unsplit fallback is rounded to the controller's quantum,
    // because the controller will round it whether or not we admit it.
    let fallback = vec![cycle_soak::CycleSegment {
        run_seconds: cycle_soak::round_up(duration_s, quantum_s),
        soak_seconds: 0,
    }];
    // Map keys are canonical; a caller may still hand the operator's own
    // spelling, so normalize before looking up rather than trying twice.
    let key = ZoneSlug::new(slug);
    let Some(z) = agronomy.get(key.as_str()) else {
        return fallback;
    };
    let precip = effective_precip_rate_mm_hr(z.sprinkler_type, z.precip_rate_mm_hr);
    let segments = cycle_soak::split(
        duration_s,
        precip,
        z.soil_texture,
        z.slope_pct,
        soak_minutes,
        quantum_s,
    );
    if segments.is_empty() {
        fallback
    } else {
        segments
    }
}

/// True wall-clock length of a morning's sequence, in seconds.
///
/// Every due zone's cycle-and-soak plan laid out on the shared valve
/// timeline under the active policy, with soak gaps and inter-zone
/// preambles included. Both the dispatch window math and the displayed
/// next-run time read this, so the time on screen and the time the
/// valves actually take are the same number.
pub fn wall_seconds(
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    zones: &[ZoneState],
    soak_minutes: u32,
    interleave_cycles: bool,
    quantum_s: u32,
) -> u64 {
    let plans: Vec<interleave::ZonePlan> = zones
        .iter()
        .filter(|z| z.planned_run_seconds > 0)
        .enumerate()
        .map(|(zone_idx, z)| interleave::ZonePlan {
            zone_idx,
            segments: cycle_plan(
                agronomy,
                &z.slug,
                z.planned_run_seconds,
                soak_minutes,
                quantum_s,
            ),
        })
        .collect();
    let policy = if interleave_cycles {
        interleave::Policy::Interleaved
    } else {
        interleave::Policy::Serial
    };
    interleave::makespan_s(&interleave::plan(&plans, policy, INTER_ZONE_PREAMBLE_S))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(slug: &str, seconds: u32) -> ZoneState {
        ZoneState {
            slug: slug.into(),
            planned_run_seconds: seconds,
            ..Default::default()
        }
    }

    /// A zone the agronomy map does not know still waters, in one run.
    ///
    /// Demo mode, a fresh install and a mid-cutover config all land here,
    /// and refusing to water would be a worse answer than not splitting.
    #[test]
    fn an_unknown_zone_waters_once_rather_than_not_at_all() {
        let empty = HashMap::new();
        let plan = cycle_plan(&empty, "front_yard", 900, 30, 1);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].run_seconds, 900);
        assert_eq!(plan[0].soak_seconds, 0);
    }

    /// The operator's own spelling resolves, because the lookup
    /// normalizes rather than guessing twice.
    #[test]
    fn the_operators_spelling_resolves() {
        let empty = HashMap::new();
        // Both spellings reach the same (missing) entry and fall back
        // identically, which is the property that matters: neither
        // spelling silently takes a different path.
        assert_eq!(
            cycle_plan(&empty, "back-yard", 600, 30, 1),
            cycle_plan(&empty, "back_yard", 600, 30, 1)
        );
    }

    /// A zone planned for nothing takes no time.
    #[test]
    fn zones_with_no_planned_run_cost_nothing() {
        let empty = HashMap::new();
        assert_eq!(wall_seconds(&empty, &[zone("a", 0)], 30, false, 1), 0);
    }

    /// A controller that runs whole minutes runs whole minutes even on
    /// the unsplit fallback, and the morning's length says so.
    #[test]
    fn a_quantized_fallback_is_still_rounded() {
        let empty = HashMap::new();
        let plan = cycle_plan(&empty, "front_yard", 200, 5, 60);
        assert_eq!(plan[0].run_seconds, 240);
        assert_eq!(
            wall_seconds(&empty, &[zone("a", 200), zone("b", 200)], 5, false, 60),
            240 + INTER_ZONE_PREAMBLE_S + 240
        );
    }

    /// The sequence is longer than the sum of its runs, because the gaps
    /// between zones are real time.
    ///
    /// The legacy estimate summed run seconds alone, so a morning
    /// overshot its finish target by the whole soak time and ended after
    /// sunrise, which is exactly what watering before dawn avoids.
    #[test]
    fn the_sequence_counts_the_gaps_between_zones() {
        let empty = HashMap::new();
        let zones = [zone("a", 600), zone("b", 600)];
        let serial = wall_seconds(&empty, &zones, 30, false, 1);
        assert!(
            serial > 1200,
            "two 10 minute zones take longer than 20 minutes back to back: {serial}"
        );
        assert_eq!(serial, 1200 + INTER_ZONE_PREAMBLE_S);
    }
}
