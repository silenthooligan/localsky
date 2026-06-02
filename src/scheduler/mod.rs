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

#[cfg(feature = "ssr")]
pub mod manual;

#[cfg(feature = "ssr")]
pub mod smart_morning;
