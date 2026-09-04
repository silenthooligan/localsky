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
    cfg_store: Arc<crate::config::FileConfigStore>,
    watering_policy: Arc<arc_swap::ArcSwap<crate::ha::WateringPolicy>>,
) -> Router {
    // POST /action needs the snapshot source, the local control store, and
    // the adoption markers that say where each control's value lives, so it
    // lives in its own sub-router with that state.
    let watering_policy_for_invite = watering_policy.clone();
    let action_router = Router::new()
        .route("/action", post(action))
        .with_state(ActionState {
            source,
            control: history.clone().map(IrrigationControlStore::new),
            sprinkler_prefix,
            cfg_store,
            watering_policy,
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
        // The soil opt-in offer exists only where a dismissal can land
        // somewhere durable: with no history database the promise "dismiss
        // and it will not return" cannot be kept, so the routes are simply
        // not registered and the popup never fires there.
        let invite_router = Router::new()
            .route("/soil-invite", get(soil_invite))
            // Mutating: privileged like tuning/dismiss (the auth
            // middleware lists the path).
            .route("/soil-invite/dismiss", post(soil_invite_dismiss))
            .with_state(SoilInviteApiState {
                history: h.clone(),
                store,
                watering_policy: watering_policy_for_invite,
            });
        merged.merge(invite_router).merge(
            Router::new()
                .route("/history", get(history_window))
                .route("/decisions", get(decisions_window))
                .route("/export", get(export))
                .route("/accuracy", get(accuracy))
                .route("/tuning", get(tuning_report))
                // Mutating: privileged + CSRF like zones/apply (the auth
                // middleware lists both paths).
                .route("/tuning/dismiss", post(tuning_dismiss))
                .route("/tuning/undismiss", post(tuning_undismiss))
                .with_state(h),
        )
    } else {
        merged
    }
}

/// State for the soil opt-in offer's two routes: the snapshot (whose
/// budget rows carry the shadow soil blocks), the live watering policy
/// (whose `scheduling_model` is the RESOLVED engine default), and the
/// history connection the dismissal record lives in.
#[derive(Clone)]
struct SoilInviteApiState {
    history: Arc<Mutex<Connection>>,
    store: Arc<IrrigationStore>,
    watering_policy: Arc<arc_swap::ArcSwap<crate::ha::WateringPolicy>>,
}

/// What the offer names about this yard, derived from the budget rows.
/// `None` means the install is not offered at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SoilInviteFacts {
    /// Weekly-governed zones whose shadow plan resolved (soil block
    /// present, i.e. not evidence-starved and not a degraded tick).
    pub shadow_zones: u32,
    /// Of those, zones carrying a live deficit right now.
    pub deficit_zones: u32,
    /// Of those, zones where the soil plan and the weekly plan disagree
    /// about watering today (either direction).
    pub differs_today: u32,
}

/// Whether this install is offered the soil model, and what the offer
/// can say about the yard. The offer exists for installs the WEEKLY
/// default governs (an engine default of soil IS the opt-in, so those
/// installs are retired with no record needed), and it needs at least
/// one weekly-governed zone whose shadow plan resolved: a yard with
/// every zone pinned to soil has nothing left to offer, and a yard
/// whose shadow is evidence-starved everywhere has nothing to show yet.
/// The starved case clears itself within the first few mornings as
/// evidence lands, so the offer simply waits rather than firing empty.
pub(crate) fn soil_invite_facts(
    engine_default_weekly: bool,
    budgets: &[crate::ha::snapshot::WaterBudget],
) -> Option<SoilInviteFacts> {
    if !engine_default_weekly {
        return None;
    }
    let shadow: Vec<&crate::ha::snapshot::WaterBudget> = budgets
        .iter()
        .filter(|b| b.scheduling_model == "weekly" && b.soil_depletion_mm.is_some())
        .collect();
    if shadow.is_empty() {
        return None;
    }
    let deficit_zones = shadow
        .iter()
        .filter(|b| b.soil_depletion_mm.unwrap_or(0.0) > 0.0)
        .count() as u32;
    let differs_today = shadow
        .iter()
        .filter(|b| (b.soil_planned_seconds > 0) != (b.today_seconds > 0))
        .count() as u32;
    Some(SoilInviteFacts {
        shadow_zones: shadow.len() as u32,
        deficit_zones,
        differs_today,
    })
}

/// GET /api/v1/irrigation/soil-invite: whether the soil opt-in offer
/// shows here, what it says, and where its dismissal stands. One read,
/// answered from the live snapshot + policy + the server-side record,
/// so the popup has no second source to race and a dismissal from any
/// device holds on every other one.
async fn soil_invite(State(st): State<SoilInviteApiState>) -> impl IntoResponse {
    use crate::config::schema::SchedulingModel;
    // A demo instance never makes the offer: its database reseeds, so a
    // dismissal could not keep its word, and a popup would cover every
    // showcase surface. Same flag /api/v1/info reports as `demo`.
    if std::env::var("LOCALSKY_DEMO").ok().as_deref() == Some("1") {
        return (StatusCode::OK, Json(json!({ "eligible": false })));
    }
    let weekly_default = st.watering_policy.load().scheduling_model == SchedulingModel::Weekly;
    let snap = st.store.snapshot();
    let Some(facts) = soil_invite_facts(weekly_default, &snap.water_budgets) else {
        return (StatusCode::OK, Json(json!({ "eligible": false })));
    };
    let store = crate::persistence::TuningDismissalsStore::new(st.history.clone());
    let now = chrono::Utc::now().timestamp();
    match store.soil_invite_state(now).await {
        Ok(state) => {
            use crate::persistence::InviteState;
            let (state_str, until) = match state {
                InviteState::Open => ("open", None),
                InviteState::Snoozed { until_epoch } => ("snoozed", Some(until_epoch)),
                InviteState::Dismissed => ("dismissed", None),
            };
            (
                StatusCode::OK,
                Json(json!({
                    "eligible": true,
                    "state": state_str,
                    "until_epoch": until,
                    "shadow_zones": facts.shadow_zones,
                    "deficit_zones": facts.deficit_zones,
                    "differs_today": facts.differs_today,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// Body for POST /soil-invite/dismiss: "snooze" (the offer returns
/// after 30 days, the tuning precedent) or "permanent" (it never does).
#[derive(Debug, Deserialize)]
struct SoilInviteDismissBody {
    kind: String,
}

/// POST /api/v1/irrigation/soil-invite/dismiss. Privileged (same
/// posture as tuning/dismiss); records the choice server side so it
/// survives restarts, other browsers, and other devices.
async fn soil_invite_dismiss(
    State(st): State<SoilInviteApiState>,
    Json(body): Json<SoilInviteDismissBody>,
) -> impl IntoResponse {
    use crate::persistence::InviteState;
    let permanent = match body.kind.as_str() {
        "snooze" => false,
        "permanent" => true,
        other => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": format!("unknown dismissal kind: {other}") })),
            );
        }
    };
    let store = crate::persistence::TuningDismissalsStore::new(st.history.clone());
    let now = chrono::Utc::now().timestamp();
    // The store keeps permanent final (a stale tab's snooze never
    // resurrects the offer) and hands back the state it actually holds,
    // so the response is a read, not an assumption.
    match store.record_soil_invite_choice(permanent, now).await {
        Ok(state) => {
            let (state_str, until) = match state {
                InviteState::Dismissed => ("dismissed", Value::Null),
                InviteState::Snoozed { until_epoch } => ("snoozed", json!(until_epoch)),
                InviteState::Open => ("open", Value::Null),
            };
            (
                StatusCode::OK,
                Json(json!({ "state": state_str, "until_epoch": until })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// Body for POST /tuning/dismiss. `kind` decides the key: a snooze
/// silences the exact `recommendation_id` for 30 days; a permanent
/// dismissal silences (zone_slug, field) forever, surviving value
/// drift.
#[derive(Debug, Deserialize)]
struct TuningDismissBody {
    zone_slug: String,
    field: String,
    #[serde(default)]
    recommendation_id: Option<String>,
    kind: String,
}

/// Body for POST /tuning/undismiss: clears every dismissal for the
/// (zone, field) pair.
#[derive(Debug, Deserialize)]
struct TuningUndismissBody {
    zone_slug: String,
    field: String,
}

/// POST /api/v1/irrigation/tuning/dismiss. Privileged (same posture as
/// zones/apply); records the dismissal and answers with the state the
/// UI needs to update in place.
async fn tuning_dismiss(
    State(h): State<Arc<Mutex<Connection>>>,
    Json(body): Json<TuningDismissBody>,
) -> impl IntoResponse {
    if body.zone_slug.trim().is_empty() || body.field.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "zone_slug and field are required" })),
        );
    }
    match body.kind.as_str() {
        "snooze" => {
            if body.recommendation_id.as_deref().unwrap_or("").is_empty() {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "error": "a snooze keys the exact recommendation_id" })),
                );
            }
        }
        "permanent" => {}
        other => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": format!("unknown dismissal kind: {other}") })),
            );
        }
    }
    let store = crate::persistence::TuningDismissalsStore::new(h);
    let now = chrono::Utc::now().timestamp();
    match store
        .dismiss(
            &body.zone_slug,
            &body.field,
            body.recommendation_id.as_deref(),
            &body.kind,
            now,
        )
        .await
    {
        Ok(()) => {
            let until =
                (body.kind == "snooze").then(|| now + crate::persistence::SNOOZE_DAYS * 86_400);
            (
                StatusCode::OK,
                Json(json!({
                    "dismissed": true,
                    "kind": body.kind,
                    "until_epoch": until,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// POST /api/v1/irrigation/tuning/undismiss. Privileged; clears the
/// (zone, field) silencing so the recommendation may derive again on
/// the next report.
async fn tuning_undismiss(
    State(h): State<Arc<Mutex<Connection>>>,
    Json(body): Json<TuningUndismissBody>,
) -> impl IntoResponse {
    if body.zone_slug.trim().is_empty() || body.field.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "zone_slug and field are required" })),
        );
    }
    let store = crate::persistence::TuningDismissalsStore::new(h);
    match store.undismiss(&body.zone_slug, &body.field).await {
        Ok(removed) => (StatusCode::OK, Json(json!({ "removed": removed }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

/// GET /api/v1/irrigation/tuning?days=N (clamp 7..=30, default 14): the
/// per-zone results-based tuning report plus the install-wide
/// forecast-skip scorecard. Read-only and unprivileged like the other
/// history GETs; generation reads the boot-registered tuning handles
/// (config store + live stores), so the route just carries the days knob.
async fn tuning_report(Query(q): Query<TuningQuery>) -> impl IntoResponse {
    match crate::tuning::generate_report(q.days).await {
        Ok(report) => (
            StatusCode::OK,
            Json(serde_json::to_value(report).unwrap_or_default()),
        ),
        Err(crate::tuning::TuningError::NotConfigured) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "tuning report requires the history database" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct TuningQuery {
    #[serde(default = "default_tuning_days")]
    days: u32,
}

fn default_tuning_days() -> u32 {
    crate::engine::tuning::DEFAULT_WINDOW_DAYS
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
/// The planned-seconds delta is expected (the native weekly water balance
/// vs SI's daily bucket size runs on different evidence) and is shown for
/// both so it can be judged; verdict + running mismatches are the signal
/// that native isn't yet equivalent.
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
        hypo.rain_tomorrow_prob_pct = Some(v);
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
                    over_line: false,
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
    /// Update a threshold (max_wind_mph / min_temp_f / rain_skip_in).
    /// Writes `engine.skip_rules`, the same field Settings > Skip rules
    /// writes. On a Home Assistant deployment whose matching `input_number`
    /// helper has not been adopted yet it still writes that helper, because
    /// until then that is what the engine reads.
    SetThreshold { key: String, value: f64 },
    /// Toggle the vacation pause or dry-run mode. Writes LocalSky's own
    /// control store, or the matching `input_boolean` on a Home Assistant
    /// deployment that has not adopted it yet.
    Toggle { key: String, on: bool },
    /// Set the vacation-pause expiry to a UTC epoch. Honored by
    /// skip_logic::evaluate as a hard skip until the timestamp passes.
    /// Pass 0 to clear (or use ClearPauseUntil). Requires a persistence DB,
    /// or, before adoption on a Home Assistant deployment, the
    /// input_datetime.irrigation_pause_until helper.
    SetPauseUntil { epoch: i64 },
    /// Convenience: clears the pause-until.
    ClearPauseUntil,
    /// One-day override for tomorrow's verdict. mode = "none" | "skip" | "run".
    /// "none" returns to skip_logic auto. LocalSky expires it at local
    /// midnight itself; the Home Assistant midnight automation that used to
    /// reset the input_select is no longer part of it.
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

/// Map a threshold key to the input_number entity ID, for the pre-adoption
/// path only. Restricts to the three known sliders so a hostile client can't
/// poke arbitrary HA inputs through this endpoint. One list, shared with the
/// read gate, so the write and the read can never disagree about which
/// entity a key means.
fn threshold_entity(key: &str) -> Option<String> {
    crate::ha_adopt::threshold_entity(key).map(str::to_string)
}

/// Map a toggle key to the input_boolean entity ID, same allow-list shape,
/// same pre-adoption-only role.
fn toggle_entity(key: &str) -> Option<String> {
    crate::ha_adopt::toggle_entity(key).map(str::to_string)
}

/// Defensive cap on Action::Run duration. The mobile UI caps at 120 min;
/// the server clamps at the same level so a buggy client or hostile
/// request can't drown the lawn.
const RUN_SECONDS_MAX: u32 = 7200;

/// HA entity for the vacation-pause expiry helper, for the pre-adoption
/// write path only. One definition, shared with the read gate.
const PAUSE_UNTIL_ENTITY: &str = crate::ha_adopt::PAUSE_UNTIL;

/// HA entity for the one-day override (none/skip/run), same role.
const OVERRIDE_ENTITY: &str = crate::ha_adopt::OVERRIDE_TOMORROW;

/// State for the POST /action handler.
///
/// Every control write routes to whichever store the ENGINE will read it
/// back from, decided by the same adoption markers the refresher uses and
/// read from the same live handle. That is not a detail: a write that lands
/// in SQLite while the gate still reads the Home Assistant helper is an owner
/// tapping vacation pause and getting a watered yard. Read and write flip
/// together, on one marker.
#[derive(Clone)]
struct ActionState {
    source: SnapshotSource,
    /// Native control store. `None` when no persistence DB is mounted; a
    /// native pause/override write then returns 503 rather than silently
    /// dropping (a dropped pause = unexpected watering).
    control: Option<IrrigationControlStore>,
    /// HA controller entity prefix (config-driven; default "opensprinkler").
    sprinkler_prefix: String,
    /// Sink for an adopted threshold: `engine.skip_rules` is where the value
    /// lives once the matching `input_number` is retired.
    cfg_store: Arc<crate::config::FileConfigStore>,
    /// The live policy, for its adoption markers and for swapping a rebuilt
    /// one in after a threshold write so the change is live on the next tick.
    watering_policy: Arc<arc_swap::ArcSwap<crate::ha::WateringPolicy>>,
}

impl ActionState {
    /// Whether this action's value now lives in LocalSky rather than in the
    /// Home Assistant helper named by `entity`. True on every native deploy,
    /// and on a Home Assistant deploy once the adoption pass has handled that
    /// entity. The refresher gates the matching READ on the identical
    /// predicate against the identical handle.
    fn owns(&self, entity: &str) -> bool {
        self.source == SnapshotSource::Native || self.watering_policy.load().ha_read_retired(entity)
    }
}

/// Whether a control write for `entity` lands in LocalSky's own store rather
/// than the Home Assistant helper.
///
/// `owns` says the READ has moved. A mounted store is the other half, and it
/// is not optional: with no persistence DB, `build_from_map` resolves
/// `control.filter(..)` to None and the gate reads the entity map again even
/// for a retired entity, so on a Home Assistant deploy the helper is what has
/// to be written. Without this, an install that adopted the toggles and later
/// lost its DB answered 503 to a vacation pause while the engine was reading
/// `input_boolean.irrigation_pause` and the helper was still writable, which
/// makes the pause unsettable from LocalSky's own UI on exactly the install
/// where the DB is already broken. Native keeps the explicit 503 instead of a
/// phantom call into a Home Assistant that is not there.
fn routes_to_native_control(st: &ActionState, entity: &str) -> bool {
    st.owns(entity) && (st.control.is_some() || st.source == SnapshotSource::Native)
}

/// Write one adopted threshold into `engine.skip_rules` and make it live.
///
/// Held under the config store's read-modify-write guard, the same one PUT
/// /api/config and the tuning apply take, so a slider and a settings save
/// cannot clobber each other. The rebuilt policy is swapped in on success so
/// the new threshold decides on the next tick rather than after a restart.
async fn config_threshold_action(
    st: &ActionState,
    key: &str,
    value: f64,
) -> (StatusCode, Json<Value>) {
    let Some((lo, hi)) = crate::ha_adopt::threshold_range(key) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown threshold: {key}") })),
        );
    };
    if !value.is_finite() || value < lo || value > hi {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("{key} must be between {lo} and {hi}"),
            })),
        );
    }
    let _guard = st.cfg_store.begin_write().await;
    let mut cfg = match crate::ports::config_store::ConfigStore::load(&*st.cfg_store).await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": format!("config unavailable: {e}") })),
            );
        }
    };
    match key {
        "max_wind_mph" => cfg.engine.skip_rules.max_wind_mph = value,
        "min_temp_f" => cfg.engine.skip_rules.min_temp_f = value,
        "rain_skip_in" => cfg.engine.skip_rules.rain_skip_in = value,
        _ => unreachable!("threshold_range gates the key"),
    }
    if let Err(e) = crate::ports::config_store::ConfigStore::save(&*st.cfg_store, &cfg).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("config save failed: {e}") })),
        );
    }
    st.watering_policy
        .store(Arc::new(crate::ha::WateringPolicy::from_config(&cfg)));
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "source": "config", "key": key, "value": value })),
    )
}

/// Write one adopted toggle (pause / dry-run) into LocalSky's control store.
async fn control_toggle_action(
    control: &Option<IrrigationControlStore>,
    key: &str,
    on: bool,
) -> (StatusCode, Json<Value>) {
    let Some(cs) = control else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "control state unavailable (no persistence DB mounted)" })),
        );
    };
    let res = match key {
        "irrigation_pause" => cs.set_paused(on).await,
        "irrigation_dry_run" => cs.set_dry_run(on).await,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown toggle: {key}") })),
            );
        }
    };
    match res {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "source": "native", "key": key, "on": on })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
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
///
/// Every failure except an unknown zone and an unsupported operation used
/// to collapse into one 502, so a revoked vendor credential, an exhausted
/// daily request budget, and a vendor rejecting the zone id were a single
/// indistinguishable status with no way to tell them apart from the
/// client. Each class now carries its own status, and the body carries a
/// stable `code` so a client never has to key on the status at all.
///
/// A vendor credential failure answers 424, NOT 401. 401 on any LocalSky
/// endpoint is the deploy's OWN auth outcome, and the shipped Home
/// Assistant integration reacts to one by invalidating its stored LocalSky
/// token and starting a reauthentication flow. Answering 401 because a
/// Rachio key was revoked would send that integration into a reauth loop
/// against a token that was never the problem.
///
/// `rate_limit_remaining` is what the controller's LAST RESPONSE reported,
/// from `IrrigationController::rate_limit_remaining`; null when that
/// response carried no number (never a zero sentinel). `mapped_zone_slugs`
/// is the controller's zone map keys, from
/// `IrrigationController::mapped_zone_slugs`, which the unknown-zone body
/// carries so a slug mismatch is visible from the error alone.
fn controller_error_response(
    e: ControllerError,
    rate_limit_remaining: Option<String>,
    mapped_zone_slugs: Vec<String>,
) -> (StatusCode, Json<Value>) {
    let status = match &e {
        ControllerError::ZoneUnknown(_) => StatusCode::BAD_REQUEST,
        ControllerError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
        ControllerError::AuthFailed => StatusCode::FAILED_DEPENDENCY,
        ControllerError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ControllerError::Offline
        | ControllerError::Remote(_)
        | ControllerError::Transport(_)
        | ControllerError::Init(_) => StatusCode::BAD_GATEWAY,
    };
    // Stable discriminator so a client branches on this rather than on the
    // status, which is what made the old 502 collapse unreadable and what
    // makes 401-vs-424 a trap for anyone who guesses.
    let code = match &e {
        ControllerError::ZoneUnknown(_) => "zone_unknown",
        ControllerError::Unsupported(_) => "controller_unsupported",
        ControllerError::AuthFailed => "controller_auth_failed",
        ControllerError::RateLimited => "controller_rate_limited",
        ControllerError::Offline
        | ControllerError::Remote(_)
        | ControllerError::Transport(_)
        | ControllerError::Init(_) => "controller_unreachable",
    };
    // "zone unknown: front_yard" named the zone but never said what the
    // miss MEANS or what fixes it, and it is the next-most-likely failure
    // once a bad station value stops overwriting a scanned zone map.
    let message = match &e {
        ControllerError::ZoneUnknown(zone) => {
            format!("zone \"{zone}\" is not mapped to a zone on this controller")
        }
        other => other.to_string(),
    };
    let mut body = json!({ "error": message, "code": code });
    match &e {
        ControllerError::ZoneUnknown(zone) => {
            // Nothing on this controller is wired to this zone. The one
            // remedy is to say which of the controller's zones it is;
            // the editor's station field is where that lives, and it now
            // lists them for the kinds that can be asked. The old advice to
            // RENAME the zone until its slug matched a vendor-named map key
            // is gone: the slug is a permanent key (history, entity ids,
            // retained MQTT topics), so renaming to fix a binding trades one
            // broken thing for a worse one. Deliberately no fuzzy name
            // matching in the lookup itself: guessing which valve the user
            // meant risks opening the wrong one.
            let hint = if mapped_zone_slugs.is_empty() {
                format!(
                    "Nothing on this controller is bound to \"{zone}\" yet. Open Settings, \
                     then Zones, edit this zone, and set Controller station to the \
                     controller's own zone. Where the controller can be asked, that field \
                     lists its zones by name."
                )
            } else {
                format!(
                    "This controller can currently fire: {}. Your zone's slug is \"{zone}\", \
                     which is not one of them, and the lookup is exact. Open Settings, then \
                     Zones, edit this zone, and set Controller station to the controller's \
                     own zone; where the controller can be asked, that field lists them by \
                     name. Do not rename the zone to match: a zone's slug is permanent \
                     because its history and its Home Assistant entities are stored under it.",
                    mapped_zone_slugs.join(", ")
                )
            };
            body["mapped_zones"] = json!(mapped_zone_slugs);
            body["hint"] = json!(hint);
        }
        ControllerError::RateLimited => {
            let hint = match rate_limit_remaining.as_deref() {
                Some(n) => format!(
                    "The controller's cloud last reported {n} API requests left for today. \
                     A longer poll interval on the controller spends fewer of them."
                ),
                None => "The controller's cloud is refusing further requests for now. \
                         A longer poll interval on the controller spends fewer of them."
                    .to_string(),
            };
            body["rate_limit_remaining"] = json!(rate_limit_remaining);
            body["hint"] = json!(hint);
        }
        ControllerError::AuthFailed => {
            body["hint"] = json!(
                "The controller rejected the credential. This is the controller's own \
                 credential, not your LocalSky login. Re-enter it under Settings, then \
                 Devices: open the controller with Edit, and use Scan zones to check it."
            );
        }
        _ => {}
    }
    (status, Json(body))
}

/// Dispatch zone Run/Stop/StopAll through the registry's default
/// controller. Confirmed manual runs are recorded in the runs table
/// (source "manual") so the history Gantt and scheduler dedupe see
/// them. Only called with the three zone-action variants.
/// Shutoff-backstop deadline for a manually dispatched run: planned end plus
/// the shared enforcement grace (30s; 90s when the controller's only stop is
/// device-wide). A graceless deadline made the reaper fire the instant the
/// planned end passed, which on a Rachio-class controller device-stops any
/// sibling zone started meanwhile.
fn manual_run_deadline(started_epoch: i64, duration_s: u32, per_zone_stop: bool) -> i64 {
    started_epoch
        + duration_s as i64
        + crate::controllers::reaper::effective_run_grace(per_zone_stop)
}

/// Ledger + history bookkeeping after a successful manual zone Stop. A
/// controller with a real per-zone stop is zone-scoped: disarm that zone's
/// deadline, truncate that zone's open manual row. A DEVICE-WIDE stop
/// (per_zone_stop=false) halted every zone on the controller, so every
/// armed row on it clears (a survivor would re-fire another device-wide
/// stop at its stale deadline) and every open manual row on it truncates
/// (a sibling's pre-written full-duration row would otherwise credit water
/// that stopped falling). Other controllers are untouched either way.
async fn stop_bookkeeping(
    active_runs: Option<&crate::persistence::ActiveRunsStore>,
    runs: Option<&RunsStore>,
    controller_id: &str,
    zone: &str,
    device_wide: bool,
    now_epoch: i64,
) {
    if let Some(ar) = active_runs {
        if device_wide {
            if let Err(e) = ar.clear_for_controllers(&[controller_id]).await {
                tracing::warn!(
                    zone = %zone, controller = %controller_id, error = %e,
                    "device-wide stop: clearing sibling deadlines failed"
                );
            }
        } else {
            let _ = ar.disarm(zone).await;
        }
    }
    if let Some(rs) = runs {
        let truncated = if device_wide {
            rs.truncate_active_for_controller(controller_id, now_epoch)
                .await
        } else {
            rs.truncate_active(zone, now_epoch).await
        };
        if let Err(e) = truncated {
            tracing::debug!(zone = %zone, error = %e, "manual-row truncate failed");
        }
    }
}

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
                            // Pretend water from a dry-run controller is
                            // recorded as such (excluded from watering
                            // evidence), mirroring the run-edge observer:
                            // a manual Run against a non-simulating
                            // DryRunController must never credit the
                            // balance or show as real watering.
                            source: if controller.simulated() {
                                "dry_run".into()
                            } else {
                                "manual".into()
                            },
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
                    // controller's own timer fires. The deadline carries the
                    // shared enforcement grace (30s; 90s on device-wide-stop
                    // clouds): the controller's own timer is the precise
                    // shutoff, and a graceless deadline made the reaper fire
                    // a stop the instant the planned end passed, which on a
                    // Rachio-class controller is a device-wide stop that
                    // kills any sibling zone the user started meanwhile.
                    if let Some(ar) = d.active_runs.as_ref() {
                        if let Err(e) = ar
                            .arm(
                                zone.clone(),
                                handle.controller_id.clone(),
                                handle.started_epoch,
                                manual_run_deadline(
                                    handle.started_epoch,
                                    clamped,
                                    controller.supports().per_zone_stop,
                                ),
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
                            // How long this controller can take to REPORT the
                            // change (null when it reads state on demand). The
                            // UI's confirm window is shorter than a throttled
                            // cloud poll, so without this a perfectly accepted
                            // run reads as a controller that never answered.
                            "confirm_within_s": controller.status_poll_interval_s(),
                        })),
                    )
                }
                Err(e) => {
                    // Nothing logged this path, so a failed dispatch left no
                    // trace on the server at all and the only copy of the
                    // reason was a response body the client threw away.
                    tracing::warn!(
                        controller = %controller.id(), zone = %zone, action = "run", error = %e,
                        "controller zone action failed"
                    );
                    controller_error_response(
                        e,
                        controller.rate_limit_remaining(),
                        controller.mapped_zone_slugs(),
                    )
                }
            }
        }
        Action::Stop { zone } => {
            // On a controller with no per-zone stop (Rachio-class clouds)
            // this zone-stop is a DEVICE-WIDE stop: every running zone on
            // the device stops. Surface the scope in the log and in the
            // response so the client's toast can say what really happened.
            let device_wide = !controller.supports().per_zone_stop;
            if device_wide {
                tracing::warn!(
                    zone = %zone, controller = %controller.id(),
                    "manual stop: controller has no per-zone stop; stopping ALL watering on the device"
                );
            }
            match controller.stop_zone(&zone).await {
                Ok(()) => {
                    // P0-1b: an explicit stop disarms the deadline so the reaper does
                    // not later re-stop an already-closed valve. A DEVICE-WIDE stop
                    // (per_zone_stop=false) halted every zone on this controller, so
                    // its bookkeeping must match the note the response carries:
                    // every armed row on this controller clears (a survivor would
                    // re-fire another device-wide stop at its stale deadline), and
                    // every open manual row on this controller truncates to the
                    // real span (a sibling's pre-written full-duration row would
                    // otherwise credit water that stopped falling). Other
                    // controllers' rows are untouched either way.
                    stop_bookkeeping(
                        d.active_runs.as_ref(),
                        d.runs.as_ref(),
                        controller.id(),
                        &zone,
                        device_wide,
                        chrono::Utc::now().timestamp(),
                    )
                    .await;
                    let mut body = json!({
                        "ok": true,
                        "dispatched": format!("controller:{}", controller.id()),
                        "stopped": zone,
                        "scope": if device_wide { "device" } else { "zone" },
                        // Same readback lag as Run: a stop is accepted long
                        // before a throttled cloud poll reports it.
                        "confirm_within_s": controller.status_poll_interval_s(),
                    });
                    if device_wide {
                        body["note"] = json!(
                            "This controller cannot stop a single zone; all watering on the device was stopped."
                        );
                    }
                    (StatusCode::OK, Json(body))
                }
                Err(e) => {
                    tracing::warn!(
                        controller = %controller.id(), zone = %zone, action = "stop", error = %e,
                        "controller zone action failed"
                    );
                    controller_error_response(
                        e,
                        controller.rate_limit_remaining(),
                        controller.mapped_zone_slugs(),
                    )
                }
            }
        }
        Action::StopAll => match controller.stop_all().await {
            Ok(()) => {
                if let Some(ar) = d.active_runs.as_ref() {
                    let _ = ar.clear_all().await;
                }
                // Same truncation as Stop, across every zone.
                if let Some(rs) = d.runs.as_ref() {
                    let now = chrono::Utc::now().timestamp();
                    if let Err(e) = rs.truncate_active_all(now).await {
                        tracing::debug!(error = %e, "manual-row truncate failed");
                    }
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "dispatched": format!("controller:{}", controller.id()),
                        "stopped": "all",
                        "confirm_within_s": controller.status_poll_interval_s(),
                    })),
                )
            }
            Err(e) => {
                tracing::warn!(
                    controller = %controller.id(), action = "stop_all", error = %e,
                    "controller zone action failed"
                );
                controller_error_response(
                    e,
                    controller.rate_limit_remaining(),
                    controller.mapped_zone_slugs(),
                )
            }
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

    // On a Home Assistant deployment, where a control or threshold write
    // lands is decided, and the write performed, under the config store's
    // write guard: the same guard the adoption pass holds from its
    // commit-time re-read of the helpers through the policy swap that retires
    // their reads. Without it a tap landing between the pass's answer-set
    // check and the swap saw the read as live, wrote the helper, and was
    // retired underneath: the pass had planned from the pre-tap answer, wrote
    // that into SQLite, and the pause the owner just set was gone with a 200
    // on screen. Held here, the write either fully precedes the pass, which
    // then re-reads it (and sees the write counter moved), or fully follows
    // the swap and routes native below. The guard is released before a native
    // write, which needs no serialization against the pass and, for a
    // threshold, takes the guard itself.
    let mut preadopt_guard = if st.source == SnapshotSource::HomeAssistant
        && matches!(
            body,
            Action::SetThreshold { .. }
                | Action::Toggle { .. }
                | Action::SetPauseUntil { .. }
                | Action::ClearPauseUntil
                | Action::SetOverrideTomorrow { .. }
        ) {
        Some(st.cfg_store.begin_write().await)
    } else {
        None
    };

    // The vacation pause and the one-day override. They route to LocalSky's
    // own store on every native deploy, and on a Home Assistant deploy once
    // the matching helper has been adopted. Until then they still write the
    // helper, because until then the engine still reads it.
    if st.control.is_some()
        && matches!(body, Action::SetPauseUntil { .. } | Action::ClearPauseUntil)
        && st.owns(crate::ha_adopt::PAUSE_UNTIL)
    {
        drop(preadopt_guard.take());
        return native_control_action(&st.control, body).await;
    }
    if st.control.is_some()
        && matches!(body, Action::SetOverrideTomorrow { .. })
        && st.owns(crate::ha_adopt::OVERRIDE_TOMORROW)
    {
        drop(preadopt_guard.take());
        return native_control_action(&st.control, body).await;
    }
    // A native deploy with no persistence DB has nowhere to put a pause. Say
    // so, rather than falling through to a Home Assistant that is not there.
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
    // The two toggles. Before 0.7.22 these fell through to the Home Assistant
    // client on every deploy, so on a standalone install the pause toggle
    // answered 500 and the engine read a value nothing could set.
    if let Action::Toggle { key, on } = &body {
        if let Some(entity) = crate::ha_adopt::toggle_entity(key) {
            if routes_to_native_control(&st, entity) {
                drop(preadopt_guard.take());
                return control_toggle_action(&st.control, key, *on).await;
            }
        }
    }
    // The three thresholds. Once adopted, the dashboard slider and Settings >
    // Skip rules write the same field, which is the end of two editors for
    // one number where only one of them was ever in effect.
    if let Action::SetThreshold { key, value } = &body {
        if let Some(entity) = crate::ha_adopt::threshold_entity(key) {
            if st.owns(entity) {
                // config_threshold_action takes the guard itself.
                drop(preadopt_guard.take());
                return config_threshold_action(&st, key, *value).await;
            }
        }
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

    // Reaching here with one of these means the write goes to a Home Assistant
    // helper the adoption pass has not retired yet, so the pass may be about
    // to read the value this call replaces. Bump BEFORE the service call
    // fires, not after: a write still in flight when the pass takes its
    // answer set has to force that commit to re-earn its evidence, or the
    // pass writes the pre-write value into SQLite and retires the read. The
    // guard taken above is still held through the service call, so the pass
    // cannot commit between this bump and the write landing, nor retire the
    // read underneath it.
    if matches!(
        body,
        Action::SetThreshold { .. }
            | Action::Toggle { .. }
            | Action::SetPauseUntil { .. }
            | Action::ClearPauseUntil
            | Action::SetOverrideTomorrow { .. }
    ) {
        crate::ha_adopt::note_preadopt_write();
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
    // JSON error envelope like every other API error path (an integrator's
    // resp.json() error handler must never hit bare text here).
    let runs = match db::window(conn.clone(), from, now).await {
        Ok(w) => w.runs,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let decisions = match db::decisions_window(conn, from, now).await {
        Ok(w) => w.decisions,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
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

    fn action_state(source: SnapshotSource, adopted: &[&str]) -> ActionState {
        let mut cfg = crate::config::schema::Config::default();
        for e in adopted {
            cfg.ha_adoption.push(crate::ha::snapshot::HaAdoptedHelper {
                entity: (*e).to_string(),
                outcome: "adopted".to_string(),
                target: crate::ha_adopt::target_of(e).to_string(),
                adopted_value: None,
                observed_value: None,
                previous_value: None,
                epoch: 1,
            });
        }
        ActionState {
            source,
            control: None,
            sprinkler_prefix: "opensprinkler".to_string(),
            cfg_store: Arc::new(crate::config::FileConfigStore::new(
                std::env::temp_dir().join("localsky-action-state-test.toml"),
            )),
            watering_policy: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::ha::WateringPolicy::from_config(&cfg),
            )),
        }
    }

    // The write cutover and the read cutover consult one marker, on one live
    // handle. If they could ever disagree, an owner tapping vacation pause
    // would write LocalSky's store while the gate still read the Home
    // Assistant helper, see false, and water the yard.
    #[test]
    fn a_control_write_follows_the_same_marker_the_engine_reads() {
        let before = action_state(SnapshotSource::HomeAssistant, &[]);
        for e in crate::ha_adopt::ENTITIES {
            assert!(
                !before.owns(e),
                "{e} must still write Home Assistant until it is adopted"
            );
        }
        let after = action_state(SnapshotSource::HomeAssistant, &crate::ha_adopt::ENTITIES);
        for e in crate::ha_adopt::ENTITIES {
            assert!(after.owns(e), "{e} must write LocalSky once adopted");
        }
    }

    #[test]
    fn one_adopted_entity_does_not_carry_the_others_with_it() {
        let st = action_state(
            SnapshotSource::HomeAssistant,
            &[crate::ha_adopt::PAUSE_UNTIL],
        );
        assert!(st.owns(crate::ha_adopt::PAUSE_UNTIL));
        assert!(!st.owns(crate::ha_adopt::OVERRIDE_TOMORROW));
        assert!(!st.owns(crate::ha_adopt::MAX_WIND));
    }

    #[test]
    fn a_standalone_deploy_owns_every_control_with_no_migration_at_all() {
        // Nothing about the migration runs on a native install: the map is
        // empty, no marker is ever written. The controls are LocalSky's
        // regardless, which is what makes the pause toggle work there for the
        // first time.
        let st = action_state(SnapshotSource::Native, &[]);
        for e in crate::ha_adopt::ENTITIES {
            assert!(st.owns(e));
        }
    }

    // The toggle branch used to test `owns` alone, so an install that adopted
    // the toggles and later lost its persistence DB answered 503 while the
    // engine was reading the helper and the helper was still writable.
    #[test]
    fn a_toggle_write_with_no_store_falls_through_to_the_helper_the_engine_reads() {
        let ha = action_state(SnapshotSource::HomeAssistant, &crate::ha_adopt::ENTITIES);
        assert!(
            ha.control.is_none(),
            "the harness builds an unmounted store"
        );
        assert!(
            !routes_to_native_control(&ha, crate::ha_adopt::PAUSE_TOGGLE),
            "with no store the write has to go where the read goes: the helper"
        );
        // Native has no helper to fall back to, so it keeps the clear 503
        // rather than a phantom call into a Home Assistant that is not there.
        let native = action_state(SnapshotSource::Native, &[]);
        assert!(routes_to_native_control(
            &native,
            crate::ha_adopt::PAUSE_TOGGLE
        ));
        // And an unadopted toggle on Home Assistant still writes the helper.
        let unadopted = action_state(SnapshotSource::HomeAssistant, &[]);
        assert!(!routes_to_native_control(
            &unadopted,
            crate::ha_adopt::PAUSE_TOGGLE
        ));
    }

    #[test]
    fn threshold_and_toggle_keys_map_to_the_entities_the_read_gate_uses() {
        assert_eq!(
            threshold_entity("max_wind_mph").as_deref(),
            Some(crate::ha_adopt::MAX_WIND)
        );
        assert_eq!(
            toggle_entity("irrigation_dry_run").as_deref(),
            Some(crate::ha_adopt::DRY_RUN_TOGGLE)
        );
        assert_eq!(threshold_entity("gravity"), None);
        assert_eq!(toggle_entity("irrigation_launch"), None);
    }

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

    /// No zone map, the shape every non-cloud adapter reports.
    fn no_zones() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn controller_errors_map_to_sensible_status() {
        let (s, _) =
            controller_error_response(ControllerError::ZoneUnknown("x".into()), None, no_zones());
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) =
            controller_error_response(ControllerError::Unsupported("y".into()), None, no_zones());
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
        // A VENDOR credential failure must never answer 401: that status is
        // this deploy's own auth outcome, and the Home Assistant integration
        // reacts to one by invalidating its LocalSky token and starting a
        // reauth loop over a token that was never at fault.
        let (s, Json(body)) =
            controller_error_response(ControllerError::AuthFailed, None, no_zones());
        assert_eq!(s, StatusCode::FAILED_DEPENDENCY);
        assert_ne!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], json!("controller_auth_failed"));
        let (s, _) = controller_error_response(ControllerError::RateLimited, None, no_zones());
        assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
        // 502 stays for the failures that really are upstream or transport.
        for e in [
            ControllerError::Offline,
            ControllerError::Remote("HTTP 500: boom".into()),
            ControllerError::Transport("connect".into()),
            ControllerError::Init("tls roots".into()),
        ] {
            let (s, Json(body)) = controller_error_response(e, None, no_zones());
            assert_eq!(s, StatusCode::BAD_GATEWAY);
            assert_eq!(body["code"], json!("controller_unreachable"));
        }
    }

    /// Every error body carries a stable `code`, so a client branches on
    /// that rather than guessing which status a failure class maps to.
    #[test]
    fn every_error_body_carries_a_stable_code() {
        for (e, want) in [
            (ControllerError::ZoneUnknown("z".into()), "zone_unknown"),
            (
                ControllerError::Unsupported("op".into()),
                "controller_unsupported",
            ),
            (ControllerError::AuthFailed, "controller_auth_failed"),
            (ControllerError::RateLimited, "controller_rate_limited"),
            (ControllerError::Offline, "controller_unreachable"),
        ] {
            let (_, Json(body)) = controller_error_response(e, None, no_zones());
            assert_eq!(body["code"], json!(want));
        }
    }

    /// The reporter's most likely remaining failure. The 400 has to show
    /// what the controller CAN fire, name the slug that missed, and point at
    /// the one field that binds it. It must NOT suggest renaming the zone:
    /// that was the old remedy and it trades a binding problem for a
    /// permanent-key problem.
    #[test]
    fn zone_unknown_body_shows_what_the_controller_can_fire_and_the_one_remedy() {
        let (s, Json(body)) = controller_error_response(
            ControllerError::ZoneUnknown("front_yard".into()),
            None,
            vec!["front_lawn".to_string(), "back_lawn".to_string()],
        );
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("front_yard"), "must name the zone: {err}");
        assert!(
            err.contains("not mapped"),
            "must say the zone is not mapped to a controller zone: {err}"
        );
        // The keys ride the body as data, not only inside the prose.
        assert_eq!(body["mapped_zones"], json!(["front_lawn", "back_lawn"]));
        let hint = body["hint"].as_str().unwrap();
        assert!(
            hint.contains("front_lawn") && hint.contains("back_lawn"),
            "the hint must show what the map actually has: {hint}"
        );
        assert!(
            hint.contains("front_yard"),
            "the hint must name the slug that missed: {hint}"
        );
        // The remedy that actually binds, and where to do it.
        assert!(
            hint.contains("Controller station"),
            "name the field that binds the zone: {hint}"
        );
        assert!(
            hint.contains("Do not rename the zone"),
            "the old remedy was to rename until the names matched; say plainly \
             that it is not the fix: {hint}"
        );
        assert!(
            hint.contains("permanent"),
            "and say why renaming is not the fix: {hint}"
        );
        assert!(
            !hint.contains("blank"),
            "must not tell a mismatched zone to blank the one field that binds it: {hint}"
        );
    }

    /// With nothing mapped at all there is no key list to show, so the hint
    /// is just the remedy. It must point at the ZONE editor, not at the
    /// controller's Scan zones button: six of the ten kinds cannot scan, and
    /// for the four that can, scanning alone never bound anything.
    #[test]
    fn zone_unknown_with_nothing_bound_points_at_the_zone_editor() {
        let (_, Json(body)) = controller_error_response(
            ControllerError::ZoneUnknown("front_yard".into()),
            None,
            no_zones(),
        );
        assert_eq!(body["mapped_zones"], json!([]));
        let hint = body["hint"].as_str().unwrap();
        assert!(hint.contains("front_yard"), "{hint}");
        assert!(hint.contains("Settings"), "{hint}");
        assert!(hint.contains("Zones"), "{hint}");
        assert!(hint.contains("Controller station"), "{hint}");
    }

    /// The server's error body and the client's reader are two ends of one
    /// contract, and the break was on the client end: it printed the bare
    /// status and discarded the body. Lock them together so whatever
    /// `controller_error_response` writes still reaches the user through
    /// `load_error_message`, which is what both action call sites now use.
    #[test]
    fn the_client_reader_surfaces_the_controller_reason_from_the_error_body() {
        use crate::components::settings_ui::load_error_message;
        // A cloud controller's Remote error carries the vendor's own status
        // and body. That text is the whole point of reading the body.
        let (status, Json(body)) = controller_error_response(
            ControllerError::Remote("HTTP 400: zone id not recognized".into()),
            None,
            no_zones(),
        );
        let msg = load_error_message(status.as_u16(), &body.to_string());
        assert!(
            msg.contains("zone id not recognized"),
            "the vendor's reason must survive to the user: {msg}"
        );
        assert!(msg.contains("502"), "the status stays visible: {msg}");

        let (status, Json(body)) =
            controller_error_response(ControllerError::RateLimited, Some("0".into()), no_zones());
        let msg = load_error_message(status.as_u16(), &body.to_string());
        assert!(msg.contains("rate limited"), "{msg}");
        assert!(
            msg.contains('0'),
            "the remaining allowance must reach the user: {msg}"
        );

        // An empty body still degrades to the bare status, the old behavior,
        // rather than to a blank message.
        assert_eq!(load_error_message(502, ""), "HTTP 502");
    }

    #[test]
    fn rate_limited_body_carries_the_remaining_allowance_when_known() {
        let (s, Json(body)) =
            controller_error_response(ControllerError::RateLimited, Some("0".into()), no_zones());
        assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["rate_limit_remaining"], json!("0"));
        let hint = body["hint"].as_str().unwrap();
        assert!(hint.contains('0'));
        // The number is what the controller's LAST RESPONSE reported, which
        // is not necessarily what it would report now. Say "last reported",
        // never a present-tense claim about a live allowance.
        assert!(
            hint.contains("last reported"),
            "the hint must not assert a live allowance: {hint}"
        );
        // Unknown stays null, never a zero that reads as "exhausted".
        let (_, Json(body)) =
            controller_error_response(ControllerError::RateLimited, None, no_zones());
        assert!(
            body["rate_limit_remaining"].is_null(),
            "an unknown allowance must be null, not a sentinel"
        );
        assert!(body["hint"].as_str().is_some());
    }

    #[test]
    fn run_sequence_now_still_deserializes() {
        // The tombstone variant must stay parseable so stale clients get
        // the 410 body rather than a 422 deserialization error.
        let a: Action = serde_json::from_str(r#"{"kind":"run_sequence_now"}"#).unwrap();
        assert!(matches!(a, Action::RunSequenceNow));
    }

    // The manual API Run arms its shutoff deadline with the shared grace:
    // base 30s, widened to 90s for device-wide-stop clouds. Zero grace here
    // made the reaper device-stop a sibling the moment a run's planned end
    // passed.
    #[test]
    fn manual_run_deadline_carries_the_shared_grace() {
        assert_eq!(manual_run_deadline(1000, 600, true), 1000 + 600 + 30);
        assert_eq!(manual_run_deadline(1000, 600, false), 1000 + 600 + 90);
    }

    fn mem_stores() -> (crate::persistence::ActiveRunsStore, RunsStore) {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut c).unwrap();
        let conn = std::sync::Arc::new(tokio::sync::Mutex::new(c));
        (
            crate::persistence::ActiveRunsStore::new(conn.clone()),
            RunsStore::new(conn),
        )
    }

    fn manual_row(zone: &str, controller: &str, start: i64) -> crate::persistence::runs::NewRun {
        crate::persistence::runs::NewRun {
            zone_slug: zone.into(),
            start_epoch: start,
            source: "manual".into(),
            controller_id: controller.into(),
            planned_duration_s: 1200,
            skip_reason: None,
            et0_mm: None,
            etc_mm: None,
            cycle_index: None,
            cycle_count: None,
        }
    }

    // Finding-7 scenario: front is running a manual 20 minute run on the
    // cloud device; the user stops BACK (a sibling zone). The device-wide
    // stop halted front too, so front's pre-written full-duration row must
    // truncate and every armed deadline on that device must clear, while
    // another controller's open run keeps its credit and its backstop.
    #[tokio::test]
    async fn device_wide_stop_bookkeeping_covers_the_whole_device() {
        let (ar, rs) = mem_stores();
        ar.arm("front".into(), "rachio_main".into(), 1000, 2290)
            .await
            .unwrap();
        ar.arm("back".into(), "rachio_main".into(), 1000, 2290)
            .await
            .unwrap();
        ar.arm("garden".into(), "os_main".into(), 1000, 2230)
            .await
            .unwrap();
        rs.insert_completed(manual_row("front", "rachio_main", 1000), 2200, 1200, None)
            .await
            .unwrap();
        rs.insert_completed(manual_row("garden", "os_main", 1000), 2200, 1200, None)
            .await
            .unwrap();

        stop_bookkeeping(Some(&ar), Some(&rs), "rachio_main", "back", true, 1300).await;

        let left = ar.due(10_000).await.unwrap();
        assert_eq!(left.len(), 1, "only the other controller's row survives");
        assert_eq!(left[0].zone_slug, "garden");
        let rows = rs.window(0, 10_000).await.unwrap();
        let front = rows.iter().find(|r| r.zone_slug == "front").unwrap();
        assert_eq!(
            front.duration_s,
            Some(300),
            "the sibling's open manual row shrinks to the real span"
        );
        let garden = rows.iter().find(|r| r.zone_slug == "garden").unwrap();
        assert_eq!(garden.duration_s, Some(1200), "other controller untouched");
    }

    // Per-zone-stop controllers keep the zone-scoped bookkeeping.
    #[tokio::test]
    async fn zone_scoped_stop_bookkeeping_touches_only_the_stopped_zone() {
        let (ar, rs) = mem_stores();
        ar.arm("front".into(), "os_main".into(), 1000, 2230)
            .await
            .unwrap();
        ar.arm("back".into(), "os_main".into(), 1000, 2230)
            .await
            .unwrap();
        rs.insert_completed(manual_row("front", "os_main", 1000), 2200, 1200, None)
            .await
            .unwrap();
        rs.insert_completed(manual_row("back", "os_main", 1000), 2200, 1200, None)
            .await
            .unwrap();

        stop_bookkeeping(Some(&ar), Some(&rs), "os_main", "back", false, 1300).await;

        let left = ar.due(10_000).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].zone_slug, "front", "front's deadline stays armed");
        let rows = rs.window(0, 10_000).await.unwrap();
        let back = rows.iter().find(|r| r.zone_slug == "back").unwrap();
        assert_eq!(back.duration_s, Some(300));
        let front = rows.iter().find(|r| r.zone_slug == "front").unwrap();
        assert_eq!(front.duration_s, Some(1200), "front's run keeps its span");
    }

    // ---- The soil opt-in offer's eligibility ----

    /// A budget row as the refresher publishes it: `model` is the
    /// resolved per-zone tag, `depletion` present only when the shadow
    /// plan resolved (not starved, not a degraded tick).
    fn invite_row(
        slug: &str,
        model: &str,
        depletion: Option<f64>,
        soil_planned: u32,
        today: u32,
    ) -> crate::ha::snapshot::WaterBudget {
        crate::ha::snapshot::WaterBudget {
            zone_slug: slug.into(),
            zone_name: slug.into(),
            scheduling_model: model.into(),
            soil_depletion_mm: depletion,
            soil_planned_seconds: soil_planned,
            today_seconds: today,
            ..Default::default()
        }
    }

    /// The install the offer exists for: weekly engine default, at
    /// least one weekly-governed zone with a resolved shadow plan. The
    /// counts are what the popup names: live deficits, and zones where
    /// the two models disagree about watering today, both directions.
    #[test]
    fn weekly_default_with_a_resolved_shadow_is_offered_with_counts() {
        let budgets = vec![
            // Deficit, and soil would water where weekly does not.
            invite_row("back", "weekly", Some(6.0), 900, 0),
            // Deficit, both models agree (both water).
            invite_row("front", "weekly", Some(3.0), 600, 600),
            // No deficit, weekly waters where soil would not: disagrees.
            invite_row("side", "weekly", Some(0.0), 0, 300),
            // Pinned to soil already: not part of the offer's counts.
            invite_row("beds", "soil", Some(8.0), 700, 700),
            // Starved shadow: no block, contributes nothing.
            invite_row("strip", "weekly", None, 0, 0),
        ];
        let facts = soil_invite_facts(true, &budgets).expect("offered");
        assert_eq!(
            facts,
            SoilInviteFacts {
                shadow_zones: 3,
                deficit_zones: 2,
                differs_today: 2,
            }
        );
    }

    /// An engine default of soil IS the opt-in: the offer retires with
    /// no record needed, whatever the rows say.
    #[test]
    fn a_soil_engine_default_is_never_offered() {
        let budgets = vec![invite_row("back", "weekly", Some(6.0), 900, 0)];
        assert_eq!(soil_invite_facts(false, &budgets), None);
    }

    /// Every zone pinned to soil individually: nothing left to offer,
    /// again with no record needed.
    #[test]
    fn every_zone_pinned_to_soil_is_never_offered() {
        let budgets = vec![
            invite_row("back", "soil", Some(6.0), 900, 900),
            invite_row("front", "soil", Some(3.0), 600, 600),
        ];
        assert_eq!(soil_invite_facts(true, &budgets), None);
    }

    /// Evidence-starved everywhere (every weekly row publishes absence):
    /// the offer has nothing to show yet, so it waits rather than
    /// firing empty. Same shape covers the no-zones install.
    #[test]
    fn a_shadow_starved_everywhere_is_not_offered_yet() {
        let budgets = vec![
            invite_row("back", "weekly", None, 0, 0),
            invite_row("front", "weekly", None, 0, 900),
        ];
        assert_eq!(soil_invite_facts(true, &budgets), None);
        assert_eq!(soil_invite_facts(true, &[]), None);
    }
}
