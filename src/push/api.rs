// HTTP API for /api/push (also mounted under /api/v1/push):
//
//   GET  /api/push/vapid-key   -> { public_key: "<base64url>" } or 503
//   POST /api/push/subscribe   -> { ok: true } (idempotent upsert)
//   POST /api/push/unsubscribe -> { ok: true, removed: <n> }
//   GET  /api/push/status      -> delivery readiness, enabled, timezone
//   POST /api/push/preferences/read -> this subscription's choices
//   POST /api/push/preferences -> validated replacement choices
//
// Push subscriptions are stored alongside the irrigation history in the
// same SQLite file. If the history db wasn't openable at startup, the
// endpoints respond 503; the rest of the app stays up.
//
// GATING: the state-changing subscribe/unsubscribe POSTs are in
// the PRIVILEGED set (auth::middleware::is_privileged_path), so in the
// shipped Disabled default an anonymous internet caller cannot seed
// subscriptions; an IP-vouched LAN/loopback caller (or an authenticated
// owner) still reaches them. The vapid-key GET stays public (the frontend
// needs it before any subscription exists). The gate runs in the middleware
// layer, not here, mirroring how POST /irrigation/action is gated.

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
        .route("/status", get(status))
        .route("/preferences/read", post(read_preferences))
        .route("/preferences", post(save_preferences))
        .with_state(state)
}

async fn get_vapid_key() -> impl IntoResponse {
    let key = crate::push::dispatcher::vapid_public_key();
    match key {
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
        preferences: Default::default(),
    };
    match store::upsert(conn, sub).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

async fn status(State(state): State<PushState>) -> impl IntoResponse {
    Json(json!({
        "ready": state.history_conn.is_some() && crate::push::dispatcher::vapid_public_key().is_some(),
        "enabled": crate::push::dispatcher::sinks_config().web_push_enabled,
        "timezone": crate::timeutil::timezone_label(),
    }))
}

#[derive(Deserialize)]
struct PreferencesBody {
    endpoint: String,
    keys: SubscribeKeys,
    preferences: crate::notification_preferences::PushPreferences,
}

async fn read_preferences(
    State(state): State<PushState>,
    Json(body): Json<SubscribeBody>,
) -> impl IntoResponse {
    preferences_response(state, body, None).await
}

async fn save_preferences(
    State(state): State<PushState>,
    Json(body): Json<PreferencesBody>,
) -> impl IntoResponse {
    if let Err(error) = body.preferences.validate() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": error })),
        );
    }
    preferences_response(
        state,
        SubscribeBody {
            endpoint: body.endpoint,
            keys: body.keys,
        },
        Some(body.preferences),
    )
    .await
}

async fn preferences_response(
    state: PushState,
    body: SubscribeBody,
    update: Option<crate::notification_preferences::PushPreferences>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(conn) = state.history_conn else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Notification storage is unavailable." })),
        );
    };
    match store::preferences(
        conn,
        body.endpoint,
        body.keys.p256dh,
        body.keys.auth,
        update,
    )
    .await
    {
        Ok(Some(prefs)) => (StatusCode::OK, Json(json!({ "preferences": prefs }))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Reconnect this device to manage its notifications." })),
        ),
        Err(error) => {
            tracing::warn!(%error, "could not read or save push preferences");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Could not save or load notification choices. Try again." })),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn preferences_require_matching_device_keys_and_validate_before_save() {
        let mut db = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut db).unwrap();
        let app = router(PushState {
            history_conn: Some(Arc::new(Mutex::new(db))),
        });
        let body = json!({ "endpoint": "https://push.example/phone", "keys": { "p256dh": "key", "auth": "secret" } });
        let request = |path: &str, value: &serde_json::Value| {
            Request::builder()
                .method("POST")
                .uri(path)
                .header("Content-Type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(request("/subscribe", &body))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let mut wrong = body.clone();
        wrong["keys"]["auth"] = json!("wrong");
        assert_eq!(
            app.clone()
                .oneshot(request("/preferences/read", &wrong))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let mut update = body.clone();
        update["preferences"] =
            serde_json::to_value(crate::notification_preferences::PushPreferences::default())
                .unwrap();
        update["preferences"]["daily_outlook"] = json!({ "enabled": true, "time": "00:00" });
        assert_eq!(
            app.clone()
                .oneshot(request("/preferences", &update))
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        update["preferences"]["daily_outlook"]["time"] = json!("09:30");
        assert_eq!(
            app.clone()
                .oneshot(request("/preferences", &update))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/subscribe", &body))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let response = app
            .oneshot(request("/preferences/read", &body))
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 10000).await.unwrap()).unwrap();
        assert_eq!(
            value["preferences"]["daily_outlook"],
            update["preferences"]["daily_outlook"]
        );
        assert!(
            value.get("endpoint").is_none(),
            "preferences responses must not list subscriptions"
        );
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
