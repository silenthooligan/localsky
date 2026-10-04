use super::*;
use crate::{
    controllers::ControllerRegistry,
    model::{IrrigationSnapshot, ZoneState},
    persistence::{ActiveRunsStore, RunsStore},
    ports::irrigation_controller::{
        ControllerCaps, ControllerResult, ControllerStatus, RunHandle, RunRecord,
    },
    refresher::WateringPolicy,
    scheduler::dispatch_gate,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct Probe {
    events: std::sync::Mutex<Vec<String>>,
    fail_run: AtomicBool,
    fail_stop: AtomicBool,
}
#[async_trait::async_trait]
impl IrrigationController for Probe {
    fn id(&self) -> &str {
        "probe"
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
    async fn run_zone(&self, zone: &str, seconds: u32) -> ControllerResult<RunHandle> {
        self.events.lock().unwrap().push(format!("run:{zone}"));
        if self.fail_run.load(Ordering::SeqCst) {
            return Err(ControllerError::Offline);
        }
        Ok(RunHandle {
            controller_id: "probe".into(),
            zone_slug: zone.into(),
            started_epoch: chrono::Utc::now().timestamp(),
            planned_duration_s: seconds,
            provider_ref: None,
        })
    }
    async fn stop_zone(&self, zone: &str) -> ControllerResult<()> {
        self.events.lock().unwrap().push(format!("stop:{zone}"));
        if self.fail_stop.load(Ordering::SeqCst) {
            Err(ControllerError::Offline)
        } else {
            Ok(())
        }
    }
    async fn stop_all(&self) -> ControllerResult<()> {
        self.stop_zone("all").await
    }
    async fn status(&self) -> ControllerResult<ControllerStatus> {
        Err(ControllerError::Unsupported("No readback".into()))
    }
    async fn run_history(&self, _: i64) -> ControllerResult<Vec<RunRecord>> {
        Ok(vec![])
    }
}

fn fixture() -> (Manager, Arc<Probe>) {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::persistence::run_migrations(&mut conn).unwrap();
    let db = Arc::new(Mutex::new(conn));
    let probe = Arc::new(Probe::default());
    let registry = ControllerRegistry::new();
    registry.set(vec![(probe.clone(), true)]);
    let snapshot = Arc::new(IrrigationStore::new());
    snapshot.store(IrrigationSnapshot {
        zones: ["front", "back"]
            .into_iter()
            .map(|slug| ZoneState {
                slug: slug.into(),
                name: slug.into(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    (
        Manager {
            dispatch: Some(DispatchState {
                registry,
                runs: Some(RunsStore::new(db.clone())),
                active_runs: Some(ActiveRunsStore::new(db.clone())),
                policy: Arc::new(arc_swap::ArcSwap::from_pointee(WateringPolicy::default())),
            }),
            snapshot,
            db: Some(db),
            current: Arc::new(Mutex::new(None)),
            start_lock: Arc::new(Mutex::new(())),
        },
        probe,
    )
}
fn request(id: &str, seconds: u32) -> QuickRunRequest {
    QuickRunRequest {
        request_id: id.into(),
        zones: ["front", "back"]
            .into_iter()
            .map(|zone| QuickRunChoice {
                zone: zone.into(),
                seconds,
            })
            .collect(),
    }
}
async fn wait_for(
    manager: &Manager,
    predicate: impl Fn(&QuickRunStatus) -> bool,
) -> QuickRunStatus {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(run) = manager.current.lock().await.clone() {
                if predicate(&run) {
                    return run;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Quick Run reached expected state")
}
async fn finished(manager: &Manager) -> QuickRunStatus {
    wait_for(manager, |r| !r.phase.active()).await
}

#[tokio::test]
async fn validates_the_whole_selection_before_dispatch() {
    let (manager, probe) = fixture();
    for zones in [
        vec![],
        vec![QuickRunChoice {
            zone: "front".into(),
            seconds: 0,
        }],
        vec![QuickRunChoice {
            zone: "front".into(),
            seconds: 3601,
        }],
        vec![
            QuickRunChoice {
                zone: "front".into(),
                seconds: 1,
            },
            QuickRunChoice {
                zone: "missing".into(),
                seconds: 1,
            },
        ],
        vec![
            QuickRunChoice {
                zone: "front".into(),
                seconds: 1
            };
            2
        ],
    ] {
        assert!(manager
            .start(QuickRunRequest {
                request_id: "bad".into(),
                zones
            })
            .await
            .is_err());
    }
    assert!(probe.events.lock().unwrap().is_empty());
    assert!(manager
        .dispatch()
        .unwrap()
        .active_runs
        .as_ref()
        .unwrap()
        .armed()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn runs_sequentially_stops_between_zones_and_records_one_session_without_invented_water() {
    dispatch_gate::isolated(async {
        let (manager, probe) = fixture();
        let run = manager
            .start(request("sequence", 1))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        let done = finished(&manager).await;
        assert_eq!(done.phase, QuickRunPhase::Finished);
        assert_eq!(done.completed, 2);
        assert_eq!(
            *probe.events.lock().unwrap(),
            ["run:front", "stop:front", "run:back", "stop:back"]
        );
        let db = manager.db.as_ref().unwrap().lock().await;
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM watering_commands WHERE session_id = ?1",
                [&run.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0,
            "Command acknowledgements do not prove watering"
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM active_runs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    })
    .await;
}

#[tokio::test]
async fn duplicate_requests_are_idempotent_and_a_second_batch_is_rejected() {
    dispatch_gate::isolated(async {
        let (manager, probe) = fixture();
        let (one, two) = tokio::join!(
            manager.start(request("same", 60)),
            manager.start(request("same", 60))
        );
        let one = one.unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(one.id, two.unwrap_or_else(|e| panic!("{}", e.1)).id);
        assert!(manager.start(request("different", 60)).await.is_err());
        assert!(manager.start(request("same", 20)).await.is_err());
        wait_for(&manager, |r| r.phase == QuickRunPhase::Running).await;
        manager
            .dispatch()
            .unwrap()
            .registry
            .zone_locks()
            .manual_session
            .cancel(&one.id);
        assert_eq!(finished(&manager).await.phase, QuickRunPhase::Stopped);
        assert_eq!(*probe.events.lock().unwrap(), ["run:front", "stop:front"]);
        let replay = manager
            .start(request("same", 60))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(replay.phase, QuickRunPhase::Stopped);
    })
    .await;
}

#[tokio::test]
async fn stop_gate_from_another_control_cancels_the_remaining_zones() {
    dispatch_gate::isolated(async {
        let (manager, probe) = fixture();
        manager
            .start(request("stop", 60))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        wait_for(&manager, |r| r.phase == QuickRunPhase::Running).await;
        dispatch_gate::request_stop();
        assert_eq!(finished(&manager).await.phase, QuickRunPhase::Stopped);
        assert_eq!(*probe.events.lock().unwrap(), ["run:front", "stop:front"]);
    })
    .await;
}

#[tokio::test]
async fn failed_stop_retains_the_deadline_and_never_opens_the_next_valve() {
    dispatch_gate::isolated(async {
        let (manager, probe) = fixture();
        probe.fail_stop.store(true, Ordering::SeqCst);
        manager
            .start(request("stop-failure", 1))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        let done = finished(&manager).await;
        assert_eq!(done.phase, QuickRunPhase::Failed);
        assert!(done.message.contains("Stop was not confirmed"));
        assert_eq!(*probe.events.lock().unwrap(), ["run:front", "stop:front"]);
        assert_eq!(
            manager
                .dispatch()
                .unwrap()
                .active_runs
                .as_ref()
                .unwrap()
                .armed()
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(manager.start(request("retry", 1)).await.is_err());
        assert!(done.stop_unconfirmed);
        probe.fail_stop.store(false, Ordering::SeqCst);
        let Json(retried) = stop(State(manager.clone()), Json(StopRequest { id: done.id }))
            .await
            .unwrap();
        assert_eq!(retried.phase, QuickRunPhase::Stopped);
        assert!(!retried.stop_unconfirmed);
        assert!(manager
            .dispatch()
            .unwrap()
            .active_runs
            .as_ref()
            .unwrap()
            .armed()
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            *probe.events.lock().unwrap(),
            ["run:front", "stop:front", "stop:all"]
        );
    })
    .await;
}

#[tokio::test]
async fn ambiguous_run_failure_attempts_stop_and_cancels_queue() {
    dispatch_gate::isolated(async {
        let (manager, probe) = fixture();
        probe.fail_run.store(true, Ordering::SeqCst);
        manager
            .start(request("run-failure", 1))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(finished(&manager).await.phase, QuickRunPhase::Failed);
        assert_eq!(*probe.events.lock().unwrap(), ["run:front", "stop:front"]);
    })
    .await;
}

#[tokio::test]
async fn a_restart_reports_interrupted_and_never_replays_a_saved_request() {
    let (manager, probe) = fixture();
    let req = request("saved", 60);
    manager
        .save(&QuickRunStatus {
            id: "previous-process".into(),
            request_id: req.request_id.clone(),
            zones: req.zones.clone(),
            names: vec!["front".into(), "back".into()],
            phase: QuickRunPhase::Running,
            completed: 0,
            current: Some(0),
            current_ends_epoch: Some(123),
            started_epoch: 100,
            message: String::new(),
            stop_unconfirmed: false,
        })
        .await
        .unwrap_or_else(|e| panic!("{}", e.1));
    let view = manager.view().await.unwrap_or_else(|e| panic!("{}", e.1));
    assert_eq!(view.run.unwrap().phase, QuickRunPhase::Interrupted);
    assert_eq!(
        manager
            .start(req)
            .await
            .unwrap_or_else(|e| panic!("{}", e.1))
            .phase,
        QuickRunPhase::Interrupted
    );
    assert!(probe.events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn another_dispatch_path_cannot_overlap_the_quick_run() {
    dispatch_gate::isolated(async {
        let (manager, probe) = fixture();
        let run = manager
            .start(request("exclusive", 60))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        wait_for(&manager, |r| r.phase == QuickRunPhase::Running).await;
        let d = manager.dispatch().unwrap();
        let controller = d.registry.default().unwrap();
        let executor = Dispatcher::new(
            d.registry.zone_locks(),
            d.runs.as_ref(),
            d.active_runs.as_ref(),
        );
        let outcome = executor
            .run(RunRequest {
                session_id: "other".into(),
                zone: "back",
                zone_name: "Back",
                controller: &controller,
                seconds: 1,
                source: Source::SmartMorning,
                ceiling_s: None,
                arm: Arm::None,
                record_row: false,
                cycle: None,
                push: None,
                now_epoch: chrono::Utc::now().timestamp(),
            })
            .await;
        assert!(matches!(
            outcome,
            RunOutcome::Failed {
                error: ControllerError::Held(_),
                ..
            }
        ));
        d.registry.zone_locks().manual_session.cancel(&run.id);
        finished(&manager).await;
        assert_eq!(*probe.events.lock().unwrap(), ["run:front", "stop:front"]);
    })
    .await;
}

#[tokio::test]
async fn controller_replacement_cancels_remaining_queue_without_redirecting_it() {
    dispatch_gate::isolated(async {
        let (manager, probe) = fixture();
        manager
            .start(request("changed", 1))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        wait_for(&manager, |r| r.phase == QuickRunPhase::Running).await;
        let replacement = Arc::new(Probe::default());
        manager
            .dispatch()
            .unwrap()
            .registry
            .set(vec![(replacement.clone(), true)]);
        assert_eq!(finished(&manager).await.phase, QuickRunPhase::Failed);
        assert_eq!(*probe.events.lock().unwrap(), ["run:front", "stop:front"]);
        assert!(replacement.events.lock().unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn stale_stop_request_cannot_cancel_a_different_session() {
    dispatch_gate::isolated(async {
        let (manager, _) = fixture();
        let run = manager
            .start(request("current", 60))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        wait_for(&manager, |r| r.phase == QuickRunPhase::Running).await;
        assert!(stop(
            State(manager.clone()),
            Json(StopRequest { id: "old".into() })
        )
        .await
        .is_err());
        assert_eq!(
            manager
                .view()
                .await
                .unwrap_or_else(|e| panic!("{}", e.1))
                .run
                .unwrap()
                .phase,
            QuickRunPhase::Running
        );
        let _ = stop(State(manager.clone()), Json(StopRequest { id: run.id }))
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(finished(&manager).await.phase, QuickRunPhase::Stopped);
    })
    .await;
}
