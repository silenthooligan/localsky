// /api/wizard router. Drives the first-run setup flow.
//
// Endpoints:
//   GET    /api/wizard/draft           -> current draft (or default)
//   PUT    /api/wizard/draft           -> save draft
//   DELETE /api/wizard/draft           -> clear draft (cancel + restart)
//   POST   /api/wizard/apply           -> validate + write /data/localsky.toml
//   POST   /api/wizard/test_source     -> dispatch through source adapter (Phase 6)
//   POST   /api/wizard/test_controller -> dispatch through controller adapter (Phase 5)
//   POST   /api/wizard/scan_zones      -> mDNS + controller probe (Phase 5)
//   GET    /api/wizard/geocode?q=...   -> server-side Nominatim proxy
//
// Wizard endpoints are only mounted when /data/localsky.toml does not yet
// exist; the setup-gate middleware redirects normal routes to /setup.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::config::wizard::{WizardDraft, WizardError, WizardStore};
use crate::config::FileConfigStore;
use crate::ports::config_store::ConfigStore;

#[derive(Clone)]
pub struct WizardApiState {
    pub draft_store: Arc<WizardStore>,
    pub config_store: Arc<FileConfigStore>,
    /// Present when the identity store booted; lets apply persist
    /// auth.mode = required when the wizard created an owner account.
    pub auth_rt: Option<Arc<crate::auth::AuthRuntime>>,
    /// Live Tempest store: passive discovery (a broadcasting hub shows
    /// up here without any probe).
    pub tempest_store: Option<Arc<crate::tempest::state::TempestStore>>,
}

pub fn router(state: WizardApiState) -> Router {
    Router::new()
        .route("/draft", get(get_draft).put(put_draft))
        .route("/draft", delete(delete_draft))
        .route("/apply", post(post_apply))
        .route("/test_source", post(post_test_source))
        .route("/test_controller", post(post_test_controller))
        .route("/test_llm", post(post_test_llm))
        .route("/scan_zones", post(post_scan_zones))
        .route("/discover", get(get_discover))
        .route("/geocode", get(get_geocode))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
    detail: Option<String>,
}

fn wizard_err(e: WizardError) -> (StatusCode, Json<ApiError>) {
    let code = match &e {
        WizardError::NotPresent => StatusCode::NOT_FOUND,
        WizardError::LicenseNotAccepted | WizardError::Validation(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        code,
        Json(ApiError {
            error: "wizard_error".into(),
            detail: Some(e.to_string()),
        }),
    )
}

async fn get_draft(State(s): State<WizardApiState>) -> impl IntoResponse {
    let store = s.draft_store.clone();
    let res = tokio::task::spawn_blocking(move || store.load()).await;
    match res {
        Ok(Ok(d)) => Json(d).into_response(),
        Ok(Err(WizardError::NotPresent)) => Json(WizardDraft::default()).into_response(),
        Ok(Err(e)) => wizard_err(e).into_response(),
        Err(e) => wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    }
}

async fn put_draft(
    State(s): State<WizardApiState>,
    Json(draft): Json<WizardDraft>,
) -> impl IntoResponse {
    let store = s.draft_store.clone();
    let res = tokio::task::spawn_blocking(move || store.save(&draft)).await;
    match res {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => wizard_err(e).into_response(),
        Err(e) => wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    }
}

async fn delete_draft(State(s): State<WizardApiState>) -> impl IntoResponse {
    let store = s.draft_store.clone();
    let res = tokio::task::spawn_blocking(move || store.clear()).await;
    match res {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => wizard_err(e).into_response(),
        Err(e) => wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    }
}

async fn post_apply(State(s): State<WizardApiState>) -> impl IntoResponse {
    let draft_store = s.draft_store.clone();
    let load_res = tokio::task::spawn_blocking(move || draft_store.load()).await;
    let mut draft = match load_res {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return wizard_err(e).into_response(),
        Err(e) => return wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    };
    // Pre-apply checks, then fill the defaults the wizard promises for
    // skipped steps (sources) before writing.
    if let Err(e) = s.draft_store.validate_for_apply(&draft) {
        return wizard_err(e).into_response();
    }
    WizardStore::finalize_for_apply(&mut draft);
    // The Account step creates the owner directly in SQLite; reflect it
    // in the written policy so login is required from first boot.
    if let Some(rt) = &s.auth_rt {
        if rt.setup_complete.load(std::sync::atomic::Ordering::Relaxed) {
            draft.config.auth.mode = crate::config::schema::AuthMode::Required;
        }
    }
    // Write the config atomically.
    match s.config_store.save(&draft.config).await {
        Ok(v) => {
            // Drop the draft. Best-effort; if cleanup fails the next boot
            // can still resume from the draft, which is harmless.
            let ds = s.draft_store.clone();
            let _ = tokio::task::spawn_blocking(move || ds.clear()).await;
            Json(v).into_response()
        }
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "config_save_failed".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

// ---- Adapter test endpoints. Real impls land alongside Phase 5/6. ----

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TestSourceBody {
    pub source: crate::config::schema::SourceEntry,
}

async fn post_test_source(
    State(_s): State<WizardApiState>,
    Json(body): Json<TestSourceBody>,
) -> impl IntoResponse {
    // The config deserialized into a typed SourceEntry, so it's structurally
    // valid. Receiver sources (Ecowitt LAN, webhook) confirm by live readings
    // on the Sensors hub once the device posts; polled sources confirm within
    // one cycle after apply. A live probe per kind is a follow-up.
    serde_json::json!({
        "ok": true,
        "id": body.source.id,
        "note": "config valid; confirm live readings on the Sensors hub after applying",
    })
    .to_string()
}

#[derive(Debug, Deserialize)]
struct TestControllerBody {
    pub controller: crate::config::schema::ControllerEntry,
}

async fn post_test_controller(
    State(_s): State<WizardApiState>,
    Json(body): Json<TestControllerBody>,
) -> impl IntoResponse {
    match crate::runtime::build_test_controller(&body.controller) {
        Ok(c) => match c.status().await {
            Ok(st) => Json(serde_json::json!({
                "ok": true,
                "reachable": st.reachable,
                "master_enabled": st.master_enabled,
                "water_level_pct": st.water_level_pct,
                "zone_count": st.zone_states.len(),
                "firmware": st.firmware,
            }))
            .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "controller_unreachable".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "controller_unsupported".into(),
                detail: Some(e),
            }),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct TestLlmBody {
    pub llm: crate::config::schema::LlmConfig,
}

async fn post_test_llm(
    State(_s): State<WizardApiState>,
    Json(body): Json<TestLlmBody>,
) -> impl IntoResponse {
    let provider = match crate::runtime::build_llm_from(&body.llm).await {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "llm_unreachable".into(),
                    detail: Some(
                        "no provider responded; for Auto, make sure Ollama / llama.cpp / LM Studio is running on this host".into(),
                    ),
                }),
            )
                .into_response()
        }
    };
    match provider.health().await {
        Ok(h) => Json(serde_json::json!({
            "ok": h.reachable,
            "provider": provider.id(),
            "model_loaded": h.model_loaded,
            "provider_version": h.provider_version,
            "detail": h.last_error,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "llm_unreachable".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

async fn post_scan_zones(
    State(_s): State<WizardApiState>,
    Json(body): Json<TestControllerBody>,
) -> impl IntoResponse {
    match crate::runtime::build_test_controller(&body.controller) {
        Ok(c) => match c.discover_zones().await {
            Ok(zones) => Json(serde_json::json!({ "zones": zones })).into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "zone_scan_failed".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "controller_unsupported".into(),
                detail: Some(e),
            }),
        )
            .into_response(),
    }
}

// ---- Network discovery. One aggregated sweep for the wizard. ----

/// GET /api/wizard/discover -> everything findable on the LAN right now:
/// passive Tempest (any hub already broadcasting on UDP 50222), Ecowitt
/// gateways (UDP broadcast probe), OpenSprinkler controllers (HTTP /24
/// sweep). User-initiated only; total wall time a few seconds.
async fn get_discover(State(s): State<WizardApiState>) -> impl IntoResponse {
    let tempest = s.tempest_store.as_ref().map(|store| {
        let snap = store.snapshot();
        let now = chrono::Utc::now().timestamp();
        let fresh = snap.last_packet_epoch > 0 && now - snap.last_packet_epoch < 300;
        serde_json::json!({
            "detected": fresh,
            "hub_serial": if snap.hub_serial.is_empty() { serde_json::Value::Null } else { snap.hub_serial.clone().into() },
            "last_seen_epoch": snap.last_packet_epoch,
        })
    });

    let (ecowitt, opensprinkler) = tokio::join!(
        crate::discovery::ecowitt::discover_ecowitt(std::time::Duration::from_secs(3)),
        crate::discovery::opensprinkler::discover_opensprinkler(std::time::Duration::from_millis(
            1500
        )),
    );

    Json(serde_json::json!({
        "tempest": tempest,
        "ecowitt": ecowitt,
        "opensprinkler": opensprinkler,
    }))
}

// ---- Geocode proxy. Lets the location step do address -> lat/lon. ----

#[derive(Debug, Deserialize)]
struct GeocodeQuery {
    q: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeocodeResult {
    display_name: String,
    lat: String,
    lon: String,
}

async fn get_geocode(Query(q): Query<GeocodeQuery>) -> impl IntoResponse {
    let url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=json&limit=5",
        urlencode(&q.q)
    );
    let client = reqwest::Client::new();
    let res = client
        .get(&url)
        // Nominatim ToS requires a meaningful User-Agent identifying the
        // operator. The deployment name + URL is reasonable; users can
        // override via wizard customization.
        .header("User-Agent", "LocalSky setup wizard")
        .send()
        .await;
    match res {
        Ok(r) => match r.json::<Vec<GeocodeResult>>().await {
            Ok(results) => Json(results).into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "geocode_parse_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "geocode_transport_error".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

fn urlencode(s: &str) -> String {
    // Lightweight encoder for query string values. Nominatim accepts most
    // punctuation as-is so we only escape the obvious offenders.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
