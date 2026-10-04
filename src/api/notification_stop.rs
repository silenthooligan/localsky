//! A notification may stop only its original, still-current watering episode.
use super::irrigation::DispatchState;
use crate::{
    controllers::{dispatch::Dispatcher, notification_runs::NotificationRun},
    refresher::IrrigationStore,
};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
struct StopState {
    store: Arc<IrrigationStore>,
    dispatch: Option<DispatchState>,
}

pub fn router(store: Arc<IrrigationStore>, dispatch: Option<DispatchState>) -> Router {
    Router::new()
        .route("/notification-stop", post(stop))
        .with_state(StopState { store, dispatch })
}

async fn stop(
    State(state): State<StopState>,
    Json(request): Json<NotificationRun>,
) -> (StatusCode, Json<Value>) {
    let Some(dispatch) = state.dispatch else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"Open LocalSky to stop watering."})),
        );
    };
    let locks = dispatch.registry.zone_locks();
    let barrier = locks.command_order();
    let _command = barrier.write().await;
    let now = chrono::Utc::now().timestamp();
    let snapshot = state.store.snapshot();
    let controller_id = locks.notification_runs.controller(&request, now);
    let zone = snapshot.zones.iter().find(|z| z.slug == request.zone);
    let valid = zone.is_some_and(|z| {
        z.is_running_or_unconfirmed() && z.controller_id.as_ref() == controller_id.as_ref()
    }) && (0..=120).contains(&(now - snapshot.last_refresh_epoch));
    let controller = controller_id
        .as_deref()
        .and_then(|id| dispatch.registry.get(id));
    let Some(controller) = controller.filter(|_| valid) else {
        return (
            StatusCode::CONFLICT,
            Json(
                json!({"error":"This notification is no longer current. Open LocalSky to check watering."}),
            ),
        );
    };
    // Only a current, authorized action cancels the scheduler/Quick Run queue.
    crate::scheduler::dispatch_gate::request_stop();
    let dispatcher = Dispatcher::new(locks, dispatch.runs.as_ref(), dispatch.active_runs.as_ref());
    match tokio::time::timeout(
        std::time::Duration::from_secs(25),
        dispatcher.stop_locked(&controller, &request.zone, now),
    )
    .await
    {
        Ok(Ok(scope)) => (
            StatusCode::OK,
            Json(
                json!({"ok":true, "scope":if matches!(scope, crate::controllers::dispatch::StopScope::Device) { "device" } else { "zone" }, "message":"Stop sent. Remaining queued zones were cancelled."}),
            ),
        ),
        _ => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error":"Stop wasn't confirmed. Open LocalSky to retry."})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        controllers::ControllerRegistry,
        model::{IrrigationSnapshot, ZoneState},
        ports::irrigation_controller::*,
        refresher::WateringPolicy,
        scheduler::dispatch_gate,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Probe {
        stops: AtomicUsize,
        fail: AtomicBool,
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
        async fn run_zone(&self, zone: &str, duration: u32) -> ControllerResult<RunHandle> {
            Ok(RunHandle {
                controller_id: "probe".into(),
                zone_slug: zone.into(),
                started_epoch: chrono::Utc::now().timestamp(),
                planned_duration_s: duration,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, _: &str) -> ControllerResult<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
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
    fn fixture() -> (StopState, NotificationRun, Arc<Probe>) {
        let probe = Arc::new(Probe::default());
        let registry = ControllerRegistry::new();
        registry.set(vec![(probe.clone(), true)]);
        let now = chrono::Utc::now().timestamp();
        let request = registry
            .zone_locks()
            .notification_runs
            .issue("lawn", "probe", now + 1, now)
            .unwrap();
        let store = Arc::new(IrrigationStore::new());
        store.store(IrrigationSnapshot {
            last_refresh_epoch: now,
            zones: vec![ZoneState {
                slug: "lawn".into(),
                name: "Lawn".into(),
                running: true,
                running_known: true,
                controller_id: Some("probe".into()),
                ..Default::default()
            }],
            ..Default::default()
        });
        (
            StopState {
                store,
                dispatch: Some(DispatchState {
                    registry,
                    runs: None,
                    active_runs: None,
                    policy: Arc::new(arc_swap::ArcSwap::from_pointee(WateringPolicy::default())),
                }),
            },
            request,
            probe,
        )
    }

    #[tokio::test]
    async fn current_notification_stops_and_cancels_remaining_queue_once() {
        dispatch_gate::isolated(async {
            let (state, request, probe) = fixture();
            let reservation = state
                .dispatch
                .as_ref()
                .unwrap()
                .registry
                .zone_locks()
                .manual_session
                .reserve("queue".into())
                .unwrap();
            assert_eq!(
                stop(State(state.clone()), Json(request.clone())).await.0,
                StatusCode::OK
            );
            assert!(reservation.session.cancelled());
            assert_eq!(probe.stops.load(Ordering::SeqCst), 1);
            assert_eq!(
                stop(State(state), Json(request)).await.0,
                StatusCode::CONFLICT
            );
            assert_eq!(probe.stops.load(Ordering::SeqCst), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn outdated_notification_never_changes_current_watering_or_stop_gate() {
        dispatch_gate::isolated(async {
            for case in [
                "new_command",
                "finished",
                "changed_controller",
                "stale_snapshot",
                "unknown_id",
            ] {
                let (state, mut request, probe) = fixture();
                let gate = dispatch_gate::generation();
                let registry = &state.dispatch.as_ref().unwrap().registry;
                let mut snapshot = (*state.store.snapshot()).clone();
                match case {
                    "new_command" => registry
                        .zone_locks()
                        .notification_runs
                        .clear_controller("probe"),
                    "finished" => snapshot.zones[0].running = false,
                    "changed_controller" => registry.set(vec![]),
                    "stale_snapshot" => snapshot.last_refresh_epoch -= 121,
                    _ => request.run_id = "wrong-id".into(),
                }
                state.store.store(snapshot);
                assert_eq!(
                    stop(State(state), Json(request)).await.0,
                    StatusCode::CONFLICT,
                    "{case}"
                );
                assert_eq!(probe.stops.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(dispatch_gate::generation(), gate, "{case}");
            }
        })
        .await;
    }

    #[tokio::test]
    async fn failed_stop_remains_retryable_and_cancels_the_queue() {
        dispatch_gate::isolated(async {
            let (state, request, probe) = fixture();
            let reservation = state
                .dispatch
                .as_ref()
                .unwrap()
                .registry
                .zone_locks()
                .manual_session
                .reserve("queue".into())
                .unwrap();
            probe.fail.store(true, Ordering::SeqCst);
            assert_eq!(
                stop(State(state.clone()), Json(request.clone())).await.0,
                StatusCode::BAD_GATEWAY
            );
            assert!(reservation.session.cancelled());
            probe.fail.store(false, Ordering::SeqCst);
            assert_eq!(stop(State(state), Json(request)).await.0, StatusCode::OK);
            assert_eq!(probe.stops.load(Ordering::SeqCst), 2);
        })
        .await;
    }

    #[tokio::test]
    async fn a_later_dispatched_run_invalidates_the_notification_before_hardware() {
        dispatch_gate::isolated(async {
            let (state, request, probe) = fixture();
            let d = state.dispatch.as_ref().unwrap();
            let controller = d.registry.get("probe").unwrap();
            let executor = Dispatcher::new(d.registry.zone_locks(), None, None);
            let result = executor
                .run(crate::controllers::dispatch::RunRequest {
                    session_id: "new-run".into(),
                    zone: "lawn",
                    zone_name: "Lawn",
                    controller: &controller,
                    seconds: 60,
                    source: crate::controllers::dispatch::Source::Manual,
                    ceiling_s: None,
                    arm: crate::controllers::dispatch::Arm::None,
                    record_row: false,
                    cycle: None,
                    push: None,
                    now_epoch: chrono::Utc::now().timestamp(),
                })
                .await;
            assert!(matches!(
                result,
                crate::controllers::dispatch::RunOutcome::Dispatched { .. }
            ));
            assert_eq!(
                stop(State(state), Json(request)).await.0,
                StatusCode::CONFLICT
            );
            assert_eq!(probe.stops.load(Ordering::SeqCst), 0);
        })
        .await;
    }
}
