// Irrigation API endpoints. Mirrors the Tempest API exactly for reads:
// a JSON snapshot and an SSE stream. Adds POST /action for the
// dashboard's interactive controls (zone runs, stops, threshold edits,
// vacation pause). Action handlers translate UI intent into HA
// service-call POSTs via HaClient.
//
// Mounted at /api/irrigation/* by api::router.

use crate::ha::rest::HaClient;
use crate::ha::IrrigationStore;
use crate::history::db;
use crate::llm::{AdvisorError, AdvisorState};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::{get, post},
    Router,
};
use futures::stream::Stream;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

pub fn router(
    store: Arc<IrrigationStore>,
    advisor: AdvisorState,
    history: Option<Arc<Mutex<Connection>>>,
) -> Router {
    let read_routes = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/stream", get(stream))
        .route("/action", post(action))
        .with_state(store.clone());

    let advisor_routes = Router::new()
        .route("/explanation", get(explanation))
        .route("/anomalies", get(anomalies))
        .with_state(AdvisorRouterState {
            store: store.clone(),
            advisor,
        });

    let merged = read_routes.merge(advisor_routes);

    if let Some(h) = history {
        merged.merge(
            Router::new()
                .route("/history", get(history_window))
                .route("/decisions", get(decisions_window))
                .with_state(h),
        )
    } else {
        merged
    }
}

/// Advisor endpoints need both the IrrigationStore (for the live
/// snapshot we hand to the LLM) and the AdvisorState (client +
/// caches). Bundle them so axum's typed-state extraction works.
#[derive(Clone)]
struct AdvisorRouterState {
    store: Arc<IrrigationStore>,
    advisor: AdvisorState,
}

#[derive(Serialize)]
struct AdvisorEnvelope<T: Serialize> {
    /// "ok" / "offline" / "disabled".
    status: &'static str,
    /// Present when status == "ok".
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    /// Coarse error tag when not ok. Surfaces in the dashboard so the
    /// tile can render the right "advisor offline" copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

impl<T: Serialize> AdvisorEnvelope<T> {
    fn ok(data: T) -> Self {
        Self { status: "ok", data: Some(data), error: None }
    }
    fn from_err(e: AdvisorError) -> Self {
        let (status, error) = match e {
            AdvisorError::Disabled => ("disabled", "disabled"),
            AdvisorError::Offline => ("offline", "offline"),
        };
        Self { status, data: None, error: Some(error) }
    }
}

async fn explanation(
    State(state): State<AdvisorRouterState>,
) -> impl IntoResponse {
    let snap = (*state.store.snapshot()).clone();
    match state.advisor.explain_today(&snap).await {
        Ok(text) => (
            StatusCode::OK,
            Json(serde_json::to_value(AdvisorEnvelope::ok(text)).unwrap()),
        ),
        Err(e) => (
            StatusCode::OK, // 200 with envelope so dashboard fetch succeeds
            Json(serde_json::to_value(AdvisorEnvelope::<String>::from_err(e)).unwrap()),
        ),
    }
}

async fn anomalies(
    State(state): State<AdvisorRouterState>,
) -> impl IntoResponse {
    let snap = (*state.store.snapshot()).clone();
    match state.advisor.detect_anomalies(&snap).await {
        Ok(list) => (
            StatusCode::OK,
            Json(serde_json::to_value(AdvisorEnvelope::ok(list)).unwrap()),
        ),
        Err(e) => (
            StatusCode::OK,
            Json(serde_json::to_value(AdvisorEnvelope::<Vec<crate::llm::Anomaly>>::from_err(e)).unwrap()),
        ),
    }
}

async fn snapshot(
    State(store): State<Arc<IrrigationStore>>,
) -> Json<crate::ha::snapshot::IrrigationSnapshot> {
    let s = store.snapshot();
    Json((*s).clone())
}

async fn stream(
    State(store): State<Arc<IrrigationStore>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = store.subscribe();
    let s = WatchStream::new(rx).map(|snap| {
        let payload = serde_json::to_string(&*snap).unwrap_or_else(|_| "{}".into());
        Ok(Event::default().event("snapshot").data(payload))
    });
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Browser → server action vocabulary. Tagged enum keeps the JSON
/// payload self-describing and lets the handler route via match.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Run a single zone for `seconds`. Maps to the existing
    /// `script.os_zone_toggle` script (toggles run/stop based on
    /// current running state) so we don't double-fire if the zone is
    /// already wet. Server clamps to <=7200s (2 hours) defensively;
    /// the mobile UI also caps at 120 min, but a buggy client or a
    /// hostile request shouldn't be able to leak large durations into
    /// the lawn.
    Run { zone: String, seconds: u32 },
    /// Stop a single zone immediately.
    Stop { zone: String },
    /// Stop all four zones in parallel.
    StopAll,
    /// Update a threshold slider (max_wind_mph / min_temp_f /
    /// rain_skip_in). Persists in HA via input_number.set_value.
    SetThreshold { key: String, value: f64 },
    /// Toggle one of the input_booleans (irrigation_pause /
    /// irrigation_dry_run).
    Toggle { key: String, on: bool },
    /// Set the vacation-pause expiry to a UTC epoch. Honored by
    /// skip_logic::evaluate as a hard skip until the timestamp passes.
    /// Pass 0 to clear (or use ClearPauseUntil).
    /// Requires HA helper: input_datetime.irrigation_pause_until.
    SetPauseUntil { epoch: i64 },
    /// Convenience: clears the pause-until.
    ClearPauseUntil,
    /// One-day override for tomorrow's verdict. mode = "none" | "skip" | "run".
    /// "none" returns to skip_logic auto. HA midnight automation should
    /// reset this to "none" each day.
    /// Requires HA helper: input_select.irrigation_override_tomorrow.
    SetOverrideTomorrow { mode: String },
    /// Trigger Irrigation Unlimited's full sequence immediately, bypassing
    /// the skip-check. Maps to irrigation_unlimited.run_now on the c1_s1
    /// sequence entity.
    RunSequenceNow,
}

/// Map a zone slug to the binary_sensor.*_station_running entity ID
/// that opensprinkler / script.os_zone_toggle expects. Anchored to
/// the four physical stations; unknown slugs return None and the
/// handler returns 400.
fn running_sensor(zone: &str) -> Option<String> {
    match zone {
        "back_yard" | "front_yard" | "side_yard" | "back_yard_shrubs" => {
            Some(format!("binary_sensor.aperture_sprinklers_{zone}_station_running"))
        }
        _ => None,
    }
}

/// Map a threshold key to the input_number entity ID. Restricts to
/// the three known sliders so a hostile client can't poke arbitrary
/// HA inputs through this endpoint.
fn threshold_entity(key: &str) -> Option<String> {
    match key {
        "max_wind_mph" | "min_temp_f" | "rain_skip_in" => {
            Some(format!("input_number.irrigation_{key}"))
        }
        _ => None,
    }
}

/// Map a toggle key to the input_boolean entity ID, with the same
/// allow-list shape.
fn toggle_entity(key: &str) -> Option<String> {
    match key {
        "irrigation_pause" | "irrigation_dry_run" => {
            Some(format!("input_boolean.{key}"))
        }
        _ => None,
    }
}

/// Defensive cap on Action::Run duration. The mobile UI caps at 120 min;
/// the server clamps at the same level so a buggy client or hostile
/// request can't drown the lawn.
const RUN_SECONDS_MAX: u32 = 7200;

/// HA entity for the vacation-pause expiry helper. Created manually in
/// HA's helpers UI; documented in .agent/agent.md.
const PAUSE_UNTIL_ENTITY: &str = "input_datetime.irrigation_pause_until";

/// HA entity for the one-day override (none/skip/run).
const OVERRIDE_ENTITY: &str = "input_select.irrigation_override_tomorrow";

/// IU sequence binary_sensor used as the target for run_now.
const IU_SEQUENCE_ENTITY: &str = "binary_sensor.irrigation_unlimited_c1_s1";

async fn action(
    State(_store): State<Arc<IrrigationStore>>,
    Json(body): Json<Action>,
) -> impl IntoResponse {
    let client = match HaClient::from_env() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("ha client init failed: {e}") })),
            );
        }
    };

    let result: Result<Value, String> = match body {
        Action::Run { zone, seconds } => {
            let Some(eid) = running_sensor(&zone) else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown zone: {zone}") })),
                );
            };
            let clamped = seconds.min(RUN_SECONDS_MAX).max(1);
            if clamped != seconds {
                tracing::warn!(
                    "irrigation::Run clamped seconds {} -> {} (max {})",
                    seconds, clamped, RUN_SECONDS_MAX
                );
            }
            client
                .call_service(
                    "script",
                    "os_zone_toggle",
                    &json!({ "station_entity": eid, "duration": clamped }),
                )
                .await
                .map(|_| json!({ "ok": true, "fired": "script.os_zone_toggle", "zone": zone, "seconds": clamped }))
                .map_err(|e| e.to_string())
        }
        Action::Stop { zone } => {
            let Some(eid) = running_sensor(&zone) else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown zone: {zone}") })),
                );
            };
            client
                .call_service("opensprinkler", "stop", &json!({ "entity_id": eid }))
                .await
                .map(|_| json!({ "ok": true, "fired": "opensprinkler.stop", "zone": zone }))
                .map_err(|e| e.to_string())
        }
        Action::StopAll => client
            .call_service("script", "os_stop_all", &json!({}))
            .await
            .map(|_| json!({ "ok": true, "fired": "script.os_stop_all" }))
            .map_err(|e| e.to_string()),
        Action::SetThreshold { key, value } => {
            let Some(eid) = threshold_entity(&key) else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown threshold: {key}") })),
                );
            };
            client
                .call_service(
                    "input_number",
                    "set_value",
                    &json!({ "entity_id": eid, "value": value }),
                )
                .await
                .map(|_| json!({ "ok": true, "fired": "input_number.set_value", "key": key, "value": value }))
                .map_err(|e| e.to_string())
        }
        Action::Toggle { key, on } => {
            let Some(eid) = toggle_entity(&key) else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown toggle: {key}") })),
                );
            };
            let service = if on { "turn_on" } else { "turn_off" };
            client
                .call_service("input_boolean", service, &json!({ "entity_id": eid }))
                .await
                .map(|_| json!({ "ok": true, "fired": format!("input_boolean.{service}"), "key": key }))
                .map_err(|e| e.to_string())
        }
        Action::SetPauseUntil { epoch } => {
            // input_datetime.set_datetime accepts a `timestamp` field
            // (UTC epoch seconds). HA stores has_date+has_time and the
            // helper renders in the local timezone. epoch <= 0 clears.
            if epoch <= 0 {
                client
                    .call_service(
                        "input_datetime",
                        "set_datetime",
                        &json!({ "entity_id": PAUSE_UNTIL_ENTITY, "timestamp": 0 }),
                    )
                    .await
                    .map(|_| json!({ "ok": true, "fired": "input_datetime.set_datetime", "cleared": true }))
                    .map_err(|e| e.to_string())
            } else {
                client
                    .call_service(
                        "input_datetime",
                        "set_datetime",
                        &json!({ "entity_id": PAUSE_UNTIL_ENTITY, "timestamp": epoch }),
                    )
                    .await
                    .map(|_| json!({ "ok": true, "fired": "input_datetime.set_datetime", "epoch": epoch }))
                    .map_err(|e| e.to_string())
            }
        }
        Action::ClearPauseUntil => client
            .call_service(
                "input_datetime",
                "set_datetime",
                &json!({ "entity_id": PAUSE_UNTIL_ENTITY, "timestamp": 0 }),
            )
            .await
            .map(|_| json!({ "ok": true, "fired": "input_datetime.set_datetime", "cleared": true }))
            .map_err(|e| e.to_string()),
        Action::SetOverrideTomorrow { mode } => {
            let opt = match mode.as_str() {
                "none" | "skip" | "run" => mode.clone(),
                _ => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("invalid override mode: {mode}") })),
                    );
                }
            };
            client
                .call_service(
                    "input_select",
                    "select_option",
                    &json!({ "entity_id": OVERRIDE_ENTITY, "option": opt }),
                )
                .await
                .map(|_| json!({ "ok": true, "fired": "input_select.select_option", "mode": mode }))
                .map_err(|e| e.to_string())
        }
        Action::RunSequenceNow => client
            .call_service(
                "irrigation_unlimited",
                "run_now",
                &json!({ "entity_id": IU_SEQUENCE_ENTITY }),
            )
            .await
            .map(|_| json!({ "ok": true, "fired": "irrigation_unlimited.run_now" }))
            .map_err(|e| e.to_string()),
    };

    match result {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e })),
        ),
    }
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    /// Window size in days, counted backward from now. Caps at 365 to
    /// keep the SVG Gantt renderable on phones.
    #[serde(default = "default_days")]
    days: u32,
}

fn default_days() -> u32 {
    30
}

async fn history_window(
    State(conn): State<Arc<Mutex<Connection>>>,
    Query(q): Query<HistoryQuery>,
) -> impl IntoResponse {
    let days = q.days.clamp(1, 365);
    let now = chrono::Utc::now().timestamp();
    let from = now - (days as i64) * 86400;
    match db::window(conn, from, now).await {
        Ok(w) => (StatusCode::OK, Json(serde_json::to_value(w).unwrap_or_default())),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

async fn decisions_window(
    State(conn): State<Arc<Mutex<Connection>>>,
    Query(q): Query<HistoryQuery>,
) -> impl IntoResponse {
    let days = q.days.clamp(1, 365);
    let now = chrono::Utc::now().timestamp();
    let from = now - (days as i64) * 86400;
    match db::decisions_window(conn, from, now).await {
        Ok(w) => (StatusCode::OK, Json(serde_json::to_value(w).unwrap_or_default())),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}
