// The one path a zone run or a stop takes, whoever asked for it.
//
// Three callers used to carry their own copy of the same sequence: the
// manual API (POST /action), the manual scheduler and the smart-morning
// executor each took the per-zone lock, clamped the seconds, judged the
// day's ceiling, called run_zone, wrote the run row, armed the shutoff
// deadline and (for the schedulers) pushed a failure. The copies drifted:
// one forgot the ceiling, one clamped differently, one armed before the
// dispatch and the others after. `Dispatcher::run` is that sequence once,
// with the two real differences named in `RunRequest` (`Arm`, `record_row`)
// instead of re-derived per caller. `stop` and `stop_all` own the ledger
// and history bookkeeping a stop implies, the same way.

use std::sync::Arc;

use crate::controllers::ceiling::{self, Admission, Usage};
use crate::controllers::guard::RUN_SECONDS_MAX;
use crate::controllers::reaper::effective_run_grace;
use crate::controllers::registry::{ControllerRegistry, StopReport};
use crate::controllers::ZoneLocks;
use crate::persistence::runs::{NewRun, RunsStore};
use crate::persistence::ActiveRunsStore;
use crate::ports::irrigation_controller::{ControllerError, IrrigationController, RunHandle};
use crate::push::dispatcher::{PushDispatcher, PushEvent};

/// The stores every dispatch writes to, and the per-zone locks it takes.
/// The stores are optional: an install without the history database still
/// runs zones, it just keeps no books.
#[derive(Clone)]
pub struct Dispatcher<'a> {
    pub locks: ZoneLocks,
    pub runs: Option<&'a RunsStore>,
    pub active_runs: Option<&'a ActiveRunsStore>,
}

/// Who asked. Becomes the run row's `source`; a simulating (dry-run)
/// controller turns it into `dry_run[:id]` so pretend water never counts
/// as watering evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Manual,
    ManualSchedule(String),
    SmartMorning,
}

impl Source {
    pub fn label(&self, simulated: bool) -> String {
        match (self, simulated) {
            (Source::Manual, false) => "manual".into(),
            (Source::Manual, true) => "dry_run".into(),
            (Source::ManualSchedule(id), false) => format!("manual:{id}"),
            (Source::ManualSchedule(id), true) => format!("dry_run:{id}"),
            (Source::SmartMorning, false) => "smart_morning".into(),
            (Source::SmartMorning, true) => "dry_run".into(),
        }
    }
}

/// How the shutoff deadline (the reaper's backstop) is armed for this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    /// Once the controller confirms: start + seconds + the enforcement
    /// grace for this controller class. The manual paths.
    AfterDispatch,
    /// The caller projected a whole-cycle deadline and wants it written
    /// BEFORE the valve is commanded (a cycle legitimately opens and closes
    /// several times; a per-segment deadline would fire during every
    /// soak). `deadline: None` means the ledger already carries this
    /// deadline or a later one, so nothing is written. On a failed
    /// dispatch the deadline is disarmed only when `disarm_on_failure`:
    /// true while no segment of the zone has ever confirmed (no valve was
    /// commanded on), false once one has (the deadline is then the only
    /// thing that closes a valve whose own shutoff may be what is failing).
    BeforeDispatch {
        deadline: Option<i64>,
        disarm_on_failure: bool,
    },
    /// No deadline (tests, or a caller that arms elsewhere).
    None,
}

pub struct RunRequest<'a> {
    /// One job across all of its cycle/soak segments.
    pub session_id: String,
    pub zone: &'a str,
    /// For the failure push; the slug when nothing better is known.
    pub zone_name: &'a str,
    pub controller: &'a Arc<dyn IrrigationController>,
    /// Requested seconds; clamped to [1, RUN_SECONDS_MAX] and then to the
    /// ceiling's remainder.
    pub seconds: u32,
    pub source: Source,
    /// `Some(cap_s)`: judge the request against what already ran today on
    /// this zone and refuse (with a history row) when the day is spent.
    /// `None`: the caller's plan already sized the run against the cap.
    pub ceiling_s: Option<u32>,
    pub arm: Arm,
    /// Write the pre-completed run row. False when a run-edge observer
    /// records this controller's runs from state readback (a row here too
    /// would double-count).
    pub record_row: bool,
    /// Cycle position for the row (index, count) on a split run.
    pub cycle: Option<(u32, u32)>,
    /// Emits DispatchFailed when the controller refuses. The API path
    /// passes None: the caller sees the error in the response.
    pub push: Option<&'a PushDispatcher>,
    pub now_epoch: i64,
}

#[derive(Debug)]
pub enum RunOutcome {
    Dispatched {
        handle: RunHandle,
        /// What the controller was asked for, after clamping and the ceiling.
        seconds: u32,
        /// The shutoff deadline was written this call (false when the
        /// ledger write failed, or nothing asked for one). A caller that
        /// tracks what it armed retries on the next step when this is
        /// false.
        deadline_armed: bool,
    },
    /// The day's ceiling refused it; a `skipped` row records why.
    Refused(Usage),
    Failed {
        error: ControllerError,
        /// A deadline armed before the dispatch survived the failure (an
        /// earlier segment of the zone had confirmed, so it stays).
        deadline_armed: bool,
    },
}

/// The scope a manual stop really had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopScope {
    Zone,
    /// The controller has no per-zone stop: every zone on the device stopped.
    Device,
}

/// The day's ceiling for a zone under the live policy: the configured
/// max duration (or the default) times the ceiling factor.
pub fn ceiling_for_zone(policy: &crate::refresher::WateringPolicy, zone: &str) -> u32 {
    policy
        .zone_runtime
        .get(crate::engine::ZoneSlug::new(zone).as_str())
        .map(|r| ceiling::daily_ceiling_s(r.max_duration_s))
        .unwrap_or_else(|| {
            ceiling::daily_ceiling_s(crate::config::schema::DEFAULT_MAX_RUN_MINUTES * 60)
        })
}

/// Planned end plus the enforcement grace: the controller's own timer is
/// the precise shutoff, the deadline only backstops it. A graceless
/// deadline made the reaper fire the instant the planned end passed, which
/// on a Rachio-class controller device-stops any sibling zone started
/// meanwhile.
pub fn run_deadline(started_epoch: i64, seconds: u32, per_zone_stop: bool) -> i64 {
    started_epoch + seconds as i64 + effective_run_grace(per_zone_stop)
}

impl<'a> Dispatcher<'a> {
    pub fn new(
        locks: ZoneLocks,
        runs: Option<&'a RunsStore>,
        active_runs: Option<&'a ActiveRunsStore>,
    ) -> Self {
        Self {
            locks,
            runs,
            active_runs,
        }
    }

    /// Run a zone: lock, clamp, ceiling, arm-before, run_zone, row,
    /// arm-after, failure push. The per-zone lock is held from the ceiling
    /// judgment through the row insert: `admit` is a plain read of the
    /// runs table, so two concurrent runs that both judged the ceiling
    /// before either wrote its row would both pass it. It is never held
    /// for the run itself (the controller owns the shutoff), so a Stop is
    /// never blocked behind a running zone and a later run on the same
    /// zone is never blocked for the length of a cycle.
    pub async fn run(&self, req: RunRequest<'a>) -> RunOutcome {
        let controller = req.controller;
        let zone = req.zone;
        let command_order = self.locks.command_order();
        let _command = command_order.read().await;
        let lock = self.locks.lock_for(zone);
        let _serialize = lock.lock().await;
        if self.locks.restart_hold().is_pending() {
            return RunOutcome::Failed {
                error: ControllerError::Held(
                    crate::controllers::restart::WATERING_HOLD_REASON.into(),
                ),
                deadline_armed: false,
            };
        }
        let mut seconds = req.seconds.min(RUN_SECONDS_MAX).max(1);
        if seconds != req.seconds {
            tracing::warn!(
                zone = %zone,
                requested = req.seconds,
                clamped = seconds,
                max = RUN_SECONDS_MAX,
                "run seconds clamped"
            );
        }
        let simulated = controller.simulated();
        let source = req.source.label(simulated);

        if let Some(cap_s) = req.ceiling_s {
            match ceiling::admit(self.runs, zone, cap_s, seconds, req.now_epoch).await {
                Admission::Allow(s) => seconds = s,
                Admission::Unavailable => {
                    tracing::warn!(zone = %zone, "daily ceiling: history unavailable; holding watering");
                    if let Some(rs) = self.runs {
                        ceiling::record_unavailable(
                            rs,
                            zone,
                            controller.id(),
                            &source,
                            seconds,
                            req.now_epoch,
                        )
                        .await;
                    }
                    if let Some(p) = req.push {
                        p.emit(PushEvent::DispatchFailed {
                            zone_name: req.zone_name.to_string(),
                            zone_slug: zone.to_string(),
                            controller_id: controller.id().to_string(),
                            error: ceiling::HISTORY_UNAVAILABLE_REASON.into(),
                        });
                    }
                    return RunOutcome::Failed {
                        error: ControllerError::Held(ceiling::HISTORY_UNAVAILABLE_REASON.into()),
                        deadline_armed: false,
                    };
                }
                Admission::Refuse(usage) => {
                    tracing::warn!(
                        zone = %zone,
                        used_s = usage.used_s,
                        cap_s = usage.cap_s,
                        source = %source,
                        "daily ceiling reached; not running"
                    );
                    if let Some(rs) = self.runs {
                        ceiling::record_refusal(
                            rs,
                            zone,
                            controller.id(),
                            &source,
                            seconds,
                            usage,
                            req.now_epoch,
                        )
                        .await;
                    }
                    return RunOutcome::Refused(usage);
                }
            }
        }

        // Persist provenance before actuation. It is not watering evidence.
        let command_id = if let Some(rs) = self.runs {
            match rs
                .commands()
                .request(NewRun {
                    session_id: Some(req.session_id.clone()),
                    zone_slug: zone.to_string(),
                    start_epoch: req.now_epoch,
                    source: source.clone(),
                    controller_id: controller.id().to_string(),
                    planned_duration_s: seconds,
                    skip_reason: None,
                    et0_mm: None,
                    etc_mm: None,
                    cycle_index: req.cycle.map(|(i, _)| i),
                    cycle_count: req.cycle.map(|(_, n)| n),
                })
                .await
            {
                Ok(id) => Some(id),
                Err(e) => {
                    return RunOutcome::Failed {
                        error: ControllerError::init(crate::diagnostics::from_error(
                            &e,
                            "irrigation journal command",
                        )),
                        deadline_armed: false,
                    }
                }
            }
        } else {
            None
        };

        let mut deadline_armed = false;
        if let Arm::BeforeDispatch {
            deadline: Some(deadline),
            ..
        } = req.arm
        {
            if let Some(ar) = self.active_runs {
                match ar
                    .arm(
                        zone.to_string(),
                        controller.id().to_string(),
                        req.now_epoch,
                        deadline,
                    )
                    .await
                {
                    Ok(()) => deadline_armed = true,
                    Err(e) => tracing::warn!(zone = %zone, error = %e, "active-run arm failed"),
                }
            }
        }

        let result = controller.run_zone(zone, seconds).await;
        if let (Some(id), Some(rs)) = (command_id, self.runs) {
            let confirmed = result
                .as_ref()
                .ok()
                .map(|h| (h.started_epoch, h.planned_duration_s.max(1)));
            if let Err(error) = rs.commands().finish(id, confirmed).await {
                tracing::warn!(zone, %error, "command outcome journal failed; attribution stays unknown");
            }
        }

        match result {
            Ok(handle) => {
                // Wait for what the controller SAID it would run, not for
                // what we asked: a cloud adapter that takes whole minutes
                // rounds 200 s up to 240 s.
                let ran_s = handle.planned_duration_s.max(1);
                if req.record_row {
                    if let Some(rs) = self.runs {
                        let row = NewRun {
                            session_id: Some(req.session_id.clone()),
                            zone_slug: zone.to_string(),
                            start_epoch: handle.started_epoch,
                            source: source.clone(),
                            controller_id: handle.controller_id.clone(),
                            planned_duration_s: ran_s,
                            skip_reason: None,
                            et0_mm: None,
                            etc_mm: None,
                            cycle_index: req.cycle.map(|(i, _)| i),
                            cycle_count: req.cycle.map(|(_, n)| n),
                        };
                        // The controller owns the shutoff timer, so end =
                        // start + duration matches what the hardware does.
                        if let Err(e) = rs
                            .insert_completed(row, handle.started_epoch + ran_s as i64, ran_s, None)
                            .await
                        {
                            tracing::warn!(zone = %zone, error = %e, "run row insert failed");
                        }
                    }
                }
                if req.arm == Arm::AfterDispatch {
                    if let Some(ar) = self.active_runs {
                        let deadline = run_deadline(
                            handle.started_epoch,
                            ran_s,
                            controller.supports().per_zone_stop,
                        );
                        match ar
                            .arm(
                                zone.to_string(),
                                handle.controller_id.clone(),
                                handle.started_epoch,
                                deadline,
                            )
                            .await
                        {
                            Ok(()) => deadline_armed = true,
                            Err(e) => {
                                tracing::warn!(zone = %zone, error = %e, "active-run arm failed")
                            }
                        }
                    }
                }
                RunOutcome::Dispatched {
                    handle,
                    seconds: ran_s,
                    deadline_armed,
                }
            }
            Err(e) => {
                tracing::warn!(
                    controller = %controller.id(),
                    zone = %zone,
                    source = %source,
                    error = %e,
                    "controller dispatch failed"
                );
                if let Arm::BeforeDispatch {
                    disarm_on_failure: true,
                    ..
                } = req.arm
                {
                    deadline_armed = false;
                    if let Some(ar) = self.active_runs {
                        if let Err(err) = ar.disarm(zone).await {
                            tracing::warn!(zone = %zone, error = %err, "active-run disarm after dispatch failure failed");
                        }
                    }
                }
                if let Some(p) = req.push {
                    p.emit(PushEvent::DispatchFailed {
                        zone_name: req.zone_name.to_string(),
                        zone_slug: zone.to_string(),
                        controller_id: controller.id().to_string(),
                        error: e.to_string(),
                    });
                }
                RunOutcome::Failed {
                    error: e,
                    deadline_armed,
                }
            }
        }
    }

    /// Stop one zone and keep the books honest about the scope. A
    /// controller with a real per-zone stop is zone-scoped: disarm that
    /// zone's deadline, truncate its open row. A device-wide stop halted
    /// every zone on the controller, so every armed row on it clears (a
    /// survivor would re-fire another device-wide stop at its stale
    /// deadline) and every open row on it truncates (a sibling's
    /// pre-written full-duration row would otherwise credit water that
    /// stopped falling). Other controllers are untouched either way.
    pub async fn stop(
        &self,
        controller: &Arc<dyn IrrigationController>,
        zone: &str,
        now_epoch: i64,
    ) -> Result<StopScope, ControllerError> {
        let command_order = self.locks.command_order();
        let _command = command_order.write().await;
        let scope = if controller.supports().per_zone_stop {
            StopScope::Zone
        } else {
            tracing::warn!(
                zone = %zone, controller = %controller.id(),
                "stop: controller has no per-zone stop; stopping ALL watering on the device"
            );
            StopScope::Device
        };
        controller.stop_zone(zone).await.inspect_err(|e| {
            tracing::warn!(
                controller = %controller.id(), zone = %zone, action = "stop", error = %e,
                "controller zone action failed"
            );
        })?;
        self.stop_bookkeeping(controller.id(), zone, scope, now_epoch)
            .await;
        Ok(scope)
    }

    pub(crate) async fn stop_bookkeeping(
        &self,
        controller_id: &str,
        zone: &str,
        scope: StopScope,
        now_epoch: i64,
    ) {
        if let Some(ar) = self.active_runs {
            match scope {
                StopScope::Device => {
                    if let Err(e) = ar.clear_for_controllers(&[controller_id]).await {
                        tracing::warn!(
                            zone = %zone, controller = %controller_id, error = %e,
                            "device-wide stop: clearing sibling deadlines failed"
                        );
                    }
                }
                StopScope::Zone => {
                    let _ = ar.disarm(zone).await;
                }
            }
        }
        if let Some(rs) = self.runs {
            let truncated = match scope {
                StopScope::Device => {
                    rs.truncate_active_for_controller(controller_id, now_epoch)
                        .await
                }
                StopScope::Zone => rs.truncate_active(zone, now_epoch).await,
            };
            if let Err(e) = truncated {
                tracing::debug!(zone = %zone, error = %e, "run-row truncate failed");
            }
        }
    }

    /// Stop every registered controller. The deadline ledger is cleared
    /// only for the controllers that confirmed, so an unreachable one keeps
    /// its backstop and the reaper keeps retrying; open rows are truncated
    /// for the confirmed ones.
    pub async fn stop_all(&self, registry: &ControllerRegistry, now_epoch: i64) -> StopReport {
        let command_order = self.locks.command_order();
        let _command = command_order.write().await;
        let report = registry.stop_everything().await;
        if let Some(ar) = self.active_runs {
            if !report.confirmed.is_empty() {
                if let Err(e) = ar.clear_for_controllers(&report.confirmed_ids()).await {
                    tracing::warn!(error = %e, "stop all: clearing confirmed controllers' deadlines failed");
                }
            }
        }
        for (id, e) in &report.failed {
            tracing::warn!(
                controller = %id, error = %e,
                "stop all: controller did not confirm; its shutoff deadlines stay armed for reaper retry"
            );
        }
        if let Some(rs) = self.runs {
            for id in &report.confirmed {
                if let Err(e) = rs.truncate_active_for_controller(id, now_epoch).await {
                    tracing::debug!(controller = %id, error = %e, "run-row truncate failed");
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::irrigation_controller::{
        ControllerCaps, ControllerResult, ControllerStatus, RunRecord,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A controller that records what it was asked and can be told to
    /// refuse, report per-zone stop or not, and simulate.
    struct Probe {
        id: String,
        runs: Mutex<Vec<(String, u32)>>,
        stops: Mutex<Vec<String>>,
        stop_alls: AtomicUsize,
        fail: bool,
        per_zone_stop: bool,
        simulated: bool,
        /// Whole-minute rounding, the way a cloud adapter does it.
        round_to_minutes: bool,
        run_entered: Option<Arc<tokio::sync::Notify>>,
        run_release: Option<Arc<tokio::sync::Notify>>,
    }

    impl Probe {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                runs: Mutex::new(Vec::new()),
                stops: Mutex::new(Vec::new()),
                stop_alls: AtomicUsize::new(0),
                fail: false,
                per_zone_stop: true,
                simulated: false,
                round_to_minutes: false,
                run_entered: None,
                run_release: None,
            }
        }
    }

    #[async_trait]
    impl IrrigationController for Probe {
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
                per_zone_stop: self.per_zone_stop,
                duration_quantum_s: 1,
            }
        }
        fn simulated(&self) -> bool {
            self.simulated
        }
        async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
            if self.fail {
                return Err(ControllerError::Offline);
            }
            if let Some(entered) = &self.run_entered {
                entered.notify_one();
            }
            if let Some(release) = &self.run_release {
                release.notified().await;
            }
            self.runs.lock().unwrap().push((slug.into(), duration_s));
            let planned = if self.round_to_minutes {
                duration_s.div_ceil(60) * 60
            } else {
                duration_s
            };
            Ok(RunHandle {
                controller_id: self.id.clone(),
                zone_slug: slug.into(),
                started_epoch: 1_000,
                planned_duration_s: planned,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
            if self.fail {
                return Err(ControllerError::Offline);
            }
            self.stops.lock().unwrap().push(slug.into());
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            if self.fail {
                return Err(ControllerError::Offline);
            }
            self.stop_alls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            Err(ControllerError::Unsupported("status".into()))
        }
        async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(vec![])
        }
    }

    async fn stores() -> (RunsStore, ActiveRunsStore) {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut c).unwrap();
        let conn = Arc::new(tokio::sync::Mutex::new(c));
        (RunsStore::new(conn.clone()), ActiveRunsStore::new(conn))
    }

    async fn rows(runs: &RunsStore) -> Vec<crate::persistence::runs::RunRow> {
        runs.window(0, i64::MAX / 2).await.unwrap()
    }

    async fn armed(ar: &ActiveRunsStore) -> Vec<crate::persistence::active_runs::ActiveRun> {
        ar.armed().await.unwrap()
    }

    fn arc(p: Probe) -> Arc<dyn IrrigationController> {
        Arc::new(p)
    }

    fn req<'a>(
        zone: &'a str,
        controller: &'a Arc<dyn IrrigationController>,
        seconds: u32,
        arm: Arm,
    ) -> RunRequest<'a> {
        RunRequest {
            session_id: crate::persistence::watering_commands::new_session_id(),
            zone,
            zone_name: zone,
            controller,
            seconds,
            source: Source::Manual,
            ceiling_s: None,
            arm,
            record_row: true,
            cycle: None,
            push: None,
            now_epoch: 1_000,
        }
    }

    /// The three callers go through `Dispatcher`; none of them talks to the
    /// controller, the run table or the deadline ledger on its own.
    #[test]
    fn every_caller_dispatches_through_this_module() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for f in [
            "src/api/irrigation.rs",
            "src/scheduler/manual.rs",
            "src/scheduler/smart_morning.rs",
        ] {
            let src = std::fs::read_to_string(root.join(f)).unwrap();
            let code = src.split("#[cfg(test)]").next().unwrap();
            for needle in [
                ".run_zone(",
                ".insert_completed(",
                ".arm(",
                ".stop_everything(",
                ".truncate_active",
                ".clear_for_controllers(",
                ".lock_for(",
            ] {
                assert!(
                    !code.contains(needle),
                    "{f} calls {needle} directly; route it through controllers::dispatch"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_manual_run_writes_the_row_and_arms_after_the_controller_confirms() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let c = arc(Probe::new("os"));
        let out = d.run(req("front", &c, 600, Arm::AfterDispatch)).await;
        let RunOutcome::Dispatched { seconds, .. } = out else {
            panic!("dispatched")
        };
        assert_eq!(seconds, 600);
        let rows = rows(&runs).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "manual");
        assert_eq!(rows[0].duration_s, Some(600));
        let armed = armed(&ar).await;
        assert_eq!(armed.len(), 1);
        assert_eq!(
            armed[0].off_deadline_epoch,
            1_000 + 600 + effective_run_grace(true),
            "deadline = start + seconds + the per-zone grace"
        );
    }

    #[tokio::test]
    async fn the_deadline_carries_the_device_stop_grace_on_a_cloud_controller() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let mut p = Probe::new("rachio");
        p.per_zone_stop = false;
        let c = arc(p);
        d.run(req("front", &c, 600, Arm::AfterDispatch)).await;
        let armed = armed(&ar).await;
        assert_eq!(
            armed[0].off_deadline_epoch,
            1_000 + 600 + effective_run_grace(false)
        );
        assert!(effective_run_grace(false) > effective_run_grace(true));
    }

    #[tokio::test]
    async fn arm_before_dispatch_writes_the_projected_deadline_and_keeps_it_on_success() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let c = arc(Probe::new("os"));
        let out = d
            .run(req(
                "front",
                &c,
                300,
                Arm::BeforeDispatch {
                    deadline: Some(5_000),
                    disarm_on_failure: true,
                },
            ))
            .await;
        assert!(matches!(out, RunOutcome::Dispatched { .. }));
        let armed = armed(&ar).await;
        assert_eq!(armed.len(), 1);
        assert_eq!(
            armed[0].off_deadline_epoch, 5_000,
            "the caller's projection, not start+seconds"
        );
    }

    #[tokio::test]
    async fn a_failed_first_segment_disarms_but_a_failed_later_one_keeps_the_backstop() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let mut p = Probe::new("os");
        p.fail = true;
        let c = arc(p);
        // Never confirmed: the deadline covers a run that never started.
        let out = d
            .run(req(
                "front",
                &c,
                300,
                Arm::BeforeDispatch {
                    deadline: Some(5_000),
                    disarm_on_failure: true,
                },
            ))
            .await;
        assert!(matches!(out, RunOutcome::Failed { .. }));
        assert!(
            armed(&ar).await.is_empty(),
            "disarmed: no valve was commanded on"
        );
        // An earlier segment confirmed: the backstop must stay.
        let out = d
            .run(req(
                "back",
                &c,
                300,
                Arm::BeforeDispatch {
                    deadline: Some(6_000),
                    disarm_on_failure: false,
                },
            ))
            .await;
        assert!(matches!(out, RunOutcome::Failed { .. }));
        let armed = armed(&ar).await;
        assert_eq!(armed.len(), 1);
        assert_eq!(armed[0].zone_slug, "back");
        assert!(
            rows(&runs).await.is_empty(),
            "no row for a run that never happened"
        );
    }

    /// Two concurrent runs on one zone: the second judges the ceiling only
    /// after the first has written its row, so it is trimmed or refused
    /// instead of both passing against an empty table.
    #[tokio::test]
    async fn concurrent_runs_on_one_zone_judge_the_ceiling_in_turn() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let c = arc(Probe::new("os"));
        let mk = || {
            let mut r = req("front", &c, 7_200, Arm::AfterDispatch);
            r.ceiling_s = Some(4_800);
            r
        };
        let (a, b) = tokio::join!(d.run(mk()), d.run(mk()));
        let dispatched = [&a, &b]
            .iter()
            .filter(|o| matches!(o, RunOutcome::Dispatched { .. }))
            .count();
        let refused = [&a, &b]
            .iter()
            .filter(|o| matches!(o, RunOutcome::Refused(_)))
            .count();
        assert_eq!((dispatched, refused), (1, 1), "{a:?} / {b:?}");
        let live: Vec<u32> = rows(&runs)
            .await
            .iter()
            .filter(|r| r.skip_reason.is_none())
            .filter_map(|r| r.duration_s)
            .collect();
        assert_eq!(live, vec![4_800], "one run, trimmed to the day's remainder");
    }

    /// The outcome says whether the deadline was written, so a caller
    /// that tracks its arms can retry after a failed ledger write.
    #[tokio::test]
    async fn the_outcome_reports_whether_the_deadline_was_armed() {
        let (runs, ar) = stores().await;
        let with = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let without = Dispatcher::new(ZoneLocks::default(), Some(&runs), None);
        let c = arc(Probe::new("os"));
        let RunOutcome::Dispatched { deadline_armed, .. } =
            with.run(req("a", &c, 60, Arm::AfterDispatch)).await
        else {
            panic!()
        };
        assert!(deadline_armed);
        let RunOutcome::Dispatched { deadline_armed, .. } =
            without.run(req("b", &c, 60, Arm::AfterDispatch)).await
        else {
            panic!()
        };
        assert!(!deadline_armed, "no ledger, nothing armed");
        let RunOutcome::Dispatched { deadline_armed, .. } = with
            .run(req(
                "c",
                &c,
                60,
                Arm::BeforeDispatch {
                    deadline: None,
                    disarm_on_failure: true,
                },
            ))
            .await
        else {
            panic!()
        };
        assert!(!deadline_armed, "nothing asked for a write");
    }

    #[tokio::test]
    async fn unavailable_history_never_opens_a_valve_or_arms_a_deadline() {
        let (_, ar) = stores().await;
        let broken = RunsStore::new(Arc::new(tokio::sync::Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        )));
        let probe = Arc::new(Probe::new("os"));
        let controller: Arc<dyn IrrigationController> = probe.clone();
        for runs in [None, Some(&broken)] {
            let d = Dispatcher::new(ZoneLocks::default(), runs, Some(&ar));
            let mut request = req("front", &controller, 600, Arm::AfterDispatch);
            request.ceiling_s = Some(7200);
            assert!(matches!(
                d.run(request).await,
                RunOutcome::Failed {
                    error: ControllerError::Held(_),
                    deadline_armed: false,
                }
            ));
        }
        assert!(probe.runs.lock().unwrap().is_empty());
        assert!(armed(&ar).await.is_empty());
        // Emergency stop remains available even with unreadable history.
        let d = Dispatcher::new(ZoneLocks::default(), Some(&broken), Some(&ar));
        assert!(d.stop(&controller, "front", 1100).await.is_ok());
        assert_eq!(probe.stops.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_ceiling_refuses_with_a_row_and_never_touches_the_controller() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let c = arc(Probe::new("os"));
        // 20 minutes already ran today against a 20 minute ceiling.
        d.run(req("front", &c, 1_200, Arm::AfterDispatch)).await;
        let mut r = req("front", &c, 600, Arm::AfterDispatch);
        r.ceiling_s = Some(1_200);
        r.now_epoch = 2_000;
        let out = d.run(r).await;
        let RunOutcome::Refused(usage) = out else {
            panic!("refused")
        };
        assert_eq!(usage.cap_s, 1_200);
        let rows = rows(&runs).await;
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().any(|r| r.skip_reason.is_some()),
            "the refusal leaves a skipped row"
        );
    }

    #[tokio::test]
    async fn a_dry_run_controller_labels_its_rows_dry_run_with_the_schedule_id() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let mut p = Probe::new("dry");
        p.simulated = true;
        let c = arc(p);
        let mut r = req("front", &c, 60, Arm::None);
        r.source = Source::ManualSchedule("evening".into());
        d.run(r).await;
        assert_eq!(rows(&runs).await[0].source, "dry_run:evening");
        assert_eq!(Source::SmartMorning.label(true), "dry_run");
        assert_eq!(Source::Manual.label(false), "manual");
    }

    #[tokio::test]
    async fn the_row_and_the_wait_follow_what_the_controller_said_it_would_run() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let mut p = Probe::new("cloud");
        p.round_to_minutes = true;
        let c = arc(p);
        let out = d.run(req("front", &c, 200, Arm::AfterDispatch)).await;
        let RunOutcome::Dispatched { seconds, .. } = out else {
            panic!()
        };
        assert_eq!(seconds, 240);
        assert_eq!(rows(&runs).await[0].duration_s, Some(240));
    }

    #[tokio::test]
    async fn seconds_are_clamped_to_the_hard_maximum_and_a_floor_of_one() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let c = arc(Probe::new("os"));
        d.run(req("a", &c, 0, Arm::None)).await;
        d.run(req("b", &c, u32::MAX, Arm::None)).await;
        let rows = rows(&runs).await;
        let mut planned: Vec<u32> = rows.iter().filter_map(|r| r.duration_s).collect();
        planned.sort();
        assert_eq!(planned, vec![1, RUN_SECONDS_MAX]);
    }

    #[tokio::test]
    async fn no_row_when_a_run_edge_observer_records_this_controller() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let c = arc(Probe::new("os"));
        let mut r = req("front", &c, 60, Arm::None);
        r.record_row = false;
        d.run(r).await;
        assert!(rows(&runs).await.is_empty());
    }

    #[tokio::test]
    async fn a_zone_stop_disarms_and_truncates_that_zone_only() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let c = arc(Probe::new("os"));
        d.run(req("front", &c, 600, Arm::AfterDispatch)).await;
        d.run(req("back", &c, 600, Arm::AfterDispatch)).await;
        let scope = d.stop(&c, "front", 1_100).await.unwrap();
        assert_eq!(scope, StopScope::Zone);
        let armed = armed(&ar).await;
        assert_eq!(armed.len(), 1);
        assert_eq!(armed[0].zone_slug, "back");
        let rows = rows(&runs).await;
        let front = rows.iter().find(|r| r.zone_slug == "front").unwrap();
        let back = rows.iter().find(|r| r.zone_slug == "back").unwrap();
        assert_eq!(front.end_epoch, Some(1_100), "truncated to the real span");
        assert_eq!(
            back.end_epoch,
            Some(1_600),
            "the sibling keeps its planned end"
        );
    }

    #[tokio::test]
    async fn a_device_wide_stop_clears_every_row_on_that_controller_and_no_other() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let mut p = Probe::new("rachio");
        p.per_zone_stop = false;
        let cloud = arc(p);
        let os = arc(Probe::new("os"));
        d.run(req("front", &cloud, 600, Arm::AfterDispatch)).await;
        d.run(req("back", &cloud, 600, Arm::AfterDispatch)).await;
        d.run(req("garden", &os, 600, Arm::AfterDispatch)).await;
        let scope = d.stop(&cloud, "front", 1_100).await.unwrap();
        assert_eq!(scope, StopScope::Device);
        let armed = armed(&ar).await;
        assert_eq!(armed.len(), 1);
        assert_eq!(
            armed[0].zone_slug, "garden",
            "the other controller is untouched"
        );
        let rows = rows(&runs).await;
        for r in &rows {
            let expect = if r.zone_slug == "garden" {
                1_600
            } else {
                1_100
            };
            assert_eq!(r.end_epoch, Some(expect), "{}", r.zone_slug);
        }
    }

    #[tokio::test]
    async fn a_failed_stop_leaves_the_books_alone() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let c = arc(Probe::new("os"));
        d.run(req("front", &c, 600, Arm::AfterDispatch)).await;
        let mut p = Probe::new("os");
        p.fail = true;
        let down = arc(p);
        assert!(d.stop(&down, "front", 1_100).await.is_err());
        assert_eq!(armed(&ar).await.len(), 1, "the backstop stays armed");
        assert_eq!(rows(&runs).await[0].end_epoch, Some(1_600));
    }

    #[tokio::test]
    async fn stop_all_clears_only_the_controllers_that_confirmed() {
        let (runs, ar) = stores().await;
        let d = Dispatcher::new(ZoneLocks::default(), Some(&runs), Some(&ar));
        let registry = ControllerRegistry::new();
        let ok = arc(Probe::new("ok"));
        let mut p = Probe::new("down");
        p.fail = true;
        let down = arc(p);
        registry.set(vec![(ok.clone(), true), (down.clone(), false)]);
        d.run(req("a", &ok, 600, Arm::AfterDispatch)).await;
        ar.arm("b".into(), "down".into(), 1_000, 1_700)
            .await
            .unwrap();
        let report = d.stop_all(&registry, 1_100).await;
        assert!(report.confirmed.contains(&"ok".to_string()));
        let armed = armed(&ar).await;
        assert!(armed.iter().all(|r| r.controller_id != "ok"));
    }

    #[tokio::test]
    async fn stop_waits_for_the_inflight_command_and_its_deadline_before_closing() {
        for stop_all in [false, true] {
            let (runs, active) = stores().await;
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let mut probe = Probe::new("controller");
            probe.run_entered = Some(entered.clone());
            probe.run_release = Some(release.clone());
            let probe = Arc::new(probe);
            let controller: Arc<dyn IrrigationController> = probe.clone();
            let registry = ControllerRegistry::new();
            registry.set(vec![(controller.clone(), true)]);
            let dispatcher = Dispatcher::new(registry.zone_locks(), Some(&runs), Some(&active));
            let stopping = async {
                entered.notified().await;
                let stop = async {
                    if stop_all {
                        dispatcher.stop_all(&registry, 1_100).await;
                    } else {
                        dispatcher.stop(&controller, "front", 1_100).await.unwrap();
                    }
                };
                tokio::pin!(stop);
                assert!(futures::poll!(stop.as_mut()).is_pending());
                assert!(
                    probe.stops.lock().unwrap().is_empty(),
                    "stop cannot overtake run acknowledgement"
                );
                assert_eq!(probe.stop_alls.load(Ordering::SeqCst), 0);
                release.notify_one();
                stop.await;
            };
            let (outcome, ()) = tokio::join!(
                dispatcher.run(req("front", &controller, 600, Arm::AfterDispatch)),
                stopping,
            );
            assert!(matches!(outcome, RunOutcome::Dispatched { .. }));
            assert!(
                armed(&active).await.is_empty(),
                "no delayed run may re-arm after stop"
            );
            let recorded = rows(&runs).await;
            assert_eq!(recorded[0].end_epoch, Some(1_100));
            assert!(recorded[0]
                .note
                .as_deref()
                .unwrap()
                .contains("Stopped early"));
        }
    }

    #[tokio::test]
    async fn pending_restart_refuses_run_but_still_allows_stop() {
        let (runs, active) = stores().await;
        let locks = ZoneLocks::default();
        locks
            .restart_hold()
            .latch(vec!["controller configuration changed".into()]);
        let dispatcher = Dispatcher::new(locks, Some(&runs), Some(&active));
        let controller = arc(Probe::new("controller"));
        let result = dispatcher
            .run(req("front", &controller, 600, Arm::AfterDispatch))
            .await;
        assert!(matches!(
            result,
            RunOutcome::Failed {
                error: ControllerError::Held(_),
                ..
            }
        ));
        assert!(rows(&runs).await.is_empty());
        assert!(armed(&active).await.is_empty());
        assert_eq!(
            dispatcher.stop(&controller, "front", 1_100).await.unwrap(),
            StopScope::Zone
        );
    }
}
