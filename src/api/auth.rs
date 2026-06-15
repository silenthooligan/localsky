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
// Setup notes: POST /setup flips auth.mode to "required" ONLY when it can
// be persisted (ACCT-02). If a config exists (enable-from-Settings on a
// configured install), it is loaded, set to required, saved, and the live
// policy flipped only after the save succeeds. If there is NO config yet
// (the wizard's Account step runs before POST /wizard/apply writes the
// file), the account + session are created but the policy is left
// disabled here; the wizard's apply persists required and flips the live
// policy then, so an in-memory required never exists without on-disk
// backing. Cookie is HttpOnly SameSite=Lax; Secure is added when the
// request arrived over https (x-forwarded-proto=https) AND, defensively,
// whenever a TLS-fronting proxy is in front (any X-Forwarded-* header) or
// the peer is non-local, so a TLS proxy that drops the proto header still
// yields a Secure cookie. Secure is omitted for a genuine bare-LAN
// plain-HTTP hit (loopback/private peer, no proxy headers) AND for a
// plain-HTTP HA Supervisor ingress hit (X-Ingress-Path present but
// X-Forwarded-Proto not https), so both installs keep a working cookie
// (WH-04). See `should_set_secure`.

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

/// Decide whether the session cookie should carry the `Secure` attribute
/// (WH-04). The goal is robust HTTPS protection without breaking a
/// pure-HTTP LAN install OR a plain-HTTP HA Supervisor ingress:
///
///   1. `X-Forwarded-Proto: https` -> Secure. The TLS proxy told us.
///   2. HA Supervisor ingress reached over plain HTTP (`X-Ingress-Path`
///      present, but X-Forwarded-Proto is NOT https) -> do NOT force
///      Secure. The Supervisor proxies ingress over HTTP on a HAOS box
///      without TLS in front, sending X-Ingress-Path with
///      X-Forwarded-Proto: http (or none). Forcing Secure there makes the
///      browser silently drop the cookie -> login lockout in Required mode
///      on an HTTP HAOS. Fall through (omit Secure) like the bare-LAN case.
///   3. Otherwise, if a proxy is in front (any OTHER forwarding header) ->
///      Secure. A TLS proxy that strips/omits X-Forwarded-Proto still lands
///      here, so default to Secure rather than emitting a cleartext cookie.
///   4. Otherwise DEFAULT to Secure, UNLESS the request is plainly a
///      direct local/loopback plain-HTTP hit (no proxy headers AND the TCP
///      peer is loopback or a private/ULA address). That is the only other
///      case where a Secure cookie would be dropped by the browser and lock
///      the operator out (http://localhost / http://10.0.0.x, no TLS).
///
/// The payoff: a self-hoster behind a TLS-terminating proxy that does NOT
/// forward `X-Forwarded-Proto` still gets a Secure cookie; a flat-LAN
/// pure-HTTP install keeps a working (non-Secure) cookie; AND a plain-HTTP
/// HAOS ingress keeps a working cookie instead of locking the operator out.
/// Secure is only ever SET when the edge is provably https.
fn should_set_secure(req: &Request<Body>) -> bool {
    let header_str = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };

    let xfp = header_str("x-forwarded-proto");
    let xfp_https = xfp
        .as_deref()
        .map(|p| p.eq_ignore_ascii_case("https"))
        .unwrap_or(false);

    // 1. Explicit https from the proxy: always Secure.
    if xfp_https {
        return true;
    }

    // 2. HA Supervisor ingress reached over plain HTTP. The Supervisor
    // proxies ingress on a HAOS box with NO TLS in front, sending
    // X-Ingress-Path alongside X-Forwarded-Proto: http (or none). We are
    // only here when xfp is NOT https (step 1 already returned), so an
    // X-Ingress-Path present now means "ingress without provable TLS". Do
    // NOT force Secure: that would make the browser drop the cookie and lock
    // the operator out of an HTTP HAOS in Required mode (WH-04). The only
    // authoritative TLS signal under ingress is xfp=https, handled in step
    // 1; absent that, the ingress edge is plain HTTP, so omit Secure exactly
    // like the bare-LAN case. (The Supervisor peer is a private Docker IP,
    // so the peer check below would reach the same answer; short-circuit it
    // here so other forwarding headers the Supervisor adds can't flip it.)
    if header_str("x-ingress-path").is_some() {
        return false;
    }

    // 3. Other proxy indicators: if any forwarding header is present, a
    // proxy is in front. A TLS proxy that strips/omits X-Forwarded-Proto
    // still lands here, so default to Secure rather than emitting a
    // cleartext cookie. (Ingress was handled above and never reaches here.)
    let behind_proxy = xfp.is_some()
        || header_str("x-forwarded-for").is_some()
        || header_str("x-forwarded-host").is_some()
        || header_str("forwarded").is_some();
    if behind_proxy {
        return true;
    }

    // No proxy in the path. Look at the raw TCP peer: only a direct
    // loopback / private-LAN peer over plain HTTP is the genuine "bare LAN
    // box" case where Secure must be omitted. Anything else (a public
    // peer hitting us directly without TLS, which should not happen, or an
    // unknown peer) errs on the side of Secure.
    match req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())
    {
        Some(ip) => !crate::auth::middleware::is_private_or_loopback(&ip),
        // No connect info (cannot prove a bare-LAN HTTP hit): default Secure.
        None => true,
    }
}

fn session_cookie_header(req: &Request<Body>, value: &str, max_age_days: u32) -> HeaderValue {
    let secure = if should_set_secure(req) {
        "; Secure"
    } else {
        ""
    };
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
    // RATE-01: key the limiter on the trusted-proxy-derived client IP, not
    // the raw socket peer. client_ip() returns the socket peer UNLESS the
    // peer is a configured trusted_proxy, in which case it walks
    // X-Forwarded-For to the last untrusted hop. So when trusted_proxies is
    // set, every real client is throttled independently behind the proxy
    // (one busy LAN client cannot exhaust the global bucket); when it is
    // not set, the limiter falls back to the proxy's own socket address
    // (documented in authentication.md as a reason to set trusted_proxies).
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

    // ACCT-02: only flip the in-memory policy to required when that
    // required mode can be PERSISTED. An in-memory "required" with no
    // on-disk auth.mode=required to back it is unrevertable-from-disk: a
    // fileless instance would be locked into required for the life of the
    // process with nothing the config refresher could read to undo it.
    //
    // Two cases:
    //   - Config exists (enable-from-Settings on a configured install):
    //     set auth.mode=required, persist it, and flip the in-memory
    //     policy ONLY after the save succeeds. A failed save leaves both
    //     the file and the policy as they were, never a half-applied
    //     required.
    //   - No config yet (the wizard's Account step runs BEFORE
    //     POST /wizard/apply writes localsky.toml): do NOT flip in-memory
    //     here. The account + session are still created so the wizard
    //     keeps working; the wizard's apply persists auth.mode=required
    //     and flips the live policy then, so required mode and its
    //     on-disk backing always appear together.
    if s.cfg_store.is_initialized() {
        match s.cfg_store.load().await {
            Ok(mut cfg) => {
                cfg.auth.mode = crate::config::schema::AuthMode::Required;
                match s.cfg_store.save(&cfg).await {
                    Ok(_) => {
                        let policy =
                            crate::auth::middleware::AuthRuntime::policy_from_cfg(&cfg.auth);
                        s.rt.policy.store(Arc::new(policy));
                    }
                    Err(e) => {
                        // Persistence failed: leave the policy alone rather
                        // than stranding the instance in unbacked required.
                        tracing::error!(error = %e, "auth setup: could not persist auth.mode=required; leaving policy unchanged");
                        return err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "account created but enabling required auth failed to persist; check storage and retry from Settings",
                        );
                    }
                }
            }
            // Config existed a moment ago but is now unreadable: do not
            // flip to an unbacked required state.
            Err(e) => {
                tracing::error!(error = %e, "auth setup: config unreadable; not flipping policy to required");
            }
        }
    }

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
    // RATE-01: per-real-client limiting via the trusted-proxy-derived IP.
    // See post_setup for the full rationale (and authentication.md for the
    // operator-facing trusted_proxies guidance).
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
    // NOT USER-SCOPED: list_api_tokens returns EVERY non-revoked token across
    // all owners, not just the caller's. This is correct under the current
    // single-owner model (there is exactly one account, so "all tokens" ==
    // "the owner's tokens"). TODO(multi-user): before any multi-user / role
    // work lands, scope this to the authenticated RequestIdentity::User(id) (a
    // `user_id` filter on the query) so one owner cannot enumerate another's
    // tokens. The token-admin gate already guarantees a real owner identity
    // here; it just does not yet partition by which owner.
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
    // LS-API-06: the token-admin gate in auth::middleware guarantees this
    // handler is only reached with a real authenticated owner identity
    // (RequestIdentity::User), in BOTH auth modes and from any network
    // position. A trusted-network / loopback caller is NOT enough to mint a
    // token (unlike the config/backup privileged gate), so there is no
    // anonymous/trusted attribution path here anymore. The `_ => 1`
    // fallback is therefore defensive only (unreachable in practice).
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
    // NOT USER-SCOPED: revoke_api_token revokes by token id alone, with no
    // check that the token belongs to the calling owner. Fine under the
    // single-owner model (only one owner exists, so any token is the owner's).
    // TODO(multi-user): before multi-user / role work, require the token's
    // user_id to match the authenticated RequestIdentity::User(id) (or an admin
    // role) so one owner cannot revoke another's tokens by guessing ids.
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ConnectInfo;
    use std::net::SocketAddr;

    fn req(peer: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::builder().method("POST").uri("/api/v1/auth/login");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut r = b.body(Body::empty()).unwrap();
        r.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer.parse().unwrap(), 40000)));
        r
    }

    #[test]
    fn secure_set_when_xfp_https() {
        assert!(should_set_secure(&req(
            "203.0.113.5",
            &[("x-forwarded-proto", "https")]
        )));
        // Even a loopback peer: if the proxy says https, it is https.
        assert!(should_set_secure(&req(
            "127.0.0.1",
            &[("x-forwarded-proto", "https")]
        )));
    }

    #[test]
    fn secure_set_when_behind_proxy_without_proto() {
        // TLS proxy that strips X-Forwarded-Proto but leaves other
        // forwarding headers: still Secure (defends the common misconfig).
        assert!(should_set_secure(&req(
            "172.18.0.2",
            &[("x-forwarded-for", "203.0.113.9")]
        )));
        assert!(should_set_secure(&req(
            "172.18.0.2",
            &[("x-forwarded-host", "sky.example.com")]
        )));
        // An explicit http proto from a (non-ingress) proxy still means a
        // proxy is in front, so default to Secure (the edge is likely TLS).
        assert!(should_set_secure(&req(
            "172.18.0.2",
            &[("x-forwarded-proto", "http")]
        )));
    }

    #[test]
    fn secure_handling_for_haos_ingress() {
        // WH-04 / HAOS-over-HTTP: HA Supervisor ingress reached over plain
        // HTTP sends X-Ingress-Path with X-Forwarded-Proto: http (or none).
        // Forcing Secure there makes the browser drop the cookie -> login
        // lockout in Required mode. So:
        //   ingress + no proto       -> NOT Secure (plain-HTTP HAOS).
        assert!(!should_set_secure(&req(
            "172.30.32.2",
            &[("x-ingress-path", "/api/hassio_ingress/abc")]
        )));
        //   ingress + xfp=http       -> NOT Secure (the lockout case).
        assert!(!should_set_secure(&req(
            "172.30.32.2",
            &[
                ("x-ingress-path", "/api/hassio_ingress/abc"),
                ("x-forwarded-proto", "http"),
            ]
        )));
        //   ingress + other fwd hdrs but no https proto -> still NOT Secure
        //   (the Supervisor may add X-Forwarded-For/Host; only xfp=https
        //   proves TLS).
        assert!(!should_set_secure(&req(
            "172.30.32.2",
            &[
                ("x-ingress-path", "/api/hassio_ingress/abc"),
                ("x-forwarded-for", "172.30.32.1"),
                ("x-forwarded-host", "homeassistant.local"),
            ]
        )));
        //   ingress + xfp=https      -> Secure (HAOS fronted by TLS).
        assert!(should_set_secure(&req(
            "172.30.32.2",
            &[
                ("x-ingress-path", "/api/hassio_ingress/abc"),
                ("x-forwarded-proto", "https"),
            ]
        )));
    }

    #[test]
    fn secure_omitted_for_bare_lan_http() {
        // Pure LAN plain-HTTP install: no proxy headers, loopback or
        // private peer. Secure must be OMITTED or the cookie is dropped
        // and the operator is locked out (WH-04 must not break this).
        assert!(!should_set_secure(&req("127.0.0.1", &[])));
        assert!(!should_set_secure(&req("10.0.0.50", &[])));
        assert!(!should_set_secure(&req("10.0.0.5", &[])));
        assert!(!should_set_secure(&req("::1", &[])));
    }

    #[test]
    fn secure_set_for_direct_public_http_peer() {
        // A public peer reaching us directly over plain HTTP (shouldn't
        // happen, but if it does) gets Secure: erring safe.
        assert!(should_set_secure(&req("203.0.113.5", &[])));
    }

    #[test]
    fn cookie_header_shape() {
        let h = session_cookie_header(&req("127.0.0.1", &[]), "lss_abc", 30);
        let s = h.to_str().unwrap();
        assert!(s.contains("localsky_session=lss_abc"));
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Lax"));
        assert!(s.contains("Max-Age=2592000"));
        assert!(!s.contains("Secure")); // bare LAN http
        let h = session_cookie_header(
            &req("127.0.0.1", &[("x-forwarded-proto", "https")]),
            "lss_abc",
            30,
        );
        assert!(h.to_str().unwrap().contains("Secure"));
    }
}
