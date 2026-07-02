// Smart morning dispatcher. The LocalSky-native replacement for
// Irrigation Unlimited's nightly sequence. Spawned from main.rs
// alongside the manual scheduler.
//
// Algorithm per tick (every 60s):
//   1. Compute today's local sunrise from (lat, lon) using NOAA's
//      analytical formula (no extra crates needed).
//   2. Snapshot the current IrrigationSnapshot. Sum planned_run_seconds
//      across zones to get the sequence total. Inter-zone preamble is
//      a fixed 2s, matching the IU controller's `preamble: "00:00:02"`.
//   3. target_finish = sunrise - 15min (matches IU's `anchor: finish,
//      sun: sunrise, before: 00:15`). target_start = target_finish -
//      sequence_total.
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
//      zones get cycle-and-soak treatment, then dispatch each segment
//      sequentially via controller.run_zone(slug, seg.run_seconds).
//      Each confirmed segment is recorded in the runs table (source
//      "smart_morning") so restarts can dedupe against completed work.
//      Sleep seg.soak_seconds between segments and
//      INTER_ZONE_PREAMBLE_S between zones. The waits poll
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
    _watering_policy: WateringPolicy,
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
            let sequence_total_s =
                total_dispatch_s + INTER_ZONE_PREAMBLE_S * (zones_to_run.saturating_sub(1)) as u64;

            let sunrise = match sunrise_utc(today, lat, lon) {
                Some(s) => s,
                None => {
                    continue;
                }
            };
            let target_finish = sunrise - chrono::Duration::minutes(15);
            let target_start = target_finish - chrono::Duration::seconds(sequence_total_s as i64);

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
            dispatch_today(
                &snap,
                &controllers,
                runs.as_ref(),
                active_runs.as_ref(),
                push.as_ref(),
                cfg.as_ref(),
                today,
                now_utc,
                zones_to_run,
                total_dispatch_s,
                dry_run,
                late,
            )
            .await;
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

    let soak_minutes = cfg.as_ref().map(|c| c.engine.soak_minutes).unwrap_or(30);

    // Manual Stop / Stop All / pause requests at or after this instant
    // abandon the remainder of the sequence.
    let cycle_start_epoch = now_utc.timestamp();

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
        let duration_s = zone.planned_run_seconds;

        // Build the cycle-and-soak plan if we have enough cfg context;
        // otherwise dispatch the zone in one shot.
        let segments = build_cycle_plan(cfg, &zone.slug, duration_s, soak_minutes);

        // P0-1b: arm the persisted shutoff deadline for the WHOLE zone cycle
        // (all run + soak segments), not per segment: the valve legitimately
        // cycles on and off within the cycle, so a per-segment deadline would make
        // the reaper fire during every soak. Deadline = cycle end + grace; the
        // reaper enforces a stop only if the valve is still commanded on past the
        // entire cycle (i.e. a controller self-shutoff genuinely failed).
        if !dry_run {
            if let Some(ar) = active_runs {
                let now = Utc::now().timestamp();
                let cycle_s: i64 = segments
                    .iter()
                    .map(|s| (s.run_seconds + s.soak_seconds) as i64)
                    .sum();
                if let Err(e) = ar
                    .arm(
                        zone.slug.clone(),
                        controller.id().to_string(),
                        now,
                        now + cycle_s + ACTIVE_RUN_GRACE_S,
                    )
                    .await
                {
                    warn!(zone = %zone.slug, error = %e, "active-run arm failed");
                }
            }
        }

        for (idx, seg) in segments.iter().enumerate() {
            if dry_run {
                info!(
                    zone = %zone.slug,
                    segment = idx,
                    of = segments.len(),
                    run_s = seg.run_seconds,
                    soak_s = seg.soak_seconds,
                    "smart morning [DRY_RUN]: would dispatch segment"
                );
                continue;
            }
            if dispatch_gate::stop_requested_since(cycle_start_epoch) {
                abandon_cycle(
                    controller.as_ref(),
                    runs,
                    active_runs,
                    &zone.slug,
                    duration_s,
                )
                .await;
                return;
            }
            // P0-8: serialize this run_zone dispatch on the zone against the
            // manual API path + manual scheduler, sharing one lock registry.
            // Held only across the dispatch (per segment, not the whole cycle),
            // so a concurrent manual run on this zone is never blocked for the
            // length of the cycle and a Stop is never blocked at all.
            let run_result = {
                let lock = crate::controllers::zone_run_lock(&zone.slug);
                let _run_serialize = lock.lock().await;
                controller.run_zone(&zone.slug, seg.run_seconds).await
            };
            match run_result {
                Ok(handle) => {
                    info!(
                        zone = %zone.slug,
                        segment = idx,
                        of = segments.len(),
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
                    // Completed work is recorded by the snapshot run-edge
                    // observer (history::ingest), which measures what the
                    // hardware actually did. Writing a planned-duration row
                    // here too double-counted every segment in History, so
                    // the scheduler only records what the observer cannot
                    // see: skips, missed windows, and manual stops.
                }
                Err(e) => {
                    warn!(
                        zone = %zone.slug,
                        segment = idx,
                        error = %e,
                        "smart morning: controller dispatch failed"
                    );
                    if !failure_notified {
                        if let Some(p) = push {
                            p.emit(PushEvent::DailyVerdict {
                                verdict: "skip".into(),
                                reason: format!(
                                    "Watering dispatch failed for {}: {}. Check the controller connection.",
                                    zone.slug, e
                                ),
                            });
                        }
                        failure_notified = true;
                    }
                    // P0-1b: the whole-cycle shutoff deadline was armed for this
                    // zone before the segment loop, but this segment's dispatch
                    // failed so no valve was commanded on. Disarm it before we
                    // break, otherwise the reaper later wakes on a deadline for a
                    // run that never started and logs a misleading "enforcing
                    // shutoff" line (it self-heals, but the log is wrong).
                    if let Some(ar) = active_runs {
                        if let Err(e) = ar.disarm(&zone.slug).await {
                            warn!(zone = %zone.slug, error = %e, "active-run disarm after dispatch failure failed");
                        }
                    }
                    break;
                }
            }
            let wait = seg.run_seconds as u64 + seg.soak_seconds as u64;
            if wait_unless_stopped(wait, cycle_start_epoch).await {
                abandon_cycle(
                    controller.as_ref(),
                    runs,
                    active_runs,
                    &zone.slug,
                    duration_s,
                )
                .await;
                return;
            }
        }
        if !dry_run {
            // Inter-zone preamble between zone N's last segment and
            // zone N+1's first segment. The last segment's soak is 0
            // so this is the only spacing here.
            if wait_unless_stopped(INTER_ZONE_PREAMBLE_S, cycle_start_epoch).await {
                abandon_cycle(
                    controller.as_ref(),
                    runs,
                    active_runs,
                    &zone.slug,
                    duration_s,
                )
                .await;
                return;
            }
        }
    }
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
/// does not re-water.
async fn abandon_cycle(
    controller: &dyn IrrigationController,
    runs: Option<&RunsStore>,
    active_runs: Option<&ActiveRunsStore>,
    current_zone: &str,
    planned_duration_s: u32,
) {
    warn!(
        zone = current_zone,
        "smart morning: manual stop requested; abandoning the rest of the sequence"
    );
    if let Err(e) = controller.stop_all().await {
        warn!(error = %e, "smart morning: stop_all after manual stop failed");
    }
    // P0-1b: stop_all physically closed every valve, so clear the deadline ledger
    // (no reaper backstop is needed for valves now known off).
    if let Some(ar) = active_runs {
        let _ = ar.clear_all().await;
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
            cycle_index: None,
            cycle_count: None,
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
    cfg: Option<&Arc<Config>>,
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
    cycle_soak::split(
        duration_s,
        precip,
        z.soil_texture,
        z.slope_pct,
        soak_minutes,
    )
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
        ControllerCaps, ControllerResult, ControllerStatus, RunHandle, RunRecord,
    };
    use std::sync::atomic::Ordering;

    // dispatch_gate's LAST_STOP_EPOCH is process-global + monotonic, and the lib
    // test binary runs these concurrently, so the epochs are ordered so no test's
    // stamp poisons another's gate check:
    //   STOP_EPOCH (low)  -- stamped by the before-cycle abandon test.
    //   MID_CYCLE_EPOCH   -- the mid-sequence test's cycle start; it stamps THIS
    //                        value AFTER zone k dispatches, so the gate is below it
    //                        at loop start (zone k runs) and at-or-above it for the
    //                        wait after zone k (the remainder is abandoned).
    //   NO_STOP_EPOCH (highest) -- the no-stop tests' cycle start. It sits above
    //                        every stamp any sibling test makes, so
    //                        stop_requested_since(NO_STOP_EPOCH) stays false for
    //                        them regardless of interleaving.
    // Each test gets its own in-memory DB, so row assertions use a wide window
    // (abandon_cycle stamps real Utc::now()).
    const STOP_EPOCH: i64 = 1_000_000_000; // ~year 2001
    const MID_CYCLE_EPOCH: i64 = 15_000_000_000; // ~year 2445
    const NO_STOP_EPOCH: i64 = 100_000_000_000; // ~year 5138 (above every stamp)
    const WIDE: (i64, i64) = (0, 999_999_999_999);

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

    fn registry_with(rec: &std::sync::Arc<DispatchRecorder>) -> ControllerRegistry {
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
            None, // push
            None, // cfg -> single segment, no soak
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
    // recorded, then stamps the gate at MID_CYCLE_EPOCH. Because the stopper is
    // runnable, the runtime cannot auto-advance the dispatch's post-zone-1 sleep
    // until the stamp is in place; when the sleep then resolves, wait_unless_stopped
    // observes the stop and abandon_cycle fires. k=1 of N=3 here: zones 2 and 3
    // must never dispatch.
    #[tokio::test(start_paused = true)]
    async fn dispatch_stop_mid_sequence_abandons_remainder_and_closes_valve() {
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
            dispatch_gate::note_stop_at(MID_CYCLE_EPOCH);
        };

        let dispatch = run_dispatch(&snap, &registry, &runs, &active_runs, at(MID_CYCLE_EPOCH));
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
}
