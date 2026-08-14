// Smart morning dispatcher. The LocalSky-native replacement for
// Irrigation Unlimited's nightly sequence. Spawned from main.rs
// alongside the manual scheduler.
//
// Algorithm per tick (every 60s):
//   1. Compute today's local sunrise from (lat, lon) using NOAA's
//      analytical formula (no extra crates needed).
//   2. Snapshot the current IrrigationSnapshot. Lay each due zone's
//      cycle-and-soak plan on the shared valve timeline
//      (engine::interleave, serial unless engine.interleave_cycles) to
//      get the sequence's true wall time, soaks included. Inter-zone
//      preamble is a fixed 2s, matching the IU controller's
//      `preamble: "00:00:02"`.
//   3. target_finish = sunrise - 15min (matches IU's `anchor: finish,
//      sun: sunrise, before: 00:15`). target_start = target_finish -
//      sequence wall time.
//   4. If `now` is within the ±60s window around target_start, AND we
//      haven't fired today (HashMap<NaiveDate, bool> dedupe), proceed.
//      If the window was missed but `now` is still within
//      CATCH_UP_GRACE of target_finish and nothing fired today, the
//      same dispatch path runs in catch-up mode. This covers both
//      late boots and in-process stalls (clock jumps, a refresher
//      outage exactly across the window, etc.).
//   5. Freshness gate: the snapshot must have refreshed within the
//      last 30 minutes (and at least once since boot). A stale or
//      empty snapshot never waters; the tick records one "stale
//      inputs" skip row per day and retries until the grace window
//      closes.
//   6. If snapshot.skip_check.will_skip, log a skip row per zone with
//      source = "smart_morning" + the verdict reason, mark fired, return.
//   7. Otherwise iterate zones with planned_run_seconds > 0. A zone
//      whose per-zone verdict is a non-global "skip" (soil saturation,
//      custom condition) is recorded as a skip row with that reason and
//      NOT dispatched; global skips never reach here (step 6). For each
//      remaining zone:
//      split the zone's runtime via engine::cycle_soak so clay-soil
//      zones get cycle-and-soak treatment, lay the segments out via
//      engine::interleave (interleaved by default: with
//      engine.interleave_cycles, other zones' cycles run during a
//      zone's soak window, one valve at a time, soaks treated as
//      minimums), then dispatch each planned step via
//      controller.run_zone(slug, seg.run_seconds). Waits between steps
//      derive from the real dispatch clock, not the planned offsets, so
//      soak minimums hold under controller latency. The waits poll
//      scheduler::dispatch_gate so a manual Stop / Stop All / vacation
//      pause abandons the rest of the sequence promptly.
//   8. Mark fired.
//
// Catch-up: on first tick after boot, consult the runs table. Any
// non-stale source="smart_morning" row for today (completed run, skip,
// manual stop, missed-window marker) means today is already handled and
// the dedupe slot is pre-marked, so a restart inside or after the
// morning window never double-waters. Past target_finish +
// CATCH_UP_GRACE with nothing recorded, a missed-window row is logged
// per zone and the day is marked so the loop doesn't retry.
//
// LOCALSKY_SMART_DRY_RUN=1: skip the actual run_zone call; info!-log
// what would have fired. Used to validate dispatch behavior overnight
// before flipping IU off.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::config::schema::Config;
use crate::controllers::registry::ControllerRegistry;
use crate::engine::cycle_soak;
use crate::engine::interleave;
use crate::engine::sprinkler_catalog::effective_precip_rate_mm_hr;
use crate::engine::sunrise::sunrise_utc;
use crate::ha::IrrigationStore;
use crate::ha::WateringPolicy;
use crate::persistence::runs::{NewRun, RunsStore};
use crate::persistence::ActiveRunsStore;
use crate::ports::irrigation_controller::IrrigationController;
use crate::push::dispatcher::{PushDispatcher, PushEvent};
use crate::scheduler::dispatch_gate;

/// Grace added to a zone's whole-cycle deadline before the reaper enforces a
/// shutoff. Covers controller-clock skew + the reaper's own poll granularity, so
/// a valve closing right on time is never falsely "enforced".
const ACTIVE_RUN_GRACE_S: i64 = 30;

/// Width of the "we are at target_start" window, in seconds. The tick
/// interval is 60s so a 90s tolerance guarantees exactly one match per
/// day even with small clock drift.
const TARGET_WINDOW_S: i64 = 90;

/// Inter-zone preamble in seconds. Matches IU's `preamble: "00:00:02"`
/// so the dispatch cadence is observable-equivalent to the prior IU
/// sequence the OS hardware was tuned against.
const INTER_ZONE_PREAMBLE_S: u64 = 2;

/// Catch-up grace window after target_finish. If LocalSky booted late
/// (or stalled across the window) and there's still daylight between
/// the dispatch window and the SJRWMD forbidden-hour cutoff (typically
/// 10am), we can still get a useful run in. Two hours is enough to land
/// before 10am for a sunrise around 06:30 with a 1500s sequence.
const CATCH_UP_GRACE_S: i64 = 2 * 3600;

/// Maximum tolerated snapshot age before dispatch. The refresher ticks
/// every 10s (180s max backoff), so anything older than 30 minutes
/// means the weather/skip inputs cannot be trusted to water on.
const MAX_SNAPSHOT_AGE_S: i64 = 30 * 60;

/// Skip-row reason recorded when the freshness gate blocks dispatch.
/// The boot dedupe ignores rows with this reason so a recovered
/// refresher (or a restart) can still water the same morning.
const STALE_INPUTS_REASON: &str = "stale inputs";

/// Days of `last_fired` dedupe entries to retain.
const LAST_FIRED_RETAIN_DAYS: i64 = 7;

/// True when the snapshot is fresh enough to drive a watering decision:
/// refreshed at least once since boot, and within MAX_SNAPSHOT_AGE_S.
fn snapshot_is_fresh(last_refresh_epoch: i64, now_epoch: i64) -> bool {
    last_refresh_epoch > 0 && (now_epoch - last_refresh_epoch) < MAX_SNAPSHOT_AGE_S
}

/// Spawn the smart-morning dispatcher. Returns immediately; the task
/// runs for the lifetime of the process. Safe to call with location
/// = (0.0, 0.0), the formula still produces a finite sunrise; in
/// practice main.rs always passes a real lat/lon from the loaded toml.
pub fn spawn(
    irrigation_store: Arc<IrrigationStore>,
    // Hot-swappable policy handle (the same one the refresher and the manual
    // dispatcher load): the per-tick loop reads the LIVE soak_minutes +
    // interleave_cycles from it, so a settings save reshapes the next tick's
    // window math and dispatch plan with no restart. The boot cfg Arc below
    // stays for build_cycle_plan's per-zone lookups only (zone-set bound).
    watering_policy: Arc<arc_swap::ArcSwap<WateringPolicy>>,
    controllers: ControllerRegistry,
    runs: Option<RunsStore>,
    active_runs: Option<ActiveRunsStore>,
    location: (f64, f64),
    cfg: Option<Arc<Config>>,
    push: Option<PushDispatcher>,
    dry_run: bool,
) {
    let (lat, lon) = location;
    info!(
        lat,
        lon,
        dry_run,
        catch_up_grace_s = CATCH_UP_GRACE_S,
        "smart morning scheduler: spawning tick"
    );
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(60));
        let mut last_fired: HashMap<NaiveDate, bool> = HashMap::new();
        let mut bootstrapped = false;
        // Date for which a "stale inputs" skip row has already been
        // recorded, so retry ticks don't spam one row per minute.
        let mut stale_row_date: Option<NaiveDate> = None;
        loop {
            tick.tick().await;
            // P1-8c: the calendar "today" for sunrise + the day-dedup keys off the
            // CONFIGURED timezone, not the container TZ. The dispatch window itself
            // is computed in UTC below (now_utc vs the sunrise-derived target), so
            // it stays DST-correct independently.
            let now_local = crate::timeutil::now_local();
            let today: NaiveDate = now_local.date_naive();

            // Bounded dedupe map: drop entries older than a week.
            last_fired
                .retain(|d, _| today.signed_duration_since(*d).num_days() < LAST_FIRED_RETAIN_DAYS);

            let snap = irrigation_store.snapshot();
            let total_dispatch_s: u64 = snap
                .zones
                .iter()
                .map(|z| z.planned_run_seconds as u64)
                .sum();
            let zones_to_run: usize = snap
                .zones
                .iter()
                .filter(|z| z.planned_run_seconds > 0)
                .count();
            // TRUE wall time of the sequence, soak gaps included. The legacy
            // estimate summed only run seconds, so a cycle/soak morning
            // overshot target_finish by the total soak time. The soak/
            // interleave knobs are read from the hot-swapped policy EACH tick,
            // so a config apply reshapes today's window with no restart.
            let policy = watering_policy.load();
            let soak_minutes = policy.soak_minutes;
            let interleave_cycles = policy.interleave_cycles;
            drop(policy);
            let sequence_total_s =
                sequence_wall_seconds(cfg.as_deref(), &snap.zones, soak_minutes, interleave_cycles);

            let sunrise = match sunrise_utc(today, lat, lon) {
                Some(s) => s,
                None => {
                    continue;
                }
            };
            let target_finish = sunrise - chrono::Duration::minutes(15);
            // Single-sourced with the refresher's next_run_epoch: clamped so a
            // soak-heavy plan never anchors its start into the previous local
            // day (which the day-keyed dedupe could never fire on time and the
            // after-midnight tick would mislabel a catch-up).
            let target_start = match crate::engine::sunrise::smart_morning_target_start(
                today,
                lat,
                lon,
                sequence_total_s,
            ) {
                Some(t) => t,
                None => {
                    continue;
                }
            };

            let now_utc = Utc::now();
            let delta_s = (now_utc - target_start).num_seconds();
            let in_window = delta_s.abs() <= TARGET_WINDOW_S;

            // Boot-time reconciliation: consult the runs table once so a
            // restart never re-fires a morning that was already handled
            // (completed runs, a skip verdict, a manual stop, or a
            // missed-window marker all count; only "stale inputs" rows
            // are ignored so recovery can still water).
            if !bootstrapped {
                bootstrapped = true;
                let already_handled_today = match runs.as_ref() {
                    Some(rs) => handled_smart_morning_today(rs, today).await,
                    None => false,
                };
                if already_handled_today {
                    info!("smart morning: runs table already has smart_morning rows for today; not re-dispatching");
                    last_fired.insert(today, true);
                }
            }

            if last_fired.get(&today).copied().unwrap_or(false) {
                continue;
            }

            let past_finish_s = (now_utc - target_finish).num_seconds();
            // Catch-up applies when the start window was missed entirely
            // (boot after the window, or an in-process stall across it)
            // but we are still within grace of the planned finish.
            let late = delta_s > TARGET_WINDOW_S;

            if late && past_finish_s > CATCH_UP_GRACE_S {
                warn!(
                    past_finish_s,
                    grace_s = CATCH_UP_GRACE_S,
                    "smart morning: missed today's window past catch-up grace; logging missed-window row"
                );
                if let Some(rs) = runs.as_ref() {
                    for zone in &snap.zones {
                        if zone.planned_run_seconds == 0 {
                            continue;
                        }
                        let row = NewRun {
                            zone_slug: zone.slug.clone(),
                            start_epoch: target_start.timestamp(),
                            source: "smart_morning".into(),
                            controller_id: controllers
                                .default()
                                .map(|c| c.id().to_string())
                                .unwrap_or_default(),
                            planned_duration_s: zone.planned_run_seconds,
                            skip_reason: None,
                            et0_mm: None,
                            etc_mm: None,
                            cycle_index: None,
                            cycle_count: None,
                        };
                        if let Err(e) = rs
                            .insert_skipped(
                                row,
                                "Missed dispatch window (LocalSky offline)".to_string(),
                            )
                            .await
                        {
                            warn!(zone = %zone.slug, error = %e, "smart morning: missed-window row insert failed");
                        }
                    }
                }
                last_fired.insert(today, true);
                continue;
            }

            if !(in_window || late) {
                continue;
            }

            // The plan physically cannot finish by the target even from the
            // clamped local-midnight start: tell the operator instead of
            // silently overshooting sunrise. Fires only on dispatch-eligible
            // ticks, so at most a handful of lines per day.
            let available_s = (target_finish - target_start).num_seconds();
            if available_s < sequence_total_s as i64 {
                warn!(
                    sequence_total_s,
                    available_s,
                    "smart morning: the cycle/soak plan is longer than the span from local \
                     midnight to sunrise-15min, so the sequence will overshoot the finish \
                     target; enable engine.interleave_cycles or shorten soak_minutes to fit"
                );
            }

            // Freshness gate: never water (or record a verdict) from a
            // stale or never-populated snapshot. Do NOT mark the day
            // fired, the refresher usually recovers within seconds of
            // boot, and the catch-up path retries until grace expires.
            if !snapshot_is_fresh(snap.last_refresh_epoch, now_utc.timestamp()) {
                if stale_row_date != Some(today) {
                    warn!(
                        last_refresh_epoch = snap.last_refresh_epoch,
                        "smart morning: snapshot stale at dispatch time; holding off (will retry within grace)"
                    );
                    if let Some(rs) = runs.as_ref() {
                        for zone in &snap.zones {
                            if zone.planned_run_seconds == 0 {
                                continue;
                            }
                            let row = NewRun {
                                zone_slug: zone.slug.clone(),
                                start_epoch: now_utc.timestamp(),
                                source: "smart_morning".into(),
                                controller_id: controllers
                                    .default()
                                    .map(|c| c.id().to_string())
                                    .unwrap_or_default(),
                                planned_duration_s: zone.planned_run_seconds,
                                skip_reason: None,
                                et0_mm: None,
                                etc_mm: None,
                                cycle_index: None,
                                cycle_count: None,
                            };
                            if let Err(e) = rs
                                .insert_skipped(row, STALE_INPUTS_REASON.to_string())
                                .await
                            {
                                warn!(zone = %zone.slug, error = %e, "smart morning: stale-inputs row insert failed");
                            }
                        }
                    }
                    stale_row_date = Some(today);
                } else {
                    debug!("smart morning: snapshot still stale; retrying next tick");
                }
                continue;
            }

            if late {
                info!(
                    past_finish_s,
                    "smart morning: catch-up, missed today's window, attempting late dispatch"
                );
            }
            // P0-8 class: a panic inside the dispatch (an adapter bug, a
            // budget-math edge, a poisoned lock) must not kill the scheduler
            // for the process lifetime, silently ending every future morning.
            // On panic the day is STILL marked fired (fail-safe: some zones
            // may already have run, and re-entering next tick would
            // double-water; any valve left commanded on is closed by the
            // armed active-run deadline via the reaper).
            {
                use futures::FutureExt;
                let outcome = std::panic::AssertUnwindSafe(dispatch_today(
                    &snap,
                    &controllers,
                    runs.as_ref(),
                    active_runs.as_ref(),
                    push.as_ref(),
                    cfg.as_ref(),
                    soak_minutes,
                    interleave_cycles,
                    today,
                    now_utc,
                    zones_to_run,
                    total_dispatch_s,
                    dry_run,
                    late,
                ))
                .catch_unwind()
                .await;
                if outcome.is_err() {
                    tracing::error!(
                        "smart morning: dispatch PANICKED mid-sequence; marking today \
                         handled (no re-fire) and relying on the reaper deadline to \
                         close any valve left commanded on"
                    );
                }
            }
            last_fired.insert(today, true);
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_today(
    snap: &crate::ha::snapshot::IrrigationSnapshot,
    controllers: &ControllerRegistry,
    runs: Option<&RunsStore>,
    active_runs: Option<&ActiveRunsStore>,
    push: Option<&PushDispatcher>,
    cfg: Option<&Arc<Config>>,
    // Live cycle/soak knobs, resolved from the hot-swapped watering policy by
    // the tick loop (not read from the boot cfg, which would pin them until a
    // restart).
    soak_minutes: u32,
    interleave_cycles: bool,
    today: NaiveDate,
    now_utc: chrono::DateTime<Utc>,
    zones_to_run: usize,
    total_dispatch_s: u64,
    dry_run: bool,
    is_catch_up: bool,
) {
    let _ = today;

    // Decide skip vs run.
    if snap.skip_check.will_skip {
        let reason = if snap.skip_check.reason.is_empty() {
            "skip-rule ladder".to_string()
        } else {
            snap.skip_check.reason.clone()
        };
        if let Some(rs) = runs {
            for zone in &snap.zones {
                if zone.planned_run_seconds == 0 {
                    continue;
                }
                let row = NewRun {
                    zone_slug: zone.slug.clone(),
                    start_epoch: now_utc.timestamp(),
                    source: "smart_morning".into(),
                    controller_id: controllers
                        .default()
                        .map(|c| c.id().to_string())
                        .unwrap_or_default(),
                    planned_duration_s: zone.planned_run_seconds,
                    skip_reason: None,
                    et0_mm: None,
                    etc_mm: None,
                    cycle_index: None,
                    cycle_count: None,
                };
                if let Err(e) = rs.insert_skipped(row, reason.clone()).await {
                    warn!(zone = %zone.slug, error = %e, "smart morning: skip-row insert failed");
                }
            }
        }
        info!(
            reason = %reason,
            zones = zones_to_run,
            is_catch_up,
            "smart morning: skipped today's run"
        );
        if let Some(p) = push {
            p.emit(PushEvent::DailyVerdict {
                verdict: "skip".into(),
                reason: reason.clone(),
            });
        }
        return;
    }

    let controller = match controllers.default() {
        Some(c) => c,
        None => {
            warn!("smart morning: no default controller configured; skipping today");
            return;
        }
    };

    // Per-zone verdict enforcement (2026-06-11 incident): decide_per_zone
    // correctly marked saturated zones "skip", but dispatch used to run
    // every zone with planned seconds anyway. Resolve the skip set up
    // front so the announced totals count only zones that will water;
    // the loop below records each skip in the runs history.
    let per_zone_skip_count = snap
        .zones
        .iter()
        .filter(|z| z.planned_run_seconds > 0 && zone_skip_verdict(snap, z).is_some())
        .count();
    let per_zone_skip_secs: u64 = snap
        .zones
        .iter()
        .filter(|z| z.planned_run_seconds > 0 && zone_skip_verdict(snap, z).is_some())
        .map(|z| z.planned_run_seconds as u64)
        .sum();
    let zones_to_run = zones_to_run.saturating_sub(per_zone_skip_count);
    let total_dispatch_s = total_dispatch_s.saturating_sub(per_zone_skip_secs);

    info!(
        zones = zones_to_run,
        zone_verdict_skips = per_zone_skip_count,
        total_s = total_dispatch_s,
        dry_run,
        is_catch_up,
        "smart morning: dispatching morning run"
    );

    let run_push_reason = {
        let total_min = (total_dispatch_s as f64 / 60.0).round() as u32;
        let prefix = if is_catch_up { "Catch-up run: " } else { "" };
        format!("{prefix}{zones_to_run} zone(s), {total_min} min total")
    };
    // Dry-run mode has no dispatch confirmation, so keep the legacy
    // upfront notification there. The real path notifies only after the
    // first segment is confirmed by the controller (no phantom-watered
    // days when dispatch fails).
    let mut announced = if dry_run {
        if let Some(p) = push {
            p.emit(PushEvent::DailyVerdict {
                verdict: "run".into(),
                reason: run_push_reason.clone(),
            });
        }
        true
    } else {
        false
    };
    let mut failure_notified = false;

    // Manual Stop / Stop All / pause requests at or after this instant
    // abandon the remainder of the sequence.
    let cycle_start_epoch = now_utc.timestamp();

    // Resolve the dispatch list up front: due zones minus per-zone verdict
    // skips, each with its cycle-and-soak plan (a single no-split segment
    // when cfg context is missing).
    struct DispatchZone<'a> {
        zone: &'a crate::ha::snapshot::ZoneState,
        segments: Vec<cycle_soak::CycleSegment>,
    }
    let mut dispatch: Vec<DispatchZone> = Vec::new();
    for zone in snap.zones.iter() {
        if zone.planned_run_seconds == 0 {
            continue;
        }
        // Per-zone verdict skip: this zone's own engine verdict (soil
        // saturation, custom condition) says no, even though the
        // yard-wide verdict was "run". Record it through the same runs
        // mechanism as the other scheduler-only rows (skips, missed
        // windows, manual stops) so History shows the per-zone reason.
        // Skip enforcement only: multipliers/extends are not applied at
        // dispatch, and manual runs (scheduler::manual) are untouched.
        if let Some(v) = zone_skip_verdict(snap, zone) {
            if dry_run {
                info!(
                    zone = %zone.slug,
                    source = %v.source,
                    reason = %v.reason,
                    "smart morning [DRY_RUN]: would skip zone on per-zone verdict"
                );
            } else {
                info!(
                    zone = %zone.slug,
                    source = %v.source,
                    reason = %v.reason,
                    "smart morning: per-zone verdict skip"
                );
            }
            if let Some(rs) = runs {
                let row = NewRun {
                    zone_slug: zone.slug.clone(),
                    start_epoch: now_utc.timestamp(),
                    source: "smart_morning".into(),
                    controller_id: controller.id().to_string(),
                    planned_duration_s: zone.planned_run_seconds,
                    skip_reason: None,
                    et0_mm: None,
                    etc_mm: None,
                    cycle_index: None,
                    cycle_count: None,
                };
                if let Err(e) = rs.insert_skipped(row, v.reason.clone()).await {
                    warn!(zone = %zone.slug, error = %e, "smart morning: per-zone skip-row insert failed");
                }
            }
            continue;
        }
        let segments = build_cycle_plan(
            cfg.map(|c| c.as_ref()),
            &zone.slug,
            zone.planned_run_seconds,
            soak_minutes,
        );
        dispatch.push(DispatchZone { zone, segments });
    }

    // Lay the segments out on the shared valve timeline. Serial reproduces
    // the legacy zone-by-zone order and spacing exactly; Interleaved
    // (engine.interleave_cycles, the default) runs other zones' cycles during
    // a zone's soak window, one valve at a time, soaks as minimums.
    let plans: Vec<interleave::ZonePlan> = dispatch
        .iter()
        .enumerate()
        .map(|(idx, dz)| interleave::ZonePlan {
            zone_idx: idx,
            segments: dz.segments.clone(),
        })
        .collect();
    let policy = if interleave_cycles {
        interleave::Policy::Interleaved
    } else {
        interleave::Policy::Serial
    };
    let steps = interleave::plan(&plans, policy, INTER_ZONE_PREAMBLE_S);

    if dry_run {
        for step in &steps {
            let dz = &dispatch[step.zone_idx];
            let seg = dz.segments[step.seg_idx];
            info!(
                zone = %dz.zone.slug,
                segment = step.seg_idx,
                of = dz.segments.len(),
                run_s = seg.run_seconds,
                soak_s = seg.soak_seconds,
                offset_s = step.start_offset_s,
                "smart morning [DRY_RUN]: would dispatch segment"
            );
        }
        return;
    }

    // Execution state, keyed by dispatch-list index. The planner's offsets
    // are estimates only: real timing derives from the actual dispatch
    // clock (ready_at / valve_free_at) so drift self-corrects and soak
    // minimums hold under real controller latency.
    let mut ready_at: Vec<i64> = vec![0; dispatch.len()];
    let mut confirmed: Vec<bool> = vec![false; dispatch.len()];
    let mut failed: Vec<bool> = vec![false; dispatch.len()];
    let mut armed_deadline: Vec<Option<i64>> = vec![None; dispatch.len()];
    let mut valve_free_at: i64 = 0;

    for (step_i, step) in steps.iter().enumerate() {
        // A failed dispatch abandons THAT zone's remaining steps only;
        // other zones continue.
        if failed[step.zone_idx] {
            continue;
        }
        let dz = &dispatch[step.zone_idx];
        let seg = dz.segments[step.seg_idx];
        // Cycle position for history rows written on this step's behalf;
        // single-segment zones keep None (no cycle plan to speak of).
        let cycle_pos =
            (dz.segments.len() > 1).then_some((step.seg_idx as u32, dz.segments.len() as u32));

        // P0-1b: arm the persisted shutoff deadline for the WHOLE zone cycle
        // (all remaining run + soak segments), not per segment: the valve
        // legitimately cycles on and off within the cycle, so a per-segment
        // deadline would make the reaper fire during every soak. The
        // remaining span is re-projected from LIVE ready times at every step
        // of this zone (interleaving can stretch a soak, so the up-front
        // plan can underestimate) and the deadline only ever EXTENDS:
        // arm() keeps MAX(off_deadline_epoch), and the write is skipped
        // entirely when the projection has not drifted later.
        if let Some(ar) = active_runs {
            let now = Utc::now().timestamp();
            let remaining: Vec<(usize, u32, u32)> = steps[step_i..]
                .iter()
                .filter(|s| !failed[s.zone_idx])
                .map(|s| {
                    let sg = dispatch[s.zone_idx].segments[s.seg_idx];
                    (s.zone_idx, sg.run_seconds, sg.soak_seconds)
                })
                .collect();
            let ready_in: Vec<(usize, u64)> = ready_at
                .iter()
                .enumerate()
                .map(|(z, &r)| (z, (r - now).max(0) as u64))
                .collect();
            if let Some(end_in) = interleave::project_zone_end(
                &remaining,
                &ready_in,
                INTER_ZONE_PREAMBLE_S,
                step.zone_idx,
            ) {
                let deadline = now + end_in as i64 + ACTIVE_RUN_GRACE_S;
                if armed_deadline[step.zone_idx].is_none_or(|d| deadline > d) {
                    if let Err(e) = ar
                        .arm(
                            dz.zone.slug.clone(),
                            controller.id().to_string(),
                            now,
                            deadline,
                        )
                        .await
                    {
                        warn!(zone = %dz.zone.slug, error = %e, "active-run arm failed");
                    } else {
                        armed_deadline[step.zone_idx] = Some(deadline);
                    }
                }
            }
        }

        if dispatch_gate::stop_requested_since(cycle_start_epoch) {
            abandon_cycle(
                controller.as_ref(),
                runs,
                active_runs,
                &dz.zone.slug,
                dz.zone.planned_run_seconds,
                cycle_pos,
            )
            .await;
            return;
        }
        // P0-8: serialize this run_zone dispatch on the zone against the
        // manual API path + manual scheduler, sharing one lock registry.
        // Held only across the dispatch (per step, not the whole cycle),
        // so a concurrent manual run on this zone is never blocked for the
        // length of the cycle and a Stop is never blocked at all.
        let run_result = {
            let lock = crate::controllers::zone_run_lock(&dz.zone.slug);
            let _run_serialize = lock.lock().await;
            controller.run_zone(&dz.zone.slug, seg.run_seconds).await
        };
        // The wait after this step anchors on the real post-dispatch clock,
        // so serial spacing matches the legacy dispatcher exactly and soak
        // minimums self-correct under dispatch latency.
        let anchor: i64;
        match run_result {
            Ok(handle) => {
                info!(
                    zone = %dz.zone.slug,
                    segment = step.seg_idx,
                    of = dz.segments.len(),
                    run_s = seg.run_seconds,
                    soak_s = seg.soak_seconds,
                    provider_ref = ?handle.provider_ref,
                    "smart morning: dispatched segment"
                );
                // Notify only once the controller has confirmed the
                // first segment, so a dead controller never produces
                // a phantom "Running today" push.
                if !announced {
                    if let Some(p) = push {
                        p.emit(PushEvent::DailyVerdict {
                            verdict: "run".into(),
                            reason: run_push_reason.clone(),
                        });
                    }
                    announced = true;
                }
                anchor = Utc::now().timestamp();
                confirmed[step.zone_idx] = true;
                ready_at[step.zone_idx] = anchor + seg.run_seconds as i64 + seg.soak_seconds as i64;
                valve_free_at = anchor + seg.run_seconds as i64;
                // Completed work is recorded by the snapshot run-edge
                // observer (history::ingest), which measures what the
                // hardware actually did. Writing a planned-duration row
                // here too double-counted every segment in History, so
                // the scheduler only records what the observer cannot
                // see: skips, missed windows, and manual stops.
            }
            Err(e) => {
                warn!(
                    zone = %dz.zone.slug,
                    segment = step.seg_idx,
                    error = %e,
                    "smart morning: controller dispatch failed"
                );
                if !failure_notified {
                    if let Some(p) = push {
                        p.emit(PushEvent::DailyVerdict {
                            verdict: "skip".into(),
                            reason: format!(
                                "Watering dispatch failed for {}: {}. Check the controller connection.",
                                dz.zone.slug, e
                            ),
                        });
                    }
                    failure_notified = true;
                }
                // P0-1b (the old idx==0 rule, generalized): disarm the
                // whole-cycle shutoff deadline ONLY when NO step of this
                // zone was ever confirmed (no valve was ever commanded on,
                // so the deadline covers a run that never started and the
                // reaper would log a misleading "enforcing shutoff" line).
                // Once ANY earlier step of the zone confirmed, its valve WAS
                // commanded on and its own self-shutoff may be the very
                // thing that is failing (same unreachable-controller blip),
                // so the deadline must STAY armed: it is the only backstop
                // that closes that valve. The reaper stopping an
                // already-closed valve is a harmless no-op; a stuck-open
                // valve with no backstop is the failure mode P0-1b exists
                // to prevent.
                if !confirmed[step.zone_idx] {
                    if let Some(ar) = active_runs {
                        if let Err(e) = ar.disarm(&dz.zone.slug).await {
                            warn!(zone = %dz.zone.slug, error = %e, "active-run disarm after dispatch failure failed");
                        }
                    }
                } else {
                    warn!(
                        zone = %dz.zone.slug,
                        segment = step.seg_idx,
                        "keeping the whole-cycle shutoff deadline armed: an earlier \
                         segment commanded the valve on and the reaper backstop must \
                         cover it"
                    );
                }
                failed[step.zone_idx] = true;
                anchor = Utc::now().timestamp();
            }
        }

        // Wait out this step's obligations before the next runnable step,
        // or drain the final run plus the trailing preamble the legacy loop
        // always slept, so a Stop during the last run still abandons
        // through this path (stop_all + history row). Interrupts are
        // attributed to this step's zone, like the legacy per-zone waits.
        let next = steps[step_i + 1..].iter().find(|s| !failed[s.zone_idx]);
        let wait_s: u64 = match next {
            Some(n) => {
                let mut until = ready_at[n.zone_idx];
                if failed[step.zone_idx] {
                    // The failed dispatch never opened the valve: only the
                    // preamble spacing from the failure instant applies
                    // (the legacy break path slept the same preamble).
                    until = until.max(anchor + INTER_ZONE_PREAMBLE_S as i64);
                } else {
                    let gap = if n.zone_idx == step.zone_idx {
                        0
                    } else {
                        INTER_ZONE_PREAMBLE_S as i64
                    };
                    until = until.max(valve_free_at + gap);
                }
                (until - anchor).max(0) as u64
            }
            None => {
                if failed[step.zone_idx] {
                    INTER_ZONE_PREAMBLE_S
                } else {
                    seg.run_seconds as u64 + seg.soak_seconds as u64 + INTER_ZONE_PREAMBLE_S
                }
            }
        };
        if wait_unless_stopped(wait_s, cycle_start_epoch).await {
            abandon_cycle(
                controller.as_ref(),
                runs,
                active_runs,
                &dz.zone.slug,
                dz.zone.planned_run_seconds,
                cycle_pos,
            )
            .await;
            return;
        }
    }
}

/// True wall-clock length (seconds) of the smart-morning sequence: every due
/// zone's cycle-and-soak plan laid out on the shared valve timeline under the
/// active policy, soak gaps and inter-zone preambles included. The legacy
/// estimate summed only run seconds, so a cycle/soak morning overshot
/// target_finish (sunrise - 15min) by the total soak time; the dispatch
/// window math above uses this instead, for both policies. `soak_minutes` +
/// `interleave_cycles` come from the caller's LIVE watering policy (both the
/// tick loop here and the refresher's compute_next_run_epoch resolve them per
/// evaluation); `cfg` is only the per-zone lookup context for
/// build_cycle_plan.
pub fn sequence_wall_seconds(
    cfg: Option<&Config>,
    zones: &[crate::ha::snapshot::ZoneState],
    soak_minutes: u32,
    interleave_cycles: bool,
) -> u64 {
    let plans: Vec<interleave::ZonePlan> = zones
        .iter()
        .filter(|z| z.planned_run_seconds > 0)
        .enumerate()
        .map(|(idx, z)| interleave::ZonePlan {
            zone_idx: idx,
            segments: build_cycle_plan(cfg, &z.slug, z.planned_run_seconds, soak_minutes),
        })
        .collect();
    let policy = if interleave_cycles {
        interleave::Policy::Interleaved
    } else {
        interleave::Policy::Serial
    };
    interleave::makespan_s(&interleave::plan(&plans, policy, INTER_ZONE_PREAMBLE_S))
}

/// The per-zone skip verdict that must block this zone's dispatch, if
/// any. Only non-global SKIP verdicts qualify: global-source skips were
/// already handled for the whole run by the aggregate skip_check (so
/// enforcing them here would double-record), and run/run_extended
/// verdicts never block. Reads the zone's back-filled verdict first,
/// falling back to the snapshot-level zone_verdicts list.
fn zone_skip_verdict<'a>(
    snap: &'a crate::ha::snapshot::IrrigationSnapshot,
    zone: &'a crate::ha::snapshot::ZoneState,
) -> Option<&'a crate::ha::snapshot::ZoneVerdict> {
    zone.verdict
        .as_ref()
        .or_else(|| snap.zone_verdicts.iter().find(|v| v.zone_slug == zone.slug))
        // A per-zone skip is honored when it is NOT inherited from a blanket
        // aggregate skip (which the will_skip early-return already handled), OR
        // when the aggregate did NOT blanket-skip. The latter is the soil-floor
        // demotion morning (P1-2): will_skip is false because a dry zone runs, so
        // a wet sibling's source:"global" skip must still be honored here.
        .filter(|v| v.verdict == "skip" && (v.source != "global" || !snap.skip_check.will_skip))
}

/// Sleep `secs`, polling the dispatch gate every couple of seconds.
/// Returns true when a manual stop interrupted the wait.
async fn wait_unless_stopped(secs: u64, cycle_start_epoch: i64) -> bool {
    const POLL_S: u64 = 2;
    let mut remaining = secs;
    while remaining > 0 {
        let step = remaining.min(POLL_S);
        tokio::time::sleep(Duration::from_secs(step)).await;
        remaining -= step;
        if dispatch_gate::stop_requested_since(cycle_start_epoch) {
            return true;
        }
    }
    false
}

/// Manual stop observed mid-sequence: stop the hardware (best effort)
/// and record a history row noting the abandonment. The row counts as
/// "handled today" in the boot dedupe, so a restart after a manual stop
/// does not re-water. `cycle_pos` is the (segment index, segment count)
/// of the zone's cycle plan at the stop, when the zone had one (None for
/// single-segment zones, matching the other scheduler rows).
async fn abandon_cycle(
    controller: &dyn IrrigationController,
    runs: Option<&RunsStore>,
    active_runs: Option<&ActiveRunsStore>,
    current_zone: &str,
    planned_duration_s: u32,
    cycle_pos: Option<(u32, u32)>,
) {
    warn!(
        zone = current_zone,
        "smart morning: manual stop requested; abandoning the rest of the sequence"
    );
    // P0-1b: only clear the shutoff-deadline ledger when stop_all actually
    // CONFIRMED every valve is off. On failure (unreachable controller, DIY
    // on/off board mid-blip) the valves may still be open, so we KEEP the
    // deadline rows and let the reaper retry the stop every tick until it
    // succeeds. Mirrors the API stop paths, which also disarm only in the
    // Ok arm, and honors the reaper invariant ("a stop that fails is
    // retried next tick, the row is kept, so we never give up on a shutoff").
    match controller.stop_all().await {
        Ok(()) => {
            if let Some(ar) = active_runs {
                let _ = ar.clear_all().await;
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                "smart morning: stop_all after manual stop failed; keeping active-run deadlines for reaper retry"
            );
        }
    }
    if let Some(rs) = runs {
        let row = NewRun {
            zone_slug: current_zone.to_string(),
            start_epoch: Utc::now().timestamp(),
            source: "smart_morning".into(),
            controller_id: controller.id().to_string(),
            planned_duration_s,
            skip_reason: None,
            et0_mm: None,
            etc_mm: None,
            cycle_index: cycle_pos.map(|(i, _)| i),
            cycle_count: cycle_pos.map(|(_, n)| n),
        };
        if let Err(e) = rs
            .insert_skipped(
                row,
                "Stopped manually; remaining sequence abandoned".to_string(),
            )
            .await
        {
            warn!(error = %e, "smart morning: manual-stop row insert failed");
        }
    }
}

/// Resolve a per-zone cycle-and-soak plan. Falls back to a single
/// no-split segment when cfg is unavailable or the zone slug doesn't
/// resolve to a configured zone (e.g. demo mode, mid-cutover state).
fn build_cycle_plan(
    cfg: Option<&Config>,
    slug: &str,
    duration_s: u32,
    soak_minutes: u32,
) -> Vec<cycle_soak::CycleSegment> {
    let fallback = vec![cycle_soak::CycleSegment {
        run_seconds: duration_s,
        soak_seconds: 0,
    }];
    let Some(cfg) = cfg else {
        return fallback;
    };
    // The refresher underscore-normalizes slugs ("back-yard" ->
    // "back_yard") before populating the snapshot; the cfg keys may be
    // in either form. Try the slug as-given, then the dashed variant.
    let zone_cfg = cfg
        .zones
        .get(slug)
        .or_else(|| cfg.zones.get(&slug.replace('_', "-")));
    let Some(z) = zone_cfg else {
        return fallback;
    };
    let precip = effective_precip_rate_mm_hr(z.sprinkler_type, z.precip_rate_mm_hr);
    let segments = cycle_soak::split(
        duration_s,
        precip,
        z.soil_texture,
        z.slope_pct,
        soak_minutes,
    );
    // Zero-effective-precip guard: split() returns no segments when the
    // effective precip rate is ~0 (a mis-/zero-configured sprinkler type or
    // precip_rate_mm_hr). With duration_s > 0 that would SILENTLY skip the zone
    // and arm a 0-second shutoff deadline for a valve never opened. Fall back to
    // watering the full duration in one pass (the safe direction) and log the
    // misconfig so it is visible instead of a quietly dry zone. (duration_s == 0
    // legitimately yields no segments and is left alone.)
    if segments.is_empty() && duration_s > 0 {
        warn!(
            zone = %slug, precip_rate_mm_hr = precip, duration_s,
            "cycle-soak produced no segments (effective precip rate ~0); watering the full \
             duration in one pass. Check this zone's sprinkler type / precip_rate_mm_hr."
        );
        return fallback;
    }
    segments
}

/// True when the runs table already has a smart_morning row for today
/// that represents a handled morning: completed runs, a skip verdict, a
/// manual stop, or a missed-window marker. "stale inputs" rows are
/// excluded so a restart (or refresher recovery) can still water a
/// morning that was only blocked by the freshness gate. Used by the
/// boot reconciliation pass so a restart inside the same morning never
/// fires the dispatch twice.
async fn handled_smart_morning_today(runs: &RunsStore, today: NaiveDate) -> bool {
    // P1-8c: the local day's UTC bounds key off the CONFIGURED timezone, so the
    // boot dedupe window matches the same "today" the dispatch loop uses.
    let (start_utc, end_utc) = match crate::timeutil::local_day_bounds_utc(today) {
        Some(b) => b,
        None => return false,
    };
    let rows = match runs
        .window(start_utc.timestamp(), end_utc.timestamp())
        .await
    {
        Ok(rs) => rs,
        Err(e) => {
            warn!(error = %e, "smart morning: catch-up window query failed");
            return false;
        }
    };
    // Two signals count as "today is handled": a scheduler marker row
    // (skip / missed / manual-stop; never written for stale inputs), or
    // observer-recorded completed runs across 2+ distinct zones (a full
    // or partial sequence actually watered). A single manual zone test
    // does not suppress the morning run.
    let marker = rows.iter().any(|r| {
        r.source == "smart_morning" && r.skip_reason.as_deref() != Some(STALE_INPUTS_REASON)
    });
    let watered_zones: std::collections::HashSet<&str> = rows
        .iter()
        .filter(|r| r.skip_reason.is_none())
        .map(|r| r.zone_slug.as_str())
        .collect();
    marker || watered_zones.len() >= 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    #[test]
    fn build_cycle_plan_fallback_when_cfg_missing() {
        let plan = build_cycle_plan(None, "back_yard", 1500, 30);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].run_seconds, 1500);
        assert_eq!(plan[0].soak_seconds, 0);
    }

    fn verdict(slug: &str, verdict: &str, source: &str) -> crate::ha::snapshot::ZoneVerdict {
        crate::ha::snapshot::ZoneVerdict {
            zone_slug: slug.into(),
            zone_name: slug.into(),
            verdict: verdict.into(),
            reason: "Soil saturated (76% at or above the 65% threshold)".into(),
            source: source.into(),
            multiplier: 1.0,
            // P1 additive fields default (reason_code "", operands None) for this
            // scheduler test fixture.
            ..Default::default()
        }
    }

    fn zone_with(
        slug: &str,
        v: Option<crate::ha::snapshot::ZoneVerdict>,
    ) -> crate::ha::snapshot::ZoneState {
        crate::ha::snapshot::ZoneState {
            slug: slug.into(),
            name: slug.into(),
            planned_run_seconds: 600,
            verdict: v,
            ..Default::default()
        }
    }

    // ── P1-4: dispatch_today actuation + fail-safe integration tests ─────────
    use crate::persistence::run_migrations;
    use crate::ports::irrigation_controller::{
        ControllerCaps, ControllerError, ControllerResult, ControllerStatus, RunHandle, RunRecord,
    };
    use std::sync::atomic::Ordering;

    // dispatch_gate's LAST_STOP_EPOCH is process-global + monotonic, and the lib
    // test binary runs these concurrently, so the epochs are ordered so no test's
    // stamp poisons another's gate check:
    //   STOP_EPOCH (low)  -- stamped by the before-cycle abandon test.
    //   MID_CYCLE_EPOCH   -- base of the epoch BANDS claimed via
    //                        claim_stop_band() by the tests that stamp the gate
    //                        MID-cycle. Each such test uses its claimed band as
    //                        its cycle start and stamps that same value only
    //                        after its first zone dispatches, so the gate is
    //                        below the band at loop start (zone 1 runs) and
    //                        at-or-above it afterwards (the remainder is
    //                        abandoned).
    //   NO_STOP_EPOCH (highest) -- the no-stop tests' cycle start. It sits above
    //                        every stamp any sibling test makes (the claimed
    //                        bands never reach it), so
    //                        stop_requested_since(NO_STOP_EPOCH) stays false for
    //                        them regardless of interleaving.
    // Each test gets its own in-memory DB, so row assertions use a wide window
    // (abandon_cycle stamps real Utc::now()).
    const STOP_EPOCH: i64 = 1_000_000_000; // ~year 2001
    const MID_CYCLE_EPOCH: i64 = 15_000_000_000; // ~year 2445
    const NO_STOP_EPOCH: i64 = 100_000_000_000; // ~year 5138 (above every stamp)
    const WIDE: (i64, i64) = (0, 999_999_999_999);

    /// Claim an epoch band for a test that stamps the gate MID-cycle. Two
    /// guarantees, both required because the gate is process-global and
    /// monotonic (it never rolls back):
    ///   * the returned guard serializes every stamping test, so a claimant's
    ///     clean phase (gate still below its own cycle start) can never race a
    ///     concurrent sibling's stamp;
    ///   * bands are handed out in increasing order, so a later claimant's
    ///     cycle start sits ABOVE every stamp an earlier claimant made.
    /// Bands step 1e9 from MID_CYCLE_EPOCH and stay far below NO_STOP_EPOCH,
    /// so the no-stop tests are never poisoned no matter how many bands are
    /// claimed.
    async fn claim_stop_band() -> (tokio::sync::MutexGuard<'static, ()>, i64) {
        static SERIALIZE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        static NEXT_BAND: std::sync::atomic::AtomicI64 =
            std::sync::atomic::AtomicI64::new(MID_CYCLE_EPOCH);
        let guard = SERIALIZE.lock().await;
        let band = NEXT_BAND.fetch_add(1_000_000_000, Ordering::SeqCst);
        (guard, band)
    }

    /// Records run_zone (slug, duration_s) in dispatch order and counts stop_all
    /// (the abandon path). Never sleeps, never fails. The default controller for
    /// the P1-4 tests.
    struct DispatchRecorder {
        id: String,
        runs: std::sync::Mutex<Vec<(String, u32)>>,
        stop_all_calls: std::sync::atomic::AtomicUsize,
    }
    impl DispatchRecorder {
        fn new(id: &str) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                id: id.into(),
                runs: std::sync::Mutex::new(Vec::new()),
                stop_all_calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
        fn log(&self) -> Vec<(String, u32)> {
            self.runs.lock().unwrap().clone()
        }
        fn stops(&self) -> usize {
            self.stop_all_calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait::async_trait]
    impl IrrigationController for DispatchRecorder {
        fn id(&self) -> &str {
            &self.id
        }
        fn supports(&self) -> ControllerCaps {
            ControllerCaps {
                flow_meter: false,
                rain_sensor: false,
                master_valve: false,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
                water_level: false,
            }
        }
        async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
            self.runs
                .lock()
                .unwrap()
                .push((slug.to_string(), duration_s));
            Ok(RunHandle {
                controller_id: self.id.clone(),
                zone_slug: slug.to_string(),
                started_epoch: Utc::now().timestamp(),
                planned_duration_s: duration_s,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, _slug: &str) -> ControllerResult<()> {
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            self.stop_all_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            Ok(ControllerStatus {
                reachable: true,
                master_enabled: None,
                water_level_pct: None,
                rain_sensor_tripped: None,
                current_program: None,
                zone_states: vec![],
                flow_gpm: None,
                flow_connected: false,
                firmware: None,
            })
        }
        async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(vec![])
        }
    }

    fn registry_with<C: IrrigationController + 'static>(
        rec: &std::sync::Arc<C>,
    ) -> ControllerRegistry {
        let ctrl: std::sync::Arc<dyn IrrigationController> = rec.clone();
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctrl, true)]);
        registry
    }

    /// One migrated in-memory DB shared by both stores (test-isolated).
    fn stores() -> (RunsStore, ActiveRunsStore) {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        run_migrations(&mut c).unwrap();
        let conn = std::sync::Arc::new(tokio::sync::Mutex::new(c));
        (RunsStore::new(conn.clone()), ActiveRunsStore::new(conn))
    }

    fn zone_secs(
        slug: &str,
        secs: u32,
        v: Option<crate::ha::snapshot::ZoneVerdict>,
    ) -> crate::ha::snapshot::ZoneState {
        crate::ha::snapshot::ZoneState {
            slug: slug.into(),
            name: slug.into(),
            planned_run_seconds: secs,
            verdict: v,
            ..Default::default()
        }
    }

    fn at(epoch: i64) -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(epoch, 0).unwrap()
    }

    fn snap_with(
        zones: Vec<crate::ha::snapshot::ZoneState>,
    ) -> crate::ha::snapshot::IrrigationSnapshot {
        let mut s = crate::ha::snapshot::IrrigationSnapshot::default();
        s.zones = zones;
        s
    }

    async fn run_dispatch(
        snap: &crate::ha::snapshot::IrrigationSnapshot,
        registry: &ControllerRegistry,
        runs: &RunsStore,
        active_runs: &ActiveRunsStore,
        now_utc: chrono::DateTime<Utc>,
    ) {
        let n = snap.zones.len();
        let total: u64 = snap
            .zones
            .iter()
            .map(|z| z.planned_run_seconds as u64)
            .sum();
        dispatch_today(
            snap,
            registry,
            Some(runs),
            Some(active_runs),
            None,  // push
            None,  // cfg -> single segment, no soak
            30,    // soak_minutes (policy default)
            false, // interleave_cycles
            chrono::NaiveDate::from_ymd_opt(2026, 6, 25).unwrap(),
            now_utc,
            n,
            total,
            false, // dry_run
            false, // is_catch_up
        )
        .await;
    }

    // (a) every due zone dispatches, in order, with its planned duration.
    #[tokio::test(start_paused = true)]
    async fn dispatch_runs_all_zones_in_order() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let snap = snap_with(vec![
            zone_secs("front", 1, None),
            zone_secs("side", 1, None),
            zone_secs("back", 1, None),
        ]);
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert_eq!(
            rec.log(),
            vec![
                ("front".to_string(), 1u32),
                ("side".into(), 1),
                ("back".into(), 1)
            ]
        );
        assert_eq!(rec.stops(), 0);
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
    }

    // (a2) the interleaved policy over single-segment plans (no soak
    // anywhere) degenerates to the serial order: every zone dispatches once,
    // in snapshot order, exactly like (a).
    #[tokio::test(start_paused = true)]
    async fn dispatch_interleave_flag_single_segments_matches_serial() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let snap = snap_with(vec![
            zone_secs("front", 1, None),
            zone_secs("side", 1, None),
            zone_secs("back", 1, None),
        ]);
        let mut cfg = Config::default();
        cfg.engine.interleave_cycles = true;
        let cfg = Arc::new(cfg);
        dispatch_today(
            &snap,
            &registry,
            Some(&runs),
            Some(&active_runs),
            None, // push
            Some(&cfg),
            cfg.engine.soak_minutes,
            cfg.engine.interleave_cycles,
            chrono::NaiveDate::from_ymd_opt(2026, 6, 25).unwrap(),
            at(NO_STOP_EPOCH),
            3,
            3,
            false, // dry_run
            false, // is_catch_up
        )
        .await;
        assert_eq!(
            rec.log(),
            vec![
                ("front".to_string(), 1u32),
                ("side".into(), 1),
                ("back".into(), 1)
            ]
        );
        assert_eq!(rec.stops(), 0);
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
    }

    #[test]
    fn sequence_wall_seconds_matches_legacy_without_cfg() {
        // No cfg -> single no-split segments: the wall time equals the
        // legacy sum(planned) + preamble * (zones - 1), zero-budget zones
        // excluded, identically under both policies.
        let zones = vec![
            zone_secs("front", 600, None),
            zone_secs("off_zone", 0, None),
            zone_secs("side", 300, None),
        ];
        assert_eq!(
            sequence_wall_seconds(None, &zones, 30, false),
            600 + 300 + 2
        );
        assert_eq!(sequence_wall_seconds(None, &zones, 30, true), 600 + 300 + 2);
        assert_eq!(sequence_wall_seconds(None, &[], 30, false), 0);
    }

    // (b) a Stop requested at/ before the cycle abandons the sequence: stop_all
    // is called once, no zone is dispatched, and the abandon row is written.
    #[tokio::test]
    async fn dispatch_stop_abandons_sequence() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let snap = snap_with(vec![
            zone_secs("front", 1, None),
            zone_secs("side", 1, None),
        ]);
        dispatch_gate::note_stop_at(STOP_EPOCH);
        run_dispatch(&snap, &registry, &runs, &active_runs, at(STOP_EPOCH)).await;
        assert!(rec.log().is_empty(), "no zone may dispatch after a stop");
        assert_eq!(rec.stops(), 1, "abandon_cycle must stop_all once");
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].zone_slug, "front");
        assert_eq!(rows[0].status, "skipped");
        assert_eq!(
            rows[0].skip_reason.as_deref(),
            Some("Stopped manually; remaining sequence abandoned")
        );
    }

    // (b2) THE FAIL-SAFE: a stop fired WHILE zone k of N is running abandons
    // zones k+1..N (they never dispatch a start) and closes the open valve via
    // stop_all. This is the real mid-sequence case the run-history row
    // "Stopped manually; remaining sequence abandoned" attests to, distinct from
    // (b) where the stop precedes the very first zone.
    //
    // Mechanism: dispatch and a stopper run concurrently on a start_paused
    // runtime. The stopper busy-yields (never parks on a timer) until zone 1 is
    // recorded, then stamps the gate at this test's claimed band. Because the
    // stopper is runnable, the runtime cannot auto-advance the dispatch's
    // post-zone-1 sleep until the stamp is in place; when the sleep then
    // resolves, wait_unless_stopped observes the stop and abandon_cycle fires.
    // k=1 of N=3 here: zones 2 and 3 must never dispatch.
    #[tokio::test(start_paused = true)]
    async fn dispatch_stop_mid_sequence_abandons_remainder_and_closes_valve() {
        let (_serialize, band) = claim_stop_band().await;
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        // Each zone's planned seconds drive the post-zone wait (run+soak). Non-zero
        // so wait_unless_stopped actually sleeps after zone 1, giving the gate a
        // wait to interrupt rather than racing the inter-zone preamble.
        let snap = snap_with(vec![
            zone_secs("front", 30, None),
            zone_secs("side", 30, None),
            zone_secs("back", 30, None),
        ]);

        let rec_for_stop = rec.clone();
        let stopper = async move {
            // Wait (busy, no timer) until zone k=1 ("front") has dispatched its
            // start, then trip the gate at the cycle's own start epoch.
            loop {
                if !rec_for_stop.log().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(
                rec_for_stop.log(),
                vec![("front".to_string(), 30u32)],
                "stop must land while exactly zone 1 is running"
            );
            dispatch_gate::note_stop_at(band);
        };

        let dispatch = run_dispatch(&snap, &registry, &runs, &active_runs, at(band));
        tokio::join!(dispatch, stopper);

        // Only zone 1 ever dispatched a start; zones 2 (side) and 3 (back) were
        // abandoned and never run_zone'd.
        assert_eq!(
            rec.log(),
            vec![("front".to_string(), 30u32)],
            "zones after the stop must never dispatch a start"
        );
        // The open valve was closed: abandon_cycle calls stop_all exactly once.
        assert_eq!(
            rec.stops(),
            1,
            "mid-sequence stop must close the valve via stop_all"
        );
        // The active-run deadline ledger was cleared (valves known off).
        assert!(
            active_runs.due(i64::MAX / 2).await.unwrap().is_empty(),
            "abandon clears the deadline ledger after stop_all"
        );
        // History records the abandonment against the zone that was running.
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1, "exactly one abandon row");
        assert_eq!(rows[0].zone_slug, "front");
        assert_eq!(rows[0].status, "skipped");
        assert_eq!(
            rows[0].skip_reason.as_deref(),
            Some("Stopped manually; remaining sequence abandoned")
        );
    }

    // (c) P1-2 demotion morning: will_skip=false, a dry zone (run/soil_floor)
    // dispatches while a wet sibling (skip/global) is skipped via the widened
    // zone_skip_verdict. The marquee dispatch proof for the moat.
    #[tokio::test(start_paused = true)]
    async fn dispatch_soil_floor_runs_dry_skips_wet() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let snap = snap_with(vec![
            zone_secs("dry_bed", 1, Some(verdict("dry_bed", "run", "soil_floor"))),
            zone_secs("wet_bed", 1, Some(verdict("wet_bed", "skip", "global"))),
        ]);
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert_eq!(
            rec.log(),
            vec![("dry_bed".to_string(), 1u32)],
            "only the dry zone runs"
        );
        assert_eq!(rec.stops(), 0);
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1, "only the wet zone gets a skip row");
        assert_eq!(rows[0].zone_slug, "wet_bed");
        assert_eq!(rows[0].status, "skipped");
    }

    // (d) a zero-budget zone is never dispatched (the planned_run_seconds guard).
    #[tokio::test]
    async fn dispatch_zero_budget_zone_noop() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let snap = snap_with(vec![zone_secs("off_zone", 0, None)]);
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert!(rec.log().is_empty());
        assert_eq!(rec.stops(), 0);
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
    }

    // (e) a blanket will_skip=true returns before the loop: no dispatch, a skip
    // row per due zone (zero-budget zones excluded).
    #[tokio::test]
    async fn dispatch_blanket_skip_early_returns() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let mut snap = snap_with(vec![
            zone_secs("front", 600, None),
            zone_secs("off_zone", 0, None),
        ]);
        snap.skip_check.will_skip = true;
        snap.skip_check.reason = "Rain expected within 4h".into();
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert!(rec.log().is_empty());
        assert_eq!(rec.stops(), 0);
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1, "one skip row for the due zone only");
        assert_eq!(rows[0].zone_slug, "front");
        assert_eq!(
            rows[0].skip_reason.as_deref(),
            Some("Rain expected within 4h")
        );
    }

    #[test]
    fn zone_skip_verdict_enforces_soil_and_condition_skips_only() {
        let snap = crate::ha::snapshot::IrrigationSnapshot::default();
        // Soil-saturation skip blocks dispatch (the incident case).
        let z = zone_with(
            "back_yard_shrubs",
            Some(verdict("back_yard_shrubs", "skip", "soil_saturation")),
        );
        assert!(zone_skip_verdict(&snap, &z).is_some());
        // Custom-condition skip blocks too.
        let z = zone_with(
            "front_yard",
            Some(verdict("front_yard", "skip", "condition")),
        );
        assert!(zone_skip_verdict(&snap, &z).is_some());
        // Global-source skip on a BLANKET-skip morning (will_skip=true) is the
        // aggregate early-return's job, not the per-zone loop's.
        let mut blanket = crate::ha::snapshot::IrrigationSnapshot::default();
        blanket.skip_check.will_skip = true;
        let z = zone_with("back_yard", Some(verdict("back_yard", "skip", "global")));
        assert!(zone_skip_verdict(&blanket, &z).is_none());
        // But on a soil-floor demotion morning (will_skip=false), a wet sibling's
        // global-source skip MUST be honored here (P1-2): the aggregate did not
        // blanket-skip, so the early-return never fired and this is where the wet
        // zone gets skipped while the dry zone runs.
        let z = zone_with("back_yard", Some(verdict("back_yard", "skip", "global")));
        assert!(zone_skip_verdict(&snap, &z).is_some());
        // Run / run_extended verdicts never block.
        let z = zone_with("side_yard", Some(verdict("side_yard", "run", "global")));
        assert!(zone_skip_verdict(&snap, &z).is_none());
        let z = zone_with(
            "side_yard",
            Some(verdict("side_yard", "run_extended", "condition")),
        );
        assert!(zone_skip_verdict(&snap, &z).is_none());
        // No verdict anywhere: nothing to enforce.
        let z = zone_with("side_yard", None);
        assert!(zone_skip_verdict(&snap, &z).is_none());
    }

    #[test]
    fn zone_skip_verdict_falls_back_to_snapshot_zone_verdicts() {
        // The zone's own back-filled copy is absent but the snapshot-level
        // list has the skip: enforcement still applies.
        let mut snap = crate::ha::snapshot::IrrigationSnapshot::default();
        snap.zone_verdicts = vec![verdict("back_yard_shrubs", "skip", "soil_saturation")];
        let z = zone_with("back_yard_shrubs", None);
        let v = zone_skip_verdict(&snap, &z).expect("fallback lookup must hit");
        assert_eq!(v.source, "soil_saturation");
        // A different zone is unaffected.
        let z = zone_with("front_yard", None);
        assert!(zone_skip_verdict(&snap, &z).is_none());
    }

    #[test]
    fn freshness_gate_rejects_unrefreshed_snapshot() {
        // A never-refreshed (boot default) snapshot must not water.
        assert!(!snapshot_is_fresh(0, 1_700_000_000));
        assert!(!snapshot_is_fresh(-1, 1_700_000_000));
    }

    #[test]
    fn freshness_gate_rejects_stale_snapshot() {
        let now = 1_700_000_000;
        assert!(!snapshot_is_fresh(now - MAX_SNAPSHOT_AGE_S, now));
        assert!(!snapshot_is_fresh(now - MAX_SNAPSHOT_AGE_S - 1, now));
    }

    #[test]
    fn freshness_gate_accepts_recent_snapshot() {
        let now = 1_700_000_000;
        assert!(snapshot_is_fresh(now, now));
        assert!(snapshot_is_fresh(now - 10, now));
        assert!(snapshot_is_fresh(now - MAX_SNAPSHOT_AGE_S + 1, now));
    }

    async fn fresh_store() -> RunsStore {
        use crate::persistence::runner;
        use rusqlite::Connection;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        RunsStore::new(Arc::new(Mutex::new(c)))
    }

    /// Today's local date plus an epoch safely inside today's window
    /// (`secs` after local midnight), so tests don't flake near midnight
    /// the way "Utc::now() - 600" does.
    fn today_and_epoch(secs: i64) -> (NaiveDate, i64) {
        let today = Local::now().date_naive();
        let midnight = Local
            .from_local_datetime(&today.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap();
        (today, midnight.timestamp() + secs)
    }

    fn row(zone: &str, source: &str, start_epoch: i64) -> NewRun {
        NewRun {
            zone_slug: zone.into(),
            start_epoch,
            source: source.into(),
            controller_id: "os_main".into(),
            planned_duration_s: 300,
            skip_reason: None,
            et0_mm: None,
            etc_mm: None,
            cycle_index: None,
            cycle_count: None,
        }
    }

    #[tokio::test]
    async fn boot_dedupe_sees_completed_scheduler_runs() {
        let store = fresh_store().await;
        let (today, t0) = today_and_epoch(3600);
        assert!(
            !handled_smart_morning_today(&store, today).await,
            "empty table must not count as handled"
        );

        // A completed smart_morning run earlier today blocks catch-up.
        store
            .insert_completed(row("back_yard", "smart_morning", t0), t0 + 300, 300, None)
            .await
            .unwrap();
        assert!(handled_smart_morning_today(&store, today).await);
    }

    #[tokio::test]
    async fn boot_dedupe_ignores_stale_inputs_rows() {
        let store = fresh_store().await;
        let (today, t0) = today_and_epoch(3600);
        store
            .insert_skipped(
                row("back_yard", "smart_morning", t0),
                STALE_INPUTS_REASON.to_string(),
            )
            .await
            .unwrap();
        assert!(
            !handled_smart_morning_today(&store, today).await,
            "a stale-inputs marker must not block recovery dispatch"
        );

        // Manual UI runs are not scheduler-attributed either.
        store
            .insert_completed(row("front_yard", "manual", t0 + 100), t0 + 220, 120, None)
            .await
            .unwrap();
        assert!(!handled_smart_morning_today(&store, today).await);
    }

    #[tokio::test]
    async fn boot_dedupe_counts_skip_and_manual_stop_rows() {
        let store = fresh_store().await;
        let (today, t0) = today_and_epoch(3600);
        store
            .insert_skipped(
                row("back_yard", "smart_morning", t0),
                "Rain skip: 0.40 in today".to_string(),
            )
            .await
            .unwrap();
        assert!(handled_smart_morning_today(&store, today).await);
    }

    // ----- Interleave-era executor coverage: live-clock wait arithmetic with
    // multi-segment / soak-bearing plans, the failed[] mask + the generalized
    // disarm rule, and stop supremacy under the interleaved policy. -----

    /// Paused-clock measurement slack, in seconds. dispatch_today computes its
    /// waits from REAL Utc::now() anchors while start_paused tests auto-advance
    /// only the tokio clock, so a wait aimed at a ready time recorded in an
    /// EARLIER loop iteration undershoots on the paused clock by however many
    /// integer real seconds the test body burned between the two anchor reads
    /// (normally 0, occasionally a couple on a slow CI box). Waits whose
    /// inputs were all anchored in the SAME iteration have no such term and
    /// are asserted exactly. Any real regression in the wait arithmetic (a
    /// dropped soak, a missing preamble, a reordered plan) is off by hundreds
    /// of seconds, far outside this slack.
    const CLOCK_SLACK_S: u64 = 30;

    /// Timing-aware controller stub for the interleave-era executor tests.
    /// Every run_zone ATTEMPT (confirmed or failed) records the paused-clock
    /// instant it was dispatched at. `fail_slug_from` makes one slug error
    /// (ControllerError::Offline) from its Nth per-slug attempt on (0-based).
    /// `stop_stamp_epoch` trips the dispatch gate from INSIDE the first
    /// attempt, which is the deterministic way to land a stop "after the
    /// first dispatched segment": the stamp is already in place before the
    /// first wait's gate poll runs, so no stopper task or yield-loop is
    /// needed.
    struct TimedRecorder {
        id: String,
        calls: std::sync::Mutex<Vec<(String, u32, tokio::time::Instant)>>,
        stop_all_calls: std::sync::atomic::AtomicUsize,
        fail_slug_from: Option<(String, usize)>,
        stop_stamp_epoch: Option<i64>,
    }
    impl TimedRecorder {
        fn build(
            id: &str,
            fail_slug_from: Option<(String, usize)>,
            stop_stamp_epoch: Option<i64>,
        ) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                id: id.into(),
                calls: std::sync::Mutex::new(Vec::new()),
                stop_all_calls: std::sync::atomic::AtomicUsize::new(0),
                fail_slug_from,
                stop_stamp_epoch,
            })
        }
        fn ok(id: &str) -> std::sync::Arc<Self> {
            Self::build(id, None, None)
        }
        fn failing(id: &str, slug: &str, from_attempt: usize) -> std::sync::Arc<Self> {
            Self::build(id, Some((slug.into(), from_attempt)), None)
        }
        fn stop_stamping(id: &str, epoch: i64) -> std::sync::Arc<Self> {
            Self::build(id, None, Some(epoch))
        }
        /// (slug, duration_s, paused-clock seconds since `t0`) per attempt,
        /// in dispatch order.
        fn timeline(&self, t0: tokio::time::Instant) -> Vec<(String, u32, u64)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(s, d, at)| (s.clone(), *d, at.duration_since(t0).as_secs()))
                .collect()
        }
        fn dispatches(&self) -> Vec<(String, u32)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(s, d, _)| (s.clone(), *d))
                .collect()
        }
        fn stops(&self) -> usize {
            self.stop_all_calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait::async_trait]
    impl IrrigationController for TimedRecorder {
        fn id(&self) -> &str {
            &self.id
        }
        fn supports(&self) -> ControllerCaps {
            ControllerCaps {
                flow_meter: false,
                rain_sensor: false,
                master_valve: false,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
                water_level: false,
            }
        }
        async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
            let (attempt, first_ever) = {
                let mut calls = self.calls.lock().unwrap();
                let attempt = calls.iter().filter(|(s, _, _)| s == slug).count();
                let first_ever = calls.is_empty();
                calls.push((slug.to_string(), duration_s, tokio::time::Instant::now()));
                (attempt, first_ever)
            };
            if first_ever {
                if let Some(epoch) = self.stop_stamp_epoch {
                    dispatch_gate::note_stop_at(epoch);
                }
            }
            if let Some((fail_slug, from)) = &self.fail_slug_from {
                if slug == fail_slug.as_str() && attempt >= *from {
                    return Err(ControllerError::Offline);
                }
            }
            Ok(RunHandle {
                controller_id: self.id.clone(),
                zone_slug: slug.to_string(),
                started_epoch: Utc::now().timestamp(),
                planned_duration_s: duration_s,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, _slug: &str) -> ControllerResult<()> {
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            self.stop_all_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            Ok(ControllerStatus {
                reachable: true,
                master_enabled: None,
                water_level_pct: None,
                rain_sensor_tripped: None,
                current_program: None,
                zone_states: vec![],
                flow_gpm: None,
                flow_connected: false,
                firmware: None,
            })
        }
        async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(vec![])
        }
    }

    /// A Config under which build_cycle_plan genuinely splits: clay soil
    /// (5 mm/hr flat infiltration) under a 15 mm/hr spray gives a 20-minute
    /// max cycle, so a 2700s plan splits 3 x 900s and an 1800s plan splits
    /// 2 x 900s (mirrors cycle_soak::tests::split_clay_high_precip_spray),
    /// while 600s stays a single soak-free segment. `soak_minutes` scales the
    /// soak gaps; `interleave` picks the layout policy.
    fn cycle_soak_cfg(slugs: &[&str], soak_minutes: u32, interleave: bool) -> Arc<Config> {
        use crate::config::schema::{GrassSpecies, SoilTexture, SprinklerType, ZoneConfig};
        let mut cfg = Config::default();
        cfg.engine.soak_minutes = soak_minutes;
        cfg.engine.interleave_cycles = interleave;
        for slug in slugs {
            cfg.zones.insert(
                (*slug).to_string(),
                ZoneConfig {
                    display_name: (*slug).to_string(),
                    area_sqft: 1000.0,
                    species: GrassSpecies::StAugustine,
                    soil_texture: SoilTexture::Clay,
                    slope_pct: 0.0,
                    sun_exposure: Default::default(),
                    sprinkler_type: SprinklerType::Spray,
                    precip_rate_mm_hr: Some(15.0),
                    precip_rate_source: Default::default(),
                    root_depth_mm: None,
                    mad_pct_override: None,
                    controller_id: "os_main".into(),
                    controller_station: "1".into(),
                    soil_sensor_id: None,
                    target_min_pct_soil: 30.0,
                    saturation_pct_soil: 70.0,
                    photo_url: None,
                    weekly_budget_in: None,
                    sessions_per_week: None,
                },
            );
        }
        Arc::new(cfg)
    }

    /// run_dispatch with a real Config, so cycle plans and the layout policy
    /// come from build_cycle_plan + engine.interleave_cycles.
    async fn run_dispatch_cfg(
        snap: &crate::ha::snapshot::IrrigationSnapshot,
        registry: &ControllerRegistry,
        runs: &RunsStore,
        active_runs: &ActiveRunsStore,
        cfg: &Arc<Config>,
        now_utc: chrono::DateTime<Utc>,
    ) {
        let n = snap
            .zones
            .iter()
            .filter(|z| z.planned_run_seconds > 0)
            .count();
        let total: u64 = snap
            .zones
            .iter()
            .map(|z| z.planned_run_seconds as u64)
            .sum();
        dispatch_today(
            snap,
            registry,
            Some(runs),
            Some(active_runs),
            None, // push
            Some(cfg),
            cfg.engine.soak_minutes,
            cfg.engine.interleave_cycles,
            chrono::NaiveDate::from_ymd_opt(2026, 6, 25).unwrap(),
            now_utc,
            n,
            total,
            false, // dry_run
            false, // is_catch_up
        )
        .await;
    }

    // Multi-segment serial spacing: the live-clock waits reproduce the legacy
    // nested-loop cadence. With soak_minutes=5, front 2700s splits to
    // [900/300, 900/300, 900/0] and back 1800s to [900/300, 900/0]. Every
    // wait in the serial layout is computed against ready/valve-free times
    // anchored in the SAME loop iteration, so the paused-clock offsets are
    // exact:
    //   front#0 at 0, front#1 at 1200 (run 900 + soak 300), front#2 at 2400,
    //   back#0 at 3302 (front's final run 900 + 2s preamble), back#1 at 4502.
    #[tokio::test(start_paused = true)]
    async fn dispatch_serial_multi_segment_spacing_matches_legacy() {
        let rec = TimedRecorder::ok("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let cfg = cycle_soak_cfg(&["front", "back"], 5, false);
        let snap = snap_with(vec![
            zone_secs("front", 2700, None),
            zone_secs("back", 1800, None),
        ]);
        let t0 = tokio::time::Instant::now();
        run_dispatch_cfg(
            &snap,
            &registry,
            &runs,
            &active_runs,
            &cfg,
            at(NO_STOP_EPOCH),
        )
        .await;

        let timeline = rec.timeline(t0);
        // Legacy nested order: ALL of front's segments, then all of back's,
        // at the exact legacy offsets.
        assert_eq!(
            timeline,
            vec![
                ("front".to_string(), 900u32, 0u64),
                ("front".into(), 900, 1200),
                ("front".into(), 900, 2400),
                ("back".into(), 900, 3302),
                ("back".into(), 900, 4502),
            ]
        );
        // The same facts spelled as the minimums the executor must hold:
        // same-zone consecutive dispatches >= run + soak apart, the zone
        // switch >= run + preamble apart.
        assert!(timeline[1].2 - timeline[0].2 >= 900 + 300);
        assert!(timeline[2].2 - timeline[1].2 >= 900 + 300);
        assert!(timeline[3].2 - timeline[2].2 >= 900 + INTER_ZONE_PREAMBLE_S);
        assert!(timeline[4].2 - timeline[3].2 >= 900 + 300);
        assert_eq!(rec.stops(), 0);
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
    }

    // Interleaved ordering: with engine.interleave_cycles=true the dispatch
    // order equals interleave::plan(..., Policy::Interleaved, preamble) for
    // the same inputs, back's single soak-free cycle runs INSIDE front's
    // soak window, and front's consecutive segments still respect its soak
    // as a minimum. With soak_minutes=30, front 1800s splits to [900/1800,
    // 900/0] and back 600s stays [600/0]; the planner lays them out at
    // offsets 0 / 902 / 2700.
    #[tokio::test(start_paused = true)]
    async fn dispatch_interleaved_matches_planner_order_and_holds_soak() {
        let rec = TimedRecorder::ok("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let cfg = cycle_soak_cfg(&["front", "back"], 30, true);
        let snap = snap_with(vec![
            zone_secs("front", 1800, None),
            zone_secs("back", 600, None),
        ]);

        // The planner's own order for the same inputs, through the same
        // seams the dispatcher uses (build_cycle_plan + interleave::plan).
        let slugs = ["front", "back"];
        let secs = [1800u32, 600];
        let plans: Vec<interleave::ZonePlan> = slugs
            .iter()
            .enumerate()
            .map(|(idx, slug)| interleave::ZonePlan {
                zone_idx: idx,
                segments: build_cycle_plan(
                    Some(cfg.as_ref()),
                    slug,
                    secs[idx],
                    cfg.engine.soak_minutes,
                ),
            })
            .collect();
        let planned: Vec<(String, u32)> = interleave::plan(
            &plans,
            interleave::Policy::Interleaved,
            INTER_ZONE_PREAMBLE_S,
        )
        .iter()
        .map(|s| (slugs[s.zone_idx].to_string(), s.run_seconds))
        .collect();
        // Fixture sanity: the shape actually interleaves (back's cycle inside
        // front's soak), otherwise this degenerates to the serial case.
        assert_eq!(
            planned,
            vec![
                ("front".to_string(), 900u32),
                ("back".into(), 600),
                ("front".into(), 900),
            ]
        );

        let t0 = tokio::time::Instant::now();
        run_dispatch_cfg(
            &snap,
            &registry,
            &runs,
            &active_runs,
            &cfg,
            at(NO_STOP_EPOCH),
        )
        .await;

        let timeline = rec.timeline(t0);
        let order: Vec<(String, u32)> = timeline.iter().map(|c| (c.0.clone(), c.1)).collect();
        assert_eq!(
            order, planned,
            "dispatch order must match the interleave planner"
        );

        // back#0 fills front's soak window: dispatched exactly at front's run
        // end + preamble (same-iteration anchor, so exact).
        assert_eq!(timeline[1].2, 900 + INTER_ZONE_PREAMBLE_S);
        // front's consecutive segments stay >= run + soak apart (the soak is
        // a minimum), with back's run inside the gap.
        let front_gap = timeline[2].2 - timeline[0].2;
        assert!(
            front_gap >= 900 + 1800,
            "front's segments must hold the soak minimum, got {front_gap}s"
        );
        // Paused-clock exactness note: the wait before front#1 targets a
        // ready time recorded at front#0's REAL-clock anchor, and the real
        // clock does not advance across auto-advanced tokio sleeps. So the
        // executor re-waits front's full run+soak AFTER back's 902s slot
        // instead of landing on the planner's 2700s offset; that is the
        // live-clock rule working as designed (ready times re-derive from the
        // dispatch clock, soaks stretch but never shrink). The gap is
        // 902 + 2700 minus the few real seconds burned between the anchors.
        assert!(
            (902 + 2700 - CLOCK_SLACK_S..=902 + 2700).contains(&front_gap),
            "front#1 must fire once its live soak expiry is reached, got {front_gap}s"
        );
        assert_eq!(rec.stops(), 0);
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
    }

    // Failure path, first segment: a zone whose FIRST dispatch fails never
    // dispatches its remaining segments (failed[] mask), the other zone's
    // full plan still runs with the failure-preamble spacing, and the failed
    // zone's shutoff-deadline row is DISARMED: no step of the zone ever
    // confirmed, so no valve was commanded on and the reaper has nothing to
    // cover.
    #[tokio::test(start_paused = true)]
    async fn dispatch_failure_first_segment_masks_zone_and_disarms_deadline() {
        let rec = TimedRecorder::failing("os_main", "front", 0);
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let cfg = cycle_soak_cfg(&["front", "back"], 5, false);
        let snap = snap_with(vec![
            zone_secs("front", 1800, None), // [900/300, 900/0], all fail-masked
            zone_secs("back", 1800, None),  // [900/300, 900/0], runs in full
        ]);
        let t0 = tokio::time::Instant::now();
        run_dispatch_cfg(
            &snap,
            &registry,
            &runs,
            &active_runs,
            &cfg,
            at(NO_STOP_EPOCH),
        )
        .await;

        // front is ATTEMPTED exactly once (the failing dispatch); its second
        // segment is skipped by the mask. back runs both segments: the first
        // a bare preamble after the failure instant (the failed dispatch
        // never opened a valve, so only the 2s spacing applies), the second a
        // full run + soak later. All waits are same-iteration anchored, so
        // the offsets are exact.
        assert_eq!(
            rec.timeline(t0),
            vec![
                ("front".to_string(), 900u32, 0u64),
                ("back".into(), 900, INTER_ZONE_PREAMBLE_S),
                ("back".into(), 900, INTER_ZONE_PREAMBLE_S + 1200),
            ]
        );
        // Never-confirmed rule: front's deadline row is disarmed; back's row
        // stays armed (completion-time cleanup belongs to the reaper and the
        // run-edge observer, not the dispatcher).
        let armed = active_runs.due(i64::MAX / 2).await.unwrap();
        let slugs: Vec<&str> = armed.iter().map(|r| r.zone_slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec!["back"],
            "failed-before-confirm zone must be disarmed; the healthy zone stays armed"
        );
        assert_eq!(rec.stops(), 0, "a dispatch failure is not a stop");
        // The scheduler writes no history row for the failure (the run-edge
        // observer owns what actually happened).
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
    }

    // Failure path, second segment: once ANY segment of the zone confirmed, a
    // later failed dispatch keeps the whole-cycle shutoff deadline ARMED (the
    // confirmed segment commanded the valve on, so the reaper backstop must
    // keep covering it), while the zone's remaining segments are still
    // fail-masked and the other zone completes.
    #[tokio::test(start_paused = true)]
    async fn dispatch_failure_second_segment_keeps_deadline_armed() {
        let rec = TimedRecorder::failing("os_main", "front", 1);
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let cfg = cycle_soak_cfg(&["front", "back"], 5, false);
        let snap = snap_with(vec![
            zone_secs("front", 2700, None), // [900/300 x2, 900/0]: #0 ok, #1 fails, #2 masked
            zone_secs("back", 1800, None),  // [900/300, 900/0]
        ]);
        let t0 = tokio::time::Instant::now();
        run_dispatch_cfg(
            &snap,
            &registry,
            &runs,
            &active_runs,
            &cfg,
            at(NO_STOP_EPOCH),
        )
        .await;

        // front#0 confirms at 0; front#1 is attempted a full run + soak later
        // and fails; front#2 never dispatches. back then runs in full, its
        // first segment a bare preamble after the failure instant.
        assert_eq!(
            rec.timeline(t0),
            vec![
                ("front".to_string(), 900u32, 0u64),
                ("front".into(), 900, 1200),
                ("back".into(), 900, 1200 + INTER_ZONE_PREAMBLE_S),
                ("back".into(), 900, 2400 + INTER_ZONE_PREAMBLE_S),
            ]
        );
        // Generalized disarm rule, the KEEP side: front had a confirmed
        // segment, so its deadline row must survive the later failure.
        let armed = active_runs.due(i64::MAX / 2).await.unwrap();
        let mut slugs: Vec<&str> = armed.iter().map(|r| r.zone_slug.as_str()).collect();
        slugs.sort_unstable();
        assert_eq!(
            slugs,
            vec!["back", "front"],
            "a zone with a confirmed segment must keep its shutoff deadline armed"
        );
        assert_eq!(rec.stops(), 0);
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
    }

    // Stop supremacy under interleave: a stop stamped DURING the first
    // dispatched segment abandons everything after it. The interleaved plan
    // would next run back's cycle inside front's soak, then front's second
    // segment; neither may dispatch. The controller stub trips the gate from
    // inside the first run_zone call, so the first wait's gate poll observes
    // it deterministically.
    #[tokio::test(start_paused = true)]
    async fn dispatch_stop_after_first_segment_interleaved_abandons_rest() {
        let (_serialize, band) = claim_stop_band().await;
        let rec = TimedRecorder::stop_stamping("os_main", band);
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let cfg = cycle_soak_cfg(&["front", "back"], 30, true);
        let snap = snap_with(vec![
            zone_secs("front", 1800, None), // [900/1800, 900/0]
            zone_secs("back", 600, None),   // [600/0], planned inside the soak
        ]);
        run_dispatch_cfg(&snap, &registry, &runs, &active_runs, &cfg, at(band)).await;

        // Only front's first segment ever dispatched: no back run inside the
        // soak, no front#1.
        assert_eq!(rec.dispatches(), vec![("front".to_string(), 900u32)]);
        // The open valve was closed and the deadline ledger cleared.
        assert_eq!(rec.stops(), 1, "abandon must stop_all exactly once");
        assert!(
            active_runs.due(i64::MAX / 2).await.unwrap().is_empty(),
            "abandon clears the deadline ledger after a confirmed stop_all"
        );
        // The abandonment is recorded against the running zone with its cycle
        // position (segment 0 of front's 2-segment plan).
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1, "exactly one abandon row");
        assert_eq!(rows[0].zone_slug, "front");
        assert_eq!(rows[0].status, "skipped");
        assert_eq!(
            rows[0].skip_reason.as_deref(),
            Some("Stopped manually; remaining sequence abandoned")
        );
        assert_eq!(rows[0].cycle_index, Some(0));
        assert_eq!(rows[0].cycle_count, Some(2));
    }
}
