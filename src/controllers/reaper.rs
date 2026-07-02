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
    for run in due {
        let Some(controller) = registry.get(&run.controller_id) else {
            // Controller no longer registered (config change). Nothing to stop;
            // drop the stale ledger row so the reaper does not spin on it.
            tracing::warn!(
                zone = %run.zone_slug, controller = %run.controller_id,
                "reaper: controller no longer registered; dropping stale active_run"
            );
            let _ = store.disarm(&run.zone_slug).await;
            continue;
        };
        match controller.stop_zone(&run.zone_slug).await {
            Ok(()) => {
                // Routine, not an alarm: the reaper is the authoritative shutoff at
                // the deadline. The controller's own (precise) timer is the
                // fast-path; this guarantees closure even if that timer failed or
                // the controller was briefly unreachable. Idempotent on an
                // already-closed valve.
                tracing::info!(
                    zone = %run.zone_slug, controller = %run.controller_id,
                    deadline = run.off_deadline_epoch,
                    "reaper: backstop shutoff issued at deadline"
                );
                let _ = store.disarm(&run.zone_slug).await;
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
            reap_once(&store, &registry, Utc::now().timestamp()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::irrigation_controller::{
        ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
        RunHandle, RunRecord,
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
}
