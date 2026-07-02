// Manual schedule dispatcher. Spawned alongside the live refresher in
// main.rs; ticks every 60 seconds and fires zone runs from operator-
// defined `ManualSchedule` entries.
//
// Workflow on each tick:
//   1. Resolve `chrono::Local::now()` into (weekday u8, hour u8, min u8,
//      date_key NaiveDate).
//   2. For each enabled schedule: skip if weekday isn't in the list, or
//      if the (hour, minute) doesn't match the current minute exactly.
//   3. Dedupe: a HashMap<schedule_id, NaiveDate> remembers the date on
//      which each schedule last fired. The same minute getting two
//      ticks in a row (clock skew, leap seconds) doesn't double-dispatch.
//   4. Evaluate the watering policy. If a Phase C restriction blocks
//      today (`skip = true`), persist a `status="skipped"` runs row with
//      the reason and skip dispatch.
//   5. Otherwise call the default controller's `run_zone(slug, duration)`.
//      The controller adapter logs to the runs table itself; the
//      schedule's `id` is included in `source = "manual:<id>"` so the
//      dashboard can attribute the run.
//
// `ManualMode::Override` is honored elsewhere: src/refresher.rs reads
// the policy + schedules at boot to decide whether to suppress the smart
// engine's dispatch for a zone with an enabled Override schedule today.
// This file only fires the manual run; it does not suppress smart.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{Datelike, NaiveDate, Timelike};
use tokio::time::interval;
use tracing::{info, warn};

use crate::config::schema::ManualSchedule;
use crate::controllers::registry::ControllerRegistry;
use crate::engine::restrictions;
use crate::ha::WateringPolicy;
use crate::persistence::runs::{NewRun, RunsStore};
use crate::persistence::ActiveRunsStore;

/// Hard cap on a single manual-schedule run (2h), mirroring the API path's
/// RUN_SECONDS_MAX (src/api/irrigation.rs). An operator typo (e.g.
/// duration_minutes = 600) must not command a valve open for hours; the
/// jurisdiction cap below tightens this further when a restriction is active.
const RUN_SECONDS_MAX: u32 = 7200;

/// Spawn the manual schedule tick. Returns immediately; the background
/// task lives for the lifetime of the process. Spawned UNCONDITIONALLY at
/// boot (main.rs drops the !is_empty() guard) so a FIRST schedule added to a
/// previously-empty config can actuate without a container restart: the tick
/// early-returns each cycle when the live schedule list is empty and starts
/// firing the moment a schedule is swapped in.
///
/// `schedules` is the SWAPPABLE handle the config-write paths
/// (apply_runtime_config) store a new schedule set into. The tick loads the
/// CURRENT set with `load_full()` at the top of every cycle, so editing or
/// adding a schedule takes effect on the next tick. Mirrors the W1.5 ArcSwap
/// pattern the watering policy / forecast priority use.
///
/// `watering_policy` is the SWAPPABLE handle the config-write paths
/// (apply_runtime_config) store a new policy into. The tick `load_full()`s the
/// CURRENT policy at the top of every cycle, so a hot-reloaded restriction /
/// cap / skip edit reaches SCHEDULED valves on the next tick with no restart
/// (previously a boot-frozen value: a restriction meant to BLOCK watering never
/// reached this dispatcher until a container restart, a silent valve-command
/// gap on the safety path). Mirrors the refresher's consumption of the same
/// handle (src/main.rs) and the schedule-set hot-reload above.
///
/// `active_runs` is the commanded-valve deadline ledger (P0-1b): each
/// dispatched run arms a shutoff deadline so the reaper closes the valve
/// even if this process dies before the controller's own (in-process)
/// timer fires. Mirrors the API path (src/api/irrigation.rs). `None` when
/// no persistence DB is mounted (the reaper is then also absent).
pub fn spawn(
    schedules: Arc<ArcSwap<Vec<ManualSchedule>>>,
    watering_policy: Arc<ArcSwap<WateringPolicy>>,
    controllers: ControllerRegistry,
    runs: Option<RunsStore>,
    active_runs: Option<ActiveRunsStore>,
) {
    info!("manual scheduler: spawning tick (hot-reloads the schedule set + watering policy each tick)");
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(60));
        // Dedup ledger keyed on (schedule id, start_hour, start_minute) -> the
        // day-window it last fired on. Keying on the TIME as well as the id (FIX 2)
        // means re-timing an already-fired schedule to later the same day re-fires
        // at the new time: the (id, new_time) key has no entry yet, while the once-
        // per-(id,time)-per-day-window guarantee still holds.
        let mut last_fired: HashMap<(String, u8, u8), NaiveDate> = HashMap::new();
        loop {
            tick.tick().await;
            // Load the CURRENT schedule set live (W1.5 ArcSwap hot-reload): an edit
            // or a first-ever schedule swapped in via apply_runtime_config is read
            // here on the next tick with no restart. Empty list -> nothing fires.
            let schedules = schedules.load_full();
            if schedules.is_empty() {
                continue;
            }
            // Load the CURRENT watering policy live (FIX 1): a hot-reloaded
            // restriction / cap / skip edit swapped into this handle by
            // apply_runtime_config reaches SCHEDULED valves on THIS tick with no
            // restart, exactly as the refresher reads the same handle. Previously a
            // boot-frozen value let a restriction meant to BLOCK watering bypass the
            // dispatcher until a container restart, a silent valve-command gap.
            let watering_policy = watering_policy.load_full();
            // P1-8c: wall-clock in the CONFIGURED timezone, not the container TZ.
            // Sampled once per tick and passed into run_tick so the dispatch decision
            // is taken against a single, frozen instant (the test seam injects this).
            let now = crate::timeutil::now_local();
            run_tick(
                now,
                &schedules,
                &watering_policy,
                &controllers,
                runs.as_ref(),
                active_runs.as_ref(),
                &mut last_fired,
            )
            .await;
        }
    });
}

/// One dispatch evaluation against a SINGLE frozen instant `now`. Pulled out of
/// the spawn loop so the loop owns only the live-handle loads (FIX 1 / W1.5) and
/// this owns the firing decision against a fixed clock. Taking `now` as a
/// parameter is the test seam (FIX 3): a test calls this directly with a pinned
/// instant and a hot-swapped policy, so the dispatch decision cannot straddle a
/// minute boundary the way a spawn-then-sleep test does.
///
/// `last_fired` is the caller-owned dedup ledger; this prunes it against the live
/// schedule set each call (FIX 4) so removed schedules' entries cannot accumulate.
#[allow(clippy::too_many_arguments)]
async fn run_tick(
    now: chrono::DateTime<chrono::FixedOffset>,
    schedules: &[ManualSchedule],
    watering_policy: &WateringPolicy,
    controllers: &ControllerRegistry,
    runs: Option<&RunsStore>,
    active_runs: Option<&ActiveRunsStore>,
    last_fired: &mut HashMap<(String, u8, u8), NaiveDate>,
) {
    // FIX 4: prune dedup entries for schedules no longer in the live set so
    // last_fired cannot grow unbounded as schedules are added then removed.
    // A re-added schedule that fired earlier today still de-dupes for the
    // rest of the day-window (its (id,time) entry is retained while present).
    last_fired.retain(|(id, _, _), _| schedules.iter().any(|s| &s.id == id));

    let weekday = now.weekday().num_days_from_sunday() as u8;
    let hour = now.hour() as u8;
    let minute = now.minute() as u8;
    let today: NaiveDate = now.date_naive();

    // Cap minutes from the policy (if any rule is active right now).
    // Each schedule's duration is min'd with this so a 90-min
    // schedule on a 60-min-capped jurisdiction doesn't overrun.
    let v = restrictions::evaluate(
        now,
        &watering_policy.restrictions,
        watering_policy.address_parity,
    );
    let cap_seconds: Option<u32> = v.max_minutes_cap.map(|m| m.saturating_mul(60));

    for s in schedules.iter() {
        if !s.enabled {
            continue;
        }
        if !s.weekdays.contains(&weekday) {
            continue;
        }
        if s.start_hour != hour || s.start_minute != minute {
            continue;
        }
        // Dedup key includes the schedule's start time (FIX 2): re-timing a
        // schedule that already fired earlier today to a later slot makes a
        // fresh (id, new_time) key, so it fires again at the new time; the
        // once-per-(id,time)-per-day-window guarantee is unchanged.
        let fire_key = (s.id.clone(), s.start_hour, s.start_minute);
        if last_fired.get(&fire_key) == Some(&today) {
            continue;
        }

        // Evaluate restrictions per-schedule. The same policy is
        // checked, but we want to log the skip reason per
        // schedule rather than once globally.
        if v.skip {
            let reason = v
                .reason
                .clone()
                .unwrap_or_else(|| "watering restriction".to_string());
            if let Some(rs) = runs.as_ref() {
                let row = NewRun {
                    zone_slug: s.zone_slug.clone(),
                    start_epoch: now.timestamp(),
                    source: format!("manual:{}", s.id),
                    controller_id: controllers
                        .default()
                        .map(|c| c.id().to_string())
                        .unwrap_or_default(),
                    planned_duration_s: s.duration_minutes.saturating_mul(60),
                    skip_reason: None,
                    et0_mm: None,
                    etc_mm: None,
                    cycle_index: None,
                    cycle_count: None,
                };
                if let Err(e) = rs.insert_skipped(row, reason.clone()).await {
                    warn!(schedule = %s.id, error = %e, "manual scheduler: skip-row insert failed");
                }
            }
            info!(
                schedule = %s.id,
                zone = %s.zone_slug,
                reason = %reason,
                "manual scheduler: skipped run (watering restriction)"
            );
            last_fired.insert(fire_key, today);
            continue;
        }

        // Dispatch through the configured default controller.
        let controller = match controllers.default() {
            Some(c) => c,
            None => {
                warn!(schedule = %s.id, "manual scheduler: no default controller configured; skipping");
                last_fired.insert(fire_key, today);
                continue;
            }
        };
        let mut duration_s = s.duration_minutes.saturating_mul(60);
        // Hard safety cap first (mirrors the API path): a configured
        // duration can never command a valve open longer than
        // RUN_SECONDS_MAX, regardless of the schedule's minutes.
        if duration_s > RUN_SECONDS_MAX {
            warn!(
                schedule = %s.id,
                zone = %s.zone_slug,
                requested_s = duration_s,
                max_s = RUN_SECONDS_MAX,
                "manual scheduler: clamping run to RUN_SECONDS_MAX"
            );
            duration_s = RUN_SECONDS_MAX;
        }
        if let Some(c) = cap_seconds {
            if c < duration_s {
                duration_s = c;
            }
        }
        // Never command a zero-length run (a 0-minute schedule or a
        // 0-minute jurisdiction cap would otherwise dispatch an empty
        // open/close). Floor at 1s like the API path's `.max(1)`.
        let duration_s = duration_s.max(1);
        info!(
            schedule = %s.id,
            zone = %s.zone_slug,
            duration_s,
            "manual scheduler: dispatching run"
        );
        // P0-8: serialize Run dispatch on this zone against the manual
        // API path + smart-morning, sharing one lock registry. Held only
        // across the dispatch (the controller owns the shutoff), so a
        // Stop is never blocked behind it.
        let run_result = {
            let lock = crate::controllers::zone_run_lock(&s.zone_slug);
            let _run_serialize = lock.lock().await;
            controller.run_zone(&s.zone_slug, duration_s).await
        };
        match run_result {
            Ok(handle) => {
                info!(
                    schedule = %s.id,
                    zone = %s.zone_slug,
                    controller = %handle.controller_id,
                    provider_ref = ?handle.provider_ref,
                    "manual scheduler: run dispatched"
                );
                // Run-history insert (mirrors the API path). Native MQTT/
                // DIY zones have no run-edge observer to record them, so
                // without this an operator's recurring run never appears in
                // history. The controller owns the shutoff timer, so end =
                // start + duration matches what the hardware does.
                if let Some(rs) = runs.as_ref() {
                    let row = NewRun {
                        zone_slug: s.zone_slug.clone(),
                        start_epoch: handle.started_epoch,
                        source: format!("manual:{}", s.id),
                        controller_id: handle.controller_id.clone(),
                        planned_duration_s: duration_s,
                        skip_reason: None,
                        et0_mm: None,
                        etc_mm: None,
                        cycle_index: None,
                        cycle_count: None,
                    };
                    if let Err(e) = rs
                        .insert_completed(
                            row,
                            handle.started_epoch + duration_s as i64,
                            duration_s,
                            None,
                        )
                        .await
                    {
                        warn!(schedule = %s.id, zone = %s.zone_slug, error = %e, "manual scheduler: run row insert failed");
                    }
                }
                // P0-1b: arm the persisted shutoff deadline so the reaper
                // closes this valve even if the process dies before the
                // controller's own (in-process) timer fires. This is the
                // safety backstop the manual scheduler previously bypassed
                // (stuck-valve risk on MQTT/DIY controllers).
                if let Some(ar) = active_runs.as_ref() {
                    if let Err(e) = ar
                        .arm(
                            s.zone_slug.clone(),
                            handle.controller_id.clone(),
                            handle.started_epoch,
                            handle.started_epoch + duration_s as i64,
                        )
                        .await
                    {
                        warn!(schedule = %s.id, zone = %s.zone_slug, error = %e, "manual scheduler: active-run arm failed");
                    }
                }
            }
            Err(e) => {
                warn!(
                    schedule = %s.id,
                    zone = %s.zone_slug,
                    error = %e,
                    "manual scheduler: controller dispatch failed"
                );
            }
        }
        last_fired.insert(fire_key, today);
    }
}

/// Returns true when any enabled `Override` schedule fires for `zone_slug`
/// on `weekday`. The refresher uses this to decide whether to suppress the
/// smart engine's dispatch for the zone, manual takes precedence under
/// Override, but smart math still computes for nerd visibility.
pub fn override_active_today(schedules: &[ManualSchedule], zone_slug: &str, weekday: u8) -> bool {
    schedules.iter().any(|s| {
        s.enabled
            && s.zone_slug == zone_slug
            && s.weekdays.contains(&weekday)
            && matches!(s.mode, crate::config::schema::ManualMode::Override)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::ManualMode;

    fn sched(id: &str, zone: &str, weekdays: Vec<u8>, mode: ManualMode) -> ManualSchedule {
        ManualSchedule {
            id: id.into(),
            name: id.into(),
            zone_slug: zone.into(),
            enabled: true,
            weekdays,
            start_hour: 5,
            start_minute: 0,
            duration_minutes: 30,
            mode,
        }
    }

    #[test]
    fn override_active_matches_zone_and_weekday() {
        let s = vec![sched("a", "back_yard", vec![3, 6], ManualMode::Override)];
        assert!(override_active_today(&s, "back_yard", 3));
        assert!(override_active_today(&s, "back_yard", 6));
        assert!(!override_active_today(&s, "back_yard", 4));
        assert!(!override_active_today(&s, "front_yard", 3));
    }

    #[test]
    fn override_active_ignores_floor_mode() {
        let s = vec![sched("a", "back_yard", vec![3], ManualMode::Floor)];
        assert!(!override_active_today(&s, "back_yard", 3));
    }

    #[test]
    fn override_active_ignores_disabled() {
        let mut s = vec![sched("a", "back_yard", vec![3], ManualMode::Override)];
        s[0].enabled = false;
        assert!(!override_active_today(&s, "back_yard", 3));
    }

    // ── Hot-reload of the manual schedule set + watering policy ───────────────
    //
    // The dispatcher loads its schedule set AND its watering policy from shared
    // Arc<ArcSwap<_>> handles at the top of every tick (load_full), and the
    // config-write path (runtime::apply_runtime_config) swaps new values into the
    // same handles. These tests lock both halves: the tick reads whatever is live
    // in the handles (so an edit / a first-ever schedule / a hot-reloaded
    // restriction is picked up with no restart), and the dispatch decision is
    // taken against a FROZEN instant via `run_tick` so it cannot flake on a
    // minute boundary (FIX 3).

    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use chrono::{FixedOffset, TimeZone};

    use crate::persistence::runner;
    use crate::persistence::runs::RunsStore;
    use crate::persistence::ActiveRunsStore;
    use rusqlite::Connection;
    use tokio::sync::Mutex;

    /// A FROZEN test instant. Wed 2026-06-25 05:00:00 in UTC offset, chosen so the
    /// dispatch decision never depends on the wall clock (no minute-boundary
    /// straddle, no ~midnight calendar flip): the schedules built by `sched_at`
    /// pin their weekday/hour/minute off THIS exact value and `run_tick` evaluates
    /// against it.
    fn frozen_now() -> chrono::DateTime<FixedOffset> {
        FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2026, 6, 25, 5, 0, 0)
            .single()
            .unwrap()
    }

    /// A schedule pinned to fire at the frozen instant `now`, so `run_tick(now, ..)`
    /// matches it exactly with no wall-clock dependency.
    fn sched_at(now: chrono::DateTime<FixedOffset>, id: &str, zone: &str) -> ManualSchedule {
        ManualSchedule {
            id: id.into(),
            name: id.into(),
            zone_slug: zone.into(),
            enabled: true,
            weekdays: vec![now.weekday().num_days_from_sunday() as u8],
            start_hour: now.hour() as u8,
            start_minute: now.minute() as u8,
            duration_minutes: 10,
            mode: ManualMode::Override,
        }
    }

    /// One migrated in-memory DB shared by both stores (test-isolated), mirroring
    /// the smart_morning test harness.
    fn stores() -> (RunsStore, ActiveRunsStore) {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        let conn = Arc::new(Mutex::new(c));
        (RunsStore::new(conn.clone()), ActiveRunsStore::new(conn))
    }

    /// A registry whose single default controller is a DryRun (always reachable,
    /// dispatches without hardware), so a fired schedule produces an observable
    /// `manual:<id>` runs row.
    fn dry_registry() -> ControllerRegistry {
        let ctl: Arc<dyn crate::ports::irrigation_controller::IrrigationController> =
            Arc::new(crate::controllers::DryRunController::new(
                "dry",
                crate::config::schema::DryRunConfig {
                    simulate_runs: false,
                },
                None,
            ));
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctl, true)]);
        registry
    }

    /// A watering restriction that ALWAYS blocks: forbidden hours 00..24 cover
    /// every hour, so `restrictions::evaluate` returns `skip = true` for any
    /// instant regardless of the wall clock. Used to prove a hot-swapped
    /// restriction reaches the dispatcher (FIX 1).
    fn always_skip_policy() -> WateringPolicy {
        use crate::config::schema::{EffectiveWindow, WateringRestriction};
        let mut p = WateringPolicy::default();
        p.restrictions = vec![WateringRestriction {
            id: "test_always".into(),
            name: "Always blocked".into(),
            enabled: true,
            effective: EffectiveWindow::AllYear,
            allowed_weekdays_odd: vec![],
            allowed_weekdays_even: vec![],
            forbidden_hour_start: Some(0),
            forbidden_hour_end: Some(24),
            max_minutes_per_zone: None,
        }];
        p
    }

    /// True when a `manual:<id>` run-row exists in the store window for the frozen
    /// test instant. A DISPATCHED run inserts a completed row; a SKIPPED run
    /// inserts a skipped row, so we additionally check status to distinguish.
    async fn dispatched_run_exists(runs: &RunsStore, id: &str) -> bool {
        let rows = runs.window(0, i64::MAX).await.unwrap();
        rows.iter()
            .any(|r| r.source == format!("manual:{id}") && r.status != "skipped")
    }

    #[test]
    fn next_tick_reads_the_live_swapped_schedule_set() {
        // (1) Changing manual_schedules in the shared handle is what the tick reads:
        // the dispatcher does exactly this `load_full()` at the top of each cycle.
        // A handle that was EMPTY at boot (a config with no schedules) reflects a
        // first schedule the moment it is swapped in, with no restart, so the next
        // tick sees it. This is the load the spawned loop performs each cycle.
        let handle: Arc<ArcSwap<Vec<ManualSchedule>>> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        assert!(
            handle.load_full().is_empty(),
            "previously-empty config: the tick's load reads an empty set"
        );

        // Swap in a first schedule (what apply_runtime_config does on a config
        // write). The SAME handle the dispatcher loop holds now yields it.
        handle.store(Arc::new(vec![sched(
            "a",
            "back_yard",
            vec![3],
            ManualMode::Override,
        )]));
        let live = handle.load_full();
        assert_eq!(
            live.len(),
            1,
            "the next tick's load reads the swapped-in set"
        );
        assert_eq!(live[0].id, "a");

        // Editing it again is likewise picked up by the next load with no restart.
        handle.store(Arc::new(vec![
            sched("a", "front_yard", vec![3], ManualMode::Override),
            sched("b", "side_yard", vec![1], ManualMode::Floor),
        ]));
        let live = handle.load_full();
        assert_eq!(
            live.len(),
            2,
            "an edit grows/changes the set the tick reads"
        );
        assert_eq!(live[0].zone_slug, "front_yard");
    }

    #[tokio::test]
    async fn first_schedule_in_previously_empty_handle_is_dispatched() {
        // (2) A first schedule added to a previously-empty config is dispatchable:
        // run_tick loads the live handle and actuates the schedule, with no
        // !is_empty() special-casing. We evaluate one tick against the FROZEN
        // instant so the schedule (pinned to that instant via sched_at) matches
        // exactly and the test cannot straddle a minute boundary (FIX 3).
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let policy = WateringPolicy::default();
        let schedules = vec![sched_at(now, "first", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &policy,
            &registry,
            Some(&runs),
            Some(&active),
            &mut last_fired,
        )
        .await;

        assert!(
            dispatched_run_exists(&runs, "first").await,
            "the first schedule in a previously-empty handle dispatched a run"
        );
    }

    #[tokio::test]
    async fn hot_swapped_restriction_blocks_scheduled_valve_next_tick() {
        // FIX 1 (safety): a hot-reloaded restrictive watering policy reaches the
        // SCHEDULED-valve path on the dispatcher's NEXT tick, not at the next
        // container restart. We run two ticks of the SAME schedule against the SAME
        // policy handle the dispatcher reads, swapping the policy between them:
        //   tick 1: permissive policy -> the schedule dispatches a run.
        //   (swap an always-skip restriction into the handle, as a PUT /api/config
        //    apply_runtime_config would)
        //   tick 2 (a different schedule id, so dedup never masks the result): the
        //    dispatcher reads the LIVE restrictive policy and does NOT open the
        //    valve, logging a skip instead.
        // The boot-frozen value the dispatcher used previously would have let the
        // second schedule water despite the live BLOCK, the silent valve-command gap.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();

        // The swappable policy handle the dispatcher reads each tick (load_full),
        // exactly the handle main.rs threads in.
        let policy_handle: Arc<ArcSwap<WateringPolicy>> =
            Arc::new(ArcSwap::from_pointee(WateringPolicy::default()));
        let mut last_fired = HashMap::new();

        // tick 1: permissive policy, schedule "allowed" fires.
        let allowed = vec![sched_at(now, "allowed", "back_yard")];
        run_tick(
            now,
            &allowed,
            &policy_handle.load_full(),
            &registry,
            Some(&runs),
            Some(&active),
            &mut last_fired,
        )
        .await;
        assert!(
            dispatched_run_exists(&runs, "allowed").await,
            "baseline: under the permissive policy the scheduled valve opened"
        );

        // Hot-swap a restriction that BLOCKS watering into the SAME handle (a
        // config write). No restart; the next run_tick reads it via load_full.
        policy_handle.store(Arc::new(always_skip_policy()));

        // tick 2: a distinct schedule "blocked" at the same instant. The dispatcher
        // must read the LIVE restrictive policy and NOT open the valve.
        let blocked = vec![sched_at(now, "blocked", "front_yard")];
        run_tick(
            now,
            &blocked,
            &policy_handle.load_full(),
            &registry,
            Some(&runs),
            Some(&active),
            &mut last_fired,
        )
        .await;
        assert!(
            !dispatched_run_exists(&runs, "blocked").await,
            "the hot-swapped restriction reached the dispatcher on the next tick: \
             the scheduled valve was NOT opened"
        );
        // And it is recorded as a skip with the restriction reason, not silently
        // dropped, so the operator sees why.
        let rows = runs.window(0, i64::MAX).await.unwrap();
        let skip = rows.iter().find(|r| r.source == "manual:blocked");
        assert!(
            skip.map(|r| r.status == "skipped").unwrap_or(false),
            "the blocked schedule logged a skip row; rows={rows:?}"
        );
    }

    #[tokio::test]
    async fn retiming_an_already_fired_schedule_refires_at_new_time() {
        // FIX 2: re-timing a schedule that already fired earlier today to a LATER
        // slot the same day must fire again at the new time. Keying dedup on
        // (id, start_hour, start_minute) makes the re-timed slot a FRESH key, so it
        // fires; the once-per-(id,time)-per-day-window guarantee still holds (a
        // repeat tick at the SAME time does not re-fire).
        //
        // We assert on the dedup ledger `last_fired`, which is the exact thing the
        // dedup keys on and is recorded only after a fire is attempted. (The run
        // ROW is not a reliable counter here: the runs table is UNIQUE(zone,
        // start_epoch) + INSERT OR IGNORE, so two fires on the same zone within the
        // same wall-clock second collapse to one row, a persistence detail unrelated
        // to the dispatch decision FIX 2 governs.)
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let today = now.date_naive();
        let mut last_fired = HashMap::new();

        // First slot: 05:00 (the frozen instant). Fires -> records the (id, 5, 0) key.
        let first = vec![sched_at(now, "s", "back_yard")];
        run_tick(
            now,
            &first,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            &mut last_fired,
        )
        .await;
        assert_eq!(
            last_fired.get(&("s".to_string(), 5, 0)),
            Some(&today),
            "the 05:00 slot fired and recorded its (id, time) dedup key"
        );

        // A repeat tick at the SAME 05:00 instant must NOT re-fire: the (id, 5, 0)
        // key already holds today, so the once-per-(id,time)-per-day-window guard
        // short-circuits before dispatch. (No (id, 5, 0) re-record / no new key.)
        run_tick(
            now,
            &first,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            &mut last_fired,
        )
        .await;
        assert_eq!(
            last_fired.len(),
            1,
            "a repeat tick at the same time added no new fire key (no double-fire)"
        );

        // Re-time the SAME id to 05:30 (later, same day). A tick at 05:30 must
        // re-fire: the (id, 5, 30) dedup key has no entry yet, so a NEW key appears.
        let later = now + chrono::Duration::minutes(30);
        let mut retimed = sched_at(now, "s", "back_yard");
        retimed.start_minute = later.minute() as u8;
        let retimed = vec![retimed];
        run_tick(
            later,
            &retimed,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            &mut last_fired,
        )
        .await;
        assert_eq!(
            last_fired.get(&("s".to_string(), 5, 30)),
            Some(&today),
            "re-timing the fired schedule to a later same-day slot fired again at \
             the new time (a fresh (id, 5, 30) dedup key)"
        );
        // Both the 05:00 and 05:30 windows are now recorded as fired today.
        assert_eq!(
            last_fired.len(),
            2,
            "both distinct (id, time) windows fired"
        );
    }

    #[tokio::test]
    async fn last_fired_is_pruned_when_a_schedule_is_removed() {
        // FIX 4: a removed schedule's dedup entry is pruned each tick, so last_fired
        // cannot grow unbounded across add/remove churn. We fire a schedule, then
        // tick with it gone and assert its key is no longer retained.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let mut last_fired = HashMap::new();

        let present = vec![sched_at(now, "gone", "back_yard")];
        run_tick(
            now,
            &present,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            &mut last_fired,
        )
        .await;
        assert!(
            last_fired.keys().any(|(id, _, _)| id == "gone"),
            "the fired schedule recorded a dedup entry"
        );

        // Next tick with the schedule removed from the live set prunes its entry.
        let empty: Vec<ManualSchedule> = vec![];
        run_tick(
            now,
            &empty,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            &mut last_fired,
        )
        .await;
        assert!(
            !last_fired.keys().any(|(id, _, _)| id == "gone"),
            "the removed schedule's dedup entry was pruned"
        );
    }
}
