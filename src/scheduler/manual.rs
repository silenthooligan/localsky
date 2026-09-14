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
//   4. Evaluate every hold the owner can set, in the skip-rule ladder's own
//      order: a per-zone Skip override, the global Skip override, Rain delay,
//      the Vacation pause toggle, then the Phase C watering restrictions, and
//      -- after the weather rung in (5) -- Dry run. Any of them persists a
//      `status="skipped"` runs row carrying the ladder's own sentence for that
//      rung, and skips dispatch. NONE of them is waivable: these are the owner
//      speaking, or the law, and `ignore_weather_safety` reaches neither.
//      Two of those rows are not word-for-word the morning's: a global Skip
//      writes `pre_soil`'s YARD-level wording ("Manual override: skip") where
//      the smart path's per-ZONE row for the same state reads "Override: skip
//      (global)", and a control surface that cannot be READ has no rung in the
//      ladder at all (the morning reuses the last state it read; this
//      dispatcher keeps nothing across ticks, so it holds and says so).
//   5. Consult the engine's WEATHER SAFETY gates off the snapshot the
//      refresher publishes, in the ladder's own position (after the
//      restrictions, before Dry run). The yard's `skip_check` already carries
//      the verdict, the reason and the reason_code, so this reads that verdict
//      rather than re-deriving one from raw weather: a skip whose reason_code
//      is freeze_now / overnight_freeze / wind_now / wind_forecast / rain_now /
//      live_data holds the schedule and writes the engine's OWN reason string
//      to History. A snapshot too old to water on (or one the refresher has
//      never populated) holds the same way, because an unknown verdict is not
//      permission. The rain-forecast and soil gates are deliberately NOT
//      consulted: a schedule exists to water at a time the owner picked, and
//      holding it because the soil model is satisfied would defeat it. A
//      schedule carrying `ignore_weather_safety` waives THIS rung only, and
//      leaves a History row naming the gate it overrode.
//   6. A run is a SPAN, not an instant, so the restriction gate is tested at
//      both ends of it: the run is trimmed so it closes before the first
//      minute it may not be open in, the way `dispatch_window::choose`
//      requires both ends of the morning's window to be permitted.
//   7. Otherwise call the default controller's `run_zone(slug, duration)`.
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
use crate::controllers::dispatch::{self, Arm, Dispatcher, RunOutcome, RunRequest, Source};
use crate::controllers::registry::ControllerRegistry;
use crate::engine::clock::CivilDay;
use crate::engine::restrictions;
use crate::model::IrrigationControlState;
use crate::model::IrrigationSnapshot;
use crate::persistence::runs::{NewRun, RunsStore};
use crate::persistence::ActiveRunsStore;
use crate::persistence::IrrigationControlStore;
use crate::push::dispatcher::PushDispatcher;
use crate::refresher::IrrigationStore;
use crate::refresher::WateringPolicy;

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
/// `active_runs` is the commanded-valve deadline ledger: each
/// dispatched run arms a shutoff deadline so the reaper closes the valve
/// even if this process dies before the controller's own (in-process)
/// timer fires. Mirrors the API path (src/api/irrigation.rs). `None` when
/// no persistence DB is mounted (the reaper is then also absent).
///
/// `control` is the control surface the owner actually taps: Rain delay, the
/// Vacation pause toggle, the sticky global and per-zone Skip overrides, Dry
/// run. It is read at the top of every tick, alongside the policy. This
/// dispatcher used to hold NOTHING but the watering restrictions, so a rain
/// delay or a vacation pause stopped the smart morning and left every enabled
/// manual schedule opening valves on its own clock, while the panel the owner
/// tapped said "Pause every zone for a set time" and the Home Assistant
/// migration guide told them to set Rain delay to hold everything. A hold binds
/// every unattended dispatch path or it is not a hold, and it binds it on a
/// FAILED read too: an unreadable surface holds every schedule due that tick,
/// the way the refresher's sibling read of the same surface holds the last
/// state it saw. `None` when no persistence DB is mounted, which reads as
/// "nothing set" -- there is then no surface for the owner to set a hold on, so
/// a missing store and a failed read are not the same fact.
///
/// `irrigation` is the snapshot the refresher publishes, and it is how this
/// dispatcher reaches the WEATHER SAFETY gates at all. It had none: freeze,
/// overnight freeze, wind, rain-falling-now and the live-data fail-safe were
/// computed once per tick, published on that snapshot, and consumed only by the
/// smart morning, so a schedule opened valves in a hard freeze on the one path
/// the owner is not awake to watch. The store is read (not subscribed) at the
/// top of every tick, the same shape as the policy and control loads, so a
/// verdict that flips between ticks binds on the next one.
///
/// Reading the SNAPSHOT rather than re-running the gates is deliberate: the
/// snapshot already carries the yard's verdict, the engine's own reason
/// sentence and the structured reason_code, so the two dispatch paths cannot
/// disagree about the weather or word the same hold two different ways.
pub fn spawn(
    schedules: Arc<ArcSwap<Vec<ManualSchedule>>>,
    watering_policy: Arc<ArcSwap<WateringPolicy>>,
    controllers: ControllerRegistry,
    runs: Option<RunsStore>,
    active_runs: Option<ActiveRunsStore>,
    control: Option<IrrigationControlStore>,
    irrigation: Arc<IrrigationStore>,
    push: Option<PushDispatcher>,
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
            // The CURRENT weather verdict, read the same way and at the same
            // point in the cycle. `snapshot()` is a load_full on an ArcSwap, so
            // this is the value the refresher published on its last tick; a
            // never-populated store yields the default snapshot, whose
            // `last_refresh_epoch` of 0 the freshness gate below reads as "no
            // verdict yet" and holds on.
            let snapshot = irrigation.snapshot();
            // Wall-clock in the CONFIGURED timezone, not the container TZ.
            // Sampled once per tick and passed into run_tick so the dispatch decision
            // is taken against a single, frozen instant (the test seam injects this).
            let now = crate::timeutil::now_local();
            // A panic inside one tick (an adapter bug, a poisoned
            // lock) must not kill the scheduler for the process lifetime,
            // silently ending all manual watering. catch_unwind turns it into
            // a logged skip; `last_fired` lives OUTSIDE the wrapped future, so
            // the dedup ledger survives and the next tick cannot double-fire.
            {
                use futures::FutureExt;
                let outcome = std::panic::AssertUnwindSafe(run_tick(
                    now,
                    &schedules,
                    &watering_policy,
                    &controllers,
                    runs.as_ref(),
                    active_runs.as_ref(),
                    control.as_ref(),
                    Some(snapshot.as_ref()),
                    push.as_ref(),
                    &mut last_fired,
                ))
                .catch_unwind()
                .await;
                if outcome.is_err() {
                    tracing::error!("manual scheduler: tick PANICKED; continuing on next tick");
                }
            }
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
///
/// `snapshot` is the weather verdict this tick is judged against. `None` means
/// there is no verdict to judge against, and it HOLDS -- there is deliberately
/// no value of this parameter that means "no weather gates, dispatch anyway".
/// The waiver is a property of the SCHEDULE (`ignore_weather_safety`), which is
/// a thing the owner set and History records the use of; an absent snapshot is
/// a thing that happened to the process, and it must never silently buy a
/// schedule the same permission.
#[allow(clippy::too_many_arguments)]
async fn run_tick(
    now: chrono::DateTime<chrono::FixedOffset>,
    schedules: &[ManualSchedule],
    watering_policy: &WateringPolicy,
    controllers: &ControllerRegistry,
    runs: Option<&RunsStore>,
    active_runs: Option<&ActiveRunsStore>,
    control: Option<&IrrigationControlStore>,
    snapshot: Option<&IrrigationSnapshot>,
    push: Option<&PushDispatcher>,
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
    let now_epoch = now.timestamp();
    let cal = crate::timeutil::deployment_calendar();

    // The control surface the owner tapped: Rain delay, the Vacation pause
    // toggle, the sticky global and per-zone Skip overrides, Dry run. Read
    // ONCE per tick against the same frozen instant the dispatch decision uses,
    // the way the spawn loop loads the policy, so no schedule in this pass can
    // be judged against a different control state than its neighbour.
    //
    // A FAILED read holds. `get_on` resolves an error to the default state,
    // which reads here as "no pause, no override" and opens the valve; its own
    // doc calls that lenient read WRONG for anything that concludes something
    // irreversible from an absence, and a valve command is irreversible. The
    // sibling reader of this same surface -- the refresher, deciding the same
    // question for the smart morning -- already fails CLOSED: `try_get`, then
    // "control state read failed; holding the last known pause and override"
    // (src/refresher/shell.rs). The two unattended paths now agree on the
    // failure posture as well as on the state. A hold that evaporates when the
    // database is busy is not a hold, and the asymmetry runs one way: a
    // fabricated hold costs one morning, a dropped hold waters through the rain
    // delay the owner set.
    //
    // A MISSING store is a different fact and keeps the old posture: no
    // persistence DB means there is no surface for the owner to set a hold ON,
    // so there is nothing to honor and nothing to be uncertain about.
    let control_read = match control {
        None => Some(IrrigationControlState::default()),
        Some(c) => match c.try_get_on(&today.to_string()).await {
            Ok(state) => Some(state),
            Err(e) => {
                warn!(
                    error = %e,
                    "manual scheduler: control state read failed; holding every schedule due this tick"
                );
                None
            }
        },
    };

    // The one instant the restriction gate is judged at, and the days this
    // week that already watered.
    //
    // This used to call `restrictions::evaluate`, the reduced entrypoint that
    // hardcodes an EMPTY watered-days slice, so `max_days_per_week` always
    // compared against zero and could never bind: a two-days-a-week district
    // with a manual schedule watered on day three while the engine path, which
    // passes the real history, skipped it. Same week frame as the engine, and
    // the run-history rollup's own evidence test: distinct local days in the
    // Sunday-to-Saturday week carrying a row that reads as watering. See
    // `watered_days_this_week` for where that parts company with the engine's
    // own list, and why the difference points at not watering.
    let when = crate::engine::clock::DecisionTime::at(cal, now_epoch);
    // Nothing but the days-per-week allowance reads this list, so the query is
    // skipped outright on the common policy that sets no such cap.
    let caps_days = watering_policy
        .restrictions
        .iter()
        .any(|r| r.max_days_per_week.is_some());
    let watered = match when.day() {
        Some(day) if caps_days => watered_days_this_week(runs, cal, day).await,
        _ => Vec::new(),
    };
    for s in schedules.iter() {
        if !s.enabled {
            continue;
        }
        if !s.weekdays.contains(&weekday) {
            continue;
        }
        // Catch-up grace: a tick that overran its minute (a slow or hung
        // dispatch on the previous tick pushing wall-clock past the target
        // minute boundary) must not drop the day's watering. Fire when `now` is
        // at or just past the scheduled minute, within CATCHUP_GRACE_S; the
        // (id, time) dedup below still makes it fire exactly once. Scoped to the
        // same day/minute window, so a cross-midnight catch-up is out of scope.
        const CATCHUP_GRACE_S: i64 = 180;
        let now_secs = hour as i64 * 3600 + minute as i64 * 60 + now.second() as i64;
        let sched_secs = s.start_hour as i64 * 3600 + s.start_minute as i64 * 60;
        let delta = now_secs - sched_secs;
        if !(0..=CATCHUP_GRACE_S).contains(&delta) {
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

        // The control surface could not be read this tick, so whether the owner
        // set a hold is UNKNOWN. Unknown is not permission: the schedule takes
        // the hold, leaves the same shape of row every other hold leaves, and
        // marks the day the way every other hold marks it. A minute of SQLite
        // contention costs this occurrence; the alternative cost is watering
        // through a vacation pause because a read was busy.
        //
        // Marking the day rather than retrying inside the catch-up grace is
        // deliberate: a retry would write one "could not read" row a minute for
        // as long as the store stayed sick, and a morning whose control surface
        // cannot be read is not a morning to open valves on a guess. The owner
        // sees the row and the log line either way.
        let Some(control) = control_read.as_ref() else {
            record_skip(runs, controllers, s, now_epoch, CONTROL_UNREADABLE_REASON).await;
            info!(
                schedule = %s.id,
                zone = %s.zone_slug,
                reason = CONTROL_UNREADABLE_REASON,
                "manual scheduler: skipped run (control state unreadable)"
            );
            last_fired.insert(fire_key, today);
            continue;
        };

        // The owner's own holds, ahead of the restriction gate because that is
        // where the skip-rule ladder puts them (`skip_rules::pre_soil`): a
        // per-zone Skip override, the global Skip override, Rain delay, the
        // Vacation pause toggle.
        if let Some(reason) = hold_reason(control, &s.zone_slug, now_epoch, cal) {
            record_skip(runs, controllers, s, now_epoch, &reason).await;
            info!(
                schedule = %s.id,
                zone = %s.zone_slug,
                reason = %reason,
                "manual scheduler: skipped run (control hold)"
            );
            last_fired.insert(fire_key, today);
            continue;
        }

        // Scope both the hold and duration cap to this schedule's configured
        // head and zone. An exemption stands aside for that rule only; other
        // applicable rules and all operator/weather/data holds still apply.
        let zone_soil_cfg = watering_policy
            .soil_zones
            .iter()
            .find(|z| z.slug == s.zone_slug);
        let scope = restrictions::ZoneScope {
            slug: &s.zone_slug,
            sprinkler: zone_soil_cfg.map(|z| z.sprinkler_type).unwrap_or_default(),
        };
        let v = restrictions::evaluate_for(
            when,
            &watering_policy.restrictions,
            watering_policy.address_parity,
            &watered,
            Some(scope),
        );
        let cap_seconds = v.max_minutes_cap.map(|m| m.saturating_mul(60));
        if v.skip {
            let reason = v
                .reason
                .clone()
                .unwrap_or_else(|| "watering restriction".to_string());
            record_skip(runs, controllers, s, now_epoch, &reason).await;
            info!(
                schedule = %s.id,
                zone = %s.zone_slug,
                reason = %reason,
                "manual scheduler: skipped run (watering restriction)"
            );
            last_fired.insert(fire_key, today);
            continue;
        }

        // A weather waiver cannot certify a configured probe without a fresh
        // engine verdict. Use the current binding, never a raw-reading guess.
        let probe_configured = zone_soil_cfg
            .and_then(|z| z.soil_sensor_id.as_deref())
            .is_some_and(|id| !id.trim().is_empty());
        if probe_configured
            && !snapshot.is_some_and(|snap| snapshot_is_fresh(snap.last_refresh_epoch, now_epoch))
        {
            record_skip(runs, controllers, s, now_epoch, NO_PROBE_VERDICT_REASON).await;
            info!(schedule = %s.id, zone = %s.zone_slug,
                "manual scheduler: configured probe has no recent engine verdict");
            // Unknown can recover on the next refresh within the catch-up
            // window, so this observation does not mark the whole day handled.
            continue;
        }

        // Probe integrity is a separate engine decision, carried even if a
        // weather gate won the displayed verdict. A standing weather waiver
        // cannot turn an unavailable or distrusted soil reading into permission.
        if let Some(reason) =
            snapshot.and_then(|snap| snap.skip_check.soil_probe_holds.get(&s.zone_slug))
        {
            record_skip(runs, controllers, s, now_epoch, reason).await;
            info!(schedule = %s.id, zone = %s.zone_slug, reason = %reason,
                "manual scheduler: skipped run (soil probe data hold)");
            last_fired.insert(fire_key, today);
            continue;
        }

        if watering_policy.script_rules_enabled
            && !snapshot.is_some_and(|snap| snapshot_is_fresh(snap.last_refresh_epoch, now_epoch))
        {
            record_skip(runs, controllers, s, now_epoch, NO_SCRIPT_VERDICT_REASON).await;
            info!(schedule = %s.id, zone = %s.zone_slug,
                "manual scheduler: enabled user scripts have no recent engine verdict");
            continue;
        }

        // Script holds are owner decisions, including scripts that failed to
        // evaluate. The brain carries this result even when weather won the
        // displayed verdict; a standing weather waiver never waives the rule.
        if let Some(hold) = snapshot.and_then(|snap| snap.skip_check.script_hold.as_ref()) {
            record_skip(runs, controllers, s, now_epoch, &hold.reason).await;
            info!(schedule = %s.id, zone = %s.zone_slug, rule = %hold.id,
                "manual scheduler: skipped run (user script hold)");
            last_fired.insert(fire_key, today);
            continue;
        }

        // The engine's WEATHER SAFETY gates, in the ladder's own position: after
        // the restrictions (so a schedule blocked by BOTH names the legal
        // block, the way `pre_soil` orders it) and before Dry run.
        //
        // This dispatcher had no weather rung at all. Freeze, overnight freeze,
        // wind now, wind forecast, rain falling now and the live-data fail-safe
        // were computed once per tick and consumed only by the smart morning,
        // so a schedule opened valves in a hard freeze, on the one dispatch path
        // nobody is awake to watch.
        let waived: Option<WeatherHold> = match weather_hold(snapshot, now_epoch, &s.zone_slug) {
            // A fresh verdict that names no safety gate. Fall through and
            // dispatch. This is also where every rain-forecast and soil skip
            // lands, deliberately: see `SAFETY_GATES`.
            None => None,
            // The owner's standing waiver. The gate is CARRIED, not spent here:
            // rungs below this one can still stop the schedule (Dry run is the
            // ladder's last rung, and the day's ceiling can still refuse), and a
            // History row saying "watering anyway" above a schedule that then
            // did not water is a lie in the one record the owner audits the
            // waiver from. It is written at the moment the valve is actually
            // commanded, so the row means what it says: this run happened
            // because the waiver was on.
            Some(hold) if s.ignore_weather_safety => Some(hold),
            Some(hold) => {
                // The engine's own sentence, verbatim, so History reads the
                // same for a held schedule as for a held morning.
                record_skip(runs, controllers, s, now_epoch, &hold.reason).await;
                info!(
                    schedule = %s.id,
                    zone = %s.zone_slug,
                    gate = %hold.code,
                    reason = %hold.reason,
                    "manual scheduler: skipped run (weather safety)"
                );
                // A real gate marks the day, the way every other hold above
                // does: the verdict is KNOWN and will not change inside the
                // 3-minute catch-up grace, so re-evaluating would only write
                // the same row again on the next two ticks.
                //
                // The no-verdict hold is the one exception, and it is the
                // freshness gate's own posture in smart_morning: the refresher
                // usually recovers within seconds of boot, so leaving the day
                // unmarked lets a schedule that was one tick too early still
                // water inside the grace. It is bounded -- the grace is three
                // ticks, so a refresher that stays down costs three rows and
                // then the window closes on its own.
                if hold.code != NO_VERDICT_CODE {
                    last_fired.insert(fire_key, today);
                }
                continue;
            }
        };

        // Dry run, the ladder's last rung (`skip_rules::post_soil`): the whole
        // point of the toggle is that nothing reaches the hardware, so the run
        // is recorded as the same hold the morning records and no valve is
        // commanded. Ordered after restrictions so a schedule blocked by BOTH
        // still names the legal block, the way the ladder does.
        if control.is_dry_run {
            let reason = "All watering is on hold";
            record_skip(runs, controllers, s, now_epoch, reason).await;
            info!(
                schedule = %s.id,
                zone = %s.zone_slug,
                reason = %reason,
                "manual scheduler: skipped run (dry run)"
            );
            last_fired.insert(fire_key, today);
            continue;
        }

        // Dispatch through the controller the zone is bound to, else the
        // default.
        let controller = match controllers.for_zone(watering_policy.controller_id_for(&s.zone_slug))
        {
            Some(c) => c,
            None => {
                warn!(schedule = %s.id, "manual scheduler: no controller configured; skipping");
                last_fired.insert(fire_key, today);
                continue;
            }
        };
        let mut duration_s = s.duration_minutes.saturating_mul(60);
        if let Some(c) = cap_seconds {
            if c < duration_s {
                duration_s = c;
            }
        }
        // The restriction gate above judged ONE instant, the tick. A run is a
        // SPAN, and the span is what the district regulates: a 60-minute
        // schedule starting at 09:30 passed a 10:00-to-16:00 ban at 09:30 and
        // then watered half an hour inside it, every week, on the one path the
        // owner is not awake to watch. Trim it to close before the first minute
        // it may not be open in.
        let permitted_s =
            permitted_span_s(cal, watering_policy, &watered, scope, now_epoch, duration_s);
        if permitted_s < duration_s {
            info!(
                schedule = %s.id,
                zone = %s.zone_slug,
                requested_s = duration_s,
                permitted_s,
                "manual scheduler: run trimmed to close before a forbidden window"
            );
            duration_s = permitted_s;
        }
        info!(
            schedule = %s.id,
            zone = %s.zone_slug,
            requested_s = duration_s,
            "manual scheduler: dispatching run"
        );
        // The shared path: hard cap, the day's ceiling (a refusal leaves a
        // row), the per-zone lock, the run row, the shutoff deadline.
        let outcome = Dispatcher::new(controllers.zone_locks(), runs, active_runs)
            .run(RunRequest {
                session_id: format!("schedule:{}:{today}:{}", s.id, s.zone_slug),
                zone: &s.zone_slug,
                zone_name: &s.zone_slug,
                controller: &controller,
                seconds: duration_s,
                source: Source::ManualSchedule(s.id.clone()),
                ceiling_s: Some(dispatch::ceiling_for_zone(watering_policy, &s.zone_slug)),
                arm: Arm::AfterDispatch,
                record_row: true,
                cycle: None,
                push,
                now_epoch,
            })
            .await;
        match outcome {
            RunOutcome::Dispatched {
                handle, seconds, ..
            } => {
                // The valve is open, so a waiver carried down from the weather
                // rung has now actually bought a run. Record it, naming the gate
                // it overrode: a waiver that leaves no trace is how someone
                // forgets it is on. Written here rather than at the rung so the
                // row can never describe a dispatch that a lower rung (Dry run,
                // the day's ceiling, a dead controller) went on to stop.
                // Logged at WARN, not INFO: a valve opening into a freeze is
                // not routine, and the log is where an operator notices first.
                if let Some(hold) = waived.as_ref() {
                    record_waiver(runs, controllers, s, now_epoch, hold).await;
                    warn!(
                        schedule = %s.id,
                        zone = %s.zone_slug,
                        gate = %hold.code,
                        reason = %hold.reason,
                        "manual scheduler: WEATHER SAFETY WAIVED; valve opened anyway"
                    );
                }
                info!(
                    schedule = %s.id,
                    zone = %s.zone_slug,
                    controller = %handle.controller_id,
                    seconds,
                    provider_ref = ?handle.provider_ref,
                    "manual scheduler: run dispatched"
                );
            }
            // The dispatcher logged the refusal or the failure, with the
            // schedule id in its `source` field.
            RunOutcome::Refused(_) | RunOutcome::Failed { .. } => {}
        }
        last_fired.insert(fire_key, today);
    }
}

/// The engine gate ids this dispatcher treats as SAFETY, and therefore holds a
/// manual schedule on. Every one of them is a gate the smart morning stops at
/// in `skip_rules::pre_soil`, and they are listed here in that ladder's order.
///
/// The list is deliberately SHORT, and what is missing from it matters as much
/// as what is on it. `rain_next_4h`, `tomorrow_rain`, `rain_3day`,
/// `already_wet`, `rain_today_forecast`, `observed_rain` and `soil_saturation`
/// are all real gates the morning honors, and none of them appears here. A
/// manual schedule exists because the owner wants water at a time of their
/// choosing; holding it because the soil model has decided the zone is already
/// fine would defeat the reason they created it. This rung is about not
/// hurting anything, not about second-guessing the schedule.
///
/// `restrictions` is likewise absent, and for a different reason: it is
/// enforced one rung ABOVE this one, unwaivably, because it is law rather than
/// weather.
///
/// KNOWN GAP, recorded rather than quietly closed. `soil_frost` -- the soil
/// probe reading below the frost threshold -- is a yard-level gate in the same
/// `pre_soil` ladder and the same `GateFamily::Freeze` as `freeze_now`, so on
/// the hazard alone it belongs here. It is not on the list because the safety
/// set was specified as these six, and a manual schedule silently honoring a
/// seventh gate that the waiver UI does not name is its own kind of wrong.
/// Adding it is one line here and one line in the waiver's description.
const SAFETY_GATES: &[&str] = &[
    "live_data",
    "rain_now",
    "freeze_now",
    "overnight_freeze",
    // Frozen ground is the same hazard as freezing air and sits in the
    // same family in the gate catalog. A schedule that honours one and
    // not the other is incoherent, and the waiver names both.
    "soil_frost",
    "wind_now",
    "wind_forecast",
];

/// Freezing, judged directly rather than through the morning's window.
///
/// The engine's freeze rungs are window-relative: `freeze_on_trial` reads
/// the WINDOW's minimum on a post-sunrise window rather than the
/// temperature now, and `overnight_freeze` only applies to a pre-dawn
/// window. That is right for the morning, which runs inside that window.
/// A manual schedule runs on its own clock, possibly hours away from it,
/// so trusting only the published reason code means the yard can be
/// freezing while the gate that names freezing did not fire.
///
/// Belt and braces on the one hazard that splits pipes.
fn freezing_now(sc: &crate::model::SkipCheck) -> Option<WeatherHold> {
    if sc.temp_now_f < sc.min_temp_f {
        return Some(WeatherHold {
            code: "freeze_now".to_string(),
            reason: format!(
                "Freezing now ({:.0}F, below the {:.0}F floor)",
                sc.temp_now_f, sc.min_temp_f
            ),
        });
    }
    None
}

use super::snapshot_is_fresh;
#[cfg(test)]
use super::MAX_SNAPSHOT_AGE_S;

/// The pseudo-gate id a stale or absent snapshot holds under.
///
/// It reuses the engine's own `live_data` id rather than inventing one, because
/// it is the same fact one layer up: `live_data` fires when the engine has no
/// live readings to judge on, and this fires when the engine has published no
/// judgement at all. Sharing the id is what makes the waiver coherent -- an
/// owner who has accepted watering through `live_data` has accepted watering
/// with no weather, and it would be incoherent to then hold them on the strictly
/// LESS informative version of the same condition.
///
/// Sharing the id has one more consequence, and it is intended: the dispatch
/// loop keys "retry inside the catch-up grace instead of marking the day" on
/// this id, so a GENUINE `live_data` skip from the engine gets the same
/// treatment as a missing snapshot. Both mean the same thing -- there is no
/// weather to judge on right now -- and both usually clear on their own within
/// seconds, so re-asking on the next tick is the right response to either.
const NO_VERDICT_CODE: &str = "live_data";

/// The History sentence a schedule leaves when there is no verdict to judge it
/// against. Not an engine string -- the engine did not speak -- so it says the
/// honest thing instead of naming a gate that never fired.
const NO_SCRIPT_VERDICT_REASON: &str =
    "Held: no recent verdict for the enabled user watering scripts";

const NO_PROBE_VERDICT_REASON: &str =
    "Held: no recent soil probe verdict for this configured probe";

const NO_VERDICT_REASON: &str =
    "Held: no recent weather verdict to check freeze, wind or rain against";

/// A weather gate that binds this schedule right now.
struct WeatherHold {
    /// The engine's structured gate id, for the log line and the waiver row.
    code: String,
    /// The engine's OWN reason sentence, written to History verbatim so the
    /// manual path and the morning cannot word one cause two ways.
    reason: String,
}

/// The WEATHER SAFETY gate holding the yard right now, if one does.
///
/// This reads the verdict the refresher published; it does not re-derive one.
/// The snapshot's `skip_check` already carries the verdict, the engine's reason
/// sentence and the structured `reason_code`, all computed by the same
/// `skip_rules` ladder the morning obeys, so reading it is what keeps the two
/// dispatch paths from disagreeing about the weather.
///
/// Returns `None` only when there IS a fresh verdict and it does not name a
/// safety gate. Every other outcome -- no snapshot, a snapshot too old, a
/// snapshot that never refreshed -- is a hold, because an unknown verdict is
/// not permission and this decision opens a valve.
fn weather_hold(
    snapshot: Option<&IrrigationSnapshot>,
    now_epoch: i64,
    zone_slug: &str,
) -> Option<WeatherHold> {
    let Some(snap) = snapshot else {
        return Some(WeatherHold {
            code: NO_VERDICT_CODE.to_string(),
            reason: NO_VERDICT_REASON.to_string(),
        });
    };
    if !snapshot_is_fresh(snap.last_refresh_epoch, now_epoch) {
        return Some(WeatherHold {
            code: NO_VERDICT_CODE.to_string(),
            reason: NO_VERDICT_REASON.to_string(),
        });
    }
    // A yard restriction can mask a safety gate on its exempt zone. Consume
    // that zone's completed engine verdict before the aggregate fallback.
    let zone_verdict = snap
        .zone_verdicts
        .iter()
        .find(|v| v.zone_slug == zone_slug)
        .or_else(|| {
            snap.zones
                .iter()
                .find(|z| z.slug == zone_slug)
                .and_then(|z| z.verdict.as_ref())
        });
    if let Some(v) = zone_verdict
        .filter(|v| v.verdict == "skip" && SAFETY_GATES.contains(&v.reason_code.as_str()))
    {
        return Some(WeatherHold {
            code: v.reason_code.clone(),
            reason: if v.reason.trim().is_empty() {
                format!("Held by the {} safety gate", v.reason_code)
            } else {
                v.reason.clone()
            },
        });
    }
    let sc = &snap.skip_check;
    // `will_skip` and `verdict` are two fields describing one decision and
    // `SkipCheck::is_coherent` is what pins them together. Read them as an OR
    // anyway: if they ever disagree, the disagreement is a bug, and the safe
    // reading of a buggy verdict on a path that opens a valve is the one that
    // does not water.
    if !sc.will_skip && sc.verdict != "skip" {
        return None;
    }
    // Asked before the gate id, because the id can be absent on exactly
    // the mornings this matters: see `freezing_now`.
    if let Some(h) = freezing_now(sc) {
        return Some(h);
    }
    if !SAFETY_GATES.contains(&sc.reason_code.as_str()) {
        return None;
    }
    Some(WeatherHold {
        code: sc.reason_code.clone(),
        // Older snapshots (and any producer that set a code but no sentence)
        // can carry an empty reason; History must never show a blank hold.
        reason: if sc.reason.trim().is_empty() {
            format!("Held by the {} safety gate", sc.reason_code)
        } else {
            sc.reason.clone()
        },
    })
}

/// The History row a declined schedule leaves, whatever declined it.
///
/// One shape for every reason: the row is attributed to the schedule
/// (`manual:<id>`) and carries the planned duration, so History shows what
/// WOULD have run and the sentence explaining why it did not.
async fn record_skip(
    runs: Option<&RunsStore>,
    controllers: &ControllerRegistry,
    s: &ManualSchedule,
    now_epoch: i64,
    reason: &str,
) {
    record_row(runs, controllers, s, now_epoch, reason).await;
}

/// The History row a dispatch made under `ignore_weather_safety` leaves.
///
/// It names the gate that was overridden, because a waiver that leaves no
/// trace is how someone forgets it is on. Two things bound when it is written,
/// and both are what keep the row worth reading:
///
///   * only when a gate ACTUALLY fired and was waived, never on every run of a
///     waived schedule -- a row on a clear morning says nothing, and that noise
///     is exactly what stops the real ones from being noticed;
///   * only once the controller has confirmed the dispatch, so it can never
///     describe a run that Dry run, the day's ceiling or a dead controller went
///     on to stop.
///
/// Written one second BEFORE the run, and that is load-bearing, not cosmetic.
/// The runs table carries `UNIQUE(zone_slug, start_epoch, controller_id)` and
/// every insert is `INSERT OR IGNORE`, so a marker written at the tick instant
/// can be the same key as the dispatcher's own run row (the controller stamps
/// that row with its own `started_epoch`, which lands in the same second as the
/// tick most of the time). The row inserted FIRST wins, so this one would
/// silently swallow the run row, and the water would then be invisible to
/// History, to the day's ceiling, to the days-per-week allowance and to the
/// soil replay. One second earlier cannot collide with a run that has not
/// started yet, and it renders where it belongs: immediately before the run it
/// authorized.
///
/// It carries `status="skipped"`, which is the only reason-bearing row shape
/// available, and that is a deliberate trade with one real cost: this marker is
/// a skip-shaped row for a run that happened. It is the SAFE direction of that
/// trade. `history::rollup::is_watering_evidence` excludes `skipped`, so the
/// marker adds no phantom water to the balance or to the week's allowance,
/// while the real completed row beside it carries the actual water. The
/// alternative shapes both lie the other way: `insert_aborted` writes a status
/// the rollup COUNTS as watering, which would double-charge the soil model for
/// one run.
async fn record_waiver(
    runs: Option<&RunsStore>,
    controllers: &ControllerRegistry,
    s: &ManualSchedule,
    now_epoch: i64,
    hold: &WeatherHold,
) {
    let reason = format!(
        "Weather safety waived ({}): watering anyway. {}",
        hold.code, hold.reason
    );
    record_row(runs, controllers, s, now_epoch - 1, &reason).await;
}

/// The shared insert behind [`record_skip`] and [`record_waiver`]: one row
/// shape, attributed to the schedule, carrying the planned duration and the
/// sentence. `row_epoch` is separate from the tick instant only so the waiver
/// marker can sit one second clear of the run row's unique key; see
/// [`record_waiver`].
async fn record_row(
    runs: Option<&RunsStore>,
    controllers: &ControllerRegistry,
    s: &ManualSchedule,
    row_epoch: i64,
    reason: &str,
) {
    let Some(rs) = runs else {
        return;
    };
    let row = NewRun {
        session_id: None,
        zone_slug: s.zone_slug.clone(),
        start_epoch: row_epoch,
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
    if let Err(e) = rs.insert_skipped(row, reason.to_string()).await {
        warn!(schedule = %s.id, error = %e, "manual scheduler: history-row insert failed");
    }
}

/// The seconds of `seconds` this run may keep without crossing into a window
/// the restrictions forbid, starting at `start_epoch`.
///
/// `restrictions::evaluate_for` judges ONE instant, and the tick instant is the
/// START of a manual run. A 60-minute schedule at 09:30 under a 10:00-to-16:00
/// ban therefore passed the gate and watered for thirty minutes inside the ban.
/// The day's window chooser has always tested both ends of a span -- it takes a
/// post-sunrise hour only when `permit(t) && permit(t + seq)`
/// (`dispatch_window::choose`) -- and this is the same test for a schedule's
/// span, walked rather than sampled at the ends so a ban that opens and closes
/// inside a long run cannot be stepped over.
///
/// The span is half-open: the valve is SHUT at `start_epoch + seconds`, so that
/// closing instant is not tested and a run ending exactly on a ban's first
/// second is left whole. Every gate walked here is hour- or day-granular
/// (forbidden hours, the weekday and date rows, the effective window, the
/// week's allowance), and every real UTC offset is a whole number of minutes,
/// so a boundary always falls on a whole minute and a minute step cannot step
/// over one. The walk stops at `RUN_SECONDS_MAX` because that is the longest
/// run the dispatcher will command whatever this returns.
///
/// Uses the same zone scope as the tick's restriction check, so an exempt
/// schedule is neither trimmed nor admitted by a different set of rules.
///
/// A trimmed run still spends a day of any `max_days_per_week` allowance, the
/// way any other short run does. That is the pre-existing shape of the
/// allowance, not something the trim introduces: before it, the same schedule
/// spent the same day AND watered through the ban.
fn permitted_span_s(
    cal: crate::engine::calendar::Calendar,
    policy: &WateringPolicy,
    watered: &[CivilDay],
    scope: restrictions::ZoneScope<'_>,
    start_epoch: i64,
    seconds: u32,
) -> u32 {
    const STEP_S: u32 = 60;
    let span = seconds.min(crate::controllers::guard::RUN_SECONDS_MAX);
    let mut offset = STEP_S;
    while offset < span {
        let blocked = restrictions::evaluate_for(
            crate::engine::clock::DecisionTime::at(cal, start_epoch + i64::from(offset)),
            &policy.restrictions,
            policy.address_parity,
            watered,
            Some(scope),
        )
        .skip;
        if blocked {
            return offset;
        }
        offset = offset.saturating_add(STEP_S);
    }
    seconds
}

/// The History sentence a schedule leaves when the control surface could not be
/// read at all.
///
/// Not one of the ladder's own strings, because the ladder has no rung for it:
/// the refresher answers the same failure by reusing the last state it read,
/// and this dispatcher deliberately keeps nothing across ticks, so it says the
/// honest thing instead of naming a hold nobody set.
const CONTROL_UNREADABLE_REASON: &str = "Held: the pause and override settings could not be read";

/// The control-surface hold binding this zone right now, if one does, worded
/// exactly as the skip-rule ladder words it.
///
/// These are `skip_rules::pre_soil`'s first rungs, in its order: a per-zone
/// Skip override, then the sticky global Skip override, then Rain delay
/// (`pause_until_epoch`), then the Vacation pause toggle. Every one of them was
/// computed once per tick, published on the snapshot and consumed ONLY by the
/// smart morning, so this dispatcher held nothing but the watering
/// restrictions and kept opening valves straight through a 72-hour rain delay.
///
/// The reason strings are the ladder's, verbatim, with one known seam: for a
/// GLOBAL Skip this writes `pre_soil`'s yard-level "Manual override: skip",
/// while the smart path's per-ZONE row for the same state reads "Override: skip
/// (global)" (skip_rules.rs's `_ => match i.global_override` arm). Both are
/// ladder strings, so History is never wrong, but a zone-level view can show
/// two sentences for one cause depending on which dispatcher declined.
///
/// A force-RUN override is deliberately NOT consulted, in EITHER form -- the
/// per-zone one this reads for "skip", or the global one. This is the decision,
/// written down rather than left to be re-derived:
///
///   * It must not fire a manual schedule EARLY. Force is an instruction about
///     the morning's decision, not about a schedule's own clock, so it can
///     never move a run to a time the owner did not ask for.
///   * It must not RESCUE one from a hold. A hold is the owner saying stop
///     everything. When force and a hold disagree, STOP WINS: the cost of not
///     watering for a day is a dry lawn, and the cost of watering through a
///     hold is a week of watering while someone is away.
///
/// The smart path answers this the other way round, and that divergence is
/// deliberate rather than an oversight to be closed. `skip_rules::pre_soil`
/// matches the GLOBAL override FIRST and returns "Manual override: force run"
/// before it ever looks at `pause_until_epoch` or `is_paused`, so a global
/// force-run is the highest rung of that whole ladder; `decide_per_zone` does
/// the same for the per-zone form. So with Override > Run set and a stale rain
/// delay still in the future, the smart morning waters and a manual schedule
/// does not. That is the intended shape: force is an answer to "should the
/// yard water this morning", and a schedule the owner set for 05:00 is not that
/// question. Only the skip-shaped rungs apply here.
fn hold_reason(
    control: &IrrigationControlState,
    zone_slug: &str,
    now_epoch: i64,
    cal: crate::engine::calendar::Calendar,
) -> Option<String> {
    if control.zone_overrides.get(zone_slug).map(String::as_str) == Some("skip") {
        return Some("Override: skip (this zone)".to_string());
    }
    if control.global_override == "skip" {
        return Some("Manual override: skip".to_string());
    }
    if control.pause_until_epoch > 0 && now_epoch > 0 && now_epoch < control.pause_until_epoch {
        return Some(format!(
            "Paused (vacation until {})",
            pause_until_label(control.pause_until_epoch, cal)
        ));
    }
    if control.is_paused {
        return Some("Paused (vacation mode)".to_string());
    }
    None
}

/// The vacation-pause "until" stamp.
///
/// A deliberate character-for-character COPY of `format_pause_until` in
/// src/engine/skip_rules.rs -- the ladder's own renderer, private to that
/// module -- format string and `epoch {epoch}` fallback included, so the two
/// paths cannot write the same hold two different ways. Nothing tests the twins
/// against each other, so an edit to either silently drifts the History wording
/// between the manual path and the morning: change both, or make that one
/// `pub(crate)` and delete this.
///
/// The offset comes from the calendar AT THE EXPIRY, not at "now": a pause set
/// in October and expiring in December would otherwise read an hour early
/// across the November transition, and for an evening expiry name the wrong
/// weekday.
fn pause_until_label(epoch: i64, cal: crate::engine::calendar::Calendar) -> String {
    match cal.at(epoch) {
        Some(z) => z.format("%a %b %-d, %H:%M"),
        None => format!("epoch {epoch}"),
    }
}

/// The distinct local days in `today`'s Sunday-to-Saturday week on which the
/// yard actually watered, for the days-per-week allowance.
///
/// The week frame is the engine's: weeks run Sunday to Saturday
/// (`restrictions::week_allowance_spent`). The evidence test is the run-history
/// rollup's: a row counts when it passes
/// `history::rollup::is_watering_evidence`, so a hand-started run and a morning
/// run both spend a day and a skip marker spends none.
///
/// That is the same EVIDENCE the rollup uses, which is not quite the same list
/// the engine builds for itself: the engine's `watered_days` comes from the
/// soil replay's per-day applied valve seconds (src/assembly/mod.rs), so a row
/// that reads as watering but moved no water -- a dispatch stopped in the same
/// second, a zero-duration row -- spends a day here and none there. On that
/// edge the manual schedule skips where the morning would run, which is the
/// direction to be wrong in. Empty without a runs store, or on a read failure,
/// which reads as "nothing spent this week":
/// the safe direction is the one that cannot invent a compliance block out of a
/// transient SQLite error, and the next tick re-reads.
async fn watered_days_this_week(
    runs: Option<&RunsStore>,
    cal: crate::engine::calendar::Calendar,
    today: CivilDay,
) -> Vec<CivilDay> {
    let Some(rs) = runs else {
        return Vec::new();
    };
    let t = today.naive();
    let week_start = t - chrono::Duration::days(i64::from(t.weekday().num_days_from_sunday()));
    let week_end = week_start + chrono::Duration::days(6);
    let Some((from_epoch, _)) = cal.day_bounds_utc(CivilDay::from_naive(week_start)) else {
        return Vec::new();
    };
    let Some((_, to_epoch)) = cal.day_bounds_utc(CivilDay::from_naive(week_end)) else {
        return Vec::new();
    };
    let rows = match rs.window(from_epoch, to_epoch).await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                error = %e,
                "manual scheduler: runs read failed; judging no watering days spent this week"
            );
            return Vec::new();
        }
    };
    let mut days: Vec<NaiveDate> = rows
        .iter()
        .filter(|r| {
            crate::history::rollup::is_watering_evidence(
                &r.source,
                &r.status,
                r.skip_reason.as_deref(),
            )
        })
        .filter_map(|r| cal.local_date(r.start_epoch))
        .collect();
    days.sort_unstable();
    days.dedup();
    days.into_iter().map(CivilDay::from_naive).collect()
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

/// Every enabled `Override` schedule bound to `zone_slug`, reduced to the
/// weekdays it suppresses smart dispatch on and the names doing it.
/// `None` when nothing suppresses the zone. Purely descriptive: the
/// suppression is still decided by `override_active_today`, this only
/// gives the UI something to say instead of an unexplained zero.
pub fn override_suppression(
    schedules: &[ManualSchedule],
    zone_slug: &str,
    today_weekday: u8,
) -> Option<crate::model::SmartSuppression> {
    let mut weekdays: Vec<u8> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for s in schedules.iter().filter(|s| {
        s.enabled
            && s.zone_slug == zone_slug
            && !s.weekdays.is_empty()
            && matches!(s.mode, crate::config::schema::ManualMode::Override)
    }) {
        for d in &s.weekdays {
            if *d <= 6 && !weekdays.contains(d) {
                weekdays.push(*d);
            }
        }
        let label = if s.name.is_empty() {
            s.id.clone()
        } else {
            s.name.clone()
        };
        if !names.contains(&label) {
            names.push(label);
        }
    }
    if weekdays.is_empty() {
        return None;
    }
    weekdays.sort_unstable();
    Some(crate::model::SmartSuppression {
        active_today: weekdays.contains(&today_weekday),
        weekdays,
        schedules: names,
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
            ignore_weather_safety: false,
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

    /// A FROZEN test instant. Thu 2026-06-25 05:00:00 in UTC offset, chosen so the
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
            ignore_weather_safety: false,
        }
    }

    /// A snapshot that is fresh at `at` and whose yard verdict names no gate:
    /// the weather rung falls through and the schedule's own subject is what
    /// the test measures. Every pre-existing `run_tick` test passes this,
    /// because after the weather rung a tick with NO verdict holds -- which is
    /// the whole point of the rung, and would otherwise turn every one of those
    /// tests green for the wrong reason.
    fn fresh_snapshot(at: chrono::DateTime<FixedOffset>) -> IrrigationSnapshot {
        let mut s = IrrigationSnapshot::default();
        s.last_refresh_epoch = at.timestamp();
        s.skip_check.decide("run", String::new(), "run".into());
        s
    }

    /// A snapshot that is fresh at `at` and whose yard verdict is a SKIP
    /// decided by `code`, worded as the engine words it.
    fn snapshot_skipping(
        at: chrono::DateTime<FixedOffset>,
        code: &str,
        reason: &str,
    ) -> IrrigationSnapshot {
        let mut s = fresh_snapshot(at);
        s.skip_check
            .decide("skip", reason.to_string(), code.to_string());
        s
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
            ..Default::default()
        }];
        p
    }

    /// True when a `manual:<id>` run-row exists in the store window for the frozen
    /// test instant. A DISPATCHED run inserts a completed row; a SKIPPED run
    /// inserts a skipped row, so we additionally check status to distinguish.
    async fn dispatched_run_exists(runs: &RunsStore, id: &str) -> bool {
        let rows = runs.window(0, i64::MAX).await.unwrap();
        // The test registry's controller is a non-simulating DryRun, so a
        // dispatched run records as pretend water (source dry_run:<id>,
        // excluded from watering evidence); a real controller records
        // manual:<id>. Either form proves the valve command was issued.
        rows.iter().any(|r| {
            (r.source == format!("manual:{id}") || r.source == format!("dry_run:{id}"))
                && r.status != "skipped"
        })
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
            None,
            Some(&fresh_snapshot(now)),
            None,
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
            None,
            Some(&fresh_snapshot(now)),
            None,
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
            None,
            Some(&fresh_snapshot(now)),
            None,
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
            None,
            Some(&fresh_snapshot(now)),
            None,
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
            None,
            Some(&fresh_snapshot(now)),
            None,
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
            None,
            Some(&fresh_snapshot(later)),
            None,
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
            None,
            Some(&fresh_snapshot(now)),
            None,
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
            None,
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;
        assert!(
            !last_fired.keys().any(|(id, _, _)| id == "gone"),
            "the removed schedule's dedup entry was pruned"
        );
    }

    // ── The holds the owner can set ───────────────────────────────────────────
    //
    // Rain delay, the Vacation pause toggle, a global Skip override, a per-zone
    // Skip override and Dry run were computed once per tick, published on the
    // snapshot and consumed ONLY by the smart morning. This dispatcher held
    // nothing but the watering restrictions, so an enabled manual schedule kept
    // opening valves straight through a 72-hour rain delay while the panel the
    // owner tapped said "Pause every zone for a set time" and the Home
    // Assistant migration guide told them to set Rain delay to hold everything.
    //
    // Each test below runs ONE tick of a schedule pinned to the frozen instant
    // (so it is unambiguously due) against a control surface carrying one hold,
    // and asserts both halves: no valve command, and a History row carrying the
    // skip-rule ladder's own sentence.

    /// A migrated, empty control surface: no pause, auto override, dry run off.
    /// On its own in-memory DB; in production it shares the history connection,
    /// but nothing in the tick couples it to the runs store.
    fn control() -> IrrigationControlStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        IrrigationControlStore::new(Arc::new(Mutex::new(c)))
    }

    /// A control surface whose reads FAIL. The connection carries no
    /// migrations, so the singleton SELECT errors ("no such table") the way a
    /// locked database, a disk error or a blocking-pool join failure errors.
    /// Distinct from `control()` above, a MIGRATED but empty surface, which is
    /// a database answering confidently that nothing was ever set.
    fn unreadable_control() -> IrrigationControlStore {
        IrrigationControlStore::new(Arc::new(Mutex::new(Connection::open_in_memory().unwrap())))
    }

    /// The reason on the skip row a declined schedule left, if it left one.
    async fn skip_row_reason(runs: &RunsStore, id: &str) -> Option<String> {
        let rows = runs.window(0, i64::MAX).await.unwrap();
        rows.iter()
            .find(|r| r.source == format!("manual:{id}") && r.status == "skipped")
            .and_then(|r| r.skip_reason.clone())
    }

    #[tokio::test]
    async fn rain_delay_holds_the_manual_schedule() {
        // The owner taps Rain delay > 72h the evening before a trip. The 05:00
        // schedule the next morning must NOT open the valve, and History must
        // say why in the same words the morning would have used.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_pause_until(now.timestamp() + 72 * 3600)
            .await
            .unwrap();
        let schedules = vec![sched_at(now, "held", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "held").await,
            "a live rain delay held the scheduled valve shut"
        );
        let reason = skip_row_reason(&runs, "held").await;
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.starts_with("Paused (vacation until ")),
            "the skip row reads as the ladder's pause sentence; reason={reason:?}"
        );
    }

    #[tokio::test]
    async fn expired_rain_delay_does_not_hold_the_manual_schedule() {
        // The other half of the pause gate: a delay whose expiry has passed is
        // not a hold, so the schedule waters. Without this the fix could pass
        // by refusing to water at all.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_pause_until(now.timestamp() - 3600).await.unwrap();
        let schedules = vec![sched_at(now, "free", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            dispatched_run_exists(&runs, "free").await,
            "an EXPIRED rain delay is not a hold; the schedule watered"
        );
    }

    #[tokio::test]
    async fn vacation_pause_toggle_holds_the_manual_schedule() {
        // The indefinite pause (`is_paused`), the native home of
        // input_boolean.irrigation_pause.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_paused(true).await.unwrap();
        let schedules = vec![sched_at(now, "held", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "held").await,
            "the vacation pause toggle held the scheduled valve shut"
        );
        assert_eq!(
            skip_row_reason(&runs, "held").await.as_deref(),
            Some("Paused (vacation mode)"),
            "the skip row carries the ladder's own sentence, verbatim"
        );
    }

    #[tokio::test]
    async fn global_skip_override_holds_the_manual_schedule() {
        // Override > Skip is sticky until switched back to Auto, and the panel
        // says "Skipping every zone until you switch back to Auto". Every zone
        // includes the ones a manual schedule drives.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_global_override("skip".to_string()).await.unwrap();
        let schedules = vec![sched_at(now, "held", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "held").await,
            "the sticky global Skip override held the scheduled valve shut"
        );
        assert_eq!(
            skip_row_reason(&runs, "held").await.as_deref(),
            Some("Manual override: skip"),
            "the skip row carries the ladder's own sentence, verbatim"
        );
    }

    #[tokio::test]
    async fn zone_skip_override_holds_only_its_own_zone() {
        // A per-zone Skip binds that zone's schedule and leaves its siblings
        // alone, so the hold is not a blunt yard-wide stop.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_zone_override("back_yard".to_string(), "skip".to_string())
            .await
            .unwrap();
        let schedules = vec![
            sched_at(now, "held", "back_yard"),
            sched_at(now, "free", "front_yard"),
        ];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "held").await,
            "the overridden zone's schedule was held"
        );
        assert_eq!(
            skip_row_reason(&runs, "held").await.as_deref(),
            Some("Override: skip (this zone)"),
            "the skip row carries the ladder's own per-zone sentence, verbatim"
        );
        assert!(
            dispatched_run_exists(&runs, "free").await,
            "a zone with no override of its own still watered"
        );
    }

    #[tokio::test]
    async fn dry_run_control_holds_the_manual_schedule() {
        // The control-surface dry run (`is_dry_run`, the native home of
        // input_boolean.irrigation_dry_run), NOT the dry_run controller kind
        // the test registry uses: this one waters nothing at all. It stopped
        // the smart morning from commanding hardware while the manual schedule
        // still issued a real valve command.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_dry_run(true).await.unwrap();
        let schedules = vec![sched_at(now, "held", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "held").await,
            "dry run held the scheduled valve shut"
        );
        assert_eq!(
            skip_row_reason(&runs, "held").await.as_deref(),
            Some("All watering is on hold"),
            "the skip row carries the ladder's own dry-run sentence, verbatim"
        );
    }

    #[tokio::test]
    async fn force_run_override_does_not_fire_a_schedule_early() {
        // Half one of the deliberate non-goal. A per-zone force-RUN override is
        // about the MORNING's decision, not about a schedule's own clock: it
        // must never pull a schedule forward to a time the owner did not ask
        // for. The schedule here is due at 05:30 and the tick is at 05:00, so
        // this pins the grace check, not the hold ladder; half two, that a
        // force-run cannot RESCUE a due schedule from a hold, is the pair of
        // tests below.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_zone_override("back_yard".to_string(), "run".to_string())
            .await
            .unwrap();
        let mut later = sched_at(now, "notyet", "back_yard");
        later.start_minute = 30;
        let schedules = vec![later];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "notyet").await,
            "a force-run override did not pull the schedule forward"
        );
        assert!(
            runs.window(0, i64::MAX).await.unwrap().is_empty(),
            "a schedule that is not due yet leaves no row at all"
        );
    }

    #[tokio::test]
    async fn a_zone_force_run_override_does_not_rescue_a_held_schedule() {
        // Half two, and the assertion that actually encodes the divergence
        // from the skip-rule ladder: the schedule is DUE at this instant, the
        // zone carries a force-RUN override, and a 72-hour rain delay is live.
        // On the smart path the override outranks the pause. Here the pause
        // wins, because a hold is the owner saying stop everything and the cost
        // of watering through one is a week of watering while they are away.
        // This is the test that fails the day someone adds a
        // `Some("run") => return None` arm to `hold_reason`.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_zone_override("back_yard".to_string(), "run".to_string())
            .await
            .unwrap();
        ctl.set_pause_until(now.timestamp() + 72 * 3600)
            .await
            .unwrap();
        let schedules = vec![sched_at(now, "held", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "held").await,
            "a per-zone force-run did NOT lift the rain delay for a due schedule"
        );
        let reason = skip_row_reason(&runs, "held").await;
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.starts_with("Paused (vacation until ")),
            "History names the pause, not the override; reason={reason:?}"
        );
    }

    #[tokio::test]
    async fn a_global_force_run_override_does_not_rescue_a_held_schedule() {
        // The same call for the GLOBAL form, which is the one that diverges
        // most: `skip_rules::pre_soil` matches the global override FIRST and
        // returns "Manual override: force run" ahead of pause_until_epoch and
        // is_paused, so a global force-run is the highest rung of that whole
        // ladder. On this path it is not a rung at all, and the vacation pause
        // still holds the schedule shut.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = control();
        ctl.set_global_override("run".to_string()).await.unwrap();
        ctl.set_paused(true).await.unwrap();
        let schedules = vec![sched_at(now, "held", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "held").await,
            "a global force-run did NOT lift the vacation pause for a due schedule"
        );
        assert_eq!(
            skip_row_reason(&runs, "held").await.as_deref(),
            Some("Paused (vacation mode)"),
            "History names the pause, not the override"
        );
    }

    #[tokio::test]
    async fn an_unreadable_control_surface_holds_the_manual_schedule() {
        // The hold used to fail OPEN. `get_on` turned a failed SELECT into the
        // default state -- no pause, no override -- so a rain delay evaporated
        // whenever the database was busy and the valve opened, while the
        // refresher, reading the SAME surface for the SAME decision, held the
        // last state it knew. A hold that a locked database can dissolve is not
        // a hold. Unknown is now a hold, and History says which.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let ctl = unreadable_control();
        assert!(
            ctl.try_get_on(&now.date_naive().to_string()).await.is_err(),
            "the fixture is genuinely unreadable, not merely empty"
        );
        let schedules = vec![sched_at(now, "held", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&ctl),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "held").await,
            "an unreadable control surface held the scheduled valve shut"
        );
        assert_eq!(
            skip_row_reason(&runs, "held").await.as_deref(),
            Some(CONTROL_UNREADABLE_REASON),
            "the skip row says the settings could not be read, not that nothing was set"
        );
    }

    #[tokio::test]
    async fn a_missing_control_store_is_not_a_failed_read() {
        // The other side of the fail-closed change, so it cannot pass by
        // refusing to water at all: with NO persistence DB there is no surface
        // for the owner to set a hold on, so there is nothing to honor and
        // nothing to be uncertain about, and the schedule still waters.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let schedules = vec![sched_at(now, "free", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            None,
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            dispatched_run_exists(&runs, "free").await,
            "a missing control store reads as 'nothing set', not as an unreadable hold"
        );
    }

    /// A ban on ONE local hour: the hour after the frozen instant's own. The
    /// tick instant is legal, the hour the run walks into is not, and nothing
    /// else is restricted.
    ///
    /// Derived from the DEPLOYMENT calendar rather than hardcoded, because the
    /// unit-test process has no configured timezone and falls back to the
    /// machine's: the CI matrix runs America/New_York, Australia/Sydney,
    /// Asia/Kolkata and Pacific/Chatham, whose local hours at the frozen
    /// instant are 01:00, 15:00, 10:30 and 17:45.
    fn ban_one_local_hour(start_h: u8) -> WateringPolicy {
        use crate::config::schema::{EffectiveWindow, WateringRestriction};
        let mut p = WateringPolicy::default();
        p.restrictions = vec![WateringRestriction {
            id: "one_hour".into(),
            name: "No watering in that hour".into(),
            enabled: true,
            effective: EffectiveWindow::AllYear,
            forbidden_hour_start: Some(start_h),
            forbidden_hour_end: Some(start_h + 1),
            ..Default::default()
        }];
        p
    }

    #[tokio::test]
    async fn a_run_is_trimmed_so_it_cannot_water_into_a_forbidden_hour() {
        // The restriction gate judged the tick INSTANT, and a run is a SPAN: a
        // 60-minute schedule starting at 09:30 passed a 10:00 ban at 09:30 and
        // then watered until 10:30, every week, on the one path nobody is awake
        // to watch. The day's window chooser has always required both ends of
        // its span to be permitted; a schedule's span is now held to the same
        // rule and closes on the ban's first second instead.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let cal = crate::timeutil::deployment_calendar();
        let at = cal
            .at(now.timestamp())
            .expect("the frozen instant resolves in the deployment calendar");
        let policy = ban_one_local_hour(((at.hour() + 1) % 24) as u8);
        // Every real UTC offset is a whole number of minutes and the frozen
        // instant is on the minute, so the local second is zero: the run may
        // keep exactly the seconds left in the local hour it starts in.
        let permitted_s = 3600 - at.minute() * 60;

        let mut ninety = sched_at(now, "spans", "back_yard");
        ninety.duration_minutes = 90;
        let schedules = vec![ninety];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &policy,
            &registry,
            Some(&runs),
            Some(&active),
            None,
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        let rows = runs.window(0, i64::MAX).await.unwrap();
        let row = rows
            .iter()
            .find(|r| r.status != "skipped")
            .unwrap_or_else(|| panic!("the schedule still watered, only shorter; rows={rows:?}"));
        assert_eq!(
            row.duration_s,
            Some(permitted_s),
            "the run closed on the ban's first second, not ninety minutes later; rows={rows:?}"
        );
    }

    #[tokio::test]
    async fn a_run_clear_of_every_forbidden_hour_is_not_trimmed() {
        // The other half, so the trim cannot pass by shortening everything: the
        // same 90-minute schedule under a ban on the hour BEFORE it keeps every
        // second the owner asked for.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let cal = crate::timeutil::deployment_calendar();
        let at = cal
            .at(now.timestamp())
            .expect("the frozen instant resolves in the deployment calendar");
        let policy = ban_one_local_hour(((at.hour() + 23) % 24) as u8);

        let mut ninety = sched_at(now, "clear", "back_yard");
        ninety.duration_minutes = 90;
        let schedules = vec![ninety];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &policy,
            &registry,
            Some(&runs),
            Some(&active),
            None,
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        let rows = runs.window(0, i64::MAX).await.unwrap();
        let row = rows
            .iter()
            .find(|r| r.status != "skipped")
            .unwrap_or_else(|| panic!("the schedule watered; rows={rows:?}"));
        assert_eq!(
            row.duration_s,
            Some(90 * 60),
            "a span clear of the ban keeps its full duration; rows={rows:?}"
        );
    }

    /// A two-days-a-week district: at most two days with a run, Sunday to
    /// Saturday, and nothing else restricted (no weekday rows, no parity, no
    /// forbidden hours), so the ONLY thing that can skip is the allowance.
    fn two_days_a_week_policy() -> WateringPolicy {
        use crate::config::schema::{EffectiveWindow, WateringRestriction};
        let mut p = WateringPolicy::default();
        p.restrictions = vec![WateringRestriction {
            id: "two_days".into(),
            name: "Two days a week".into(),
            enabled: true,
            effective: EffectiveWindow::AllYear,
            max_days_per_week: Some(2),
            ..Default::default()
        }];
        p
    }

    /// A completed watering row `days_back` days before `now`: the evidence
    /// that a day already spent one of the week's allowance.
    async fn watered_on(runs: &RunsStore, now_epoch: i64, days_back: i64, id: &str) {
        let start = now_epoch - days_back * 86_400;
        runs.insert_completed(
            NewRun {
                session_id: None,
                zone_slug: "back_yard".into(),
                start_epoch: start,
                source: format!("manual:{id}"),
                controller_id: "dry".into(),
                planned_duration_s: 600,
                skip_reason: None,
                et0_mm: None,
                etc_mm: None,
                cycle_index: None,
                cycle_count: None,
            },
            start + 600,
            600,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn days_per_week_allowance_binds_the_manual_schedule() {
        // The manual path called `restrictions::evaluate`, the reduced
        // entrypoint that hardcodes an EMPTY watered-days slice, so
        // `max_days_per_week` always compared against zero and could never
        // fire: a two-days-a-week district with a manual schedule watered on
        // day three while the engine path, which passes the real history,
        // skipped it. Two earlier days of this Sunday-to-Saturday week already
        // watered here (the frozen instant is a Thursday, so one and two days
        // back are both inside the same week whatever the deployment offset).
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        watered_on(&runs, now.timestamp(), 2, "prior_a").await;
        watered_on(&runs, now.timestamp(), 1, "prior_b").await;

        let schedules = vec![sched_at(now, "third_day", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &two_days_a_week_policy(),
            &registry,
            Some(&runs),
            Some(&active),
            None,
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "third_day").await,
            "the week's allowance was spent, so the third day did not water"
        );
        let reason = skip_row_reason(&runs, "third_day").await;
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("allowance of watering days is used up")),
            "History says the allowance stopped it; reason={reason:?}"
        );
    }

    #[tokio::test]
    async fn days_per_week_allowance_still_permits_the_second_day() {
        // The other half: one day spent against a cap of two leaves today's
        // schedule free to water. Without this the allowance fix could pass by
        // refusing every manual run.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        watered_on(&runs, now.timestamp(), 1, "prior_a").await;

        let schedules = vec![sched_at(now, "second_day", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &two_days_a_week_policy(),
            &registry,
            Some(&runs),
            Some(&active),
            None,
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            dispatched_run_exists(&runs, "second_day").await,
            "one day spent against a cap of two still permits today"
        );
    }

    /// Minimal cloud-style controller: NO per-zone stop, pinned
    /// started_epoch so the armed deadline is exactly assertable.
    struct PinnedCloud;

    #[async_trait::async_trait]
    impl crate::ports::irrigation_controller::IrrigationController for PinnedCloud {
        fn id(&self) -> &str {
            "cloud_main"
        }
        fn supports(&self) -> crate::ports::irrigation_controller::ControllerCaps {
            crate::ports::irrigation_controller::ControllerCaps {
                flow_meter: false,
                rain_sensor: false,
                master_valve: true,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
                water_level: false,
                per_zone_stop: false,
                duration_quantum_s: 1,
            }
        }
        async fn run_zone(
            &self,
            slug: &str,
            duration_s: u32,
        ) -> crate::ports::irrigation_controller::ControllerResult<
            crate::ports::irrigation_controller::RunHandle,
        > {
            Ok(crate::ports::irrigation_controller::RunHandle {
                controller_id: "cloud_main".into(),
                zone_slug: slug.to_string(),
                started_epoch: 1000,
                planned_duration_s: duration_s,
                provider_ref: None,
            })
        }
        async fn stop_zone(
            &self,
            _slug: &str,
        ) -> crate::ports::irrigation_controller::ControllerResult<()> {
            Ok(())
        }
        async fn stop_all(&self) -> crate::ports::irrigation_controller::ControllerResult<()> {
            Ok(())
        }
        async fn status(
            &self,
        ) -> crate::ports::irrigation_controller::ControllerResult<
            crate::ports::irrigation_controller::ControllerStatus,
        > {
            Ok(crate::ports::irrigation_controller::ControllerStatus {
                observed_epoch: None,
                reachable: true,
                master_enabled: Some(true),
                water_level_pct: None,
                rain_sensor_tripped: None,
                current_program: None,
                zone_states: vec![],
                flow_gpm: None,
                flow_connected: false,
                firmware: None,
            })
        }
        async fn run_history(
            &self,
            _since_epoch: i64,
        ) -> crate::ports::irrigation_controller::ControllerResult<
            Vec<crate::ports::irrigation_controller::RunRecord>,
        > {
            Ok(vec![])
        }
    }

    // The manual scheduler's arm site carries the shared enforcement grace:
    // a device-wide-stop cloud gets 90s past the planned end, so the reaper
    // never device-stops a sibling the instant a scheduled run's planned
    // end passes. (sched_at pins a 10-minute schedule and PinnedCloud pins
    // started_epoch=1000: deadline = 1000 + 600 + 90.)
    #[tokio::test]
    async fn manual_scheduler_arms_deadline_with_device_wide_grace() {
        let (runs, active) = stores();
        let registry = ControllerRegistry::new();
        let ctl: Arc<dyn crate::ports::irrigation_controller::IrrigationController> =
            Arc::new(PinnedCloud);
        registry.set(vec![(ctl, true)]);
        let now = frozen_now();
        let schedules = vec![sched_at(now, "cloudrun", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            None,
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        let due = active.due(i64::MAX / 2).await.unwrap();
        assert_eq!(due.len(), 1, "the dispatched run armed its backstop");
        assert_eq!(
            due[0].off_deadline_epoch,
            1000 + 600 + 90,
            "deadline = start + duration + device-wide-stop grace"
        );
    }

    // -- The weather safety gates --------------------------------------------
    //
    // This dispatcher had NO weather rung. Freeze, overnight freeze, wind now,
    // wind forecast, rain falling now and the live-data fail-safe were computed
    // once per tick by the engine, published on the snapshot and consumed only
    // by the smart morning, so an enabled manual schedule opened valves in a
    // hard freeze. `run_tick` did not even take a snapshot to consult.
    //
    // Each test below runs ONE tick of a schedule pinned to the frozen instant
    // (so it is unambiguously due) against a published yard verdict, and asserts
    // both halves: whether the valve was commanded, and what History was told.

    /// The WAIVER marker a schedule left, if it left one.
    async fn waiver_row_reason(runs: &RunsStore, id: &str) -> Option<String> {
        let rows = runs.window(0, i64::MAX).await.unwrap();
        rows.iter()
            .find(|r| {
                r.source == format!("manual:{id}")
                    && r.status == "skipped"
                    && r.skip_reason
                        .as_deref()
                        .is_some_and(|s| s.starts_with("Weather safety waived"))
            })
            .and_then(|r| r.skip_reason.clone())
    }

    #[tokio::test]
    async fn freeze_verdict_holds_the_manual_schedule() {
        // THE RED. A hard freeze is published on the snapshot the refresher
        // owns; the smart morning stops on it. A 05:00 manual schedule must
        // stop on it too, and History must carry the ENGINE's own sentence
        // rather than a second wording invented here.
        //
        // Before the weather rung existed this dispatched: `run_tick` consulted
        // the control surface and the restrictions and nothing else, so the
        // valve opened at 28F.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let snap = snapshot_skipping(now, "freeze_now", "Freeze risk now (28F < 38F)");
        let schedules = vec![sched_at(now, "dawn", "back_yard")];
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &schedules,
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&control()),
            Some(&snap),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "dawn").await,
            "a published freeze verdict held the scheduled valve shut"
        );
        assert_eq!(
            skip_row_reason(&runs, "dawn").await.as_deref(),
            Some("Freeze risk now (28F < 38F)"),
            "History carries the engine's own reason string, verbatim"
        );
        assert!(
            last_fired.keys().any(|(id, _, _)| id == "dawn"),
            "a KNOWN gate marks the day: the verdict will not change inside the \
             three-tick catch-up grace, so re-asking would only rewrite the row"
        );
    }

    #[tokio::test]
    async fn every_safety_gate_holds_the_manual_schedule() {
        // The whole safety set, each fired on its own. One of these regressing
        // is a valve opening into that hazard on the unattended path.
        for (code, reason) in [
            ("freeze_now", "Freeze risk now (28F < 38F)"),
            ("overnight_freeze", "Overnight freeze forecast (30F < 38F)"),
            ("wind_now", "Wind too high now (24.0 mph > 15 mph)"),
            ("wind_forecast", "Windy day forecast (26 mph peak)"),
            ("rain_now", "Currently raining (0.30 in/hr)"),
            (
                "live_data",
                "Live weather unavailable (no station data or forecast); failing safe",
            ),
        ] {
            let (runs, active) = stores();
            let registry = dry_registry();
            let now = frozen_now();
            let snap = snapshot_skipping(now, code, reason);
            let schedules = vec![sched_at(now, "s", "back_yard")];
            let mut last_fired = HashMap::new();

            run_tick(
                now,
                &schedules,
                &WateringPolicy::default(),
                &registry,
                Some(&runs),
                Some(&active),
                Some(&control()),
                Some(&snap),
                None,
                &mut last_fired,
            )
            .await;

            assert!(
                !dispatched_run_exists(&runs, "s").await,
                "the {code} safety gate held the scheduled valve shut"
            );
            assert_eq!(
                skip_row_reason(&runs, "s").await.as_deref(),
                Some(reason),
                "the {code} skip row carries the engine's own sentence"
            );
        }
    }

    #[tokio::test]
    async fn rain_forecast_and_soil_gates_do_not_hold_the_manual_schedule() {
        // The deliberate NON-gates, and the half of the design that keeps a
        // schedule worth setting. A manual schedule exists because the owner
        // wants water at a time of their choosing; holding it because the soil
        // model is satisfied, or because rain is forecast for tomorrow, would
        // quietly convert it into a second smart engine. Only safety binds.
        for code in [
            "rain_next_4h",
            "tomorrow_rain",
            "rain_3day",
            "already_wet",
            "rain_today_forecast",
            "observed_rain",
            "soil_saturation",
            "soil_model",
        ] {
            let (runs, active) = stores();
            let registry = dry_registry();
            let now = frozen_now();
            let snap = snapshot_skipping(now, code, "the yard has water coming");
            let schedules = vec![sched_at(now, "s", "back_yard")];
            let mut last_fired = HashMap::new();

            run_tick(
                now,
                &schedules,
                &WateringPolicy::default(),
                &registry,
                Some(&runs),
                Some(&active),
                Some(&control()),
                Some(&snap),
                None,
                &mut last_fired,
            )
            .await;

            assert!(
                dispatched_run_exists(&runs, "s").await,
                "a {code} skip is not a safety gate: the schedule still watered"
            );
        }
    }

    #[tokio::test]
    async fn no_verdict_holds_the_schedule_but_leaves_the_day_open() {
        // An unknown verdict is not permission. A snapshot the refresher has
        // never populated (last_refresh_epoch 0), one older than the freshness
        // window, and no snapshot at all must each hold -- there is no value of
        // the parameter that means "dispatch anyway".
        //
        // And unlike a known gate, none of them marks the day: the refresher
        // usually recovers within seconds of boot, so a schedule that ticked one
        // minute too early still waters inside the catch-up grace.
        let stale = {
            let mut s = fresh_snapshot(frozen_now());
            s.last_refresh_epoch = frozen_now().timestamp() - (MAX_SNAPSHOT_AGE_S + 1);
            s
        };
        let never = IrrigationSnapshot::default();
        let cases: [(&str, Option<&IrrigationSnapshot>); 3] = [
            ("none", None),
            ("stale", Some(&stale)),
            ("never_refreshed", Some(&never)),
        ];
        for (label, snap) in cases {
            let (runs, active) = stores();
            let registry = dry_registry();
            let now = frozen_now();
            let schedules = vec![sched_at(now, "s", "back_yard")];
            let mut last_fired = HashMap::new();

            run_tick(
                now,
                &schedules,
                &WateringPolicy::default(),
                &registry,
                Some(&runs),
                Some(&active),
                Some(&control()),
                snap,
                None,
                &mut last_fired,
            )
            .await;

            assert!(
                !dispatched_run_exists(&runs, "s").await,
                "{label}: no verdict to judge on is not permission to water"
            );
            assert!(
                skip_row_reason(&runs, "s")
                    .await
                    .is_some_and(|r| r.starts_with("Held: no recent weather verdict")),
                "{label}: History says the honest thing, not a gate that never fired"
            );
            assert!(
                last_fired.is_empty(),
                "{label}: the day stays open so a recovering refresher can still \
                 water inside the catch-up grace"
            );
        }
    }

    #[tokio::test]
    async fn engine_probe_hold_blocks_only_its_schedule_even_with_weather_waiver() {
        use crate::config::schema::SkipRuleParams;
        use crate::engine::scripting::CompiledScripts;
        use crate::engine::skip_rules::{evaluate_decisions, Inputs, ZoneSoil};
        for (reading, freeze) in [
            (None, false),
            (Some(95.0), false),
            (None, true),
            (Some(95.0), true),
        ] {
            let (runs, active) = stores();
            let registry = dry_registry();
            let now = frozen_now();
            let mut i = Inputs {
                temp_now_f: if freeze { 25.0 } else { 70.0 },
                temp_min_24h_f: Some(60.0),
                min_temp_f: 38.0,
                max_wind_mph: 15.0,
                rain_skip_in: 0.25,
                ..Default::default()
            };
            i.soil_zones = [
                ("back_yard", reading),
                ("front_yard", Some(28.0)),
                ("side_yard", Some(30.0)),
            ]
            .into_iter()
            .map(|(slug, pct)| ZoneSoil {
                slug: slug.into(),
                name: slug.into(),
                pct,
                probe_configured: true,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                ..Default::default()
            })
            .collect();
            let answer = evaluate_decisions(
                &i,
                &SkipRuleParams::default(),
                &[],
                &CompiledScripts::compile(&[]),
            );
            let mut snap = fresh_snapshot(now);
            snap.skip_check = answer.skip_check;
            snap.zone_verdicts = answer.zones;
            assert_eq!(snap.skip_check.soil_probe_holds.len(), 1);
            if freeze {
                assert_eq!(snap.skip_check.reason_code, "freeze_now");
            }
            let mut held = sched_at(now, "held", "back_yard");
            held.ignore_weather_safety = true;
            let mut healthy = sched_at(now, "healthy", "front_yard");
            healthy.ignore_weather_safety = true;
            let mut last_fired = HashMap::new();
            run_tick(
                now,
                &[held, healthy],
                &WateringPolicy::default(),
                &registry,
                Some(&runs),
                Some(&active),
                Some(&control()),
                Some(&snap),
                None,
                &mut last_fired,
            )
            .await;
            assert!(
                !dispatched_run_exists(&runs, "held").await,
                "a weather waiver cannot waive probe integrity"
            );
            assert!(skip_row_reason(&runs, "held")
                .await
                .unwrap()
                .contains("probe"));
            assert!(
                dispatched_run_exists(&runs, "healthy").await,
                "a healthy sibling remains eligible"
            );
        }
    }

    #[tokio::test]
    async fn scripts_hold_manual_schedules_even_under_a_waived_weather_gate() {
        use crate::config::schema::{ScriptRule, SkipRuleParams};
        use crate::engine::scripting::CompiledScripts;
        use crate::engine::skip_rules::{evaluate_decisions, Inputs, ZoneSoil};
        for (script, holds) in [
            ("true", true),
            ("bad syntax (", true),
            ("missing_function()", true),
            ("42", true),
            ("false", false),
            ("\"\"", false),
        ] {
            for freeze in [false, true] {
                let (runs, active) = stores();
                let registry = dry_registry();
                let now = frozen_now();
                let i = Inputs {
                    temp_now_f: if freeze { 20.0 } else { 70.0 },
                    min_temp_f: 38.0,
                    temp_min_24h_f: Some(60.0),
                    max_wind_mph: 15.0,
                    rain_skip_in: 0.25,
                    soil_zones: vec![ZoneSoil {
                        slug: "lawn".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                let scripts = CompiledScripts::compile(&[ScriptRule {
                    id: "owner_rule".into(),
                    name: "Owner watering hold".into(),
                    enabled: true,
                    script: script.into(),
                }]);
                let answer = evaluate_decisions(&i, &SkipRuleParams::default(), &[], &scripts);
                let mut snap = fresh_snapshot(now);
                snap.skip_check = answer.skip_check;
                snap.zone_verdicts = answer.zones;
                let expected_reason = snap
                    .skip_check
                    .script_hold
                    .as_ref()
                    .map(|h| h.reason.clone());
                if freeze {
                    assert_eq!(snap.skip_check.reason_code, "freeze_now");
                }
                let mut schedule = sched_at(now, "script_test", "lawn");
                schedule.ignore_weather_safety = true;
                let mut policy = WateringPolicy::default();
                policy.script_rules_enabled = true;
                let mut last_fired = HashMap::new();
                run_tick(
                    now,
                    &[schedule],
                    &policy,
                    &registry,
                    Some(&runs),
                    Some(&active),
                    Some(&control()),
                    Some(&snap),
                    None,
                    &mut last_fired,
                )
                .await;
                assert_eq!(
                    dispatched_run_exists(&runs, "script_test").await,
                    !holds,
                    "script={script:?}, freeze={freeze}"
                );
                if holds {
                    assert_eq!(skip_row_reason(&runs, "script_test").await, expected_reason);
                    assert!(waiver_row_reason(&runs, "script_test").await.is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn enabled_scripts_require_a_fresh_verdict_even_with_weather_waiver() {
        for enabled in [false, true] {
            for stale in [false, true] {
                let (runs, active) = stores();
                let registry = dry_registry();
                let now = frozen_now();
                let mut policy = WateringPolicy::default();
                policy.script_rules_enabled = enabled;
                let mut schedule = sched_at(now, "script_test", "lawn");
                schedule.ignore_weather_safety = true;
                let mut snap = fresh_snapshot(now);
                snap.last_refresh_epoch -= MAX_SNAPSHOT_AGE_S + 1;
                let snapshot = stale.then_some(&snap);
                let mut last_fired = HashMap::new();
                run_tick(
                    now,
                    &[schedule],
                    &policy,
                    &registry,
                    Some(&runs),
                    Some(&active),
                    Some(&control()),
                    snapshot,
                    None,
                    &mut last_fired,
                )
                .await;
                assert_eq!(dispatched_run_exists(&runs, "script_test").await, !enabled);
                if enabled {
                    assert_eq!(
                        skip_row_reason(&runs, "script_test").await.as_deref(),
                        Some(NO_SCRIPT_VERDICT_REASON)
                    );
                    assert!(last_fired.is_empty());
                }
            }
        }
    }

    fn spray_and_drip_policy() -> WateringPolicy {
        use crate::config::schema::SprinklerType;
        let mut policy = WateringPolicy::default();
        policy.soil_zones = [
            ("lawn", SprinklerType::Spray),
            ("beds", SprinklerType::Drip),
        ]
        .into_iter()
        .map(
            |(slug, sprinkler_type)| crate::refresher::policy::ZoneSoilCfg {
                slug: slug.into(),
                name: slug.into(),
                soil_sensor_id: None,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                sprinkler_type,
            },
        )
        .collect();
        policy
    }

    #[tokio::test]
    async fn manual_restriction_exemption_is_scoped_and_other_rules_still_bind() {
        use crate::config::schema::SprinklerType;
        for additional_bed_rule in [false, true] {
            let (runs, active) = stores();
            let registry = dry_registry();
            let now = frozen_now();
            let mut policy = spray_and_drip_policy();
            let mut restriction = always_skip_policy().restrictions.remove(0);
            restriction.exempt_sprinklers = vec![SprinklerType::Drip];
            policy.restrictions.push(restriction);
            if additional_bed_rule {
                let mut bed_rule = always_skip_policy().restrictions.remove(0);
                bed_rule.name = "Beds hold".into();
                bed_rule.zones = vec!["beds".into()];
                policy.restrictions.push(bed_rule);
            }
            let schedules = [sched_at(now, "lawn", "lawn"), sched_at(now, "beds", "beds")];
            let mut last_fired = HashMap::new();
            run_tick(
                now,
                &schedules,
                &policy,
                &registry,
                Some(&runs),
                Some(&active),
                Some(&control()),
                Some(&fresh_snapshot(now)),
                None,
                &mut last_fired,
            )
            .await;
            assert!(!dispatched_run_exists(&runs, "lawn").await);
            assert_eq!(
                dispatched_run_exists(&runs, "beds").await,
                !additional_bed_rule
            );
            if additional_bed_rule {
                assert!(skip_row_reason(&runs, "beds")
                    .await
                    .unwrap()
                    .contains("Beds hold"));
            }
        }
    }

    #[tokio::test]
    async fn manual_restriction_caps_apply_only_to_their_zone_scope() {
        use crate::config::schema::WateringRestriction;
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let mut policy = spray_and_drip_policy();
        policy.restrictions = vec![WateringRestriction {
            id: "lawn_cap".into(),
            name: "Lawn cap".into(),
            enabled: true,
            zones: vec!["lawn".into()],
            max_minutes_per_zone: Some(2),
            ..Default::default()
        }];
        let mut last_fired = HashMap::new();
        run_tick(
            now,
            &[sched_at(now, "lawn", "lawn"), sched_at(now, "beds", "beds")],
            &policy,
            &registry,
            Some(&runs),
            Some(&active),
            Some(&control()),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;
        let rows = runs.window(0, i64::MAX).await.unwrap();
        for (slug, duration) in [("lawn", 120), ("beds", 600)] {
            let row = rows
                .iter()
                .find(|r| r.zone_slug == slug && r.status != "skipped")
                .unwrap();
            assert_eq!(row.duration_s, Some(duration), "{slug}");
        }
    }

    #[tokio::test]
    async fn manual_span_trimming_preserves_the_same_head_exemption() {
        use crate::config::schema::SprinklerType;
        let (runs, active) = stores();
        let registry = dry_registry();
        let cal = crate::timeutil::deployment_calendar();
        let start = frozen_now();
        let minute = cal.at(start.timestamp()).unwrap().minute();
        let now = start + chrono::Duration::minutes(i64::from(59 - minute));
        let next_hour = ((cal.at(now.timestamp()).unwrap().hour() + 1) % 24) as u8;
        let mut policy = spray_and_drip_policy();
        let mut restriction = ban_one_local_hour(next_hour).restrictions.remove(0);
        restriction.exempt_sprinklers = vec![SprinklerType::Drip];
        policy.restrictions.push(restriction);
        let mut last_fired = HashMap::new();
        run_tick(
            now,
            &[sched_at(now, "lawn", "lawn"), sched_at(now, "beds", "beds")],
            &policy,
            &registry,
            Some(&runs),
            Some(&active),
            Some(&control()),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;
        let rows = runs.window(0, i64::MAX).await.unwrap();
        for (slug, duration) in [("lawn", 60), ("beds", 600)] {
            let row = rows
                .iter()
                .find(|r| r.zone_slug == slug && r.status != "skipped")
                .unwrap();
            assert_eq!(row.duration_s, Some(duration), "{slug}");
        }
    }

    #[tokio::test]
    async fn exempt_manual_schedule_obeys_its_engine_weather_hold() {
        use crate::config::schema::{SkipRuleParams, SprinklerType};
        use crate::engine::scripting::CompiledScripts;
        use crate::engine::skip_rules::{evaluate_decisions, Inputs, ZoneSoil};
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let mut policy = spray_and_drip_policy();
        let mut restriction = always_skip_policy().restrictions.remove(0);
        restriction.exempt_sprinklers = vec![SprinklerType::Drip];
        policy.restrictions.push(restriction);
        let i = Inputs {
            when: crate::engine::clock::DecisionTime::at(
                crate::timeutil::deployment_calendar(),
                now.timestamp(),
            ),
            watering_restrictions: policy.restrictions.clone(),
            temp_now_f: 70.0,
            temp_min_24h_f: Some(60.0),
            min_temp_f: 38.0,
            wind_now_mph: 25.0,
            max_wind_mph: 15.0,
            rain_skip_in: 0.25,
            soil_zones: policy
                .soil_zones
                .iter()
                .map(|z| ZoneSoil {
                    slug: z.slug.clone(),
                    name: z.name.clone(),
                    sprinkler_type: z.sprinkler_type,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let answer = evaluate_decisions(
            &i,
            &SkipRuleParams::default(),
            &[],
            &CompiledScripts::compile(&[]),
        );
        assert_eq!(
            answer.skip_check.reason_code, "restrictions",
            "the yard headline masks wind"
        );
        assert_eq!(
            answer
                .zones
                .iter()
                .find(|z| z.zone_slug == "beds")
                .unwrap()
                .reason_code,
            "wind_now"
        );
        let mut snap = fresh_snapshot(now);
        snap.skip_check = answer.skip_check;
        snap.zone_verdicts = answer.zones;
        let mut last_fired = HashMap::new();
        run_tick(
            now,
            &[sched_at(now, "beds", "beds")],
            &policy,
            &registry,
            Some(&runs),
            Some(&active),
            Some(&control()),
            Some(&snap),
            None,
            &mut last_fired,
        )
        .await;
        assert!(!dispatched_run_exists(&runs, "beds").await);
        assert!(skip_row_reason(&runs, "beds")
            .await
            .unwrap()
            .contains("Wind too high"));
    }

    // -- The waiver -----------------------------------------------------------

    #[tokio::test]
    async fn waiver_waters_through_a_freeze_and_records_the_override() {
        // The owner's explicit decision, and the trace it must leave. With
        // `ignore_weather_safety` on, the freeze does not hold the schedule --
        // and History gets a row naming the gate that was overridden, because a
        // waiver nobody can see is how someone forgets it is on.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let snap = snapshot_skipping(now, "freeze_now", "Freeze risk now (28F < 38F)");
        let mut s = sched_at(now, "dawn", "back_yard");
        s.ignore_weather_safety = true;
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &[s],
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&control()),
            Some(&snap),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            dispatched_run_exists(&runs, "dawn").await,
            "the waiver dispatched the schedule through the freeze gate"
        );
        let waiver = waiver_row_reason(&runs, "dawn").await;
        assert!(
            waiver
                .as_deref()
                .is_some_and(|r| r.contains("freeze_now") && r.contains("Freeze risk now")),
            "the waiver row NAMES the gate it overrode; row={waiver:?}"
        );

        // The marker must not eat the run row. `runs` carries
        // UNIQUE(zone_slug, start_epoch, controller_id) and every insert is
        // INSERT OR IGNORE, so a marker written at the tick instant would win
        // the key and silently swallow the dispatcher's own row -- leaving the
        // water invisible to History, to the day's ceiling, to the week's
        // allowance and to the soil replay. It is written one second earlier
        // for exactly this reason.
        let rows = runs.window(0, i64::MAX).await.unwrap();
        let marker = rows
            .iter()
            .find(|r| r.status == "skipped")
            .expect("the waiver marker row exists");
        let ran = rows
            .iter()
            .find(|r| r.status != "skipped")
            .expect("the RUN row survived alongside the waiver marker");
        assert_eq!(
            marker.start_epoch,
            now.timestamp() - 1,
            "the marker sits one second clear of the tick instant, so it cannot \
             take the run row's unique key; marker={marker:?}"
        );
        assert!(
            marker.start_epoch < ran.start_epoch,
            "and it lands immediately before the run it authorized; ran={ran:?}"
        );
    }

    #[tokio::test]
    async fn waiver_writes_no_row_when_no_gate_fired() {
        // The waiver is loud, not noisy. A waived schedule on a clear morning
        // overrode nothing, so it leaves no override row -- otherwise every
        // ordinary run buries the one that matters.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let mut s = sched_at(now, "clear", "back_yard");
        s.ignore_weather_safety = true;
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &[s],
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&control()),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(dispatched_run_exists(&runs, "clear").await, "it watered");
        assert_eq!(
            waiver_row_reason(&runs, "clear").await,
            None,
            "nothing was overridden, so nothing claims to have been"
        );
    }

    #[tokio::test]
    async fn waiver_never_beats_an_operator_hold() {
        // Requirement 2, the one that keeps the flag from being a foot-gun. A
        // hold is the owner saying stop RIGHT NOW; the waiver is something they
        // set weeks ago. Stop wins, every time, and the waiver must not even
        // leave an override row: it overrode nothing.
        let now = frozen_now();
        for label in ["rain_delay", "vacation_pause", "global_skip", "hold_all"] {
            let (runs, active) = stores();
            let registry = dry_registry();
            let ctl = control();
            match label {
                "rain_delay" => ctl
                    .set_pause_until(now.timestamp() + 72 * 3600)
                    .await
                    .unwrap(),
                "vacation_pause" => ctl.set_paused(true).await.unwrap(),
                "global_skip" => ctl.set_global_override("skip".to_string()).await.unwrap(),
                _ => ctl.set_dry_run(true).await.unwrap(),
            }
            // Both hazards at once: a hold AND a freeze the waiver would waive.
            let snap = snapshot_skipping(now, "freeze_now", "Freeze risk now (28F < 38F)");
            let mut s = sched_at(now, "held", "back_yard");
            s.ignore_weather_safety = true;
            let mut last_fired = HashMap::new();

            run_tick(
                now,
                &[s],
                &WateringPolicy::default(),
                &registry,
                Some(&runs),
                Some(&active),
                Some(&ctl),
                Some(&snap),
                None,
                &mut last_fired,
            )
            .await;

            assert!(
                !dispatched_run_exists(&runs, "held").await,
                "{label}: the weather waiver does not beat an operator hold"
            );
            assert_eq!(
                waiver_row_reason(&runs, "held").await,
                None,
                "{label}: the hold stopped it before the weather rung, so the \
                 waiver overrode nothing and claims nothing"
            );
        }
    }

    #[tokio::test]
    async fn waiver_never_beats_a_watering_restriction() {
        // The waiver is about WEATHER, which the owner may choose to accept the
        // risk of. A watering restriction is law, enforced a rung above, and no
        // per-schedule flag reaches it.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let mut s = sched_at(now, "banned", "back_yard");
        s.ignore_weather_safety = true;
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &[s],
            &always_skip_policy(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&control()),
            Some(&fresh_snapshot(now)),
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            !dispatched_run_exists(&runs, "banned").await,
            "the weather waiver does not buy a schedule past the ordinance"
        );
    }

    #[tokio::test]
    async fn configured_probe_requires_a_fresh_verdict_even_with_weather_waiver() {
        for binding in [None, Some("source:soil:soilmoisture1")] {
            for state in ["absent", "stale", "never_refreshed"] {
                let (runs, active) = stores();
                let registry = dry_registry();
                let now = frozen_now();
                let mut policy = spray_and_drip_policy();
                policy
                    .soil_zones
                    .iter_mut()
                    .find(|z| z.slug == "lawn")
                    .unwrap()
                    .soil_sensor_id = binding.map(str::to_string);
                let mut schedule = sched_at(now, "waived", "lawn");
                schedule.ignore_weather_safety = true;
                let mut snap = fresh_snapshot(now);
                snap.last_refresh_epoch = match state {
                    "never_refreshed" => 0,
                    _ => now.timestamp() - (MAX_SNAPSHOT_AGE_S + 1),
                };
                let snapshot = (state != "absent").then_some(&snap);
                let mut last_fired = HashMap::new();
                run_tick(
                    now,
                    &[schedule],
                    &policy,
                    &registry,
                    Some(&runs),
                    Some(&active),
                    Some(&control()),
                    snapshot,
                    None,
                    &mut last_fired,
                )
                .await;
                assert_eq!(
                    dispatched_run_exists(&runs, "waived").await,
                    binding.is_none(),
                    "binding={binding:?}, snapshot={state}"
                );
                if binding.is_some() {
                    assert_eq!(
                        skip_row_reason(&runs, "waived").await.as_deref(),
                        Some(NO_PROBE_VERDICT_REASON)
                    );
                    assert!(
                        last_fired.is_empty(),
                        "a recovering verdict can still reach this occurrence"
                    );
                    assert!(
                        waiver_row_reason(&runs, "waived").await.is_none(),
                        "the waiver bought no watering"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn waiver_covers_a_missing_verdict_too() {
        // The waiver has to be coherent to be safe. `live_data` -- the engine
        // saying it has no weather -- is on the waivable list, and a stale or
        // absent snapshot is the same fact one layer up. Holding the waived
        // schedule on the strictly LESS informative version of the condition
        // would mean the flag lets you water in a KNOWN freeze but not in
        // unknown weather, which is backwards.
        let (runs, active) = stores();
        let registry = dry_registry();
        let now = frozen_now();
        let mut s = sched_at(now, "blind", "back_yard");
        s.ignore_weather_safety = true;
        let mut last_fired = HashMap::new();

        run_tick(
            now,
            &[s],
            &WateringPolicy::default(),
            &registry,
            Some(&runs),
            Some(&active),
            Some(&control()),
            None,
            None,
            &mut last_fired,
        )
        .await;

        assert!(
            dispatched_run_exists(&runs, "blind").await,
            "the waiver dispatches with no verdict at all"
        );
        assert!(
            waiver_row_reason(&runs, "blind").await.is_some(),
            "and it still says so in History"
        );
    }
}
