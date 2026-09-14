// How long a zone actually runs, once the engine has decided it should.
//
// This arithmetic lived in the refresher, and the tuning report reached
// across into it to predict what a morning would dispatch. That is the
// recurring shape: irrigation logic outside src/engine, with a second
// caller re-deriving it. The two agreed only because one imported the
// other, and the scorecard's numbers are only trustworthy while they do.
//
// The order matters and is easy to get wrong in a re-implementation.
// The seasonal dial scales the budget FIRST and the per-zone ceiling
// clamps SECOND, because a dial above 100% can push an already-capped
// budget back over the ceiling. That ceiling also folds in any
// jurisdictional duration cap, so getting the order backwards is not
// merely untidy, it can dispatch past a legal limit.

/// Light run a deliberate force falls back to when the computed budget
/// came out zero because the soil is already satisfied.
///
/// Without it, a Force on a wet yard flips the verdict to run and
/// dispatches nothing, because the scheduler skips zones planned for
/// zero seconds. A force that silently does nothing is worse than a
/// force that is refused.
pub const FORCE_RUN_DEFAULT_S: u32 = 300;

/// The seasonal dial as a multiplier.
///
/// Zero means "not set" rather than "water nothing", and the range is
/// clamped so a mistyped dial cannot triple a run or zero a yard.
pub fn seasonal_multiplier(pct: u32) -> f64 {
    if pct == 0 {
        1.0
    } else {
        (pct as f64 / 100.0).clamp(0.5, 1.5)
    }
}

/// Apply the seasonal dial, then re-clamp to the per-zone ceiling.
///
/// `max_dur == 0` means no known ceiling and leaves the value alone. It
/// never zeroes a run: disabling a zone goes through the verdict ladder,
/// not through a zero-length cap, so the two readings of zero cannot
/// collide in practice.
pub fn seasonal_capped(raw_seconds: u32, seasonal_pct: u32, max_dur: u32) -> u32 {
    let scaled = (raw_seconds as f64 * seasonal_multiplier(seasonal_pct)).round() as u32;
    if max_dur > 0 {
        scaled.min(max_dur)
    } else {
        scaled
    }
}

/// True when the ceiling, rather than the budget, decided the length.
///
/// Mirrors `seasonal_capped` exactly, and lives beside it so the two
/// cannot drift into disagreeing about whether the cap bound.
pub fn seasonal_cap_binds(raw_seconds: u32, seasonal_pct: u32, max_dur: u32) -> bool {
    max_dur > 0
        && ((raw_seconds as f64 * seasonal_multiplier(seasonal_pct)).round() as u32) > max_dur
}

/// Give a forced run a bounded length when the budget came out zero.
///
/// Decouples the forced-run VERDICT from its DURATION. A zone that is
/// zero because nothing asked it to run stays zero; only a deliberate
/// force gets the floor.
pub fn force_run_floor(
    zone_override: &str,
    global_override: &str,
    computed: u32,
    max_dur: u32,
) -> u32 {
    if computed > 0 {
        return computed;
    }
    let forced = zone_override == "run" || (zone_override == "auto" && global_override == "run");
    if !forced {
        return computed;
    }
    if max_dur > 0 {
        FORCE_RUN_DEFAULT_S.min(max_dur)
    } else {
        FORCE_RUN_DEFAULT_S
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_dial_changes_nothing() {
        assert_eq!(seasonal_multiplier(0), 1.0);
        assert_eq!(seasonal_capped(600, 0, 0), 600);
    }

    /// A mistyped dial cannot triple a run or dry out a yard.
    #[test]
    fn the_dial_is_clamped_at_both_ends() {
        assert_eq!(seasonal_multiplier(10), 0.5);
        assert_eq!(seasonal_multiplier(900), 1.5);
    }

    /// The ceiling clamps AFTER the dial scales.
    ///
    /// Backwards, a dial above 100% pushes an already-capped budget over
    /// the ceiling, and that ceiling folds in any jurisdictional duration
    /// cap. This is the ordering the tuning report used to re-derive by
    /// reaching into the refresher.
    #[test]
    fn the_ceiling_clamps_after_the_dial_scales() {
        // 600s at 150% is 900s, clamped to an 800s ceiling.
        assert_eq!(seasonal_capped(600, 150, 800), 800);
        assert!(seasonal_cap_binds(600, 150, 800));
        // Scale first then clamp: had the clamp come first, 600 would
        // have stayed 600 and then scaled to 900, over the ceiling.
        assert_eq!(seasonal_capped(600, 150, 0), 900);
    }

    #[test]
    fn the_cap_predicate_agrees_with_the_arithmetic() {
        for raw in [0u32, 100, 600, 3600] {
            for pct in [0u32, 50, 100, 150] {
                for cap in [0u32, 300, 900] {
                    let out = seasonal_capped(raw, pct, cap);
                    let binds = seasonal_cap_binds(raw, pct, cap);
                    assert_eq!(
                        binds,
                        cap > 0
                            && out == cap
                            && out < (raw as f64 * seasonal_multiplier(pct)).round() as u32,
                        "raw {raw} pct {pct} cap {cap}"
                    );
                }
            }
        }
    }

    /// A Force on an already-satisfied yard waters something, or the
    /// button is a lie.
    #[test]
    fn a_forced_zone_with_no_budget_still_waters() {
        assert_eq!(force_run_floor("run", "auto", 0, 0), FORCE_RUN_DEFAULT_S);
        assert_eq!(force_run_floor("auto", "run", 0, 0), FORCE_RUN_DEFAULT_S);
        // Bounded by the zone's own ceiling.
        assert_eq!(force_run_floor("run", "auto", 0, 120), 120);
    }

    /// A zone that is zero because nothing asked it to run stays zero.
    #[test]
    fn an_unforced_zone_is_left_alone() {
        assert_eq!(force_run_floor("auto", "auto", 0, 600), 0);
        assert_eq!(force_run_floor("skip", "run", 0, 600), 0);
        // And a real budget is never overwritten by the floor.
        assert_eq!(force_run_floor("run", "auto", 900, 0), 900);
    }
}
