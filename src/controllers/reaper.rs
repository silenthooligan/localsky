// Deadline reaper (P0-1b). Enforces the active_runs ledger's shutoff deadlines
// independent of any controller's own shutoff: for every commanded-ON zone past
// its deadline, issue stop_zone and disarm. A stop that fails is retried next tick
// (the row is kept), so we never give up enforcing a shutoff. This is the
// authoritative backstop that makes "no valve stays open past its deadline" hold
// even when an adapter's in-process shutoff timer fails or the controller was
// briefly unreachable. (Process death is covered separately by boot
// reconcile_stop_all + the refresher watchdog restart.)

use std::time::Duration;

use chrono::Utc;

use crate::controllers::registry::ControllerRegistry;
use crate::persistence::ActiveRunsStore;

/// Poll granularity: the only slack on a failed-self-shutoff valve. The
/// controller's own timer stays the precise fast-path; this is the guaranteed
/// backstop, so 10s is ample.
const REAP_INTERVAL: Duration = Duration::from_secs(10);

/// Grace added to a run's shutoff deadline before the reaper enforces a
/// shutoff. Covers controller-clock skew + the reaper's own poll granularity,
/// so a valve closing right on time is never falsely "enforced". Shared by
/// every deadline-arm site (smart morning, the manual API run, the manual
/// scheduler).
pub const ACTIVE_RUN_GRACE_S: i64 = 30;

/// Wider grace for controllers whose only stop is DEVICE-WIDE
/// (caps.per_zone_stop = false: the Rachio-class clouds). Two reasons: their
/// dispatch path rides a cloud (15s HTTP timeout plus cloud-to-controller
/// propagation makes 30s tight against an on-time self-shutoff), and a false
/// enforcement is expensive there because the reaper's stop kills every
/// sibling zone on the device. The effective grace for these controllers is
/// max(ACTIVE_RUN_GRACE_S, this); the controller's own duration timer stays
/// the primary shutoff either way.
pub const CLOUD_DEVICE_STOP_GRACE_S: i64 = 90;

/// Deadline grace for a controller: the tight default when it can stop one
/// zone precisely, the widened cloud grace when a zone-stop is device-wide.
pub fn effective_run_grace(per_zone_stop: bool) -> i64 {
    if per_zone_stop {
        ACTIVE_RUN_GRACE_S
    } else {
        ACTIVE_RUN_GRACE_S.max(CLOUD_DEVICE_STOP_GRACE_S)
    }
}

/// One reaper pass: enforce shutoff for every armed run at or past `now`. Returns
/// the number of zones successfully stopped + disarmed this pass.
pub async fn reap_once(store: &ActiveRunsStore, registry: &ControllerRegistry, now: i64) -> usize {
    let due = match store.due(now).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "reaper: could not read active_runs; skipping pass");
            return 0;
        }
    };
    let mut enforced = 0;
    // Controllers already device-wide-stopped THIS pass: their remaining due
    // rows were cleared from the ledger below, but this pass's `due` snapshot
    // still lists them; skip rather than re-fire another device stop.
    let mut device_stopped: std::collections::HashSet<String> = std::collections::HashSet::new();
    for run in due {
        if device_stopped.contains(&run.controller_id) {
            continue;
        }
        // Resolve the controller to stop through. If the run's own controller
        // id is no longer registered, DO NOT silently disarm: that drops the
        // shutoff backstop and can strand a valve open. This id-miss happens
        // when a config hot-reload changes a controller's id while a zone is
        // armed (same physical hardware, new id) -- the MQTT/DIY path's only
        // other off is an in-process timer bound to the now-dropped instance,
        // so the ledger is the last defense. Fall back to the current default
        // controller (the same hardware in the common rename case; stopping a
        // zone that isn't running there is an idempotent no-op). With no
        // controller registered at all, keep the row and retry next tick rather
        // than abandon the backstop (boot_reconcile closes valves at next start).
        let controller = match registry.get(&run.controller_id) {
            Some(c) => c,
            None => match registry.default() {
                Some(def) => {
                    tracing::warn!(
                        zone = %run.zone_slug, controller = %run.controller_id,
                        fallback = %def.id(),
                        "reaper: run's controller no longer registered; enforcing shutoff via default controller"
                    );
                    def
                }
                None => {
                    tracing::error!(
                        zone = %run.zone_slug, controller = %run.controller_id,
                        "reaper: run's controller gone and no controller registered; keeping row for retry"
                    );
                    continue;
                }
            },
        };
        // A controller without a per-zone stop (Rachio, B-hyve, Rain Bird
        // clouds) turns this zone-stop into a DEVICE-WIDE stop: any sibling
        // zone running on the same device stops with it. Because nothing
        // disarms a row on natural completion, the routine case at a
        // deadline on these controllers is a zone that ALREADY finished on
        // its own timer while a sibling is legitimately running (a serial
        // multi-zone morning). VERIFY before enforcing: a zone the
        // controller confirms idle (known, not running) satisfies its
        // deadline with no stop at all. Only a zone reported running, a
        // zone whose state is unknown, or an unreachable controller still
        // gets the device-wide stop; that is the genuine stuck-valve case,
        // where collateral is the accepted price of closing the valve.
        let device_wide = !controller.supports().per_zone_stop;
        if device_wide {
            let confirmed_idle = match controller.status().await {
                Ok(st) => st
                    .zone_states
                    .iter()
                    .find(|z| z.slug == run.zone_slug)
                    .map(|z| z.running_known && !z.running)
                    .unwrap_or(false),
                Err(_) => false, // unreachable: keep the fail-safe stop
            };
            if confirmed_idle {
                tracing::info!(
                    zone = %run.zone_slug, controller = %controller.id(),
                    deadline = run.off_deadline_epoch,
                    "reaper: zone confirmed idle at its deadline; disarmed without a device-wide stop"
                );
                let _ = store.disarm(&run.zone_slug).await;
                continue;
            }
            tracing::warn!(
                zone = %run.zone_slug, controller = %controller.id(),
                "reaper: this controller has no per-zone stop; enforcement stops ALL watering on the device"
            );
        }
        match controller.stop_zone(&run.zone_slug).await {
            Ok(()) => {
                // Routine, not an alarm: the reaper is the authoritative shutoff at
                // the deadline. The controller's own (precise) timer is the
                // fast-path; this guarantees closure even if that timer failed or
                // the controller was briefly unreachable. Idempotent on an
                // already-closed valve.
                tracing::info!(
                    zone = %run.zone_slug, controller = %run.controller_id,
                    device_wide = device_wide,
                    deadline = run.off_deadline_epoch,
                    "reaper: backstop shutoff issued at deadline"
                );
                if device_wide {
                    // The one stop closed EVERY valve on this device, so all
                    // of its armed rows are satisfied; leaving them would
                    // re-fire more device-wide stops against whatever the
                    // operator restarts. Clear under both the row's id and
                    // the resolved controller's id (rename fallback). Other
                    // controllers' rows are untouched: their valves are
                    // still open and their backstop must hold.
                    match store
                        .clear_for_controllers(&[run.controller_id.as_str(), controller.id()])
                        .await
                    {
                        Ok(n) if n > 1 => tracing::info!(
                            controller = %controller.id(),
                            cleared = n,
                            "reaper: device-wide stop satisfied sibling deadlines; cleared their rows"
                        ),
                        Ok(_) => {}
                        Err(e) => tracing::warn!(
                            controller = %controller.id(), error = %e,
                            "reaper: could not clear sibling rows after device-wide stop"
                        ),
                    }
                    device_stopped.insert(run.controller_id.clone());
                    device_stopped.insert(controller.id().to_string());
                } else {
                    let _ = store.disarm(&run.zone_slug).await;
                }
                enforced += 1;
            }
            Err(e) => {
                // Keep the row and retry next tick: an unconfirmed stop is worse
                // than a redundant one.
                tracing::error!(
                    zone = %run.zone_slug, controller = %run.controller_id, error = %e,
                    "reaper: stop_zone failed; will retry next tick"
                );
            }
        }
    }
    enforced
}

/// P0-1 boot reconciliation. Physically close every zone on every registered
/// controller, then clear the persisted deadline ledger (if any). Run once at
/// startup before the schedulers or API can dispatch, so a valve left open by a
/// crash/redeploy mid-run (the MQTT path's shutoff is an in-process timer that
/// dies with the process) is closed on the next start instead of staying open
/// until a human notices. The ledger is cleared because `reconcile_stop_all`
/// just closed everything, so any pre-restart deadlines are moot and must not
/// make the reaper re-stop a valve already known off. Returns the ids of
/// controllers that did not confirm stop_all (unreachable at boot); best-effort,
/// never fatal. A `None` store is a no-op clear: no DB means no persisted
/// deadlines, but the valves are still physically closed regardless.
pub async fn boot_reconcile(
    registry: &ControllerRegistry,
    active_runs: Option<&ActiveRunsStore>,
) -> Vec<String> {
    let failed = registry.reconcile_stop_all().await;
    if let Some(ar) = active_runs {
        match ar.clear_all().await {
            Ok(n) if n > 0 => {
                tracing::info!(
                    cleared = n,
                    "boot reconcile: cleared stale active-run deadlines"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "boot reconcile: could not clear active_runs"),
        }
    }
    failed
}

/// Spawn the reaper loop, polling every `REAP_INTERVAL`.
pub fn spawn_run_reaper(store: ActiveRunsStore, registry: ControllerRegistry) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REAP_INTERVAL);
        loop {
            tick.tick().await;
            // P0-8 class: the reaper is the LAST line of stuck-valve defense,
            // so a panic inside one reap (a poisoned lock, an adapter bug)
            // must not kill the loop for the process lifetime. catch_unwind
            // turns it into a logged skip; the next tick retries against the
            // same persisted ledger. Mirrors the push dispatcher's supervisor.
            use futures::FutureExt;
            let outcome =
                std::panic::AssertUnwindSafe(reap_once(&store, &registry, Utc::now().timestamp()))
                    .catch_unwind()
                    .await;
            if outcome.is_err() {
                tracing::error!("run reaper: reap tick PANICKED; continuing on next tick");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::irrigation_controller::{
        ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
        RunHandle, RunRecord, ZoneRuntimeStatus,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Mutex as TokioMutex;

    /// Records the zones it was asked to stop + how many times stop_all was
    /// called (the boot-reconcile path); can be told to fail per-zone stops.
    struct StopRecorder {
        id: String,
        stopped: Arc<Mutex<Vec<String>>>,
        stop_alls: Arc<AtomicUsize>,
        fail: AtomicBool,
    }

    #[async_trait::async_trait]
    impl IrrigationController for StopRecorder {
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
            }
        }
        async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
            Ok(RunHandle {
                controller_id: self.id.clone(),
                zone_slug: slug.to_string(),
                started_epoch: 0,
                planned_duration_s: duration_s,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(ControllerError::Transport("unreachable".into()));
            }
            self.stopped.lock().unwrap().push(slug.to_string());
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            self.stop_alls.fetch_add(1, Ordering::SeqCst);
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

    fn mem_store() -> ActiveRunsStore {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut c).unwrap();
        ActiveRunsStore::new(Arc::new(TokioMutex::new(c)))
    }

    #[tokio::test]
    async fn reaper_enforces_past_deadline_and_leaves_future_alone() {
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let ctrl: Arc<dyn IrrigationController> = Arc::new(StopRecorder {
            id: "ctrl".into(),
            stopped: stopped.clone(),
            stop_alls: Arc::new(AtomicUsize::new(0)),
            fail: AtomicBool::new(false),
        });
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctrl, true)]);

        let store = mem_store();
        store
            .arm("past".into(), "ctrl".into(), 0, 100)
            .await
            .unwrap();
        store
            .arm("future".into(), "ctrl".into(), 0, 9_999)
            .await
            .unwrap();

        let enforced = reap_once(&store, &registry, 200).await;
        assert_eq!(enforced, 1, "only the past-deadline zone is enforced");
        assert_eq!(*stopped.lock().unwrap(), vec!["past".to_string()]);
        // The enforced row is disarmed; the future one remains.
        assert!(store.due(200).await.unwrap().is_empty());
        assert_eq!(store.due(10_000).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reaper_keeps_row_and_retries_when_stop_fails() {
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::new(StopRecorder {
            id: "ctrl".into(),
            stopped: stopped.clone(),
            stop_alls: Arc::new(AtomicUsize::new(0)),
            fail: AtomicBool::new(true),
        });
        let ctrl: Arc<dyn IrrigationController> = recorder.clone();
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctrl, true)]);

        let store = mem_store();
        store.arm("z".into(), "ctrl".into(), 0, 100).await.unwrap();

        // First pass: stop fails, row is kept (not disarmed), nothing recorded.
        assert_eq!(reap_once(&store, &registry, 200).await, 0);
        assert_eq!(store.due(200).await.unwrap().len(), 1, "row kept for retry");
        assert!(stopped.lock().unwrap().is_empty());

        // Controller recovers; next pass enforces + disarms.
        recorder.fail.store(false, Ordering::SeqCst);
        assert_eq!(reap_once(&store, &registry, 200).await, 1);
        assert_eq!(*stopped.lock().unwrap(), vec!["z".to_string()]);
        assert!(store.due(200).await.unwrap().is_empty());
    }

    // Regression: a config hot-reload changed the controller's id while a zone
    // was armed. The run's stored controller_id no longer resolves, but the
    // default controller (the same hardware under a new id) must still enforce
    // the shutoff -- the reaper must NOT silently disarm and strand the valve.
    #[tokio::test]
    async fn reaper_enforces_via_default_when_run_controller_id_is_gone() {
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let ctrl: Arc<dyn IrrigationController> = Arc::new(StopRecorder {
            id: "new_id".into(),
            stopped: stopped.clone(),
            stop_alls: Arc::new(AtomicUsize::new(0)),
            fail: AtomicBool::new(false),
        });
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctrl, true)]); // "new_id" is the default

        let store = mem_store();
        // Armed under the OLD id, which is no longer in the registry.
        store
            .arm("back_yard".into(), "old_id".into(), 0, 100)
            .await
            .unwrap();

        let enforced = reap_once(&store, &registry, 200).await;
        assert_eq!(enforced, 1, "shutoff enforced via the default controller");
        assert_eq!(*stopped.lock().unwrap(), vec!["back_yard".to_string()]);
        assert!(
            store.due(200).await.unwrap().is_empty(),
            "row disarmed only after a confirmed stop"
        );
    }

    // Edge: the run's controller_id is gone AND no controller is registered at
    // all. There is nothing to stop through, so the row must be KEPT for retry
    // (not disarmed) -- abandoning it would drop the backstop entirely.
    #[tokio::test]
    async fn reaper_keeps_row_when_no_controller_registered() {
        let registry = ControllerRegistry::new(); // empty
        let store = mem_store();
        store.arm("z".into(), "gone".into(), 0, 100).await.unwrap();

        let enforced = reap_once(&store, &registry, 200).await;
        assert_eq!(enforced, 0, "nothing could be stopped");
        assert_eq!(
            store.due(200).await.unwrap().len(),
            1,
            "row kept for retry, not silently disarmed"
        );
    }

    // P0-1 end-to-end: a run was in progress when the process was killed (its
    // deadline is armed in the ledger and the in-process shutoff timer died with
    // it). Boot must physically close every valve AND clear the stale ledger so
    // the reaper does not later re-stop a valve already known off.
    #[tokio::test]
    async fn boot_reconcile_closes_valves_and_clears_stale_deadlines() {
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let stop_alls = Arc::new(AtomicUsize::new(0));
        let ctrl: Arc<dyn IrrigationController> = Arc::new(StopRecorder {
            id: "ctrl".into(),
            stopped: stopped.clone(),
            stop_alls: stop_alls.clone(),
            fail: AtomicBool::new(false),
        });
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctrl, true)]);

        // Simulate the crashed-mid-run state: a deadline persisted in the ledger.
        let store = mem_store();
        store
            .arm("back_yard".into(), "ctrl".into(), 0, 100)
            .await
            .unwrap();

        // Boot reconciliation.
        let failed = boot_reconcile(&registry, Some(&store)).await;
        assert!(
            failed.is_empty(),
            "a reachable controller confirms stop_all"
        );
        assert_eq!(
            stop_alls.load(Ordering::SeqCst),
            1,
            "boot emits stop_all on every controller"
        );
        assert!(
            store.due(1_000_000).await.unwrap().is_empty(),
            "stale deadlines cleared so the reaper has nothing to re-fire"
        );

        // Post-boot the reaper finds nothing due: the crashed run's deadline is
        // gone, so it issues no redundant (or worse, wrong) per-zone shutoff.
        assert_eq!(reap_once(&store, &registry, 1_000_000).await, 0);
        assert!(
            stopped.lock().unwrap().is_empty(),
            "no per-zone stop after the ledger was cleared at boot"
        );
    }

    // P0-1 edge: no persistence DB means no ActiveRunsStore, but boot must still
    // physically close every valve in case a crash left one open. The None-store
    // clear is a no-op; reconcile_stop_all is not.
    #[tokio::test]
    async fn boot_reconcile_without_persistence_still_closes_valves() {
        let stop_alls = Arc::new(AtomicUsize::new(0));
        let ctrl: Arc<dyn IrrigationController> = Arc::new(StopRecorder {
            id: "ctrl".into(),
            stopped: Arc::new(Mutex::new(Vec::new())),
            stop_alls: stop_alls.clone(),
            fail: AtomicBool::new(false),
        });
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctrl, true)]);

        let failed = boot_reconcile(&registry, None).await;
        assert!(failed.is_empty());
        assert_eq!(
            stop_alls.load(Ordering::SeqCst),
            1,
            "valves are closed at boot even without a deadline ledger"
        );
    }

    // ---- device-wide-stop (per_zone_stop=false) enforcement ----

    /// Cloud-style double: NO per-zone stop (stop_zone stops the device),
    /// configurable zone states, records every stop, can be made
    /// unreachable for status.
    struct CloudRecorder {
        id: String,
        stopped: Arc<Mutex<Vec<String>>>,
        states: Arc<Mutex<Vec<ZoneRuntimeStatus>>>,
        status_fails: AtomicBool,
    }

    impl CloudRecorder {
        fn new(id: &str, states: Vec<ZoneRuntimeStatus>) -> Arc<Self> {
            Arc::new(Self {
                id: id.into(),
                stopped: Arc::new(Mutex::new(Vec::new())),
                states: Arc::new(Mutex::new(states)),
                status_fails: AtomicBool::new(false),
            })
        }
    }

    fn zstate(slug: &str, running: bool, running_known: bool) -> ZoneRuntimeStatus {
        ZoneRuntimeStatus {
            slug: slug.into(),
            running,
            remaining_s: None,
            last_run_epoch: None,
            running_known,
        }
    }

    #[async_trait::async_trait]
    impl IrrigationController for CloudRecorder {
        fn id(&self) -> &str {
            &self.id
        }
        fn supports(&self) -> ControllerCaps {
            ControllerCaps {
                flow_meter: false,
                rain_sensor: false,
                master_valve: true,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
                water_level: false,
                per_zone_stop: false,
            }
        }
        async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
            Ok(RunHandle {
                controller_id: self.id.clone(),
                zone_slug: slug.to_string(),
                started_epoch: 0,
                planned_duration_s: duration_s,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
            self.stopped.lock().unwrap().push(slug.to_string());
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            Ok(())
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            if self.status_fails.load(Ordering::SeqCst) {
                return Err(ControllerError::Transport("unreachable".into()));
            }
            Ok(ControllerStatus {
                reachable: true,
                master_enabled: Some(true),
                water_level_pct: None,
                rain_sensor_tripped: None,
                current_program: None,
                zone_states: self.states.lock().unwrap().clone(),
                flow_gpm: None,
                flow_connected: false,
                firmware: None,
            })
        }
        async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(vec![])
        }
    }

    // THE two-zone cloud morning: front finished on its own timer (status
    // confirms idle) while back is legitimately running. Front's deadline
    // must disarm QUIETLY; a device-wide stop here would kill back
    // mid-run, which is the exact multi-zone morning defect.
    #[tokio::test]
    async fn cloud_reaper_disarms_confirmed_idle_zone_without_device_stop() {
        let cloud = CloudRecorder::new(
            "rachio_main",
            vec![zstate("front", false, true), zstate("back", true, true)],
        );
        let stopped = cloud.stopped.clone();
        let ctl: Arc<dyn IrrigationController> = cloud;
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctl, true)]);

        let store = mem_store();
        // front's deadline has passed (it finished naturally); back's is
        // still in the future (it is mid-run).
        store
            .arm("front".into(), "rachio_main".into(), 0, 100)
            .await
            .unwrap();
        store
            .arm("back".into(), "rachio_main".into(), 0, 9_999)
            .await
            .unwrap();

        let enforced = reap_once(&store, &registry, 200).await;
        assert_eq!(enforced, 0, "confirmed idle needs no enforcement");
        assert!(
            stopped.lock().unwrap().is_empty(),
            "NO device-wide stop may fire while the sibling zone runs"
        );
        assert!(
            store.due(200).await.unwrap().is_empty(),
            "front's satisfied deadline is disarmed"
        );
        assert_eq!(
            store.due(10_000).await.unwrap().len(),
            1,
            "back's backstop row is untouched"
        );
    }

    // Fail-safe direction: a zone REPORTED running at its deadline is the
    // genuine stuck-valve case; the device-wide stop fires, and because it
    // stopped every valve on the device, ALL of that controller's armed
    // rows clear so no second device-wide stop can re-fire later.
    #[tokio::test]
    async fn cloud_reaper_stops_running_zone_and_clears_sibling_rows() {
        let cloud = CloudRecorder::new(
            "rachio_main",
            vec![zstate("front", true, true), zstate("back", true, true)],
        );
        let stopped = cloud.stopped.clone();
        let ctl: Arc<dyn IrrigationController> = cloud;
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctl, true)]);

        let store = mem_store();
        store
            .arm("front".into(), "rachio_main".into(), 0, 100)
            .await
            .unwrap();
        store
            .arm("back".into(), "rachio_main".into(), 0, 9_999)
            .await
            .unwrap();

        let enforced = reap_once(&store, &registry, 200).await;
        assert_eq!(enforced, 1);
        assert_eq!(*stopped.lock().unwrap(), vec!["front".to_string()]);
        assert!(
            store.due(10_000).await.unwrap().is_empty(),
            "the device-wide stop satisfied every row on this controller"
        );
    }

    // Unknown state and unreachable status keep the fail-safe stop: the
    // reaper may only skip enforcement on a CONFIRMED idle.
    #[tokio::test]
    async fn cloud_reaper_enforces_on_unknown_state_or_unreachable_status() {
        // running_known=false: carried-forward state is not confirmation.
        let cloud = CloudRecorder::new("cloud_a", vec![zstate("front", false, false)]);
        let stopped = cloud.stopped.clone();
        let ctl: Arc<dyn IrrigationController> = cloud;
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctl, true)]);
        let store = mem_store();
        store
            .arm("front".into(), "cloud_a".into(), 0, 100)
            .await
            .unwrap();
        assert_eq!(reap_once(&store, &registry, 200).await, 1);
        assert_eq!(*stopped.lock().unwrap(), vec!["front".to_string()]);

        // Status unreachable: same fail-safe.
        let cloud = CloudRecorder::new("cloud_b", vec![]);
        cloud.status_fails.store(true, Ordering::SeqCst);
        let stopped = cloud.stopped.clone();
        let ctl: Arc<dyn IrrigationController> = cloud;
        let registry = ControllerRegistry::new();
        registry.set(vec![(ctl, true)]);
        let store = mem_store();
        store
            .arm("back".into(), "cloud_b".into(), 0, 100)
            .await
            .unwrap();
        assert_eq!(reap_once(&store, &registry, 200).await, 1);
        assert_eq!(*stopped.lock().unwrap(), vec!["back".to_string()]);
    }

    // Pin the deadline-grace contract shared by every arm site: precise
    // per-zone-stop controllers keep the tight 30s; device-wide-stop clouds
    // get 90s so cloud latency never triggers an enforcement that would
    // kill sibling zones.
    #[test]
    fn run_grace_widens_for_device_wide_stop_controllers() {
        assert_eq!(effective_run_grace(true), 30);
        assert_eq!(effective_run_grace(false), 90);
    }
}
