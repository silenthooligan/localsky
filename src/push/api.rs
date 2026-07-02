// HTTP API for /api/push. Three endpoints:
//
//   GET  /api/push/vapid-key   -> { public_key: "<base64url>" } or 503
//   POST /api/push/subscribe   -> { ok: true } (idempotent upsert)
//   POST /api/push/unsubscribe -> { ok: true, removed: <n> }
//
// Push subscriptions are stored alongside the irrigation history in the
// same SQLite file. If the history db wasn't openable at startup, the
// endpoints respond 503; the rest of the app stays up.
//
// GATING (LS-REC-05): the state-changing subscribe/unsubscribe POSTs are in
// the PRIVILEGED set (auth::middleware::is_privileged_path), so in the
// shipped Disabled default an anonymous internet caller cannot seed
// subscriptions; an IP-vouched LAN/loopback caller (or an authenticated
// owner) still reaches them. The vapid-key GET stays public (the frontend
// needs it before any subscription exists). The gate runs in the middleware
// layer, not here, mirroring how POST /irrigation/action is gated.

use crate::push::dispatcher::vapid_public_key;
use crate::push::store::{self, StoredSubscription};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct PushState {
    pub history_conn: Option<Arc<Mutex<Connection>>>,
}

pub fn router(state: PushState) -> Router {
    Router::new()
        .route("/vapid-key", get(get_vapid_key))
        .route("/subscribe", post(subscribe))
        .route("/unsubscribe", post(unsubscribe))
        .with_state(state)
}

async fn get_vapid_key() -> impl IntoResponse {
    match vapid_public_key() {
        Some(k) => (StatusCode::OK, Json(json!({ "public_key": k }))),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "vapid not configured" })),
        ),
    }
}

#[derive(Deserialize)]
struct SubscribeBody {
    endpoint: String,
    keys: SubscribeKeys,
}

#[derive(Deserialize)]
struct SubscribeKeys {
    p256dh: String,
    auth: String,
}

async fn subscribe(
    State(state): State<PushState>,
    Json(body): Json<SubscribeBody>,
) -> impl IntoResponse {
    let Some(conn) = state.history_conn else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "history db not configured" })),
        );
    };
    let sub = StoredSubscription {
        endpoint: body.endpoint,
        p256dh: body.keys.p256dh,
        auth: body.keys.auth,
    };
    match store::upsert(conn, sub).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

#[derive(Deserialize)]
struct UnsubscribeBody {
    endpoint: String,
}

async fn unsubscribe(
    State(state): State<PushState>,
    Json(body): Json<UnsubscribeBody>,
) -> impl IntoResponse {
    let Some(conn) = state.history_conn else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "history db not configured" })),
        );
    };
    match store::delete_endpoint(conn, body.endpoint).await {
        Ok(n) => (StatusCode::OK, Json(json!({ "ok": true, "removed": n }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}
