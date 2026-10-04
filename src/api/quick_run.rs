//! Server-owned, sequential manual watering. No browser timer actuates valves.
use super::irrigation::DispatchState;
use crate::{
    controllers::{
        dispatch::{self, Arm, Dispatcher, RunOutcome, RunRequest, Source},
        manual_session::Reservation,
    },
    model::quick_run::*,
    ports::irrigation_controller::{ControllerError, IrrigationController},
    refresher::IrrigationStore,
};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::Mutex;

type ApiResult<T> = Result<T, QuickRunError>;
#[derive(Debug)]
struct QuickRunError(StatusCode, String);
impl IntoResponse for QuickRunError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}
fn conflict(message: impl Into<String>) -> QuickRunError {
    QuickRunError(StatusCode::CONFLICT, message.into())
}
fn invalid(message: impl Into<String>) -> QuickRunError {
    QuickRunError(StatusCode::UNPROCESSABLE_ENTITY, message.into())
}
fn storage_error(error: impl std::fmt::Display) -> QuickRunError {
    tracing::error!(%error, "Quick Run journal unavailable");
    QuickRunError(
        StatusCode::SERVICE_UNAVAILABLE,
        "Quick Run history is unavailable. No new run was started.".into(),
    )
}

#[derive(Clone)]
struct Manager {
    dispatch: Option<DispatchState>,
    snapshot: Arc<IrrigationStore>,
    db: Option<Arc<Mutex<Connection>>>,
    current: Arc<Mutex<Option<QuickRunStatus>>>,
    // Serializes start/idempotency validation, not the watering duration.
    start_lock: Arc<Mutex<()>>,
}

pub fn router(
    snapshot: Arc<IrrigationStore>,
    dispatch: Option<DispatchState>,
    db: Option<Arc<Mutex<Connection>>>,
) -> Router {
    Router::new()
        .route("/quick-run", get(status).post(start))
        .route("/quick-run/stop", post(stop))
        .with_state(Manager {
            dispatch,
            snapshot,
            db,
            current: Arc::new(Mutex::new(None)),
            start_lock: Arc::new(Mutex::new(())),
        })
}

impl Manager {
    fn dispatch(&self) -> ApiResult<&DispatchState> {
        self.dispatch
            .as_ref()
            .filter(|d| self.db.is_some() && d.runs.is_some() && d.active_runs.is_some())
            .ok_or_else(|| {
                QuickRunError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Add a supported controller in Settings to use Quick Run.".into(),
                )
            })
    }

    async fn read_saved(&self, request_id: Option<String>) -> ApiResult<Option<QuickRunStatus>> {
        let Some(db) = self.db.clone() else {
            return Ok(None);
        };
        let json = tokio::task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            if let Some(id) = request_id {
                conn.query_row(
                    "SELECT status_json FROM quick_runs WHERE request_id = ?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            } else {
                conn.query_row(
                    "SELECT status_json FROM quick_runs ORDER BY rowid DESC LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            }
        })
        .await
        .map_err(storage_error)?
        .map_err(storage_error)?;
        json.map(|value| serde_json::from_str(&value).map_err(storage_error))
            .transpose()
    }

    async fn save(&self, run: &QuickRunStatus) -> ApiResult<()> {
        let db = self
            .db
            .clone()
            .ok_or_else(|| storage_error("missing database"))?;
        let json = serde_json::to_string(run).map_err(storage_error)?;
        let run = run.clone();
        tokio::task::spawn_blocking(move || {
            db.blocking_lock().execute(
                "INSERT INTO quick_runs (id, request_id, started_epoch, status_json) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(id) DO UPDATE SET status_json = excluded.status_json",
                rusqlite::params![run.id, run.request_id, run.started_epoch, json],
            )
        }).await.map_err(storage_error)?.map_err(storage_error)?;
        Ok(())
    }

    fn interrupted(mut run: QuickRunStatus) -> QuickRunStatus {
        if run.phase.active() {
            run.phase = QuickRunPhase::Interrupted;
            run.current_ends_epoch = None;
            run.message = "LocalSky restarted. Remaining zones were cancelled; check current watering before starting again.".into();
        }
        run
    }

    async fn view(&self) -> ApiResult<QuickRunView> {
        let mut current = self.current.lock().await;
        if current.is_none() {
            *current = self.read_saved(None).await?.map(Self::interrupted);
        }
        let dispatch = self.dispatch();
        let zones = dispatch
            .as_ref()
            .map(|d| {
                let policy = d.policy.load();
                self.snapshot
                    .snapshot()
                    .zones
                    .iter()
                    .map(|zone| QuickRunZone {
                        zone: zone.slug.clone(),
                        name: zone.name.clone(),
                        max_seconds: policy
                            .zone_runtime
                            .get(&zone.slug)
                            .map(|z| z.max_duration_s)
                            .unwrap_or(3600)
                            .min(7200),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(QuickRunView {
            available: dispatch.is_ok(),
            reason: dispatch.err().map(|e| e.1),
            zones,
            run: current.clone(),
        })
    }

    async fn start(&self, request: QuickRunRequest) -> ApiResult<QuickRunStatus> {
        let _start = self.start_lock.lock().await;
        let dispatch = self.dispatch()?.clone();
        if request.request_id.is_empty() || request.request_id.len() > 100 {
            return Err(invalid("Quick Run needs a valid request ID."));
        }
        // A lost HTTP response must not duplicate watering, including after a restart.
        if let Some(saved) = self.read_saved(Some(request.request_id.clone())).await? {
            if saved.zones != request.zones {
                return Err(conflict(
                    "This request ID belongs to a different Quick Run.",
                ));
            }
            let current = self.current.lock().await;
            return Ok(current
                .as_ref()
                .filter(|r| r.id == saved.id)
                .cloned()
                .unwrap_or_else(|| Self::interrupted(saved)));
        }
        if request.zones.is_empty() || request.zones.len() > 128 {
            return Err(invalid("Choose at least one zone (up to 128)."));
        }
        let snapshot = self.snapshot.snapshot();
        let policy = dispatch.policy.load_full();
        let mut seen = HashSet::new();
        let mut total = 0u64;
        let mut plan = Vec::new();
        let mut names = Vec::new();
        for choice in &request.zones {
            if !seen.insert(choice.zone.clone()) {
                return Err(invalid("Each zone can appear only once in a Quick Run."));
            }
            let zone = snapshot
                .zones
                .iter()
                .find(|z| z.slug == choice.zone)
                .ok_or_else(|| {
                    invalid("A selected zone no longer exists. Refresh the zone list.")
                })?;
            let cap = policy
                .zone_runtime
                .get(&choice.zone)
                .map(|z| z.max_duration_s)
                .unwrap_or(3600)
                .min(7200);
            let controller = match policy.controller_id_for(&choice.zone) {
                Some(id) => dispatch.registry.get(id),
                None => dispatch.registry.default(),
            }
            .ok_or_else(|| {
                invalid(format!(
                    "{} has no available controller. Check its connection in Settings.",
                    zone.name
                ))
            })?;
            let mapped = controller.mapped_zone_slugs();
            if !mapped.is_empty() && !mapped.contains(&choice.zone) {
                return Err(invalid(format!(
                    "{} needs a controller station in Settings.",
                    zone.name
                )));
            }
            let quantum = controller.supports().duration_quantum_s.max(1);
            let rounded = (choice.seconds as u64).div_ceil(quantum as u64) * quantum as u64;
            if choice.seconds == 0 || rounded > cap as u64 {
                return Err(invalid(format!(
                    "{} allows up to {} minutes per run.",
                    zone.name,
                    cap / 60
                )));
            }
            total += rounded;
            plan.push(controller);
            names.push(zone.name.clone());
        }
        if total > 21_600 {
            return Err(invalid("Keep a Quick Run within 6 hours total."));
        }
        let locks = dispatch.registry.zone_locks();
        let order = locks.command_order();
        let _exclusive = order.write().await;
        if locks.restart_hold().is_pending() {
            return Err(conflict(
                "Restart LocalSky to finish applying the controller changes before watering.",
            ));
        }
        let id = crate::persistence::watering_commands::new_session_id();
        let reservation = locks.manual_session.reserve(id.clone()).ok_or_else(|| {
            conflict("A Quick Run is already active. Open it to see progress or stop it.")
        })?;
        if snapshot.zones.iter().any(|z| z.is_running_or_unconfirmed())
            || !dispatch
                .active_runs
                .as_ref()
                .unwrap()
                .armed()
                .await
                .map_err(storage_error)?
                .is_empty()
        {
            return Err(conflict(
                "Watering is already active. Stop it before starting a Quick Run.",
            ));
        }
        let mut checked = HashSet::new();
        for controller in &plan {
            if !checked.insert(controller.id().to_string()) {
                continue;
            }
            match tokio::time::timeout(Duration::from_secs(15), controller.status()).await {
                Ok(Ok(status))
                    if status.reachable && !status.zone_states.iter().any(|z| z.running) => {}
                Ok(Err(ControllerError::Unsupported(_))) => {} // command-only adapters have no live readback
                Ok(Ok(status)) if status.zone_states.iter().any(|z| z.running) => {
                    return Err(conflict(
                        "The controller is watering. Stop it before starting a Quick Run.",
                    ))
                }
                _ => {
                    return Err(conflict(
                        "The controller is unavailable. Check its connection and try again.",
                    ))
                }
            }
        }
        let run = QuickRunStatus {
            id,
            request_id: request.request_id,
            phase: QuickRunPhase::Starting,
            zones: request.zones,
            names,
            completed: 0,
            current: None,
            current_ends_epoch: None,
            started_epoch: chrono::Utc::now().timestamp(),
            message: "Zones run one at a time. You can close this page.".into(),
            stop_unconfirmed: false,
        };
        self.save(&run).await?;
        *self.current.lock().await = Some(run.clone());
        let manager = self.clone();
        let queued = run.clone();
        // Guard release on every return/panic; the durable deadline remains the backstop.
        tokio::spawn(async move {
            manager.execute(dispatch, queued, plan, reservation).await;
        });
        Ok(run)
    }

    async fn publish(&self, run: &QuickRunStatus) -> ApiResult<()> {
        self.save(run).await?;
        *self.current.lock().await = Some(run.clone());
        Ok(())
    }

    async fn execute(
        &self,
        dispatch: DispatchState,
        mut run: QuickRunStatus,
        plan: Vec<Arc<dyn IrrigationController>>,
        reservation: Reservation,
    ) {
        let executor = Dispatcher::new(
            dispatch.registry.zone_locks(),
            dispatch.runs.as_ref(),
            dispatch.active_runs.as_ref(),
        );
        for (index, controller) in plan.iter().enumerate() {
            if reservation.session.cancelled() {
                run.phase = QuickRunPhase::Stopped;
                break;
            }
            let choice = run.zones[index].clone();
            run.current = Some(index);
            run.phase = QuickRunPhase::Starting;
            run.message = format!("Starting {}…", run.names[index]);
            if self.publish(&run).await.is_err() {
                run.phase = QuickRunPhase::Failed;
                run.message =
                    "Quick Run could not save progress. Remaining zones were cancelled.".into();
                break;
            }
            // Recheck hot bindings/limits. Never move a saved queue onto different hardware.
            let policy = dispatch.policy.load_full();
            let binding = policy
                .controller_id_for(&choice.zone)
                .map(str::to_owned)
                .or_else(|| dispatch.registry.default().map(|c| c.id().to_string()));
            let cap = policy
                .zone_runtime
                .get(&choice.zone)
                .map(|z| z.max_duration_s)
                .unwrap_or(3600)
                .min(7200);
            let quantum = controller.supports().duration_quantum_s.max(1);
            let requested = choice.seconds.div_ceil(quantum) * quantum;
            if binding.as_deref() != Some(controller.id())
                || !dispatch
                    .registry
                    .get(controller.id())
                    .is_some_and(|live| Arc::ptr_eq(&live, controller))
                || !self
                    .snapshot
                    .snapshot()
                    .zones
                    .iter()
                    .any(|z| z.slug == choice.zone)
                || requested > cap
            {
                run.phase = QuickRunPhase::Failed;
                run.message = "Zone settings changed. Remaining zones were cancelled; review them before starting again.".into();
                break;
            }
            let now = chrono::Utc::now().timestamp();
            let outcome = tokio::time::timeout(
                Duration::from_secs(30),
                executor.run(RunRequest {
                    session_id: run.id.clone(),
                    zone: &choice.zone,
                    zone_name: &run.names[index],
                    controller,
                    seconds: requested,
                    source: Source::Manual,
                    ceiling_s: Some(dispatch::ceiling_for_zone(&policy, &choice.zone)),
                    arm: Arm::BeforeDispatch {
                        deadline: Some(dispatch::run_deadline(
                            now,
                            requested,
                            controller.supports().per_zone_stop,
                        )),
                        disarm_on_failure: false,
                    },
                    record_row: false,
                    cycle: None,
                    push: None,
                    now_epoch: now,
                }),
            )
            .await;
            let mut failure = match outcome {
                Ok(RunOutcome::Dispatched { handle, seconds, deadline_armed: true }) => {
                    run.phase = QuickRunPhase::Running;
                    run.current_ends_epoch = Some(handle.started_epoch + seconds as i64);
                    run.message = if seconds < requested { "Run time shortened by the daily watering limit.".into() } else { "You can close this page. Quick Run will continue.".into() };
                    if self.publish(&run).await.is_err() { Some("Could not save run progress. Remaining zones were cancelled.".to_string()) } else {
                        let remaining = (handle.started_epoch + seconds as i64 - chrono::Utc::now().timestamp()).clamp(0, seconds as i64) as u64;
                        let end = tokio::time::Instant::now() + Duration::from_secs(remaining);
                        while tokio::time::Instant::now() < end && !reservation.session.cancelled() {
                            tokio::time::sleep_until(end.min(tokio::time::Instant::now() + Duration::from_millis(250))).await;
                        }
                        None
                    }
                },
                Ok(RunOutcome::Refused(_)) => Some("The daily watering limit was reached. Remaining zones were cancelled.".into()),
                Ok(RunOutcome::Failed { error: ControllerError::Held(_), .. }) if reservation.session.cancelled() => None,
                Ok(RunOutcome::Failed { error, .. }) => Some(format!("{} could not start: {error}. Remaining zones were cancelled.", run.names[index])),
                Ok(RunOutcome::Dispatched { .. }) => Some("The shutoff backstop could not be saved. Remaining zones were cancelled.".into()),
                Err(_) => Some("The controller did not confirm the run command. Remaining zones were cancelled.".into()),
            };
            // Also stop after a timeout/ambiguous dispatch. Never open the next
            // valve until the current stop is acknowledged; retain failed deadlines.
            run.phase = if reservation.session.cancelled() {
                QuickRunPhase::Stopping
            } else {
                QuickRunPhase::Finishing
            };
            let _ = self.publish(&run).await;
            match tokio::time::timeout(
                Duration::from_secs(30),
                executor.stop(controller, &choice.zone, chrono::Utc::now().timestamp()),
            )
            .await
            {
                Ok(Ok(_)) => {}
                _ => {
                    run.stop_unconfirmed = true;
                    failure = Some("Stop was not confirmed. Check the controller or retry Stop all watering. Remaining zones were cancelled.".into());
                }
            }
            if let Some(message) = failure {
                run.phase = QuickRunPhase::Failed;
                run.message = message;
                break;
            }
            if reservation.session.cancelled() {
                run.phase = QuickRunPhase::Stopped;
                break;
            }
            run.completed += 1;
            run.current = None;
            run.current_ends_epoch = None;
            run.phase = QuickRunPhase::Finished;
        }
        run.current_ends_epoch = None;
        if run.phase == QuickRunPhase::Finished {
            run.message =
                "All selected run times finished. View History for reported watering.".into();
        }
        if run.phase == QuickRunPhase::Stopped {
            run.message = "Quick Run stopped. Remaining zones will not start.".into();
        }
        if self.publish(&run).await.is_err() {
            run.phase = QuickRunPhase::Failed;
            run.message = "Quick Run ended, but its final status could not be saved. Check current watering before starting again.".into();
            *self.current.lock().await = Some(run);
        }
    }
}

async fn status(State(manager): State<Manager>) -> ApiResult<Json<QuickRunView>> {
    manager.view().await.map(Json)
}
async fn start(
    State(manager): State<Manager>,
    Json(request): Json<QuickRunRequest>,
) -> ApiResult<Json<QuickRunStatus>> {
    manager.start(request).await.map(Json)
}
#[derive(serde::Deserialize)]
struct StopRequest {
    id: String,
}
async fn stop(
    State(manager): State<Manager>,
    Json(request): Json<StopRequest>,
) -> ApiResult<Json<QuickRunStatus>> {
    let _start = manager.start_lock.lock().await;
    let dispatch = manager.dispatch()?;
    let mut run = manager
        .current
        .lock()
        .await
        .as_ref()
        .filter(|r| r.id == request.id)
        .cloned()
        .ok_or_else(|| conflict("This Quick Run is no longer active. Refresh its status."))?;
    if run.stop_unconfirmed {
        crate::scheduler::dispatch_gate::request_stop();
        let executor = Dispatcher::new(
            dispatch.registry.zone_locks(),
            dispatch.runs.as_ref(),
            dispatch.active_runs.as_ref(),
        );
        let report = tokio::time::timeout(
            Duration::from_secs(30),
            executor.stop_all(&dispatch.registry, chrono::Utc::now().timestamp()),
        )
        .await;
        if report.is_ok_and(|report| !report.confirmed.is_empty() && report.failed.is_empty()) {
            run.stop_unconfirmed = false;
            run.phase = QuickRunPhase::Stopped;
            run.message =
                "All controllers acknowledged Stop. Remaining zones will not start.".into();
            manager.publish(&run).await?;
        }
        return Ok(Json(run));
    }
    if run.phase.active()
        && dispatch
            .registry
            .zone_locks()
            .manual_session
            .cancel(&request.id)
    {
        run.phase = QuickRunPhase::Stopping;
        run.message = "Stopping the current zone and cancelling the rest…".into();
    }
    Ok(Json(run))
}

#[cfg(test)]
mod tests;
