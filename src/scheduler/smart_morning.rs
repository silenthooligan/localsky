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
//   6. If snapshot.skip_check.will_skip, the yard holds, with exactly
//      ONE exception: a hold whose reason_code is "restrictions" does
//      not bind a zone the engine judged source:"exempt" (the ordinance
//      stands aside for that zone's head), because for that zone the
//      engine re-ran the WHOLE ladder with only the restrictions that
//      bind it and it still came back "run". Every other due zone gets a
//      skip row with source = "smart_morning" and its own reason, and
//      drops out of the morning. If nothing is exempt: mark fired and
//      return. Nothing else escapes a yard-wide hold, and in particular
//      a bare per-zone verdict of "run" does NOT: `decide_per_zone`'s
//      baseline is `global_verdict`, which omits the aggregate ladder's
//      soil-saturation rung and knows nothing of the user Rhai script
//      pass, so "run" there is not a claim that every gate passed.
//   7. Iterate the zones still due. A zone whose per-zone verdict is a
//      non-global "skip" (soil saturation, custom condition) is recorded
//      as a skip row with that reason and NOT dispatched; the zones a
//      global skip held are already recorded and gone (step 6). For each
//      remaining zone:
//      split the zone's runtime via engine::cycle_soak so clay-soil
//      zones get cycle-and-soak treatment, lay the segments out via
//      engine::interleave (interleaved by default: with
//      engine.interleave_cycles, other zones' cycles run during a
//      zone's soak window, one valve at a time, soaks treated as
//      minimums), then dispatch each planned step through
//      controllers::dispatch (the same path the manual paths take,
//      with the whole-cycle deadline armed first). Waits between steps
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
// morning window never double-waters. A step-6 partial hold is the one
// row that does NOT mean that: those rows carry PARTIAL_HOLD_NOTE and
// settle only their own zone, because the exempt zones still owe water.
// Past target_finish + CATCH_UP_GRACE with nothing recorded, a
// missed-window row is logged per zone and the day is marked so the
// loop doesn't retry.
//
// LOCALSKY_SMART_DRY_RUN=1: skip the actual dispatch; info!-log
// what would have fired. Used to validate dispatch behavior overnight
// before flipping IU off.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeZone;
use chrono::{NaiveDate, Utc};
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::controllers::dispatch::{Arm, Dispatcher, RunOutcome, RunRequest, Source};
use crate::controllers::reaper::effective_run_grace;
use crate::controllers::registry::ControllerRegistry;
use crate::engine::cycle_soak;
use crate::engine::interleave;
use crate::engine::sprinkler_catalog::effective_precip_rate_mm_hr;
use crate::engine::sunrise::sunrise_utc;
use crate::persistence::runs::{NewRun, RunsStore};
use crate::persistence::ActiveRunsStore;
use crate::ports::irrigation_controller::IrrigationController;
use crate::push::dispatcher::{PushDispatcher, PushEvent};
use crate::refresher::IrrigationStore;
use crate::refresher::{WateringPolicy, ZoneAgronomyCfg};
use crate::scheduler::dispatch_gate;

// The whole-cycle shutoff deadline grace (base + widened device-wide-stop
// variants) is shared with the manual arm sites and lives beside its
// enforcement: crate::controllers::reaper::effective_run_grace.

/// Width of the "we are at target_start" window, in seconds. The tick
/// interval is 60s so a 90s tolerance guarantees exactly one match per
/// day even with small clock drift.
const TARGET_WINDOW_S: i64 = 90;

/// Inter-zone preamble in seconds. Matches IU's `preamble: "00:00:02"`
/// so the dispatch cadence is observable-equivalent to the prior IU
/// sequence the OS hardware was tuned against.
use crate::engine::sequence::INTER_ZONE_PREAMBLE_S;

/// Catch-up grace window after target_finish. If LocalSky booted late
/// (or stalled across the window) and there's still daylight between
/// the dispatch window and the SJRWMD forbidden-hour cutoff (typically
/// 10am), we can still get a useful run in. Two hours is enough to land
/// before 10am for a sunrise around 06:30 with a 1500s sequence.
const CATCH_UP_GRACE_S: i64 = 2 * 3600;

use super::snapshot_is_fresh;
#[cfg(test)]
use super::MAX_SNAPSHOT_AGE_S;

/// Skip-row reason recorded when the freshness gate blocks dispatch.
/// The boot dedupe ignores rows with this reason so a recovered
/// refresher (or a restart) can still water the same morning.
const STALE_INPUTS_REASON: &str = "stale inputs";

/// Prefix of the skip-row reason recorded when the controller itself
/// refused the dispatch. The boot dedupe ignores these rows for the same
/// reason it ignores STALE_INPUTS_REASON: a refusal applied no water, so a
/// restart inside the catch-up grace must still be able to water. This is
/// the behavior the release before it had, where the failure path wrote no
/// row at all, while keeping the failure visible in History.
///
/// The `watered_zones >= 2` signal is what suppresses a re-fire once water
/// actually landed, and it is day-wide, not per zone: a controller that
/// died partway through a sequence leaves two watered zones marking the
/// WHOLE morning handled, so the zones that never ran wait for tomorrow.
/// Making that promise unconditional needs a zone-aware catch-up, not a
/// looser guard here; pinned by
/// `boot_dedupe_partial_sequence_marks_the_whole_day_handled` and stated in
/// the changelog and the troubleshooting guide.
///
/// The one thing that must never happen on any of these paths, re-opening a
/// valve that is already open, is guarded at the dispatch site by
/// `already_running` rather than by this reason string.
const DISPATCH_FAILED_REASON_PREFIX: &str = "Controller dispatch failed:";

/// Appended to the History reason of a zone the yard held on a morning
/// where OTHER zones watered anyway (the restriction-exemption escape in
/// `dispatch_today`). It does two jobs, both load-bearing:
///
/// - It tells the owner the truth. "No watering on Tuesday" on a row
///   next to a zone that ran the same morning reads like a bug; this
///   says the ordinance holds THIS zone, not the yard.
/// - The boot dedupe treats it the way it treats STALE_INPUTS_REASON: a
///   row carrying it is not a marker that the morning is settled. These
///   rows are written BEFORE the first valve opens, so without this a
///   restart anywhere in the window (a container redeploy, a Watchtower
///   cycle, a crash) would mark the whole day handled and the exempt
///   zones would never water and never catch up. `held_today` below then
///   excludes those zones from the per-zone evidence sweep, since a zone
///   the morning deliberately held owes no water and would otherwise
///   keep the day forever unfinished.
const PARTIAL_HOLD_NOTE: &str = "Other zones are exempt from it and watered this morning.";

/// Days of `last_fired` dedupe entries to retain.
const LAST_FIRED_RETAIN_DAYS: i64 = 7;

/// Spawn the smart-morning dispatcher. Returns immediately; the task
/// runs for the lifetime of the process. Safe to call with location
/// = (0.0, 0.0), the formula still produces a finite sunrise; in
/// practice main.rs always passes a real lat/lon from the loaded toml.
pub fn spawn(
    irrigation_store: Arc<IrrigationStore>,
    // Hot-swappable policy handle (the same one the refresher and the manual
    // dispatcher load): the per-tick loop reads the LIVE soak_minutes +
    // interleave_cycles AND the per-zone cycle agronomy (zone_agronomy) from
    // it, so a settings save (or a tuning-report Apply of soil_texture /
    // precip_rate / sprinkler_type / slope) reshapes the next tick's window
    // math and dispatch plan with no restart.
    watering_policy: Arc<arc_swap::ArcSwap<WateringPolicy>>,
    controllers: ControllerRegistry,
    runs: Option<RunsStore>,
    active_runs: Option<ActiveRunsStore>,
    location: (f64, f64),
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
            // The calendar "today" for sunrise + the day-dedup keys off the
            // CONFIGURED timezone, not the container TZ. The dispatch window itself
            // is computed in UTC below (now_utc vs the sunrise-derived target), so
            // it stays DST-correct independently.
            let now_local = crate::timeutil::now_local();
            let today: NaiveDate = now_local.date_naive();

            // Bounded dedupe map: drop entries older than a week.
            last_fired
                .retain(|d, _| today.signed_duration_since(*d).num_days() < LAST_FIRED_RETAIN_DAYS);

            let snap = irrigation_store.snapshot();
            // TRUE wall time of the sequence, soak gaps included. The legacy
            // estimate summed only run seconds, so a cycle/soak morning
            // overshot target_finish by the total soak time. The soak/
            // interleave knobs are read from the hot-swapped policy EACH tick,
            // so a config apply reshapes today's window with no restart.
            // load_full (an owned Arc, not the guard): the policy is read
            // again below across dispatch awaits, and holding an arc-swap
            // guard that long would push writers into their slow path.
            let policy = watering_policy.load_full();
            let soak_minutes = policy.soak_minutes;
            let interleave_cycles = policy.interleave_cycles;
            let sequence_total_s = sequence_wall_seconds(
                &policy.zone_agronomy,
                &snap.zones,
                soak_minutes,
                interleave_cycles,
                policy.duration_quantum_s,
            );

            let sunrise = match sunrise_utc(today, lat, lon) {
                Some(s) => s,
                None => {
                    continue;
                }
            };
            // Pre-dawn as always, single-sourced with the refresher's
            // next_run_epoch and clamped so a soak-heavy plan never anchors
            // its start into the previous local day.
            let pre_dawn_start = match crate::engine::sunrise::smart_morning_target_start(
                today,
                lat,
                lon,
                sequence_total_s,
                crate::timeutil::deployment_calendar(),
            ) {
                Some(t) => t,
                None => {
                    continue;
                }
            };
            let pre_dawn_finish = sunrise - chrono::Duration::minutes(15);
            // Unless the refresher chose a post-sunrise window for today,
            // because the pre-dawn hours were below the freeze threshold.
            // The refresher decided under the forecast it holds; the
            // dispatcher executes that decision rather than re-deriving
            // one from data it does not have. The finish follows the live
            // sequence length so a hot-reloaded plan is still waited for.
            let today_civil = crate::engine::clock::CivilDay::from_naive(today);
            let post = snap
                .today_window
                .filter(|w| w.day == today_civil)
                .filter(|w| w.kind == crate::engine::dispatch_window::WindowKind::PostSunrise);
            let post_sunrise = post.is_some();
            let (target_start, target_finish) = match post {
                Some(w) => {
                    let start = Utc
                        .timestamp_opt(w.start, 0)
                        .single()
                        .unwrap_or(pre_dawn_start);
                    (
                        start,
                        start + chrono::Duration::seconds(sequence_total_s as i64),
                    )
                }
                None => (pre_dawn_start, pre_dawn_finish),
            };

            let now_utc = Utc::now();
            let delta_s = (now_utc - target_start).num_seconds();
            let in_window = delta_s.abs() <= TARGET_WINDOW_S;

            // Boot-time reconciliation: consult the runs table once so a
            // restart never re-fires a morning that was already handled
            // (completed runs, a skip verdict, a manual stop, or a
            // missed-window marker all count; "stale inputs" and
            // dispatch-failure rows are ignored so recovery can still
            // water a morning that applied nothing).
            if !bootstrapped {
                bootstrapped = true;
                let already_handled_today = match runs.as_ref() {
                    Some(rs) => {
                        handled_smart_morning_today(
                            rs,
                            today,
                            &snap.zones,
                            target_start.timestamp(),
                            Utc::now().timestamp(),
                        )
                        .await
                    }
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
                // A cold/stalled refresher cannot close today's journal. Retry
                // until fresh evidence can record the missed window truthfully.
                if !snapshot_is_fresh(snap.last_refresh_epoch, now_utc.timestamp()) {
                    continue;
                }
                if !dry_run {
                    if let Some(rs) = runs.as_ref() {
                        let decision = crate::persistence::daily_irrigation::from_snapshot(
                            &snap,
                            today,
                            now_utc.timestamp(),
                            true,
                        );
                        if let Err(error) = rs.daily().record(decision).await {
                            warn!(error = %error, "smart morning: missed-window journal write failed");
                            continue;
                        }
                    }
                }
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
                            session_id: None,
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
            // ticks, so at most a handful of lines per day. Single-sourced
            // with the tuning report's raised-cap window test via
            // engine::sunrise::smart_morning_available_s, so the report and
            // the dispatcher can never disagree about what fits. Sunrise
            // exists here (the earlier continue guards None), so the
            // fallback arm is unreachable; it reuses the local values.
            let available_s = crate::engine::sunrise::smart_morning_available_s(
                today,
                lat,
                lon,
                sequence_total_s,
                crate::timeutil::deployment_calendar(),
            )
            .unwrap_or_else(|| (target_finish - target_start).num_seconds());
            if post_sunrise {
                info!(
                    start = %target_start,
                    finish = %target_finish,
                    "smart morning: pre-dawn hours below the freeze threshold; watering after sunrise"
                );
            } else if available_s < sequence_total_s as i64 {
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
                                session_id: None,
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
            // Record the actual scheduled decision, including soil zones whose
            // forecast deferral left zero planned seconds. Freshness and timing
            // passed above; ordinary refreshes and watch-only runs never spend
            // the allowance. Day/zone uniqueness preserves it across restarts.
            if !dry_run {
                if let Some(rs) = runs.as_ref() {
                    let daily = crate::persistence::daily_irrigation::from_snapshot(
                        &snap,
                        today,
                        now_utc.timestamp(),
                        false,
                    );
                    if let Err(error) = rs.daily().record(daily).await {
                        warn!(error = %error, "smart morning: daily journal write failed; holding for retry");
                        continue;
                    }
                    let mut decisions = crate::engine::soil_decisions::from_snapshot(&snap);
                    for decision in &mut decisions {
                        if controllers
                            .for_zone(
                                policy
                                    .zone_controller
                                    .get(&decision.zone_slug)
                                    .map(String::as_str),
                            )
                            .is_none()
                        {
                            decision.outcome =
                                crate::engine::soil_decisions::MorningOutcome::OtherHold;
                            decision.reason_code = "no_controller".into();
                        }
                    }
                    if let Err(error) = rs
                        .soil_decisions()
                        .record_morning(today, now_utc.timestamp(), decisions)
                        .await
                    {
                        warn!(error = %error, "smart morning: decision history write failed; this morning will not spend the forecast-defer allowance");
                    }
                }
            }
            // A panic inside the dispatch (an adapter bug, a
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
                    &policy.zone_agronomy,
                    soak_minutes,
                    interleave_cycles,
                    &policy.zone_controller,
                    &policy
                        .zone_runtime
                        .iter()
                        .map(|(k, v)| (k.clone(), v.max_duration_s))
                        .collect(),
                    today,
                    now_utc,
                    dry_run,
                    late,
                    target_start.timestamp(),
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

/// A zone the controller currently reports open.
///
/// The MORNING DISPATCHER never commands such a zone on again, on either
/// the scheduled or the catch-up path. Manual runs are NOT gated by this:
/// `scheduler::manual` and the API's Run action both command the zone on
/// whatever the controller reports. The success path here deliberately
/// writes no run row (the snapshot run-edge observer records the falling
/// edge later), so a restart mid-sequence can leave the boot catch-up with
/// no evidence that this zone watered; without this guard the catch-up
/// would re-open a valve that is already open, on top of water already on
/// the ground.
///
/// `running_known` gates it because an UNKNOWN running state is not a claim
/// that the zone is running: a fire-and-forget controller (MQTT) reports
/// `running_known == false`, and treating that as "already open" would
/// silently stop those installs from ever watering.
fn already_running(z: &crate::model::ZoneState) -> bool {
    z.running && z.running_known
}

/// Does this controller report the zone running RIGHT NOW?
///
/// Only asked once the snapshot has already claimed the zone is open, and
/// only to decide whether to believe that claim, so every uncertain answer
/// keeps it: a read that fails, a controller that does not list the zone,
/// and a zone whose running state the adapter cannot determine all answer
/// yes. The claim itself can be stale because a cloud controller is polled
/// on the interval it declares (`controllers::guard::Throttled`), and a
/// zone whose manual run ended in the minute before the morning would
/// otherwise be dropped from the day with no row to explain the dry zone.
async fn still_running(controller: &Arc<dyn IrrigationController>, slug: &str) -> bool {
    match controller.status_fresh().await {
        Ok(st) => st
            .zone_states
            .iter()
            .find(|z| z.slug == slug)
            .map(|z| z.running || !z.running_known)
            .unwrap_or(true),
        Err(_) => true,
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_today(
    snap: &crate::model::IrrigationSnapshot,
    controllers: &ControllerRegistry,
    runs: Option<&RunsStore>,
    active_runs: Option<&ActiveRunsStore>,
    push: Option<&PushDispatcher>,
    // Live per-zone cycle agronomy + cycle/soak knobs, all resolved from the
    // hot-swapped watering policy by the tick loop (never a boot cfg, which
    // would pin them until a restart).
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    soak_minutes: u32,
    interleave_cycles: bool,
    // Which controller each zone is bound to (`WateringPolicy::
    // zone_controller`); the registry resolves an unbound or unknown zone
    // to the default.
    zone_controller: &HashMap<String, String>,
    // Each zone's single-run cap (`WateringPolicy::zone_runtime`), from
    // which its daily ceiling follows.
    max_duration_s: &HashMap<String, u32>,
    today: NaiveDate,
    now_utc: chrono::DateTime<Utc>,
    dry_run: bool,
    is_catch_up: bool,
    // When this morning's window opened. A catch-up counts only the water
    // applied since then, so a manual test before the window does not
    // shorten the morning and a zone that finished before the restart is
    // not watered twice.
    window_start_epoch: i64,
) {
    let controller_for = |slug: &str| {
        controllers.for_zone(
            zone_controller
                .get(crate::engine::ZoneSlug::new(slug).as_str())
                .map(String::as_str),
        )
    };

    // The zones this morning may still water. Normally every zone with
    // planned seconds; a yard-wide skip narrows it to the zones that
    // water through it, and everything below dispatches from this list.
    let mut due: Vec<&crate::model::ZoneState> = snap
        .zones
        .iter()
        .filter(|z| z.planned_run_seconds > 0)
        .collect();
    // An armed cycle found at boot does not tell us which portions were
    // valve-open time. Its explicit uncertainty marker holds only that zone
    // for this local day, rather than letting catch-up repeat unknown water.
    if let Some(rs) = runs {
        let Some((start, end)) = crate::timeutil::local_day_bounds_utc(today) else {
            return;
        };
        let rows = match rs.window(start.timestamp(), end.timestamp()).await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(%error, "smart morning: cannot verify restart holds; watering held");
                return;
            }
        };
        due.retain(|z| {
            !rows
                .iter()
                .any(|r| r.zone_slug == z.slug && restart_duration_hold(r))
        });
    }
    // Set on a partial-hold morning to the yard's own hold reason, so the
    // "running today" push says the yard is holding and only the exempt
    // zones water rather than announcing a plain run.
    let mut partial_hold_reason: Option<String> = None;

    // Decide skip vs run.
    //
    // A yard-wide skip is almost always yard-wide. The ONE morning it is
    // not is a watering restriction: an ordinance that exempts drip or
    // bubbler heads holds the sprinkler zones and says nothing about the
    // drip bed, and `decide_per_zone` re-judges such a zone by re-running
    // the WHOLE global ladder with only the restrictions that bind it
    // (skip_rules.rs, the `gcode == "restrictions"` branch). A "run" from
    // THAT branch is a positive statement that freeze, wind, rain,
    // vacation pause and the live-data fail-safe were all re-tested for
    // this zone and none of them fired, which is why it is safe to open
    // its valve while the yard holds. Nothing else is.
    //
    // In particular a bare per-zone verdict of "run" is NOT that
    // statement, and must never be read as one:
    //
    // - `decide_per_zone`'s baseline is `global_verdict`, which is
    //   pre_soil-or-post_soil and deliberately OMITS the aggregate
    //   ladder's soil_saturation rung. On an all-saturated morning that
    //   also forecasts rain the yard skips on soil_saturation while every
    //   soil-model zone reads "run" (source "soil_model"), so a blind
    //   partition would water saturated ground: the 2026-06-11 incident.
    // - The user Rhai skip pass runs against the AGGREGATE only
    //   (assembly/pass.rs) and leaves every per-zone verdict at "run", so
    //   a blind partition would silently delete an operator's own hold
    //   and water the whole yard.
    // - A per-zone force-run override returns "run" BEFORE every gate
    //   (skip_rules.rs, the override branch), so a blind partition would
    //   water through a freeze, a vacation pause and the live-data
    //   fail-safe. A convenience override must not beat a freeze; that
    //   bursts a pipe. If it is ever meant to, it needs its own change:
    //   the override path re-judging the safety ladder the way the exempt
    //   path does, a per-zone `force_overrode_guard`, and a hero that
    //   stops claiming the yard is skipping.
    //
    // Every zone that is not exempt is held, and its History row carries
    // ITS OWN reason rather than the yard's.
    //
    // KNOWN RESIDUAL, deliberately not fixed here: on an escape morning
    // `snap.skip_check.will_skip` stays true, so the hero still renders
    // OpenSkip and the seven-day strip's cell still reads as a full skip
    // while two valves open. The codebase's own fix for that shape is to
    // demote the yard verdict in assembly after `decide_per_zone` (see
    // `apply_soil_gate_inertness` and MIXED_SKIP_NOTE), which is a
    // different file; the push below is made honest so at least the
    // surface this function owns does not lie.
    if snap.skip_check.will_skip {
        let reason = if snap.skip_check.reason.is_empty() {
            "skip-rule ladder".to_string()
        } else {
            snap.skip_check.reason.clone()
        };
        let (runs_anyway, mut held): (
            Vec<&crate::model::ZoneState>,
            Vec<&crate::model::ZoneState>,
        ) = due
            .iter()
            .copied()
            .partition(|&z| zone_exempt_from_yard_skip(snap, z).is_some());
        // A hold can already have reduced the entire plan to zero. Preserve
        // its actual morning decision even then; the live verdict is not a
        // durable record and must not be used to reconstruct history later.
        held.extend(snap.zones.iter().filter(|z| {
            z.planned_run_seconds == 0 && zone_exempt_from_yard_skip(snap, z).is_none()
        }));
        // The note goes on the held rows only when a zone really did
        // water through the hold: on an ordinary blanket-skip morning the
        // rows must stay plain marker rows, or the boot dedupe stops
        // recognizing an ordinary skip morning and re-enters it.
        let partial = !runs_anyway.is_empty();
        if let Some(rs) = runs {
            for zone in held.iter().copied() {
                let row = NewRun {
                    session_id: None,
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
                // This zone's own skip reason where the engine gave it one
                // (a per-zone override, a restriction re-judged against this
                // zone's head), so History reads the same sentence the zone
                // card shows; the yard's ladder reason otherwise.
                let own_reason = zone_verdict(snap, zone)
                    .filter(|v| v.verdict == "skip" && !v.reason.is_empty())
                    .map(|v| v.reason.clone())
                    .unwrap_or_else(|| reason.clone());
                let zone_reason = if partial {
                    let head = own_reason.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
                    format!("{head}. {PARTIAL_HOLD_NOTE}")
                } else {
                    own_reason
                };
                if let Err(e) = rs.insert_skipped(row, zone_reason).await {
                    warn!(zone = %zone.slug, error = %e, "smart morning: skip-row insert failed");
                }
            }
        }
        if runs_anyway.is_empty() {
            info!(
                reason = %reason,
                zones = held.len(),
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
        // Some zones water, so the day is not a skip and gets no skip push:
        // the "running today" push follows the first confirmed dispatch, as
        // on any other morning, and carries the hold in its reason.
        //
        // warn!, not info!: for as long as the yard verdict is not demoted
        // in assembly, this line is the record that the hero and the
        // seven-day strip are saying "skipping" while valves open.
        warn!(
            reason = %reason,
            zones_held = held.len(),
            zones_running = runs_anyway.len(),
            is_catch_up,
            "smart morning: watering restriction holds the yard; dispatching the zones it \
             exempts (the hero still reads as skipping until the yard verdict is demoted \
             in assembly)"
        );
        partial_hold_reason = Some(reason.clone());
        due = runs_anyway;
    }

    let controller = match controllers.default() {
        Some(c) => c,
        None => {
            // The morning was due and nothing could water it. That used to
            // be a log line. A row per planned zone puts it in History,
            // and a push tells the operator today, not when the lawn does.
            warn!("smart morning: no default controller configured; skipping today");
            if let Some(rs) = runs {
                for zone in due.iter().copied() {
                    let row = NewRun {
                        session_id: None,
                        zone_slug: zone.slug.clone(),
                        start_epoch: now_utc.timestamp(),
                        source: "smart_morning".into(),
                        controller_id: String::new(),
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
                            "No controller is configured; nothing could water this morning"
                                .to_string(),
                        )
                        .await
                    {
                        warn!(zone = %zone.slug, error = %e, "smart morning: no-controller row insert failed");
                    }
                }
            }
            if let Some(p) = push {
                p.emit(PushEvent::ControllerOffline {
                    controller_id: "(none configured)".into(),
                    error: "no irrigation controller is configured, so this morning did not water"
                        .into(),
                });
            }
            return;
        }
    };

    // Per-zone verdict enforcement (2026-06-11 incident): decide_per_zone
    // correctly marked saturated zones "skip", but dispatch used to run
    // every zone with planned seconds anyway. Resolve the skip set up
    // front so the announced totals count only zones that will water;
    // the loop below records each skip in the runs history.
    //
    // Counted in ONE positive pass over the zones this morning may water, so
    // a zone that is both already open and verdict-skipped cannot be counted
    // (or subtracted) twice. Zones a yard-wide skip already held are not in
    // `due` at all: they were recorded above with their own reason.
    let mut per_zone_skip_count = 0usize;
    let mut zones_to_run = 0usize;
    let mut total_dispatch_s = 0u64;
    for z in due.iter().copied() {
        // An already-open zone is not dispatched either (the loop below
        // re-asks the controller), but it is not a verdict skip; it just
        // does not add to the announced totals.
        if already_running(z) {
            continue;
        }
        if zone_skip_verdict(snap, z).is_some() {
            per_zone_skip_count += 1;
            continue;
        }
        zones_to_run += 1;
        total_dispatch_s += z.planned_run_seconds as u64;
    }

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
        let body = format!("{prefix}{zones_to_run} zone(s), {total_min} min total");
        // On a partial-hold morning the notification must not read as a
        // plain run: the yard IS holding, and only the zones the
        // restriction exempts water. The one surface this function owns,
        // told the truth.
        match &partial_hold_reason {
            Some(hold) => {
                let hold = hold.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
                format!("{hold}. Only the zones it exempts water: {body}")
            }
            None => body,
        }
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
    // The stop gate's generation at the start of this cycle. A stop
    // requested from here on abandons the sequence; one requested before
    // this line belonged to whatever was running then.
    let cycle_start_epoch = dispatch_gate::generation();

    // Resolve the dispatch list up front: due zones minus per-zone verdict
    // skips, each with its cycle-and-soak plan (a single no-split segment
    // when cfg context is missing).
    struct DispatchZone<'a> {
        zone: &'a crate::model::ZoneState,
        segments: Vec<cycle_soak::CycleSegment>,
        /// The controller this zone dispatches through.
        controller: Arc<dyn IrrigationController>,
    }
    let mut dispatch: Vec<DispatchZone> = Vec::new();
    for zone in due.iter().copied() {
        // Never command open a valve the controller already reports open.
        // Checked before the verdict skip so the two stay in the same order
        // as the totals resolved above.
        if already_running(zone) {
            let zone_controller = controller_for(&zone.slug).unwrap_or_else(|| controller.clone());
            if still_running(&zone_controller, &zone.slug).await {
                info!(
                    zone = %zone.slug,
                    is_catch_up,
                    "smart morning: zone already running; not dispatching it again"
                );
                continue;
            }
            info!(
                zone = %zone.slug,
                "smart morning: the snapshot showed this zone open, the controller reports it \
                 closed; dispatching it"
            );
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
                    session_id: None,
                    zone_slug: zone.slug.clone(),
                    start_epoch: now_utc.timestamp(),
                    source: "smart_morning".into(),
                    controller_id: controller_for(&zone.slug)
                        .map(|c| c.id().to_string())
                        .unwrap_or_else(|| controller.id().to_string()),
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
        // The zone's own controller: the one it is bound to, else the
        // default. It is the authority on its own quantum; the policy's
        // copy is for estimates made without one.
        let zone_controller_arc = controller_for(&zone.slug).unwrap_or_else(|| controller.clone());
        // A catch-up gives each zone only what this morning still owes
        // it: a zone that finished before the restart is done, a zone the
        // restart cut short gets its remainder, and the rest get their
        // full plan. Judged per zone from the runs table, which the boot
        // pass and the observer both write, rather than from a count of
        // zones seen watering.
        let wanted_s = if is_catch_up {
            let done_s = match runs {
                Some(rs) => {
                    watered_since(rs, &zone.slug, window_start_epoch, now_utc.timestamp()).await
                }
                None => 0,
            };
            let remainder = zone.planned_run_seconds.saturating_sub(done_s);
            if remainder == 0 {
                info!(
                    zone = %zone.slug,
                    done_s,
                    planned_s = zone.planned_run_seconds,
                    "smart morning: catch-up finds this zone already watered this morning; not dispatching it again"
                );
                continue;
            }
            if done_s > 0 {
                info!(
                    zone = %zone.slug,
                    done_s,
                    remainder_s = remainder,
                    "smart morning: catch-up dispatches only the remainder"
                );
            }
            remainder
        } else {
            zone.planned_run_seconds
        };
        // The day's ceiling, judged against everything that already ran
        // today on this zone, catch-ups and manual runs included. A
        // refusal leaves a row; a trim plans what the day has left.
        let cap_s = max_duration_s
            .get(crate::engine::ZoneSlug::new(&zone.slug).as_str())
            .map(|d| crate::controllers::ceiling::daily_ceiling_s(*d))
            .unwrap_or_else(|| {
                crate::controllers::ceiling::daily_ceiling_s(
                    crate::config::schema::DEFAULT_MAX_RUN_MINUTES * 60,
                )
            });
        let admitted_s = match crate::controllers::ceiling::admit(
            runs,
            &zone.slug,
            cap_s,
            wanted_s,
            now_utc.timestamp(),
        )
        .await
        {
            crate::controllers::ceiling::Admission::Allow(s) => s,
            crate::controllers::ceiling::Admission::Unavailable => {
                let reason = crate::controllers::ceiling::HISTORY_UNAVAILABLE_REASON;
                warn!(zone = %zone.slug, reason, "smart morning: history unavailable; not dispatching this zone");
                if let Some(rs) = runs {
                    crate::controllers::ceiling::record_unavailable(
                        rs,
                        &zone.slug,
                        zone_controller_arc.id(),
                        "smart_morning",
                        wanted_s,
                        now_utc.timestamp(),
                    )
                    .await;
                }
                continue;
            }
            crate::controllers::ceiling::Admission::Refuse(usage) => {
                warn!(
                    zone = %zone.slug,
                    used_s = usage.used_s,
                    cap_s = usage.cap_s,
                    "smart morning: daily ceiling reached; not dispatching this zone"
                );
                if let Some(rs) = runs {
                    crate::controllers::ceiling::record_refusal(
                        rs,
                        &zone.slug,
                        zone_controller_arc.id(),
                        "smart_morning",
                        wanted_s,
                        usage,
                        now_utc.timestamp(),
                    )
                    .await;
                }
                continue;
            }
        };
        let segments = build_cycle_plan(
            agronomy,
            &zone.slug,
            admitted_s,
            soak_minutes,
            zone_controller_arc.supports().duration_quantum_s,
        );
        dispatch.push(DispatchZone {
            zone,
            segments,
            controller: zone_controller_arc,
        });
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

        // Arm the persisted shutoff deadline for the WHOLE zone cycle
        // (all remaining run + soak segments), not per segment: the valve
        // legitimately cycles on and off within the cycle, so a per-segment
        // deadline would make the reaper fire during every soak. The
        // remaining span is re-projected from LIVE ready times at every step
        // of this zone (interleaving can stretch a soak, so the up-front
        // plan can underestimate) and the deadline only ever EXTENDS:
        // arm() keeps MAX(off_deadline_epoch), and the write is skipped
        // entirely when the projection has not drifted later.
        let now = Utc::now().timestamp();
        let mut arm_deadline: Option<i64> = None;
        if active_runs.is_some() {
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
                let grace = effective_run_grace(dz.controller.supports().per_zone_stop);
                let deadline = now + end_in as i64 + grace;
                if armed_deadline[step.zone_idx].is_none_or(|d| deadline > d) {
                    arm_deadline = Some(deadline);
                }
            }
        }

        if dispatch_gate::stop_requested_since(cycle_start_epoch) {
            abandon_cycle(
                controllers,
                dz.controller.id(),
                runs,
                active_runs,
                &dz.zone.slug,
                dz.zone.planned_run_seconds,
                cycle_pos,
            )
            .await;
            return;
        }
        // The shared path: per-zone lock, clamp, the whole-cycle deadline
        // armed BEFORE the valve is commanded (a per-segment deadline would
        // fire during every soak), the run row only where no run-edge
        // observer records this controller, and the failure push. A
        // failed step disarms the deadline only while no segment of this
        // zone has ever confirmed: once one has, the deadline is the only
        // backstop for a valve whose own shutoff may be what is failing.
        let outcome = Dispatcher::new(controllers.zone_locks(), runs, active_runs)
            .run(RunRequest {
                session_id: format!("smart:{today}:{}", dz.zone.slug),
                zone: &dz.zone.slug,
                zone_name: &dz.zone.name,
                controller: &dz.controller,
                seconds: seg.run_seconds,
                source: Source::SmartMorning,
                // The plan already sized every segment against the cap.
                ceiling_s: None,
                arm: Arm::BeforeDispatch {
                    deadline: arm_deadline,
                    disarm_on_failure: !confirmed[step.zone_idx],
                },
                record_row: !dz.zone.running_known,
                cycle: cycle_pos,
                push,
                now_epoch: now,
            })
            .await;
        // Remember the deadline only when the ledger took it, so a failed
        // write is retried on the zone's next step instead of assumed.
        if let Some(d) = arm_deadline {
            let armed = match &outcome {
                RunOutcome::Dispatched { deadline_armed, .. }
                | RunOutcome::Failed { deadline_armed, .. } => *deadline_armed,
                RunOutcome::Refused(_) => false,
            };
            if armed {
                armed_deadline[step.zone_idx] = Some(d);
            }
        }
        // The wait after this step anchors on the real post-dispatch clock,
        // so serial spacing matches the legacy dispatcher exactly and soak
        // minimums self-correct under dispatch latency.
        //
        // And it waits for what the controller SAID it would run, not for
        // what we asked. A cloud adapter that takes whole minutes rounds
        // 200 s up to 240 s; waiting 200 s would open the next valve while
        // this one is still running.
        let anchor: i64;
        let mut ran_s = seg.run_seconds;
        match outcome {
            RunOutcome::Refused(_) => unreachable!("smart morning passes no ceiling"),
            RunOutcome::Dispatched {
                handle, seconds, ..
            } => {
                info!(
                    zone = %dz.zone.slug,
                    segment = step.seg_idx,
                    of = dz.segments.len(),
                    run_s = seg.run_seconds,
                    sent_s = seconds,
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
                ran_s = seconds;
                confirmed[step.zone_idx] = true;
                ready_at[step.zone_idx] = anchor + ran_s as i64 + seg.soak_seconds as i64;
                valve_free_at = anchor + ran_s as i64;
            }
            RunOutcome::Failed { error: e, .. } => {
                // The dispatcher logged the failure; this names the segment.
                warn!(
                    zone = %dz.zone.slug,
                    segment = step.seg_idx,
                    of = dz.segments.len(),
                    "smart morning: segment not dispatched"
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
                failed[step.zone_idx] = true;
                // A failed dispatch used to write NOTHING: no run row, no
                // skip row, nothing. A morning that threw was indis-
                // tinguishable in History from a morning that planned
                // nothing, which is how weeks of failures left zero
                // evidence anywhere. Record it with the controller's own
                // error text, once per zone (the loop skips a failed
                // zone's remaining segments), as a `skipped` row so it
                // never counts as applied water.
                if let Some(rs) = runs {
                    let row = NewRun {
                        session_id: None,
                        zone_slug: dz.zone.slug.clone(),
                        start_epoch: now_utc.timestamp(),
                        source: "smart_morning".into(),
                        controller_id: dz.controller.id().to_string(),
                        planned_duration_s: dz.zone.planned_run_seconds,
                        skip_reason: None,
                        et0_mm: None,
                        etc_mm: None,
                        cycle_index: cycle_pos.map(|(i, _)| i),
                        cycle_count: cycle_pos.map(|(_, n)| n),
                    };
                    if let Err(err) = rs
                        .insert_skipped(row, format!("{DISPATCH_FAILED_REASON_PREFIX} {e}"))
                        .await
                    {
                        warn!(zone = %dz.zone.slug, error = %err, "smart morning: dispatch-failure row insert failed");
                    }
                }
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
                    ran_s as u64 + seg.soak_seconds as u64 + INTER_ZONE_PREAMBLE_S
                }
            }
        };
        if wait_unless_stopped(wait_s, cycle_start_epoch).await {
            abandon_cycle(
                controllers,
                dz.controller.id(),
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
/// `interleave_cycles` AND the per-zone `agronomy` map come from the
/// caller's LIVE watering policy (both the tick loop here and the
/// refresher's compute_next_run_epoch resolve them per evaluation), so an
/// applied zone change reshapes the estimate with no restart.
pub fn sequence_wall_seconds(
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    zones: &[crate::model::ZoneState],
    soak_minutes: u32,
    interleave_cycles: bool,
    quantum_s: u32,
) -> u64 {
    let plans: Vec<interleave::ZonePlan> = zones
        .iter()
        .filter(|z| z.planned_run_seconds > 0)
        .enumerate()
        .map(|(idx, z)| interleave::ZonePlan {
            zone_idx: idx,
            segments: build_cycle_plan(
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

/// This zone's own engine verdict, whichever copy carries it: the zone's
/// back-filled `verdict` first, falling back to the snapshot-level
/// `zone_verdicts` list. `None` for a zone the engine never judged, which
/// every caller here must read as "no per-zone opinion", never as a run.
fn zone_verdict<'a>(
    snap: &'a crate::model::IrrigationSnapshot,
    zone: &'a crate::model::ZoneState,
) -> Option<&'a crate::model::ZoneVerdict> {
    zone.verdict
        .as_ref()
        .or_else(|| snap.zone_verdicts.iter().find(|v| v.zone_slug == zone.slug))
}

/// The ONE zone shape that waters through a yard-wide hold: the yard is
/// held by a watering restriction, and the engine judged THIS zone exempt
/// from it.
///
/// The predicate names the source it accepts rather than testing the
/// verdict string, because "run" alone is not a statement that every gate
/// passed. `decide_per_zone` has four run-returning paths and only this
/// one re-judges the whole safety ladder for the zone:
///
/// - `source: "override"` returns "run" BEFORE any gate, so it would
///   water through a freeze, a vacation pause and the live-data fail-safe.
/// - `source: "soil_model"` rides through the forward-rain gates only, and
///   `decide_per_zone`'s baseline (`global_verdict`) does not even contain
///   the aggregate ladder's soil_saturation rung, so on a saturated
///   morning with forecast rain every soil zone reads "run" while the yard
///   skips on soil_saturation.
/// - `source: "soil_floor"` is a yard-agreement case: the aggregate
///   demotes on the same morning, so `will_skip` is false and this
///   function is not consulted at all.
/// - `source: "exempt"` (this one) is produced only from the
///   `gcode == "restrictions"` branch, which re-runs the ENTIRE global
///   ladder for the zone with the restrictions that bind it and returns
///   "run" only if that re-run does not skip. Freeze, wind, rain, pause
///   and live-data were all re-tested for this zone.
///
/// Both halves are required. The `reason_code` test is what keeps a user
/// Rhai skip rule from being erased: that pass runs against the aggregate
/// only (assembly/pass.rs) and leaves every per-zone verdict at "run", and
/// it can only fire on a morning the deterministic ladder called a run, so
/// no zone can carry source "exempt" then. Anything else (no verdict, a
/// different source, or a verdict that is not "run") stays held by the
/// yard, so an unexpected shape fails toward NOT watering.
fn zone_exempt_from_yard_skip<'a>(
    snap: &'a crate::model::IrrigationSnapshot,
    zone: &'a crate::model::ZoneState,
) -> Option<&'a crate::model::ZoneVerdict> {
    if snap.skip_check.reason_code != "restrictions" {
        return None;
    }
    zone_verdict(snap, zone)
        .filter(|v| matches!(v.verdict.as_str(), "run" | "run_extended") && v.source == "exempt")
}

/// The per-zone skip verdict that must block this zone's dispatch, if
/// any. Only non-global SKIP verdicts qualify: global-source skips are
/// handled for the whole run by the aggregate skip_check branch, which
/// records the zones it holds and drops them from the dispatch list (so
/// enforcing them here would double-record), and run/run_extended
/// verdicts never block.
///
/// pub(crate): the soil admission pass prices its fixed base with this
/// same predicate (against the post-inertness snapshot), so a zone this
/// verdict blocks at dispatch never occupies morning-window seconds in
/// admission either.
pub(crate) fn zone_skip_verdict<'a>(
    snap: &'a crate::model::IrrigationSnapshot,
    zone: &'a crate::model::ZoneState,
) -> Option<&'a crate::model::ZoneVerdict> {
    zone_verdict(snap, zone)
        // A per-zone skip is honored when it is NOT inherited from a blanket
        // aggregate skip (which the will_skip branch already recorded), OR
        // when the aggregate did NOT blanket-skip. The latter is the soil-floor
        // demotion morning: will_skip is false because a dry zone runs, so
        // a wet sibling's source:"global" skip must still be honored here.
        .filter(|v| v.verdict == "skip" && (v.source != "global" || !snap.skip_check.will_skip))
}

/// Sleep `secs`, polling the dispatch gate every couple of seconds.
/// Returns true when a manual stop interrupted the wait.
async fn wait_unless_stopped(secs: u64, cycle_start_epoch: u64) -> bool {
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
    controllers: &ControllerRegistry,
    current_controller_id: &str,
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
    // Every registered controller is stopped, not only the default: a
    // sequence can span two, and the one not stopped would have kept
    // watering with its backstop cleared. The dispatcher clears deadlines
    // and truncates open rows only for the controllers that CONFIRMED;
    // an unreachable one keeps its rows for the reaper to retry.
    let _report = Dispatcher::new(controllers.zone_locks(), runs, active_runs)
        .stop_all(controllers, Utc::now().timestamp())
        .await;
    if let Some(rs) = runs {
        let row = NewRun {
            session_id: None,
            zone_slug: current_zone.to_string(),
            start_epoch: Utc::now().timestamp(),
            source: "smart_morning".into(),
            controller_id: current_controller_id.to_string(),
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
/// no-split segment when the zone slug doesn't resolve in the policy's
/// agronomy map (e.g. demo mode, unconfigured install, mid-cutover
/// state). The map comes from the hot-swapped WateringPolicy, so an
/// applied texture/sprinkler/precip/slope change re-plans the very next
/// evaluation with no restart.
fn build_cycle_plan(
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    slug: &str,
    duration_s: u32,
    soak_minutes: u32,
    quantum_s: u32,
) -> Vec<cycle_soak::CycleSegment> {
    let fallback = vec![cycle_soak::CycleSegment {
        run_seconds: cycle_soak::round_up(duration_s, quantum_s),
        soak_seconds: 0,
    }];
    // Map keys are underscore-normalized by WateringPolicy::from_config;
    // callers may still hand a dashed slug, so try the normalized form too.
    let zone_cfg = agronomy
        .get(slug)
        .or_else(|| agronomy.get(&slug.replace('-', "_")));
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
        quantum_s,
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
/// manual stop, or a missed-window marker. "stale inputs" and
/// dispatch-failure rows are excluded so a restart (or refresher
/// recovery) can still water a morning that applied nothing: one was
/// blocked by the freshness gate, the other was refused by the
/// controller, and neither is a decision about the yard. Used by the
/// boot reconciliation pass so a restart inside the same morning never
/// fires the dispatch twice.
///
/// Note the boundary this draws: ANY other smart_morning row marks the
/// day handled, including a per-zone verdict skip. So a morning where NO
/// zone watered can still be handled, if even one zone carries a skip
/// row while the rest were refused by the controller. That is deliberate
/// (a skip is a decision about the yard, and the alternative is a
/// zone-aware catch-up, not a looser guard) but it means "no zone
/// watered" is not the condition; "every row is a dispatch failure" is.
/// Seconds of watering evidence for `slug` from `since_epoch` to now:
/// completed and aborted rows the rollup counts as watering, plus a row
/// still running, reduced to the interval UNION of their valve-open
/// windows. Dry-run and skip rows count for nothing.
///
/// The union rather than a sum, because a manual run is persisted TWICE on
/// any controller with state readback: the dispatcher writes its completed
/// row and the run-edge observer writes another for the same physical valve
/// window, with start epochs a tick apart so the unique index collapses
/// nothing. `history::rollup` says so where it does the same thing for the
/// water balance, and this is the same primitive the balance uses, so the
/// two surfaces cannot disagree about a morning. Summing them read a
/// ten-minute hand-started run inside the window as twenty: every planned
/// zone came out satisfied, `handled_smart_morning_today` marked the morning
/// done, and the yard silently skipped the day.
async fn watered_since(runs: &RunsStore, slug: &str, since_epoch: i64, now_epoch: i64) -> u32 {
    use crate::history::rollup::{applied_in_window, RunSegment};
    let rows = match runs.window(since_epoch, now_epoch + 1).await {
        Ok(r) => r,
        Err(e) => {
            warn!(zone = slug, error = %e, "smart morning: catch-up evidence read failed; judging nothing watered");
            return 0;
        }
    };
    let segments: Vec<RunSegment> = rows
        .iter()
        .filter(|r| r.zone_slug == slug)
        .filter(|r| {
            r.status == "running"
                || crate::history::rollup::is_watering_evidence(
                    &r.source,
                    &r.status,
                    r.skip_reason.as_deref(),
                )
        })
        .map(|r| {
            // The row's own end where it has one, else the window it was
            // commanded for, else (no duration either) up to now.
            let end_epoch = match (r.end_epoch, r.duration_s) {
                (Some(end), _) => end,
                (None, Some(d)) => r.start_epoch + i64::from(d),
                (None, None) => now_epoch,
            };
            RunSegment {
                session_id: r.session_id.clone(),
                start_epoch: r.start_epoch,
                end_epoch,
            }
        })
        .collect();
    // The window closes at the furthest segment end, not at `now`: a run row
    // is written when the valve is COMMANDED, carrying the end the
    // controller's own timer will honor, so truncating at `now` would drop
    // the tail of a run in flight and hand the caller a remainder that is
    // already on its way to the ground. Rows starting before `since_epoch`
    // are not in the query, so the window's start truncates nothing.
    let window_end = segments
        .iter()
        .map(|s| s.end_epoch)
        .fold(now_epoch, |a, b| a.max(b));
    applied_in_window(&segments, since_epoch, window_end)
        .valve_open_s
        .clamp(0, i64::from(u32::MAX)) as u32
}

fn restart_duration_hold(row: &crate::persistence::runs::RunRow) -> bool {
    row.source == "restart"
        && row.status == "skipped"
        && row.skip_reason.as_deref().is_some_and(|reason| {
            reason.starts_with(crate::controllers::reaper::RESTART_UNKNOWN_DURATION_REASON)
        })
}

async fn handled_smart_morning_today(
    runs: &RunsStore,
    today: NaiveDate,
    zones: &[crate::model::ZoneState],
    window_start_epoch: i64,
    now_epoch: i64,
) -> bool {
    // The local day's UTC bounds key off the CONFIGURED timezone, so the
    // boot dedupe window matches the same "today" the dispatch loop uses.
    let (start_utc, end_utc) = match crate::timeutil::local_day_bounds_utc(today) {
        Some(b) => b,
        // A day with no bounds at all. This used to include every day
        // whose local midnight was skipped or repeated by a clock change,
        // and returning false here says "today is NOT handled", which
        // re-arms the dispatch. A restart on such a day could therefore
        // run a second full irrigation cycle. Those days have bounds now;
        // this arm is left for a genuinely unrepresentable date, where
        // failing closed is right.
        None => return true,
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
    // (skip / missed / manual-stop; never written for stale inputs, a
    // refused dispatch, or a partial hold), or every planned zone that
    // was not held having received its planned water since this morning's
    // window opened. The old rule
    // counted zones seen watering and called two "handled", so a restart
    // after the second of five zones abandoned the other three, while a
    // manual test of one zone was worth nothing and of two everything.
    // Per-zone evidence replaces the count; a partial morning is finished
    // by the catch-up dispatch, zone by zone, remainder by remainder.
    // A partial-hold row settles ONE zone, not the morning: the exempt
    // zones still owe their water when it is written, and it is written
    // before the first valve opens. Recognized here rather than in the
    // marker test alone because the per-zone sweep below must also treat
    // the zone as done, or a morning that deliberately held a zone could
    // never be finished and every restart would re-enter it.
    let held_today: std::collections::HashSet<&str> = rows
        .iter()
        .filter(|r| {
            restart_duration_hold(r)
                || (r.source == "smart_morning"
                    && r.skip_reason
                        .as_deref()
                        .is_some_and(|s| s.ends_with(PARTIAL_HOLD_NOTE)))
        })
        .map(|r| r.zone_slug.as_str())
        .collect();
    let marker = rows.iter().any(|r| {
        let reason = r.skip_reason.as_deref().unwrap_or_default();
        r.source == "smart_morning"
            && reason != STALE_INPUTS_REASON
            && !reason.starts_with(DISPATCH_FAILED_REASON_PREFIX)
            && !reason.ends_with(PARTIAL_HOLD_NOTE)
    });
    if marker {
        return true;
    }
    let planned: Vec<&crate::model::ZoneState> =
        zones.iter().filter(|z| z.planned_run_seconds > 0).collect();
    if planned.is_empty() {
        // Nothing planned (or no snapshot yet): nothing says the morning
        // happened, and nothing will be dispatched until there is a plan.
        return false;
    }
    for z in planned {
        // A zone this morning deliberately held owes no water, so it must
        // not keep the day unfinished forever. Only the zones that were
        // NOT held decide whether the morning still has work, which on a
        // partial-hold morning is exactly the exempt ones.
        if held_today.contains(z.slug.as_str()) {
            continue;
        }
        let done = watered_since(runs, &z.slug, window_start_epoch, now_epoch).await;
        if done < z.planned_run_seconds {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Config;
    use chrono::{Local, TimeZone};

    #[test]
    fn build_cycle_plan_fallback_when_zone_unconfigured() {
        // Empty agronomy map (Default policy / demo mode) -> one no-split
        // segment, the pre-config behavior.
        let plan = build_cycle_plan(&HashMap::new(), "back_yard", 1500, 30, 1);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].run_seconds, 1500);
        assert_eq!(plan[0].soak_seconds, 0);
    }

    /// The hot-reload contract for the tuning-report Apply: a soil_texture
    /// change re-derives through WateringPolicy::from_config into the
    /// agronomy map build_cycle_plan reads, so the NEXT computed plan
    /// changes with no restart (and no boot cfg involved).
    #[test]
    fn build_cycle_plan_reads_swapped_agronomy() {
        let cfg0 = cycle_soak_cfg(&["front"], 5, false);
        let policy0 = WateringPolicy::from_config(&cfg0);
        // Clay under a 15 mm/hr spray splits a 2700s run into 3 cycles.
        let plan0 = build_cycle_plan(&policy0.zone_agronomy, "front", 2700, 5, 1);
        assert_eq!(plan0.len(), 3, "clay splits the run");
        // Apply changes the texture to sand (50 mm/hr infiltration): the
        // re-derived policy plans a single unsplit segment.
        let mut cfg1 = (*cfg0).clone();
        cfg1.zones.get_mut("front").unwrap().soil_texture =
            crate::config::schema::SoilTexture::Sand;
        let policy1 = WateringPolicy::from_config(&cfg1);
        let plan1 = build_cycle_plan(&policy1.zone_agronomy, "front", 2700, 5, 1);
        assert_eq!(plan1.len(), 1, "sand infiltrates faster than the spray");
    }

    /// Dashed config keys normalize to underscores in the policy map; a
    /// runtime slug in either form must resolve the same zone.
    #[test]
    fn build_cycle_plan_resolves_dashed_and_underscored_slugs() {
        let mut cfg = Config::default();
        cfg.engine.soak_minutes = 5;
        let zone = cycle_soak_cfg(&["placeholder"], 5, false).zones["placeholder"].clone();
        cfg.zones.insert("back-yard".to_string(), zone);
        let policy = WateringPolicy::from_config(&cfg);
        let via_underscore = build_cycle_plan(&policy.zone_agronomy, "back_yard", 2700, 5, 1);
        let via_dash = build_cycle_plan(&policy.zone_agronomy, "back-yard", 2700, 5, 1);
        assert_eq!(via_underscore.len(), 3);
        assert_eq!(via_underscore, via_dash);
    }

    fn verdict(slug: &str, verdict: &str, source: &str) -> crate::model::ZoneVerdict {
        crate::model::ZoneVerdict {
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

    fn zone_with(slug: &str, v: Option<crate::model::ZoneVerdict>) -> crate::model::ZoneState {
        crate::model::ZoneState {
            slug: slug.into(),
            name: slug.into(),
            planned_run_seconds: 600,
            verdict: v,
            ..Default::default()
        }
    }

    // ── dispatch_today actuation + fail-safe integration tests ─────────
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
    /// the dispatch tests.
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
                per_zone_stop: true,
                duration_quantum_s: 1,
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
                observed_epoch: None,
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
        v: Option<crate::model::ZoneVerdict>,
    ) -> crate::model::ZoneState {
        crate::model::ZoneState {
            slug: slug.into(),
            name: slug.into(),
            planned_run_seconds: secs,
            verdict: v,
            // A controller with state readback, the assumption every
            // timeline test here was written under: the observer records
            // its runs, so the dispatcher writes no run rows of its own.
            running_known: true,
            ..Default::default()
        }
    }

    /// A zone on a controller that cannot report state: no observer will
    /// record its runs, so the dispatcher must.
    fn zone_secs_no_readback(slug: &str, secs: u32) -> crate::model::ZoneState {
        crate::model::ZoneState {
            running_known: false,
            ..zone_secs(slug, secs, None)
        }
    }

    fn at(epoch: i64) -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(epoch, 0).unwrap()
    }

    fn snap_with(zones: Vec<crate::model::ZoneState>) -> crate::model::IrrigationSnapshot {
        let mut s = crate::model::IrrigationSnapshot::default();
        s.zones = zones;
        s
    }

    async fn run_dispatch(
        snap: &crate::model::IrrigationSnapshot,
        registry: &ControllerRegistry,
        runs: &RunsStore,
        active_runs: &ActiveRunsStore,
        now_utc: chrono::DateTime<Utc>,
    ) {
        dispatch_gate::isolated(dispatch_today(
            snap,
            registry,
            Some(runs),
            Some(active_runs),
            None,            // push
            &HashMap::new(), // empty agronomy -> single segment, no soak
            30,              // soak_minutes (policy default)
            false,           // interleave_cycles
            &HashMap::new(), // no zone bindings -> the default controller
            &HashMap::new(), // no per-zone caps -> the default ceiling
            chrono::NaiveDate::from_ymd_opt(2026, 6, 25).unwrap(),
            now_utc,
            false, // dry_run
            false, // is_catch_up
            0,     // window_start_epoch
        ))
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
        // A clean morning writes no scheduler row: completed work is recorded
        // by the run-edge observer.
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
        let policy = WateringPolicy::from_config(&cfg);
        dispatch_today(
            &snap,
            &registry,
            Some(&runs),
            Some(&active_runs),
            None, // push
            &policy.zone_agronomy,
            cfg.engine.soak_minutes,
            cfg.engine.interleave_cycles,
            &policy.zone_controller,
            &policy
                .zone_runtime
                .iter()
                .map(|(k, v)| (k.clone(), v.max_duration_s))
                .collect(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 25).unwrap(),
            at(NO_STOP_EPOCH),
            false, // dry_run
            false, // is_catch_up
            0,     // window_start_epoch
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
    fn sequence_wall_seconds_matches_legacy_without_agronomy() {
        // Empty agronomy map -> single no-split segments: the wall time
        // equals the legacy sum(planned) + preamble * (zones - 1),
        // zero-budget zones excluded, identically under both policies.
        let zones = vec![
            zone_secs("front", 600, None),
            zone_secs("off_zone", 0, None),
            zone_secs("side", 300, None),
        ];
        let empty: HashMap<String, ZoneAgronomyCfg> = HashMap::new();
        assert_eq!(
            sequence_wall_seconds(&empty, &zones, 30, false, 1),
            600 + 300 + 2
        );
        assert_eq!(
            sequence_wall_seconds(&empty, &zones, 30, true, 1),
            600 + 300 + 2
        );
        assert_eq!(sequence_wall_seconds(&empty, &[], 30, false, 1), 0);
    }

    // (b) a Stop requested BEFORE the cycle begins belongs to whatever was
    // running then, not to this morning: the sequence proceeds in full and
    // nothing is abandoned. The gate used to compare wall-clock epochs and
    // let a stop in the same second (or, on a host whose clock read the
    // future, any later second) abandon a morning it predated.
    #[tokio::test]
    async fn a_stop_before_the_cycle_does_not_abandon_it() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let snap = snap_with(vec![
            zone_secs("front", 1, None),
            zone_secs("side", 1, None),
        ]);
        dispatch_gate::note_stop_at(STOP_EPOCH);
        run_dispatch(&snap, &registry, &runs, &active_runs, at(STOP_EPOCH)).await;
        assert_eq!(rec.log().len(), 2, "both zones dispatch: {:?}", rec.log());
        assert_eq!(rec.stops(), 0, "nothing to abandon");
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
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
        dispatch_gate::isolated(async {
            tokio::join!(dispatch, stopper);
        })
        .await;

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

    // (c) demotion morning: will_skip=false, a dry zone (run/soil_floor)
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

    // (e) a blanket will_skip=true with no zone escaping it returns before the
    // loop: no dispatch, including a recorded hold for zero-minute zones.
    #[tokio::test]
    async fn dispatch_blanket_skip_early_returns() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let mut snap = snap_with(vec![
            zone_secs("front", 600, None),
            zone_secs("off_zone", 0, None),
        ]);
        snap.skip_check.decide(
            "skip",
            snap.skip_check.reason.clone(),
            snap.skip_check.reason_code.clone(),
        );
        snap.skip_check.reason = "Rain expected within 4h".into();
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert!(rec.log().is_empty());
        assert_eq!(rec.stops(), 0);
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(
            rows.len(),
            2,
            "zero-minute plans must retain their hold reason"
        );
        assert!(rows
            .iter()
            .all(|r| r.skip_reason.as_deref() == Some("Rain expected within 4h")));
    }

    #[tokio::test]
    async fn a_zero_minute_held_morning_is_durable_without_opening_a_valve() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let mut snap = snap_with(vec![zone_secs("front", 0, None)]);
        snap.skip_check.decide(
            "skip",
            "Rain is forecast".into(),
            "rain_today_forecast".into(),
        );
        let now = at(NO_STOP_EPOCH);
        run_dispatch(&snap, &registry, &runs, &active_runs, now).await;
        assert!(rec.log().is_empty());
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].skip_reason.as_deref(), Some("Rain is forecast"));
        let today = crate::timeutil::local_date(now.timestamp()).unwrap();
        assert!(
            handled_smart_morning_today(
                &runs,
                today,
                &snap.zones,
                now.timestamp(),
                now.timestamp()
            )
            .await
        );
    }

    // (e2) the ONE morning a yard-wide hold is not yard-wide: a watering
    // restriction that exempts this zone's head. `decide_per_zone` re-runs
    // the whole global ladder for that zone with only the rules that bind
    // it, so its "run" is a statement that freeze, wind, rain, pause and
    // live-data were all re-tested. It waters; the zone the ordinance does
    // bind does not, and its row carries ITS OWN reason plus the note that
    // says the hold was partial. The dispatcher used to early-return on the
    // aggregate, so the exempt zone read as running on its card while its
    // valve stayed shut.
    #[tokio::test(start_paused = true)]
    async fn dispatch_restriction_hold_runs_only_the_exempt_zone() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let exempt = crate::model::ZoneVerdict {
            reason: "Exempt from the restriction holding the yard. (No watering on Tuesday)".into(),
            ..verdict("drip_bed", "run", "exempt")
        };
        let bound = crate::model::ZoneVerdict {
            reason: "No watering on Tuesday".into(),
            ..verdict("back", "skip", "global")
        };
        let mut snap = snap_with(vec![
            zone_secs("drip_bed", 1, Some(exempt)),
            zone_secs("back", 1, Some(bound)),
        ]);
        snap.skip_check.decide(
            "skip",
            "Watering restriction: no watering on Tuesday".to_string(),
            "restrictions".to_string(),
        );
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert_eq!(
            rec.log(),
            vec![("drip_bed".to_string(), 1u32)],
            "the exempt zone waters through the restriction; nothing else does"
        );
        assert_eq!(rec.stops(), 0);
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1, "only the held zone gets a skip row");
        assert_eq!(rows[0].zone_slug, "back");
        assert_eq!(rows[0].status, "skipped");
        assert_eq!(
            rows[0].skip_reason.as_deref(),
            Some(
                "No watering on Tuesday. Other zones are exempt from it and watered this morning."
            ),
            "the held zone's row names why THAT zone is dry, and says the hold was partial"
        );
    }

    // (e2b) a per-zone force-run override does NOT water through a yard-wide
    // hold. `decide_per_zone` returns that verdict BEFORE every gate, so
    // honoring it would open a valve through a freeze, a vacation pause and
    // the live-data fail-safe. A convenience override must not beat a freeze.
    #[tokio::test(start_paused = true)]
    async fn dispatch_freeze_hold_does_not_honor_a_per_zone_force_run() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let forced = crate::model::ZoneVerdict {
            reason: "Override: force run (this zone)".into(),
            ..verdict("front", "run", "override")
        };
        let mut snap = snap_with(vec![zone_secs("front", 1, Some(forced))]);
        snap.skip_check.decide(
            "skip",
            "Freeze risk overnight (28°F < 36°F)".to_string(),
            "overnight_freeze".to_string(),
        );
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert!(
            rec.log().is_empty(),
            "a force-run override does not water into a freeze"
        );
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "skipped");
        assert_eq!(
            rows[0].skip_reason.as_deref(),
            Some("Freeze risk overnight (28°F < 36°F)"),
            "a blanket hold writes a plain marker row, no partial-hold note"
        );
    }

    // (e2c) a user Rhai skip rule holds the WHOLE yard. The script pass runs
    // against the aggregate only (assembly/pass.rs) and never touches the
    // per-zone verdicts, so every zone still reads "run" on the morning the
    // operator's own rule fires. Reading that as an escape would silently
    // delete the hold and water everything.
    #[tokio::test(start_paused = true)]
    async fn dispatch_user_script_hold_waters_nothing_though_every_zone_reads_run() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let mut snap = snap_with(vec![
            zone_secs("front", 1, Some(verdict("front", "run", "global"))),
            zone_secs("back", 1, Some(verdict("back", "run", "global"))),
        ]);
        // The shape apply_engine leaves behind: a reason_code decide_per_zone
        // never produces, over per-zone verdicts it never re-judged.
        snap.skip_check.decide(
            "skip",
            "Pool party this weekend".to_string(),
            "user_pool_party".to_string(),
        );
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert!(
            rec.log().is_empty(),
            "the operator's own skip rule holds every zone"
        );
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(
            rows.len(),
            2,
            "both zones recorded against the script's hold"
        );
        for r in &rows {
            assert_eq!(r.skip_reason.as_deref(), Some("Pool party this weekend"));
        }
    }

    // (e2d) the 2026-06-11 incident class, one layer up: an ordinary post-rain
    // Florida morning where every probe reads saturated AND more rain is
    // forecast. The aggregate ladder skips on its soil_saturation rung, which
    // `decide_per_zone`'s baseline (`global_verdict`) does not have, so every
    // soil-model zone comes back "run" sourced soil_model. Nothing waters.
    #[tokio::test(start_paused = true)]
    async fn dispatch_soil_saturation_hold_does_not_honor_a_soil_model_run() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let rides = |slug: &str| crate::model::ZoneVerdict {
            reason: "Waters anyway: soil zones already count this forecast rain against their \
                     deficit. (Rain expected within 4h)"
                .into(),
            ..verdict(slug, "run", "soil_model")
        };
        let mut snap = snap_with(vec![
            zone_secs("front", 1, Some(rides("front"))),
            zone_secs("back", 1, Some(rides("back"))),
        ]);
        snap.skip_check.decide(
            "skip",
            "Soil saturated (76% at or above the 65% threshold)".to_string(),
            "soil_saturation".to_string(),
        );
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert!(
            rec.log().is_empty(),
            "saturated ground is not watered because the per-zone baseline never checked it"
        );
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    // (e2e) even a genuinely exempt zone stays held when the yard's hold is
    // something OTHER than the restriction it is exempt from. The reason_code
    // half of the predicate is what enforces that.
    #[tokio::test(start_paused = true)]
    async fn dispatch_pause_hold_does_not_honor_a_stale_exempt_verdict() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let exempt = crate::model::ZoneVerdict {
            reason: "Exempt from the restriction holding the yard. (No watering on Tuesday)".into(),
            ..verdict("drip_bed", "run", "exempt")
        };
        let mut snap = snap_with(vec![zone_secs("drip_bed", 1, Some(exempt))]);
        snap.skip_check.decide(
            "skip",
            "All watering is on hold".to_string(),
            "paused".to_string(),
        );
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert!(
            rec.log().is_empty(),
            "an exemption from the ordinance is not an exemption from the pause"
        );
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].skip_reason.as_deref(),
            Some("All watering is on hold")
        );
    }

    // (e3) a zone with no verdict of its own is held by the yard, exactly as
    // before: the escape is a restriction-exempt verdict, never an absent or
    // unexpected one.
    #[tokio::test(start_paused = true)]
    async fn dispatch_blanket_skip_holds_a_zone_with_no_verdict() {
        let rec = DispatchRecorder::new("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let mut snap = snap_with(vec![
            zone_secs("front", 1, None),
            zone_secs(
                "back",
                1,
                Some(verdict("back", "run_extended", "condition")),
            ),
        ]);
        snap.skip_check.decide(
            "skip",
            "Freeze risk overnight (28°F < 36°F)".to_string(),
            "overnight_freeze".to_string(),
        );
        run_dispatch(&snap, &registry, &runs, &active_runs, at(NO_STOP_EPOCH)).await;
        assert!(rec.log().is_empty(), "nothing waters into a freeze");
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 2, "both zones are held, both recorded");
        for r in &rows {
            assert_eq!(r.status, "skipped");
            assert_eq!(
                r.skip_reason.as_deref(),
                Some("Freeze risk overnight (28°F < 36°F)")
            );
        }
    }

    #[test]
    fn zone_skip_verdict_enforces_soil_and_condition_skips_only() {
        let snap = crate::model::IrrigationSnapshot::default();
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
        // Global-source skip on a BLANKET-skip morning (will_skip=true) belongs to
        // the aggregate skip branch, which records it, not to the per-zone loop.
        let mut blanket = crate::model::IrrigationSnapshot::default();
        blanket.skip_check.decide(
            "skip",
            blanket.skip_check.reason.clone(),
            blanket.skip_check.reason_code.clone(),
        );
        let z = zone_with("back_yard", Some(verdict("back_yard", "skip", "global")));
        assert!(zone_skip_verdict(&blanket, &z).is_none());
        // But on a soil-floor demotion morning (will_skip=false), a wet sibling's
        // global-source skip MUST be honored here: the aggregate did not
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
        let mut snap = crate::model::IrrigationSnapshot::default();
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
            session_id: None,
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
            !handled_smart_morning_today(&store, today, &[], 0, t0 + 7200).await,
            "empty table must not count as handled"
        );

        // A completed smart_morning run earlier today blocks catch-up.
        store
            .insert_completed(row("back_yard", "smart_morning", t0), t0 + 300, 300, None)
            .await
            .unwrap();
        assert!(handled_smart_morning_today(&store, today, &[], 0, t0 + 7200).await);
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
            !handled_smart_morning_today(&store, today, &[], 0, t0 + 7200).await,
            "a stale-inputs marker must not block recovery dispatch"
        );

        // Manual UI runs are not scheduler-attributed either.
        store
            .insert_completed(row("front_yard", "manual", t0 + 100), t0 + 220, 120, None)
            .await
            .unwrap();
        assert!(!handled_smart_morning_today(&store, today, &[], 0, t0 + 7200).await);
    }

    /// A partial-hold row is written BEFORE the first valve opens, and it
    /// settles only its own zone: the exempt zones still owe their water.
    /// Treated as a marker (which is what any other smart_morning skip row
    /// is), a restart anywhere in the morning window would mark the whole
    /// day handled and the exempt zones would never water and never catch
    /// up. Instead the held zone is excluded from the evidence sweep and
    /// the exempt zone decides.
    #[tokio::test]
    async fn boot_dedupe_is_not_marked_by_a_partial_hold_row() {
        let store = fresh_store().await;
        let (today, t0) = today_and_epoch(6 * 3600);
        store
            .insert_skipped(
                row("back_yard", "smart_morning", t0),
                format!("No watering on Tuesday. {PARTIAL_HOLD_NOTE}"),
            )
            .await
            .unwrap();
        let planned = vec![
            zone_secs("back_yard", 300, None),
            zone_secs("drip_bed", 300, None),
        ];
        assert!(
            !handled_smart_morning_today(&store, today, &planned, t0, t0 + 3600).await,
            "the exempt zone has not watered yet, so the morning is not handled"
        );

        // Once the exempt zone has had its water, the morning IS handled:
        // the held zone owes nothing, so it must not keep the day open and
        // re-enter the dispatch on every restart. The evidence is the
        // run-edge observer's row, not another smart_morning marker, so
        // this asserts the evidence sweep and not the marker shortcut.
        store
            .insert_completed(
                row("drip_bed", "ha_refresher", t0 + 60),
                t0 + 360,
                300,
                None,
            )
            .await
            .unwrap();
        assert!(
            handled_smart_morning_today(&store, today, &planned, t0, t0 + 3600).await,
            "a zone the morning deliberately held owes no water"
        );
    }

    /// A controller that refused the dispatch applied no water, so the
    /// failure rows it leaves behind must not mark the day handled: a
    /// restart inside the same morning (redeploy, OOM, host reboot) has to
    /// be able to catch up once the controller is back. The rows stay in
    /// History either way, which is what they are there for.
    #[tokio::test]
    async fn boot_dedupe_ignores_dispatch_failure_rows() {
        let store = fresh_store().await;
        let (today, t0) = today_and_epoch(3600);
        for (i, zone) in ["back_yard", "front_yard", "side_yard"].iter().enumerate() {
            store
                .insert_skipped(
                    row(zone, "smart_morning", t0 + i as i64 * 10),
                    format!("{DISPATCH_FAILED_REASON_PREFIX} HTTP 502"),
                )
                .await
                .unwrap();
        }
        assert!(
            !handled_smart_morning_today(&store, today, &[], 0, t0 + 7200).await,
            "a refused dispatch applied no water; a restart must still be able to water"
        );

        // A single observed run is not a morning: one zone could be a manual
        // test. The day stays unhandled and the catch-up may still fire.
        //
        // Source "ha_refresher" is what the run-edge observer actually
        // writes. The scheduler records NOTHING on the success path, so a
        // completed "smart_morning" row never exists on a live install; a
        // test using one asserts through the `marker` branch and leaves the
        // `watered_zones >= 2` threshold, which is the only backstop left
        // once dispatch-failure rows are excluded, untested.
        store
            .insert_completed(
                row("back_yard", "ha_refresher", t0 + 600),
                t0 + 900,
                300,
                None,
            )
            .await
            .unwrap();
        let planned = vec![
            zone_secs("back_yard", 300, None),
            zone_secs("front_yard", 300, None),
        ];
        assert!(
            !handled_smart_morning_today(&store, today, &planned, t0, t0 + 7200).await,
            "front_yard is still owed its water"
        );

        // Both planned zones watered since the window opened: the morning
        // landed, so the day IS handled even though every scheduler row is
        // a dispatch failure. Per-zone evidence is the backstop the
        // exclusion leans on; it used to be a count of zones seen watering.
        store
            .insert_completed(
                row("front_yard", "ha_refresher", t0 + 960),
                t0 + 1260,
                300,
                None,
            )
            .await
            .unwrap();
        assert!(handled_smart_morning_today(&store, today, &planned, t0, t0 + 7200).await);
        // A third planned zone with nothing recorded keeps the day open.
        let mut wider = planned.clone();
        wider.push(zone_secs("side_yard", 300, None));
        assert!(!handled_smart_morning_today(&store, today, &wider, t0, t0 + 7200).await);
    }

    /// A partial morning is not a handled morning. Two zones watered and
    /// five were refused by the controller: the day stays open so the
    /// catch-up dispatch can water the five, each for what it is owed.
    /// The old rule counted two watered zones as "the sequence landed"
    /// and left the five dry until tomorrow.
    #[tokio::test]
    async fn boot_dedupe_partial_sequence_leaves_the_dry_zones_owed() {
        let store = fresh_store().await;
        let (today, t0) = today_and_epoch(3600);
        // Zones 1 and 2 completed; the observer closed both runs.
        for (i, zone) in ["zone_1", "zone_2"].iter().enumerate() {
            store
                .insert_completed(
                    row(zone, "ha_refresher", t0 + i as i64 * 400),
                    t0 + i as i64 * 400 + 300,
                    300,
                    None,
                )
                .await
                .unwrap();
        }
        // Zones 3 through 7 were refused by the controller.
        for (i, zone) in ["zone_3", "zone_4", "zone_5", "zone_6", "zone_7"]
            .iter()
            .enumerate()
        {
            store
                .insert_skipped(
                    row(zone, "smart_morning", t0 + 800 + i as i64 * 10),
                    format!("{DISPATCH_FAILED_REASON_PREFIX} HTTP 502"),
                )
                .await
                .unwrap();
        }
        let planned: Vec<_> = (1..=7)
            .map(|i| zone_secs(&format!("zone_{i}"), 300, None))
            .collect();
        assert!(
            !handled_smart_morning_today(&store, today, &planned, t0, t0 + 7200).await,
            "five zones are still owed their water; the catch-up finishes the morning"
        );
    }

    /// A manual run is persisted TWICE on any controller that reports zone
    /// state: the dispatcher writes its completed row when the valve is
    /// commanded, and the run-edge observer writes another for the same
    /// physical window a tick later. The two start epochs differ, so the
    /// unique index collapses nothing and both rows stand. The evidence read
    /// must credit their interval UNION, the way the water balance already
    /// does, not the sum of their durations.
    #[tokio::test]
    async fn watered_since_counts_a_double_persisted_run_once() {
        let store = fresh_store().await;
        let (_today, t0) = today_and_epoch(6 * 3600);
        // Ten minutes of water, hand-started at t0.
        store
            .insert_completed(row("front_yard", "manual", t0), t0 + 600, 600, None)
            .await
            .unwrap();
        // The run-edge observer's row for the SAME run: the refresher tick
        // that first saw the valve open, five seconds in.
        store
            .insert_completed(
                row("front_yard", "ha_refresher", t0 + 5),
                t0 + 600,
                595,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            watered_since(&store, "front_yard", t0 - 60, t0 + 3600).await,
            600,
            "one ten-minute run, not the 1195 s the two rows sum to"
        );
    }

    /// What the sum cost the yard: a ten-minute run started by hand inside
    /// the morning window read as twenty, satisfied a fifteen-minute plan,
    /// marked the whole morning handled at boot, and dispatched nothing.
    #[tokio::test]
    async fn boot_dedupe_is_not_satisfied_by_a_double_persisted_manual_run() {
        let store = fresh_store().await;
        let (today, t0) = today_and_epoch(6 * 3600);
        store
            .insert_completed(row("front_yard", "manual", t0), t0 + 600, 600, None)
            .await
            .unwrap();
        store
            .insert_completed(
                row("front_yard", "ha_refresher", t0 + 5),
                t0 + 600,
                595,
                None,
            )
            .await
            .unwrap();
        let planned = vec![zone_secs("front_yard", 900, None)];
        assert!(
            !handled_smart_morning_today(&store, today, &planned, t0, t0 + 3600).await,
            "ten minutes of water does not finish a fifteen-minute plan"
        );
    }

    /// The dispatch guard that keeps an already-open valve from being
    /// commanded open a second time, on any path.
    #[test]
    fn dispatch_never_reopens_a_valve_the_controller_reports_open() {
        use crate::model::ZoneState;
        let z = |running: bool, known: bool| ZoneState {
            running,
            running_known: known,
            ..Default::default()
        };
        assert!(already_running(&z(true, true)), "confirmed open");
        assert!(!already_running(&z(false, true)), "confirmed closed");
        // An UNKNOWN running state is not a claim that the zone is running.
        // A fire-and-forget controller (MQTT) reports running_known false on
        // every zone; treating that as "already open" would stop those
        // installs from ever watering.
        assert!(!already_running(&z(true, false)), "unknown is not open");
        assert!(!already_running(&z(false, false)));
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
        assert!(handled_smart_morning_today(&store, today, &[], 0, t0 + 7200).await);
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
        /// The smallest run this controller can be told, like the cloud
        /// adapters that take whole minutes. 1 = seconds.
        quantum_s: u32,
        /// The paused clock's origin, so a handle's started_epoch advances
        /// with tokio's paused time rather than stamping every segment of
        /// a morning into the same wall second.
        created: tokio::time::Instant,
        epoch0: i64,
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
                quantum_s: 1,
                created: tokio::time::Instant::now(),
                epoch0: Utc::now().timestamp(),
            })
        }
        fn ok(id: &str) -> std::sync::Arc<Self> {
            Self::build(id, None, None)
        }
        /// A controller that runs whole multiples of `quantum_s`, rounding
        /// up and reporting the rounded figure, as B-hyve and Rain Bird do.
        fn quantized(id: &str, quantum_s: u32) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                id: id.into(),
                calls: std::sync::Mutex::new(Vec::new()),
                stop_all_calls: std::sync::atomic::AtomicUsize::new(0),
                fail_slug_from: None,
                stop_stamp_epoch: None,
                quantum_s,
                created: tokio::time::Instant::now(),
                epoch0: Utc::now().timestamp(),
            })
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
                per_zone_stop: true,
                duration_quantum_s: self.quantum_s,
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
                started_epoch: self.epoch0 + self.created.elapsed().as_secs() as i64,
                planned_duration_s: cycle_soak::round_up(duration_s, self.quantum_s),
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
                observed_epoch: None,
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
    /// while 600s stays a single soak-free segment. The soak between
    /// cycles is derived from what a 900 s cycle leaves standing on clay,
    /// which is 1800 s; `soak_minutes` is only a floor on it, so 5 does
    /// not bind and 30 lands on the same figure. `interleave` picks the
    /// layout policy.
    fn cycle_soak_cfg(slugs: &[&str], soak_minutes: u32, interleave: bool) -> Arc<Config> {
        use crate::config::schema::{GrassSpecies, SoilTexture, SprinklerType, ZoneConfig};
        let mut cfg = Config::default();
        cfg.engine.soak_minutes = soak_minutes;
        cfg.engine.interleave_cycles = interleave;
        for slug in slugs {
            cfg.zones.insert(
                (*slug).to_string(),
                ZoneConfig {
                    scheduling_model: None,
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
                    controller_zone_name: None,
                    soil_sensor_id: None,
                    target_min_pct_soil: 30.0,
                    saturation_pct_soil: 70.0,
                    photo_url: None,
                    weekly_budget_in: None,
                    sessions_per_week: None,
                    rain_credit_cap_in: None,
                    max_run_minutes: None,
                },
            );
        }
        Arc::new(cfg)
    }

    /// run_dispatch with a real Config, so cycle plans and the layout policy
    /// come from build_cycle_plan + engine.interleave_cycles.
    async fn run_dispatch_cfg(
        snap: &crate::model::IrrigationSnapshot,
        registry: &ControllerRegistry,
        runs: &RunsStore,
        active_runs: &ActiveRunsStore,
        cfg: &Arc<Config>,
        now_utc: chrono::DateTime<Utc>,
    ) {
        let policy = WateringPolicy::from_config(cfg);
        dispatch_gate::isolated(dispatch_today(
            snap,
            registry,
            Some(runs),
            Some(active_runs),
            None, // push
            &policy.zone_agronomy,
            cfg.engine.soak_minutes,
            cfg.engine.interleave_cycles,
            &policy.zone_controller,
            &policy
                .zone_runtime
                .iter()
                .map(|(k, v)| (k.clone(), v.max_duration_s))
                .collect(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 25).unwrap(),
            now_utc,
            false, // dry_run
            false, // is_catch_up
            0,     // window_start_epoch
        ))
        .await;
    }

    // Multi-segment serial spacing: the live-clock waits reproduce the legacy
    // nested-loop cadence. On clay under spray, front 2700s splits to
    // [900/1800, 900/1800, 900/0] and back 1800s to [900/1800, 900/0], the
    // soak being what a 900 s cycle leaves standing (soak_minutes=5 is a
    // floor that does not bind). Every
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
        // Legacy nested order: ALL of front's segments, then all of back's.
        // The soak is the derived 1800 s a 900 s clay cycle needs, not the
        // 300 s floor the config names.
        assert_eq!(
            timeline,
            vec![
                ("front".to_string(), 900u32, 0u64),
                ("front".into(), 900, 2700),
                ("front".into(), 900, 5400),
                ("back".into(), 900, 6302),
                ("back".into(), 900, 9002),
            ]
        );
        // The same facts spelled as the minimums the executor must hold:
        // same-zone consecutive dispatches >= run + soak apart, the zone
        // switch >= run + preamble apart.
        assert!(timeline[1].2 - timeline[0].2 >= 900 + 1800);
        assert!(timeline[2].2 - timeline[1].2 >= 900 + 1800);
        assert!(timeline[3].2 - timeline[2].2 >= 900 + INTER_ZONE_PREAMBLE_S);
        assert!(timeline[4].2 - timeline[3].2 >= 900 + 1800);
        assert_eq!(rec.stops(), 0);
        assert!(runs.window(WIDE.0, WIDE.1).await.unwrap().is_empty());
    }

    /// `run_dispatch_cfg` as a catch-up: the morning's window opened at
    /// `window_start`, and only water since then counts.
    async fn run_catch_up_cfg(
        snap: &crate::model::IrrigationSnapshot,
        registry: &ControllerRegistry,
        runs: &RunsStore,
        active_runs: &ActiveRunsStore,
        cfg: &Arc<Config>,
        now_utc: chrono::DateTime<Utc>,
        window_start: i64,
    ) {
        let policy = WateringPolicy::from_config(cfg);
        dispatch_gate::isolated(dispatch_today(
            snap,
            registry,
            Some(runs),
            Some(active_runs),
            None,
            &policy.zone_agronomy,
            cfg.engine.soak_minutes,
            cfg.engine.interleave_cycles,
            &policy.zone_controller,
            &policy
                .zone_runtime
                .iter()
                .map(|(k, v)| (k.clone(), v.max_duration_s))
                .collect(),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 25).unwrap(),
            now_utc,
            false,
            true,
            window_start,
        ))
        .await;
    }

    /// A restart after the first zone finished waters the other two, and
    /// a zone the restart cut short gets only its remainder. A manual
    /// test before the window opened counts for nothing.
    #[tokio::test(start_paused = true)]
    async fn a_catch_up_waters_what_the_morning_still_owes() {
        let rec = TimedRecorder::ok("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let cfg = Arc::new(Config::default());
        let now = Utc::now().timestamp();
        let window_start = now - 1800;
        // front finished before the restart; side was cut short at 200 of
        // 600 s (the boot pass wrote its aborted row); back never started.
        // A manual test of back before the window is not this morning's.
        let row =
            |zone: &str, source: &str, start: i64, secs: u32| crate::persistence::runs::NewRun {
                session_id: None,
                zone_slug: zone.into(),
                start_epoch: start,
                source: source.into(),
                controller_id: "os_main".into(),
                planned_duration_s: secs,
                skip_reason: None,
                et0_mm: None,
                etc_mm: None,
                cycle_index: None,
                cycle_count: None,
            };
        runs.insert_completed(
            row("front", "ha_refresher", window_start + 10, 600),
            window_start + 610,
            600,
            None,
        )
        .await
        .unwrap();
        runs.insert_aborted(
            row("side", "restart", window_start + 620, 600),
            window_start + 820,
            "ended by restart",
        )
        .await
        .unwrap();
        runs.insert_completed(
            row("back", "manual", window_start - 3600, 600),
            window_start - 3000,
            600,
            None,
        )
        .await
        .unwrap();
        let snap = snap_with(vec![
            zone_secs("front", 600, None),
            zone_secs("side", 600, None),
            zone_secs("back", 600, None),
        ]);
        // Not handled: back and side are still owed water.
        assert!(
            !handled_smart_morning_today(
                &runs,
                chrono::Utc::now().date_naive(),
                &snap.zones,
                window_start,
                chrono::Utc::now().timestamp(),
            )
            .await
        );
        let t0 = tokio::time::Instant::now();
        run_catch_up_cfg(
            &snap,
            &registry,
            &runs,
            &active_runs,
            &cfg,
            at(NO_STOP_EPOCH),
            window_start,
        )
        .await;
        let dispatched: Vec<(String, u32)> = rec
            .timeline(t0)
            .into_iter()
            .map(|(z, d, _)| (z, d))
            .collect();
        assert_eq!(
            dispatched,
            vec![("side".to_string(), 400u32), ("back".to_string(), 600u32)],
            "front is done, side gets its remainder, back gets its plan"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_restart_duration_holds_only_its_zone_without_crediting_water() {
        let rec = TimedRecorder::ok("os_main");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let (today, epoch) = today_and_epoch(6 * 3600);
        runs.insert_skipped(
            row("front", "restart", epoch),
            crate::controllers::reaper::RESTART_UNKNOWN_DURATION_REASON.into(),
        )
        .await
        .unwrap();
        let front = zone_secs("front", 600, None);
        let back = zone_secs("back", 600, None);
        assert_eq!(
            watered_since(&runs, "front", epoch - 600, epoch + 60).await,
            0
        );
        assert!(
            handled_smart_morning_today(
                &runs,
                today,
                std::slice::from_ref(&front),
                epoch - 600,
                epoch + 60
            )
            .await
        );
        assert!(
            !handled_smart_morning_today(
                &runs,
                today,
                &[front.clone(), back.clone()],
                epoch - 600,
                epoch + 60
            )
            .await
        );
        let snap = snap_with(vec![front, back]);
        let cfg = Config::default();
        let policy = WateringPolicy::from_config(&cfg);
        dispatch_gate::isolated(dispatch_today(
            &snap,
            &registry,
            Some(&runs),
            Some(&active_runs),
            None,
            &policy.zone_agronomy,
            cfg.engine.soak_minutes,
            cfg.engine.interleave_cycles,
            &policy.zone_controller,
            &HashMap::new(),
            today,
            at(epoch + 60),
            false,
            true,
            epoch - 600,
        ))
        .await;
        let dispatched: Vec<_> = rec
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(slug, _, _)| slug.clone())
            .collect();
        assert_eq!(dispatched, vec!["back"]);
    }

    /// A zone on a controller with no state readback gets its run rows
    /// written at dispatch, one per segment, pre-completed to what the
    /// controller was told; the morning then counts as handled, so a
    /// restart inside grace does not dispatch it twice. Its sibling with
    /// readback gets no rows from the dispatcher: the observer owns those.
    #[tokio::test(start_paused = true)]
    async fn a_no_readback_zone_gets_its_run_rows_at_dispatch() {
        let rec = TimedRecorder::ok("mqtt");
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        let cfg = cycle_soak_cfg(&["front", "back"], 5, false);
        let snap = snap_with(vec![
            zone_secs_no_readback("front", 1800),
            zone_secs("back", 600, None),
        ]);
        run_dispatch_cfg(
            &snap,
            &registry,
            &runs,
            &active_runs,
            &cfg,
            at(NO_STOP_EPOCH),
        )
        .await;
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        let front: Vec<_> = rows.iter().filter(|r| r.zone_slug == "front").collect();
        assert_eq!(front.len(), 2, "one row per segment: {rows:?}");
        assert!(front
            .iter()
            .all(|r| r.source == "smart_morning" && r.skip_reason.is_none()));
        assert!(front.iter().all(|r| r.controller_id == "mqtt"));
        assert!(
            rows.iter().all(|r| r.zone_slug != "back"),
            "a readback zone's rows belong to the observer: {rows:?}"
        );
        // And the day the rows landed on reads as handled, so a restart
        // inside grace does not dispatch the morning twice.
        let day = crate::timeutil::deployment_calendar()
            .local_date(front[0].start_epoch)
            .expect("a civil day");
        assert!(
            handled_smart_morning_today(
                &runs,
                day,
                &snap.zones,
                front[0].start_epoch - 60,
                chrono::Utc::now().timestamp()
            )
            .await
        );
    }

    /// Two controllers, the zone bound to the second: the morning
    /// dispatches it there, and the unbound zone still takes the default.
    #[tokio::test(start_paused = true)]
    async fn a_zone_bound_to_the_second_controller_dispatches_there() {
        let a = TimedRecorder::ok("a");
        let b = TimedRecorder::ok("b");
        let registry = ControllerRegistry::new();
        {
            let ca: std::sync::Arc<dyn IrrigationController> = a.clone();
            let cb: std::sync::Arc<dyn IrrigationController> = b.clone();
            registry.set(vec![(ca, true), (cb, false)]);
        }
        let (runs, active_runs) = stores();
        let mut cfg = (*cycle_soak_cfg(&["front", "back"], 5, false)).clone();
        cfg.zones.get_mut("front").unwrap().controller_id = "b".into();
        cfg.zones.get_mut("back").unwrap().controller_id = String::new();
        let cfg = Arc::new(cfg);
        let snap = snap_with(vec![
            zone_secs("front", 600, None),
            zone_secs("back", 600, None),
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
        let on_b: Vec<String> = b.timeline(t0).into_iter().map(|(z, _, _)| z).collect();
        let on_a: Vec<String> = a.timeline(t0).into_iter().map(|(z, _, _)| z).collect();
        assert_eq!(on_b, vec!["front".to_string()], "front is bound to b");
        assert_eq!(
            on_a,
            vec!["back".to_string()],
            "back is unbound and takes the default"
        );
    }

    /// A 200 s segment on a whole-minute controller yields a 240 s plan
    /// and a 240 s wait.
    ///
    /// The executor used to wait for what it ASKED, so on B-hyve or Rain
    /// Bird the next valve was commanded open while the previous one had
    /// up to 59 seconds left to run. Now the plan is rounded to the
    /// controller's quantum before dispatch and the wait follows the
    /// duration the adapter reports it sent.
    #[tokio::test(start_paused = true)]
    async fn a_whole_minute_controller_is_waited_for_in_whole_minutes() {
        let rec = TimedRecorder::quantized("bhyve", 60);
        let registry = registry_with(&rec);
        let (runs, active_runs) = stores();
        // No zone agronomy: the unsplit fallback, which still rounds.
        let cfg = Arc::new(Config::default());
        let snap = snap_with(vec![
            zone_secs("front", 200, None),
            zone_secs("back", 200, None),
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
        assert_eq!(
            timeline,
            vec![
                ("front".to_string(), 240u32, 0u64),
                ("back".into(), 240, 240 + INTER_ZONE_PREAMBLE_S),
            ]
        );
        assert_eq!(rec.stops(), 0);
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
        let planner_policy = WateringPolicy::from_config(&cfg);
        let plans: Vec<interleave::ZonePlan> = slugs
            .iter()
            .enumerate()
            .map(|(idx, slug)| interleave::ZonePlan {
                zone_idx: idx,
                segments: build_cycle_plan(
                    &planner_policy.zone_agronomy,
                    slug,
                    secs[idx],
                    cfg.engine.soak_minutes,
                    1,
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
            zone_secs("front", 1800, None), // [900/1800, 900/0], all fail-masked
            zone_secs("back", 1800, None),  // [900/1800, 900/0], runs in full
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
                ("back".into(), 900, INTER_ZONE_PREAMBLE_S + 2700),
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
        // A failed dispatch leaves a trace. Before this it wrote nothing at
        // all, so a morning that threw was indistinguishable in History from
        // a morning that planned nothing, and weeks of failures left zero
        // evidence anywhere. One row per zone (the mask skips the zone's
        // remaining segments), status "skipped" so it never counts as applied
        // water, carrying the controller's own error text.
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1, "one failure row for the failed zone");
        assert_eq!(rows[0].zone_slug, "front");
        assert_eq!(rows[0].source, "smart_morning");
        assert_eq!(rows[0].status, "skipped");
        assert!(
            rows[0]
                .skip_reason
                .as_deref()
                .unwrap_or_default()
                .starts_with("Controller dispatch failed:"),
            "reason carries the controller's own text: {:?}",
            rows[0].skip_reason
        );
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
            zone_secs("front", 2700, None), // [900/1800 x2, 900/0]: #0 ok, #1 fails, #2 masked
            zone_secs("back", 1800, None),  // [900/1800, 900/0]
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
                ("front".into(), 900, 2700),
                ("back".into(), 900, 2700 + INTER_ZONE_PREAMBLE_S),
                ("back".into(), 900, 5400 + INTER_ZONE_PREAMBLE_S),
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
        // The failed segment still leaves its trace, once, for that zone.
        let rows = runs.window(WIDE.0, WIDE.1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].zone_slug, "front");
        assert_eq!(rows[0].status, "skipped");
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
