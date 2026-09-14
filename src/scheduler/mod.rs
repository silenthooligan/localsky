// Background task orchestrator. Phase 5+ collapses the ad-hoc tokio task
// spawning in src/main.rs into a single Scheduler actor that owns:
//   - All source pollers (cadence per source.capabilities)
//   - Engine tick (60s default, computes verdict + budget + soil projection)
//   - Controller status polling (10s default)
//   - Daily ET integration at sunset
//   - Daily verdict commit at 23:30 local
//   - Boot-time controller history backfill
//
// Shutdown via watch<bool>; every adapter task drops within 5s or is aborted.

/// Maximum age of an unattended watering decision. The refresher normally
/// ticks every ten seconds, with backoff capped at three minutes.
pub(crate) const MAX_SNAPSHOT_AGE_S: i64 = 30 * 60;

/// A snapshot must have been refreshed, and its age must be known and within
/// the dispatch limit. A future timestamp after a clock correction is unknown.
pub(crate) fn snapshot_is_fresh(last_refresh_epoch: i64, now_epoch: i64) -> bool {
    last_refresh_epoch > 0
        && now_epoch
            .checked_sub(last_refresh_epoch)
            .is_some_and(|age| (0..MAX_SNAPSHOT_AGE_S).contains(&age))
}

#[cfg(test)]
mod freshness_tests {
    use super::*;

    #[test]
    fn both_schedulers_require_a_recent_past_snapshot() {
        let now = 1_700_000_000;
        for refreshed in [0, -1, now + 1, now - MAX_SNAPSHOT_AGE_S, i64::MAX] {
            assert!(!snapshot_is_fresh(refreshed, now), "{refreshed}");
        }
        assert!(snapshot_is_fresh(now, now));
        assert!(snapshot_is_fresh(now - MAX_SNAPSHOT_AGE_S + 1, now));
        assert!(!snapshot_is_fresh(i64::MAX, i64::MIN));
    }
}

#[cfg(feature = "ssr")]
pub mod backup;

#[cfg(feature = "ssr")]
pub mod dispatch_gate;

#[cfg(feature = "ssr")]
pub mod manual;

#[cfg(feature = "ssr")]
pub mod smart_morning;

#[cfg(feature = "ssr")]
pub mod tuning_report;
