// /api/auth + /api/v1/auth router.
//
//   GET  /status         -> { mode, setup_complete, authenticated }   (public)
//   POST /setup          -> create the FIRST account + session cookie (public until a user exists)
//   POST /login          -> session cookie                            (public)
//   POST /logout         -> clear session
//   GET  /session        -> current user
//   GET  /tokens         -> list API tokens
//   POST /tokens {name}  -> { token } shown exactly once
//   DELETE /tokens/{id}  -> revoke
//
// Setup notes: POST /setup also flips config auth.mode to "required"
// when the config exists (new installs get it via the wizard's
// finalize; an existing install enabling auth from Settings goes
// through here too). Cookie is HttpOnly SameSite=Lax, Secure added
// when the request arrived over https (x-forwarded-proto).

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderValue, Request, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use serde::Deserialize;

use crate::auth::{middleware::client_ip, AuthRuntime, RequestIdentity, SESSION_COOKIE};
use crate::config::FileConfigStore;
use crate::ports::config_store::ConfigStore;

#[derive(Clone)]
pub struct AuthApiState {
    pub rt: Arc<AuthRuntime>,
    pub cfg_store: Arc<FileConfigStore>,
}

pub fn router(state: AuthApiState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/setup", post(post_setup))
        .route("/login", post(post_login))
        .route("/logout", post(post_logout))
        .route("/session", get(get_session))
        .route("/tokens", get(get_tokens).post(post_token))
        .route("/tokens/{id}", delete(delete_token))
        .with_state(state)
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn session_cookie_header(req: &Request<Body>, value: &str, max_age_days: u32) -> HeaderValue {
    let https = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let secure = if https { "; Secure" } else { "" };
    let max_age = u64::from(max_age_days) * 86_400;
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn clear_cookie_header() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn get_status(State(s): State<AuthApiState>, req: Request<Body>) -> Response {
    let policy = s.rt.policy.load();
    let setup_complete =
        s.rt.setup_complete
            .load(std::sync::atomic::Ordering::Relaxed);
    let authenticated = matches!(
        req.extensions().get::<RequestIdentity>(),
        Some(RequestIdentity::User(_)) | Some(RequestIdentity::TrustedNetwork)
    );
    Json(serde_json::json!({
        "mode": if policy.required { "required" } else { "disabled" },
        "setup_complete": setup_complete,
        "authenticated": authenticated || !policy.required,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

async fn post_setup(State(s): State<AuthApiState>, req: Request<Body>) -> Response {
    if let Some(ip) = client_ip(&req, &s.rt.policy.load().trusted_proxies) {
        if !s.rt.allow_login_attempt(ip).await {
            return err(
                StatusCode::TOO_MANY_REQUESTS,
                "too many attempts; wait a minute",
            );
        }
    }
    // Only the FIRST account can be created unauthenticated.
    match s.rt.store.user_count().await {
        Ok(0) => {}
        Ok(_) => return err(StatusCode::CONFLICT, "an account already exists"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
    let (body, parts_req): (Credentials, Request<Body>) = match parse_body_keep(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let user_id = match s.rt.store.create_user(&body.username, &body.password).await {
        Ok(id) => id,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    s.rt.setup_complete
        .store(true, std::sync::atomic::Ordering::Relaxed);

    // Flip policy to required: in-memory immediately, and persist into
    // the config when one exists (fresh wizard installs write it on
    // apply; this covers enable-from-Settings on an existing install).
    let mut policy =
        crate::auth::middleware::AuthRuntime::policy_from_cfg(&crate::config::schema::AuthConfig {
            mode: crate::config::schema::AuthMode::Required,
            ..Default::default()
        });
    if let Ok(cfg) = s.cfg_store.load().await {
        let mut cfg = cfg;
        cfg.auth.mode = crate::config::schema::AuthMode::Required;
        policy = crate::auth::middleware::AuthRuntime::policy_from_cfg(&cfg.auth);
        if s.cfg_store.is_initialized() {
            let _ = s.cfg_store.save(&cfg).await;
        }
    }
    s.rt.policy.store(Arc::new(policy));

    let ttl = s.rt.policy.load().session_ttl_days;
    match s.rt.store.create_session(user_id, ttl, None).await {
        Ok(cookie_value) => {
            let mut resp = (
                StatusCode::CREATED,
                Json(serde_json::json!({ "ok": true, "user_id": user_id })),
            )
                .into_response();
            resp.headers_mut().insert(
                header::SET_COOKIE,
                session_cookie_header(&parts_req, &cookie_value, ttl),
            );
            resp
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn post_login(State(s): State<AuthApiState>, req: Request<Body>) -> Response {
    if let Some(ip) = client_ip(&req, &s.rt.policy.load().trusted_proxies) {
        if !s.rt.allow_login_attempt(ip).await {
            return err(
                StatusCode::TOO_MANY_REQUESTS,
                "too many attempts; wait a minute",
            );
        }
    }
    let ua = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(180).collect::<String>());
    let (body, parts_req): (Credentials, Request<Body>) = match parse_body_keep(req).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let user_id = match s
        .rt
        .store
        .verify_login(&body.username, &body.password)
        .await
    {
        Ok(id) => id,
        Err(_) => return err(StatusCode::UNAUTHORIZED, "invalid credentials"),
    };
    let ttl = s.rt.policy.load().session_ttl_days;
    match s.rt.store.create_session(user_id, ttl, ua).await {
        Ok(cookie_value) => {
            let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
            resp.headers_mut().insert(
                header::SET_COOKIE,
                session_cookie_header(&parts_req, &cookie_value, ttl),
            );
            resp
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn post_logout(State(s): State<AuthApiState>, req: Request<Body>) -> Response {
    if let Some(cookie) = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let (k, v) = c.trim().split_once('=')?;
                (k == SESSION_COOKIE).then(|| v.trim().to_string())
            })
        })
    {
        let _ = s.rt.store.delete_session(&cookie).await;
    }
    let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, clear_cookie_header());
    resp
}

async fn get_session(State(s): State<AuthApiState>, req: Request<Body>) -> Response {
    match req.extensions().get::<RequestIdentity>() {
        Some(RequestIdentity::User(id)) => match s.rt.store.get_user(*id).await {
            Ok(Some(u)) => Json(serde_json::json!({
                "user": { "id": u.id, "username": u.username, "role": u.role }
            }))
            .into_response(),
            _ => err(StatusCode::UNAUTHORIZED, "unknown user"),
        },
        Some(RequestIdentity::TrustedNetwork) => {
            Json(serde_json::json!({ "user": null, "trusted_network": true })).into_response()
        }
        _ => {
            let policy = s.rt.policy.load();
            if policy.required {
                err(StatusCode::UNAUTHORIZED, "not signed in")
            } else {
                Json(serde_json::json!({ "user": null, "auth_disabled": true })).into_response()
            }
        }
    }
}

async fn get_tokens(State(s): State<AuthApiState>) -> Response {
    match s.rt.store.list_api_tokens().await {
        Ok(list) => Json(serde_json::json!({ "tokens": list })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

async fn post_token(State(s): State<AuthApiState>, req: Request<Body>) -> Response {
    // Token creation needs a real signed-in user (or disabled mode /
    // trusted network, where we attribute to the first account).
    let user_id = match req.extensions().get::<RequestIdentity>() {
        Some(RequestIdentity::User(id)) => *id,
        _ => 1,
    };
    let body: NewToken = match parse_body(req).await {
        Ok((b, _)) => b,
        Err(resp) => return resp,
    };
    // With zero users (auth disabled, never set up), tokens cannot be
    // attributed; create requires an account first.
    match s.rt.store.user_count().await {
        Ok(0) => {
            return err(
                StatusCode::CONFLICT,
                "create an owner account first (Settings -> Account)",
            )
        }
        Ok(_) => {}
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
    match s.rt.store.create_api_token(user_id, &body.name).await {
        Ok(token) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "token": token, "note": "shown once; store it now" })),
        )
            .into_response(),
        Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, e),
    }
}

async fn delete_token(State(s): State<AuthApiState>, Path(id): Path<i64>) -> Response {
    match s.rt.store.revoke_api_token(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Read + JSON-parse the body, consuming the request.
async fn parse_body<T: serde::de::DeserializeOwned>(
    req: Request<Body>,
) -> Result<(T, ()), Response> {
    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("body: {e}")))?;
    let v = serde_json::from_slice::<T>(&bytes)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("json: {e}")))?;
    Ok((v, ()))
}

/// Same, but hands back a header-only request clone for cookie attrs.
async fn parse_body_keep<T: serde::de::DeserializeOwned>(
    req: Request<Body>,
) -> Result<(T, Request<Body>), Response> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024)
        .await
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("body: {e}")))?;
    let v = serde_json::from_slice::<T>(&bytes)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("json: {e}")))?;
    Ok((v, Request::from_parts(parts, Body::empty())))
}
