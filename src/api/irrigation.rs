// Irrigation API endpoints. Mirrors the Tempest API exactly for reads:
// a JSON snapshot and an SSE stream. Adds POST /action for the
// dashboard's interactive controls (zone runs, stops, threshold edits,
// vacation pause).
//
// Zone Run/Stop/StopAll dispatch through the ControllerRegistry (the
// same adapters the scheduler uses) whenever the deploy is native OR a
// default controller is configured; only legacy HA deploys with no
// configured controllers fall back to HA service calls against the
// public opensprinkler integration (prefix-driven, no private scripts).
//
// Mounted at /api/irrigation/* by api::router.

use crate::config::schema::SkipRuleParams;
use crate::controllers::registry::ControllerRegistry;
use crate::ha::rest::HaClient;
use crate::ha::{IrrigationStore, SnapshotSource};
use crate::history::db;
use crate::llm::{AdvisorError, AdvisorState};
use crate::persistence::runs::{NewRun, RunsStore};
use crate::persistence::IrrigationControlStore;
use crate::ports::irrigation_controller::ControllerError;
use crate::scheduler::dispatch_gate;
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
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

/// Optional shadow store: when shadow mode is on, the native snapshot
/// builder writes here each tick (alongside the authoritative HA store) so
/// it can be compared without ever driving dispatch. Set once at boot.
static SHADOW_STORE: std::sync::OnceLock<Arc<IrrigationStore>> = std::sync::OnceLock::new();

/// Register the shadow store (called from main at boot when shadow_native).
pub fn set_shadow_store(s: Arc<IrrigationStore>) {
    let _ = SHADOW_STORE.set(s);
}

/// Dispatch plumbing for POST /action zone controls: the controller
/// registry (hot-swappable; same instance the schedulers use) plus the
/// runs store for recording manual runs. Set once at boot from main.rs.
/// Unset (demo mode, or boot before wiring) means the registry route
/// answers 503 rather than guessing at HA scripts.
struct DispatchHandles {
    registry: ControllerRegistry,
    runs: Option<RunsStore>,
    /// Deadline ledger (P0-1b): a manual Run arms a persisted shutoff deadline so
    /// the reaper closes the valve even if this process dies before its timer.
    active_runs: Option<crate::persistence::ActiveRunsStore>,
}

static DISPATCH: std::sync::OnceLock<DispatchHandles> = std::sync::OnceLock::new();

/// Register the controller registry + runs store + active-run ledger for manual
/// zone dispatch (called from main at boot).
pub fn set_dispatch_handles(
    registry: ControllerRegistry,
    runs: Option<RunsStore>,
    active_runs: Option<crate::persistence::ActiveRunsStore>,
) {
    let _ = DISPATCH.set(DispatchHandles {
        registry,
        runs,
        active_runs,
    });
}

/// Configured engine skip-rule thresholds for the What-If simulator,
/// from `cfg.engine.skip_rules` (called from main at boot). Unset falls
/// back to SkipRuleParams::default(), which equals an untouched config.
static SIM_SKIP_PARAMS: std::sync::OnceLock<SkipRuleParams> = std::sync::OnceLock::new();

/// Register the configured skip params used by POST /simulate.
pub fn set_sim_skip_params(params: SkipRuleParams) {
    let _ = SIM_SKIP_PARAMS.set(params);
}

pub fn router(
    store: Arc<IrrigationStore>,
    advisor: AdvisorState,
    history: Option<Arc<Mutex<Connection>>>,
    source: SnapshotSource,
    sprinkler_prefix: String,
) -> Router {
    // POST /action needs the snapshot source + (for native) the local
    // control store, so it lives in its own sub-router with that state.
    let action_router = Router::new()
        .route("/action", post(action))
        .with_state(ActionState {
            source,
            control: history.clone().map(IrrigationControlStore::new),
            sprinkler_prefix,
        });

    let read_routes = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/stream", get(stream))
        .route("/simulate", post(simulate))
        // Shadow mode: the native (standalone) snapshot built alongside the
        // HA one for comparison. Empty unless shadow_native is enabled.
        .route("/shadow/snapshot", get(shadow_snapshot))
        .route("/shadow/diff", get(shadow_diff))
        .with_state(store.clone());

    let advisor_routes = Router::new()
        .route("/explanation", get(explanation))
        .route("/anomalies", get(anomalies))
        .with_state(AdvisorRouterState {
            store: store.clone(),
            advisor,
        });

    let merged = read_routes.merge(advisor_routes).merge(action_router);

    if let Some(h) = history {
        merged.merge(
            Router::new()
                .route("/history", get(history_window))
                .route("/decisions", get(decisions_window))
                .route("/export", get(export))
                .route("/accuracy", get(accuracy))
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
        Self {
            status: "ok",
            data: Some(data),
            error: None,
        }
    }
    fn from_err(e: AdvisorError) -> Self {
        let (status, error) = match e {
            AdvisorError::Disabled => ("disabled", "disabled"),
            AdvisorError::Offline => ("offline", "offline"),
        };
        Self {
            status,
            data: None,
            error: Some(error),
        }
    }
}

async fn explanation(State(state): State<AdvisorRouterState>) -> impl IntoResponse {
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

async fn anomalies(State(state): State<AdvisorRouterState>) -> impl IntoResponse {
    let snap = (*state.store.snapshot()).clone();
    match state.advisor.detect_anomalies(&snap).await {
        Ok(list) => (
            StatusCode::OK,
            Json(serde_json::to_value(AdvisorEnvelope::ok(list)).unwrap()),
        ),
        Err(e) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(AdvisorEnvelope::<Vec<crate::llm::Anomaly>>::from_err(e))
                    .unwrap(),
            ),
        ),
    }
}

async fn snapshot(
    State(store): State<Arc<IrrigationStore>>,
) -> Json<crate::ha::snapshot::IrrigationSnapshot> {
    let s = store.snapshot();
    Json((*s).clone())
}

/// The native (standalone) snapshot built in shadow alongside HA. Returns
/// `{"shadow":"disabled"}` when shadow mode is off.
async fn shadow_snapshot() -> Json<Value> {
    match SHADOW_STORE.get() {
        Some(s) => Json(serde_json::to_value(&*s.snapshot()).unwrap_or(Value::Null)),
        None => Json(json!({ "shadow": "disabled" })),
    }
}

/// Side-by-side diff of the authoritative (HA) snapshot vs the native
/// shadow: the aggregate verdict, and per-zone running + planned-seconds.
/// The planned-seconds delta is expected (native budget vs SI bucket) and
/// is shown for both so it can be judged; verdict + running mismatches are
/// the signal that native isn't yet equivalent.
async fn shadow_diff(State(live): State<Arc<IrrigationStore>>) -> Json<Value> {
    let Some(shadow) = SHADOW_STORE.get() else {
        return Json(json!({ "shadow": "disabled" }));
    };
    let h = live.snapshot();
    let n = shadow.snapshot();
    let zones: Vec<Value> = h
        .zones
        .iter()
        .map(|hz| {
            let nz = n.zones.iter().find(|z| z.slug == hz.slug);
            json!({
                "slug": hz.slug,
                "ha_running": hz.running,
                "native_running": nz.map(|z| z.running),
                "native_running_known": nz.map(|z| z.running_known),
                "ha_planned_s": hz.planned_run_seconds,
                "native_planned_s": nz.map(|z| z.planned_run_seconds),
                "ha_verdict": hz.verdict.as_ref().map(|v| v.verdict.clone()),
                "native_verdict": nz.and_then(|z| z.verdict.as_ref().map(|v| v.verdict.clone())),
            })
        })
        .collect();
    Json(json!({
        "ha_verdict": h.skip_check.verdict,
        "native_verdict": n.skip_check.verdict,
        "verdict_match": h.skip_check.verdict == n.skip_check.verdict,
        "ha_reason": h.skip_check.reason,
        "native_reason": n.skip_check.reason,
        "ha_master_enable": h.master_enable,
        "native_master_enable": n.master_enable,
        "zones": zones,
    }))
}

/// What-If: seed engine Inputs from the live SkipCheck, override the
/// Some fields from the request, re-run the EXACT production ladder
/// (`decide_traced`) on baseline + hypothetical, return both traces.
/// Pure read, writes nothing.
async fn simulate(
    State(store): State<Arc<IrrigationStore>>,
    Json(req): Json<crate::ha::snapshot::SimRequest>,
) -> Json<crate::ha::snapshot::SimResult> {
    use crate::engine::skip_rules::{decide_traced, inputs_from_skipcheck};

    let snap = store.snapshot();
    let base = inputs_from_skipcheck(&snap.skip_check);
    let mut hypo = base.clone();
    if let Some(v) = req.temp_now_f {
        hypo.temp_now_f = v;
    }
    if let Some(v) = req.humidity_now_pct {
        hypo.humidity_now_pct = v;
    }
    if let Some(v) = req.wind_now_mph {
        hypo.wind_now_mph = v;
    }
    if let Some(v) = req.rain_today_in {
        hypo.rain_today_in = v;
    }
    if let Some(v) = req.rain_intensity_now_in_hr {
        hypo.rain_intensity_now_in_hr = v;
    }
    if let Some(v) = req.forecast_in {
        hypo.forecast_in = v;
    }
    if let Some(v) = req.rain_tomorrow_prob_pct {
        hypo.rain_tomorrow_prob_pct = v;
    }
    if let Some(v) = req.rain_next_4h_in {
        hypo.rain_next_4h_in = v;
    }
    if let Some(v) = req.wind_max_today_mph {
        hypo.wind_max_today_mph = v;
    }
    if let Some(v) = req.temp_max_3day_f {
        hypo.temp_max_3day_f = v;
    }
    if let Some(v) = req.rain_3day_weighted_in {
        hypo.rain_3day_weighted_in = v;
    }

    // Use the operator's configured skip thresholds (set at boot from
    // cfg.engine.skip_rules) so the What-If traces match the production
    // ladder. Falls back to defaults, which equal an untouched config.
    let p = SIM_SKIP_PARAMS.get().cloned().unwrap_or_default();
    let baseline = decide_traced(&base, &p);
    let mut hypothetical = decide_traced(&hypo, &p);

    // Optional ad-hoc script test: augment-only, same boundary as the
    // live engine, only consulted when the hypothetical verdict is "run".
    if let Some(src) = req.test_script.as_ref().filter(|s| !s.trim().is_empty()) {
        if hypothetical.verdict == "run" {
            use crate::config::schema::ScriptRule;
            use crate::engine::scripting::CompiledScripts;
            let scripts = CompiledScripts::compile(&[ScriptRule {
                id: "test".into(),
                name: "Custom rule".into(),
                enabled: true,
                script: src.clone(),
            }]);
            if let Some(us) = scripts.apply_user_skip(&hypo) {
                hypothetical.verdict = "skip".into();
                hypothetical.reason = us.reason.clone();
                // P1: this custom test rule decided the hypothetical; mirror its id
                // into the trace's reason_code. The metric is user-defined, so no
                // canonical engine operands (value/threshold/unit_kind stay None).
                hypothetical.reason_code = us.id.clone();
                hypothetical.rules.push(crate::ha::snapshot::RuleEval {
                    id: us.id,
                    label: us.name,
                    category: "script".into(),
                    detail: "your test rule".into(),
                    outcome: "fired".into(),
                    verdict: Some("skip".into()),
                    margin_label: None,
                    value: None,
                    threshold: None,
                    unit_kind: None,
                });
            }
        }
    }

    Json(crate::ha::snapshot::SimResult {
        baseline,
        hypothetical,
    })
}

/// Live count of non-browser consumers on the irrigation stream plus
/// the epoch of the most recent connect. The Home Assistant integration
/// is the only steady-state non-Mozilla SSE client, so this doubles as
/// its liveness signal in /api/v1/health's `ha` block.
pub static INTEGRATION_STREAMS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
pub static LAST_INTEGRATION_STREAM_EPOCH: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

/// Decrements the live-stream gauge when the SSE connection drops (the
/// stream and its closures are dropped by axum on disconnect).
struct IntegrationStreamGuard;
impl Drop for IntegrationStreamGuard {
    fn drop(&mut self) {
        INTEGRATION_STREAMS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn stream(
    State(store): State<Arc<IrrigationStore>>,
    headers: axum::http::HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let is_integration = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| !ua.starts_with("Mozilla"))
        .unwrap_or(true);
    let guard = is_integration.then(|| {
        INTEGRATION_STREAMS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        LAST_INTEGRATION_STREAM_EPOCH.store(
            chrono::Utc::now().timestamp(),
            std::sync::atomic::Ordering::Relaxed,
        );
        IntegrationStreamGuard
    });
    let rx = store.subscribe();
    let s = WatchStream::new(rx).map(move |snap| {
        let _hold = &guard;
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
    /// Run a single zone for `seconds`. Dispatches through the
    /// ControllerRegistry's default controller (native deploys, or any
    /// deploy with configured controllers); legacy HA-only deploys fall
    /// back to the opensprinkler integration's `run` service. Server
    /// clamps to <=7200s (2 hours) defensively; the mobile UI also caps
    /// at 120 min, but a buggy client or a hostile request shouldn't be
    /// able to leak large durations into the lawn.
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
    /// Sticky global override (LocalSky-native; persists until changed, no
    /// nightly reset). mode = "auto" | "skip" | "run". "run" forces watering
    /// past the skip conditions; "skip" force-skips; "auto" follows the engine.
    SetGlobalOverride { mode: String },
    /// Sticky per-zone override. zone = slug, mode = "auto" | "skip" | "run".
    /// A zone override beats the global one; "auto" clears it so the zone
    /// falls back to the global override / engine verdict.
    SetZoneOverride { zone: String, mode: String },
    /// Tombstone: previously triggered Irrigation Unlimited's full
    /// sequence via irrigation_unlimited.run_now. IU support has been
    /// removed; the variant stays deserializable so stale clients get a
    /// clear 410 instead of a generic parse error.
    RunSequenceNow,
}

/// Map a zone slug to the binary_sensor.*_station_running entity ID
/// that opensprinkler / script.os_zone_toggle expects. Anchored to
/// the four physical stations; unknown slugs return None and the
/// handler returns 400.
fn running_sensor(zone: &str, prefix: &str) -> Option<String> {
    // Accept any safe slug (lowercase alnum + underscore) so the endpoint
    // works for any configured zone set, while still rejecting arbitrary
    // entity-id injection. `prefix` is the operator's controller entity
    // prefix (config-driven; default "opensprinkler").
    if zone.is_empty()
        || !zone
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    Some(format!("binary_sensor.{prefix}_{zone}_station_running"))
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
        "irrigation_pause" | "irrigation_dry_run" => Some(format!("input_boolean.{key}")),
        _ => None,
    }
}

/// Defensive cap on Action::Run duration. The mobile UI caps at 120 min;
/// the server clamps at the same level so a buggy client or hostile
/// request can't drown the lawn.
const RUN_SECONDS_MAX: u32 = 7200;

/// HA entity for the vacation-pause expiry helper. Created manually in
/// HA's helpers UI (an input_datetime named irrigation_pause_until).
const PAUSE_UNTIL_ENTITY: &str = "input_datetime.irrigation_pause_until";

/// HA entity for the one-day override (none/skip/run).
const OVERRIDE_ENTITY: &str = "input_select.irrigation_override_tomorrow";

/// State for the POST /action handler. The vacation pause + one-day
/// override are routed to local persisted state on a native (standalone)
/// deploy and to HA helpers on an HA deploy; everything else is HA-only.
#[derive(Clone)]
struct ActionState {
    source: SnapshotSource,
    /// Native control store. `None` when no persistence DB is mounted; a
    /// native pause/override write then returns 503 rather than silently
    /// dropping (a dropped pause = unexpected watering).
    control: Option<IrrigationControlStore>,
    /// HA controller entity prefix (config-driven; default "opensprinkler").
    sprinkler_prefix: String,
}

/// Handle the native (no-HA) vacation pause + one-day override by writing
/// local persisted state instead of calling HA helpers. Only reached for
/// the three control actions on a native deploy; all other actions stay on
/// the HA path.
async fn native_control_action(
    control: &Option<IrrigationControlStore>,
    body: Action,
) -> (StatusCode, Json<Value>) {
    let Some(cs) = control else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "control state unavailable (no persistence DB mounted)" })),
        );
    };
    let result: Result<Value, String> = match body {
        Action::SetPauseUntil { epoch } => {
            let epoch = epoch.max(0);
            cs.set_pause_until(epoch)
                .await
                .map(|_| json!({ "ok": true, "source": "native", "pause_until_epoch": epoch }))
                .map_err(|e| e.to_string())
        }
        Action::ClearPauseUntil => cs
            .set_pause_until(0)
            .await
            .map(|_| json!({ "ok": true, "source": "native", "cleared": true }))
            .map_err(|e| e.to_string()),
        Action::SetOverrideTomorrow { mode } => match mode.as_str() {
            "none" | "skip" | "run" => cs
                .set_override_tomorrow(mode.clone())
                .await
                .map(|_| json!({ "ok": true, "source": "native", "mode": mode }))
                .map_err(|e| e.to_string()),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid override mode: {mode}") })),
                );
            }
        },
        Action::SetGlobalOverride { mode } => match mode.as_str() {
            "auto" | "skip" | "run" => cs
                .set_global_override(mode.clone())
                .await
                .map(|_| json!({ "ok": true, "source": "native", "mode": mode }))
                .map_err(|e| e.to_string()),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid override mode: {mode}") })),
                );
            }
        },
        Action::SetZoneOverride { zone, mode } => {
            // Same slug allow-list as running_sensor: reject entity-id injection.
            let safe = !zone.is_empty()
                && zone
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            if !safe {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("invalid zone slug: {zone}") })),
                );
            }
            match mode.as_str() {
                "auto" | "skip" | "run" => cs
                    .set_zone_override(zone.clone(), mode.clone())
                    .await
                    .map(|_| json!({ "ok": true, "source": "native", "zone": zone, "mode": mode }))
                    .map_err(|e| e.to_string()),
                _ => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": format!("invalid override mode: {mode}") })),
                    );
                }
            }
        }
        // native_control_action is only called for the control variants; any
        // other variant is a programming error.
        _ => unreachable!("native_control_action called with non-control action"),
    };
    match result {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        ),
    }
}

/// Routing decision for zone Run/Stop/StopAll: dispatch through the
/// ControllerRegistry whenever the deploy is native OR a default
/// controller is configured (the registry adapters are what the
/// schedulers use, so manual taps behave exactly like scheduled runs).
/// Only legacy HA deploys with no configured controllers fall back to
/// HA service calls. Pure so the decision is unit-testable.
fn route_via_registry(source: SnapshotSource, has_default_controller: bool) -> bool {
    source == SnapshotSource::Native || has_default_controller
}

/// Map a ControllerError to an HTTP response for the action endpoint.
fn controller_error_response(e: ControllerError) -> (StatusCode, Json<Value>) {
    let status = match &e {
        ControllerError::ZoneUnknown(_) => StatusCode::BAD_REQUEST,
        ControllerError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": e.to_string() })))
}

/// Dispatch zone Run/Stop/StopAll through the registry's default
/// controller. Confirmed manual runs are recorded in the runs table
/// (source "manual") so the history Gantt and scheduler dedupe see
/// them. Only called with the three zone-action variants.
async fn registry_zone_action(body: Action) -> (StatusCode, Json<Value>) {
    let Some(d) = DISPATCH.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "error": "controller dispatch unavailable (controller registry not initialized; is the persistence DB mounted?)" }),
            ),
        );
    };
    let Some(controller) = d.registry.default() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "error": "no irrigation controller configured; add one in Settings (or localsky.toml [controllers]) to run zones" }),
            ),
        );
    };
    match body {
        Action::Run { zone, seconds } => {
            // Serialize concurrent Run actions on the SAME zone: two near-
            // simultaneous POSTs would otherwise both resolve the controller and
            // call run_zone, racing the hardware timer (last-writer-wins on OS/
            // HTTP, two shutoff timers on MQTT closing at the shorter duration) and
            // double-writing the manual run row. Held only for Run, and keyed by
            // zone, so a Stop / StopAll is never blocked behind a running zone.
            let run_lock = crate::controllers::zone_run_lock(&zone);
            let _run_serialize = run_lock.lock().await;
            let clamped = seconds.min(RUN_SECONDS_MAX).max(1);
            if clamped != seconds {
                tracing::warn!(
                    "irrigation::Run clamped seconds {} -> {} (max {})",
                    seconds,
                    clamped,
                    RUN_SECONDS_MAX
                );
            }
            match controller.run_zone(&zone, clamped).await {
                Ok(handle) => {
                    if let Some(rs) = d.runs.as_ref() {
                        let row = NewRun {
                            zone_slug: zone.clone(),
                            start_epoch: handle.started_epoch,
                            source: "manual".into(),
                            controller_id: handle.controller_id.clone(),
                            planned_duration_s: clamped,
                            skip_reason: None,
                            et0_mm: None,
                            etc_mm: None,
                            cycle_index: None,
                            cycle_count: None,
                        };
                        // The controller owns the shutoff timer, so end =
                        // start + duration matches what the hardware does.
                        if let Err(e) = rs
                            .insert_completed(
                                row,
                                handle.started_epoch + clamped as i64,
                                clamped,
                                None,
                            )
                            .await
                        {
                            tracing::warn!(zone = %zone, error = %e, "manual run row insert failed");
                        }
                    }
                    // P0-1b: arm the persisted shutoff deadline so the reaper
                    // closes this valve even if the process dies before the
                    // controller's own timer fires.
                    if let Some(ar) = d.active_runs.as_ref() {
                        if let Err(e) = ar
                            .arm(
                                zone.clone(),
                                handle.controller_id.clone(),
                                handle.started_epoch,
                                handle.started_epoch + clamped as i64,
                            )
                            .await
                        {
                            tracing::warn!(zone = %zone, error = %e, "active-run arm failed");
                        }
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "ok": true,
                            "dispatched": format!("controller:{}", handle.controller_id),
                            "zone": zone,
                            "seconds": clamped,
                        })),
                    )
                }
                Err(e) => controller_error_response(e),
            }
        }
        Action::Stop { zone } => match controller.stop_zone(&zone).await {
            Ok(()) => {
                // P0-1b: an explicit stop disarms the deadline so the reaper does
                // not later re-stop an already-closed valve.
                if let Some(ar) = d.active_runs.as_ref() {
                    let _ = ar.disarm(&zone).await;
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "dispatched": format!("controller:{}", controller.id()),
                        "stopped": zone,
                    })),
                )
            }
            Err(e) => controller_error_response(e),
        },
        Action::StopAll => match controller.stop_all().await {
            Ok(()) => {
                if let Some(ar) = d.active_runs.as_ref() {
                    let _ = ar.clear_all().await;
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "dispatched": format!("controller:{}", controller.id()),
                        "stopped": "all",
                    })),
                )
            }
            Err(e) => controller_error_response(e),
        },
        // Only the three zone-action variants reach this fn.
        _ => unreachable!("registry_zone_action called with non-zone action"),
    }
}

async fn action(State(st): State<ActionState>, Json(body): Json<Action>) -> impl IntoResponse {
    // Stop / Stop All / pause must also interrupt any in-flight
    // smart-morning sequence: flag the dispatch gate before routing so
    // the scheduler abandons remaining segments regardless of which
    // backend executes the stop.
    match &body {
        Action::Stop { .. } | Action::StopAll => dispatch_gate::request_stop(),
        Action::SetPauseUntil { epoch } if *epoch > chrono::Utc::now().timestamp() => {
            dispatch_gate::request_stop()
        }
        Action::Toggle { key, on } if key == "irrigation_pause" && *on => {
            dispatch_gate::request_stop()
        }
        _ => {}
    }

    // Zone run/stop dispatch through the controller registry (native
    // deploys, or any deploy with configured controllers).
    if matches!(
        body,
        Action::Run { .. } | Action::Stop { .. } | Action::StopAll
    ) {
        let has_default = DISPATCH
            .get()
            .map(|d| d.registry.default().is_some())
            .unwrap_or(false);
        if route_via_registry(st.source, has_default) {
            return registry_zone_action(body).await;
        }
    }

    // Sticky global/zone overrides are always LocalSky-native (their own
    // sqlite), independent of source: route them to local state whenever a
    // store is mounted (standalone, or HA mode with a persistence DB). This
    // is what makes the new override surface work even on an HA-source deploy.
    if st.control.is_some()
        && matches!(
            body,
            Action::SetGlobalOverride { .. } | Action::SetZoneOverride { .. }
        )
    {
        return native_control_action(&st.control, body).await;
    }

    // Native deploys have no HA helpers; the vacation pause + one-day
    // override live in local state. Route those three actions there. Every
    // other action (thresholds, toggles) stays HA-only.
    if st.source == SnapshotSource::Native
        && matches!(
            body,
            Action::SetPauseUntil { .. }
                | Action::ClearPauseUntil
                | Action::SetOverrideTomorrow { .. }
        )
    {
        return native_control_action(&st.control, body).await;
    }

    // Irrigation Unlimited support has been removed; answer stale
    // clients with a clear 410 instead of dispatching anything.
    if matches!(body, Action::RunSequenceNow) {
        return (
            StatusCode::GONE,
            Json(
                json!({ "error": "run_sequence_now was removed along with Irrigation Unlimited support; use per-zone Run instead" }),
            ),
        );
    }

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
            let Some(eid) = running_sensor(&zone, &st.sprinkler_prefix) else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("unknown zone: {zone}") })),
                );
            };
            let clamped = seconds.min(RUN_SECONDS_MAX).max(1);
            if clamped != seconds {
                tracing::warn!(
                    "irrigation::Run clamped seconds {} -> {} (max {})",
                    seconds,
                    clamped,
                    RUN_SECONDS_MAX
                );
            }
            // Public opensprinkler integration service (prefix-driven
            // entity), replacing the old private script.os_zone_toggle.
            client
                .call_service(
                    "opensprinkler",
                    "run",
                    &json!({ "entity_id": eid, "run_seconds": clamped }),
                )
                .await
                .map(|_| json!({ "ok": true, "fired": "opensprinkler.run", "zone": zone, "seconds": clamped }))
                .map_err(|e| e.to_string())
        }
        Action::Stop { zone } => {
            let Some(eid) = running_sensor(&zone, &st.sprinkler_prefix) else {
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
        Action::StopAll => {
            // The opensprinkler integration stops ALL stations when its
            // stop service targets the controller-level switch (the same
            // `switch.<prefix>_enabled` entity the refresher reads for
            // master enable). Replaces the old private script.os_stop_all.
            let eid = format!("switch.{}_enabled", st.sprinkler_prefix);
            client
                .call_service("opensprinkler", "stop", &json!({ "entity_id": eid }))
                .await
                .map(|_| json!({ "ok": true, "fired": "opensprinkler.stop", "stopped": "all" }))
                .map_err(|e| e.to_string())
        }
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
        // Sticky overrides are native-only; they route to native_control_action
        // above whenever a store is mounted. Reaching the HA path means there's
        // no persistence DB to hold them.
        Action::SetGlobalOverride { .. } | Action::SetZoneOverride { .. } => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "sticky override requires a persistence DB (none mounted)"
                })),
            );
        }
        // Handled by the 410 early-return above; IU is gone.
        Action::RunSequenceNow => unreachable!("run_sequence_now answered before the HA path"),
    };

    match result {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))),
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
        Ok(w) => (
            StatusCode::OK,
            Json(serde_json::to_value(w).unwrap_or_default()),
        ),
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
        Ok(w) => (
            StatusCode::OK,
            Json(serde_json::to_value(w).unwrap_or_default()),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// P3-4: the forecast-accuracy scoreboard for the last `?days=N` (default 30,
/// cap 365). One row per local day pairing the morning verdict with the rain
/// that actually fell, plus the honest matched/scored tally.
async fn accuracy(
    State(conn): State<Arc<Mutex<Connection>>>,
    Query(q): Query<HistoryQuery>,
) -> impl IntoResponse {
    let days = q.days.clamp(1, 365);
    let from = chrono::Utc::now().timestamp() - (days as i64) * 86400;
    let store = crate::persistence::verdict_history::VerdictHistoryStore::new(conn);
    match store.accuracy_window(from).await {
        Ok(res) => (
            StatusCode::OK,
            Json(serde_json::to_value(res).unwrap_or_default()),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// P2-11: portable history export over the existing windowed readers.
/// `?format=csv` (default) streams the run/skip events as CSV; `?format=json`
/// returns the full `{runs, decisions}` structured export. `?days=N` bounds the
/// window (default 365, max 3650). Served under the same gated history routes.
#[derive(Debug, serde::Deserialize)]
struct ExportQuery {
    #[serde(default = "default_export_days")]
    days: u32,
    #[serde(default = "default_export_format")]
    format: String,
}
fn default_export_days() -> u32 {
    365
}
fn default_export_format() -> String {
    "csv".to_string()
}

/// Minimal RFC 4180 CSV field escaping: quote when the value contains a comma,
/// quote, or newline, doubling embedded quotes.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

async fn export(
    State(conn): State<Arc<Mutex<Connection>>>,
    Query(q): Query<ExportQuery>,
) -> impl IntoResponse {
    let days = q.days.clamp(1, 3650);
    let now = chrono::Utc::now().timestamp();
    let from = now - (days as i64) * 86400;
    let runs = match db::window(conn.clone(), from, now).await {
        Ok(w) => w.runs,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    let decisions = match db::decisions_window(conn, from, now).await {
        Ok(w) => w.decisions,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    if q.format.eq_ignore_ascii_case("json") {
        return (
            StatusCode::OK,
            [(
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"localsky-history.json\"",
            )],
            Json(json!({
                "from_epoch": from,
                "to_epoch": now,
                "runs": runs,
                "decisions": decisions,
            })),
        )
            .into_response();
    }

    // CSV of run/skip events: the portable "what watered when, and what got
    // skipped and why" log.
    let mut out = String::from("timestamp_utc,zone,event,duration_s,reason\n");
    for r in &runs {
        let ts = chrono::DateTime::from_timestamp(r.start_epoch, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default();
        let (event, reason) = match &r.skip_reason {
            Some(reason) => ("skip", reason.as_str()),
            None => ("run", ""),
        };
        out.push_str(&format!(
            "{ts},{},{event},{},{}\n",
            csv_field(&r.zone),
            r.duration_s,
            csv_field(reason),
        ));
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"localsky-history.csv\"",
            ),
        ],
        out,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_field_escapes_per_rfc4180() {
        assert_eq!(csv_field("back_yard"), "back_yard");
        // Comma -> quoted.
        assert_eq!(csv_field("Rain, then freeze"), "\"Rain, then freeze\"");
        // Embedded quote -> doubled + quoted.
        assert_eq!(csv_field("said \"skip\""), "\"said \"\"skip\"\"\"");
        // Newline -> quoted.
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
        assert_eq!(csv_field(""), "");
    }

    #[test]
    fn native_deploy_always_routes_registry() {
        // Even with no controller configured the native deploy must NOT
        // fall through to HA scripts (that's the every-tap-500s bug);
        // the registry path answers 503 with a clear message instead.
        assert!(route_via_registry(SnapshotSource::Native, false));
        assert!(route_via_registry(SnapshotSource::Native, true));
    }

    #[test]
    fn ha_deploy_with_controller_routes_registry() {
        assert!(route_via_registry(SnapshotSource::HomeAssistant, true));
    }

    #[test]
    fn legacy_ha_deploy_without_controller_keeps_ha_path() {
        assert!(!route_via_registry(SnapshotSource::HomeAssistant, false));
    }

    #[test]
    fn controller_errors_map_to_sensible_status() {
        let (s, _) = controller_error_response(ControllerError::ZoneUnknown("x".into()));
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = controller_error_response(ControllerError::Unsupported("y".into()));
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        let (s, _) = controller_error_response(ControllerError::AuthFailed);
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        let (s, _) = controller_error_response(ControllerError::Offline);
        assert_eq!(s, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn run_sequence_now_still_deserializes() {
        // The tombstone variant must stay parseable so stale clients get
        // the 410 body rather than a 422 deserialization error.
        let a: Action = serde_json::from_str(r#"{"kind":"run_sequence_now"}"#).unwrap();
        assert!(matches!(a, Action::RunSequenceNow));
    }
}
