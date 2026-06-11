// The request gate. Layered over the entire router (Leptos pages, /api,
// /api/v1, static assets via the fallback) when auth is wired in main.
//
// Exemption table (the load-bearing part):
//   - /pkg/* and /sw.js: ALWAYS public. Leptos HydrationScripts emits
//     crossorigin script/link tags, so browsers fetch these without
//     credentials; gating them kills hydration. Compiled assets only.
//   - Static asset files at the site root (favicon, brand mark, PWA
//     manifest, fonts): public. Browsers fetch manifests without
//     credentials. User photos under /site/photos stay protected.
//   - /api/v1/info + /api/info: public. The HACS probe + pairing
//     precheck; carries auth_required so clients know to ask for a token.
//   - /api/*/auth/status|login|setup: public (setup only honored while
//     zero users exist; enforced in the handler).
//   - /login page: public (it hosts the form).
//   - /ingest/*: public. Ecowitt consoles + webhook hardware cannot
//     authenticate; per-source path secrets remain the mitigation.
//   - /api/health + /api/v1/health: middleware lets them through but
//     marks the request, and the handler trims to a liveness-only body
//     for anonymous callers (Docker healthchecks keep working).
//   - /setup pages + wizard APIs: public until the first user exists,
//     so docker run -> browser -> wizard works; locked after.
//
// Acceptance order: Authorization Bearer (lsk_ API tokens) -> session
// cookie -> ?access_token=lsk_... but only on paths ending in /stream
// (EventSource cannot set headers).
//
// Unauthenticated outcomes: HTML GETs 302 to /login; API calls get 401
// JSON with WWW-Authenticate: Bearer.

use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use tokio::sync::Mutex;

use crate::config::schema::AuthMode;
use crate::config::FileConfigStore;
use crate::ports::config_store::ConfigStore;

use super::AuthStore;

/// Whether auth is currently required (request extension on every
/// request). /api/v1/info reports it so integration clients know to
/// prompt for a token; the health handler uses it for body trimming.
#[derive(Clone, Copy, Debug)]
pub struct AuthRequired(pub bool);

/// Identity attached to authenticated requests (request extension).
/// Handlers that care (health trimming, token CRUD) read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestIdentity {
    /// Auth disabled or path exempt: nobody asked.
    Anonymous,
    /// A valid session or API token for this user id.
    User(i64),
    /// Allowed through by a trusted_networks CIDR match.
    TrustedNetwork,
}

/// Hot policy snapshot. Rebuilt by a small refresh task whenever the
/// config file changes (10s cadence), so PUT /api/config flips take
/// effect without a restart.
#[derive(Clone, Debug, Default)]
pub struct AuthPolicy {
    pub required: bool,
    pub session_ttl_days: u32,
    pub trusted: Vec<ipnet::IpNet>,
    /// Reverse proxies whose X-Forwarded-For is believed (see client_ip).
    pub trusted_proxies: Vec<ipnet::IpNet>,
    /// Extra Origins allowed to make state-changing calls cross-origin.
    pub trusted_origins: Vec<String>,
}

pub struct AuthRuntime {
    pub store: AuthStore,
    pub policy: arc_swap::ArcSwap<AuthPolicy>,
    /// users-exist flag, cached; refreshed alongside policy + flipped
    /// true immediately by the setup handler.
    pub setup_complete: std::sync::atomic::AtomicBool,
    /// Per-IP fixed-window limiter for login/setup.
    pub login_attempts: Mutex<std::collections::HashMap<IpAddr, (u32, i64)>>,
}

impl AuthRuntime {
    pub fn new(store: AuthStore) -> Self {
        Self {
            store,
            policy: arc_swap::ArcSwap::from_pointee(AuthPolicy::default()),
            setup_complete: std::sync::atomic::AtomicBool::new(false),
            login_attempts: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn policy_from_cfg(cfg: &crate::config::schema::AuthConfig) -> AuthPolicy {
        AuthPolicy {
            required: cfg.mode == AuthMode::Required,
            session_ttl_days: cfg.session_ttl_days.max(1),
            trusted: cfg
                .trusted_networks
                .iter()
                .filter_map(|s| s.parse::<ipnet::IpNet>().ok())
                .collect(),
            trusted_proxies: cfg
                .trusted_proxies
                .iter()
                .filter_map(|s| s.parse::<ipnet::IpNet>().ok())
                .collect(),
            trusted_origins: cfg.trusted_origins.clone(),
        }
    }

    /// Spawn the policy refresher: re-reads the config + user count on a
    /// 10s cadence. Cheap (one small TOML parse + one COUNT(*)).
    pub fn spawn_refresh(self: &Arc<Self>, cfg_store: Arc<FileConfigStore>) {
        let rt = self.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(cfg) = cfg_store.load().await {
                    rt.policy.store(Arc::new(Self::policy_from_cfg(&cfg.auth)));
                }
                if let Ok(n) = rt.store.user_count().await {
                    rt.setup_complete
                        .store(n > 0, std::sync::atomic::Ordering::Relaxed);
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
    }

    /// Fixed-window limiter: max 10 attempts/min/IP on login + setup.
    pub async fn allow_login_attempt(&self, ip: IpAddr) -> bool {
        let now = chrono::Utc::now().timestamp();
        let mut map = self.login_attempts.lock().await;
        let entry = map.entry(ip).or_insert((0, now));
        if now - entry.1 >= 60 {
            *entry = (0, now);
        }
        entry.0 += 1;
        // Opportunistic shrink so the map can't grow unbounded.
        if map.len() > 4096 {
            map.retain(|_, (_, start)| now - *start < 60);
        }
        map.get(&ip).map(|(n, _)| *n <= 10).unwrap_or(true)
    }
}

/// True for compiled/static assets that must never be credential-gated.
fn is_public_asset(path: &str) -> bool {
    if path.starts_with("/pkg/") || path == "/sw.js" {
        return true;
    }
    if path.starts_with("/site/photos") {
        return false;
    }
    // Root-level static files (favicon, brand mark, manifest, fonts).
    // One path segment + a known asset extension.
    let is_root_file = path.rfind('/') == Some(0);
    let ext_ok = [
        ".svg",
        ".png",
        ".ico",
        ".webmanifest",
        ".woff2",
        ".woff",
        ".css",
        ".js",
        ".map",
        ".txt",
    ]
    .iter()
    .any(|e| path.ends_with(e));
    is_root_file && ext_ok
}

fn is_auth_endpoint(path: &str) -> bool {
    matches!(
        path,
        "/login"
            | "/api/auth/status"
            | "/api/auth/login"
            | "/api/auth/setup"
            | "/api/v1/auth/status"
            | "/api/v1/auth/login"
            | "/api/v1/auth/setup"
    )
}

fn is_health(path: &str) -> bool {
    path == "/api/health"
        || path == "/api/v1/health"
        || path == "/api/health/"
        || path == "/api/v1/health/"
}

fn is_info(path: &str) -> bool {
    path == "/api/info" || path == "/api/v1/info"
}

fn is_wizard_surface(path: &str) -> bool {
    path == "/setup"
        || path.starts_with("/setup/")
        || path.starts_with("/api/wizard")
        || path.starts_with("/api/v1/wizard")
}

fn wants_html(req: &Request<Body>) -> bool {
    req.method() == Method::GET
        && req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(|a| a.contains("text/html"))
            .unwrap_or(false)
}

fn cookie_token(req: &Request<Body>) -> Option<String> {
    let cookies = req.headers().get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == super::SESSION_COOKIE).then(|| v.trim().to_string())
    })
}

fn bearer_token(req: &Request<Body>) -> Option<String> {
    let v = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ").map(|t| t.trim().to_string())
}

/// ?access_token=lsk_..., accepted only on SSE endpoints.
fn query_token(req: &Request<Body>) -> Option<String> {
    let path = req.uri().path();
    if !path.ends_with("/stream") {
        return None;
    }
    let q = req.uri().query()?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "access_token").then(|| v.to_string())
    })
}

/// Client IP for policy decisions (trusted_networks bypass, login rate
/// limiting). The socket peer address is authoritative. X-Forwarded-For
/// is honored ONLY when the peer itself is a configured trusted proxy,
/// and then the LAST hop not in trusted_proxies wins: rightmost entries
/// were appended by our own proxy chain, while anything left of the
/// first untrusted hop is client-supplied and trivially spoofable.
pub fn client_ip(req: &Request<Body>, trusted_proxies: &[ipnet::IpNet]) -> Option<IpAddr> {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())?;
    if !trusted_proxies.iter().any(|net| net.contains(&peer)) {
        return Some(peer);
    }
    let Some(xff) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
    else {
        return Some(peer);
    };
    for hop in xff.split(',').rev() {
        match hop.trim().parse::<IpAddr>() {
            Ok(ip) if trusted_proxies.iter().any(|net| net.contains(&ip)) => continue,
            Ok(ip) => return Some(ip),
            // Malformed hop: stop walking, fall back to the peer.
            Err(_) => break,
        }
    }
    Some(peer)
}

/// True when a browser-supplied Origin may perform a state-changing
/// request. Same-origin (Origin host equal to the Host or
/// X-Forwarded-Host the request arrived with) passes; so does an exact
/// entry in auth.trusted_origins (full origin or bare host). "null" and
/// malformed Origins are rejected: they only show up cross-origin
/// (sandboxed iframes, data: URLs) where a write has no business.
fn origin_allowed(
    origin: &str,
    host: Option<&str>,
    fwd_host: Option<&str>,
    trusted_origins: &[String],
) -> bool {
    let origin = origin.trim().trim_end_matches('/');
    let Some(origin_host) = origin.split("://").nth(1).filter(|h| !h.is_empty()) else {
        return false;
    };
    if trusted_origins
        .iter()
        .map(|t| t.trim().trim_end_matches('/'))
        .any(|t| t.eq_ignore_ascii_case(origin) || t.eq_ignore_ascii_case(origin_host))
    {
        return true;
    }
    [host, fwd_host]
        .iter()
        .flatten()
        .any(|h| h.trim().eq_ignore_ascii_case(origin_host))
}

fn unauthorized_api() -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({ "error": "unauthorized" })),
    )
        .into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"localsky\""),
    );
    resp
}

pub async fn enforce(
    State(rt): State<Arc<AuthRuntime>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let policy = rt.policy.load_full();
    let path = req.uri().path().to_string();
    req.extensions_mut().insert(AuthRequired(policy.required));

    // Origin check on state-changing requests (CSRF + DNS-rebinding
    // hardening alongside SameSite=Lax). Enforced in BOTH auth modes:
    // with auth disabled this is the only thing stopping a hostile
    // website from firing cross-origin writes at a LAN instance.
    // Browsers always attach Origin to cross-origin POST/PUT/PATCH/
    // DELETE; header-less clients (curl, integrations) pass. /ingest/*
    // is exempt (weather hardware POSTs; per-source secrets gate those).
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        && !path.starts_with("/ingest")
        && !path.starts_with("/api/v1/ingest")
    {
        if let Some(origin) = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        {
            let host = req
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok());
            let fwd_host = req
                .headers()
                .get("x-forwarded-host")
                .and_then(|v| v.to_str().ok());
            if !origin_allowed(origin, host, fwd_host, &policy.trusted_origins) {
                return (StatusCode::FORBIDDEN, "cross-origin write rejected").into_response();
            }
        }
    }

    if !policy.required {
        req.extensions_mut().insert(RequestIdentity::Anonymous);
        return next.run(req).await;
    }

    let setup_complete = rt.setup_complete.load(std::sync::atomic::Ordering::Relaxed);

    // Exemptions.
    if is_public_asset(&path)
        || is_auth_endpoint(&path)
        || is_info(&path)
        || path.starts_with("/ingest")
        || path.starts_with("/api/v1/ingest")
        || (!setup_complete && is_wizard_surface(&path))
    {
        req.extensions_mut().insert(RequestIdentity::Anonymous);
        return next.run(req).await;
    }

    // Trusted-network bypass.
    if let Some(ip) = client_ip(&req, &policy.trusted_proxies) {
        if policy.trusted.iter().any(|net| net.contains(&ip)) {
            req.extensions_mut().insert(RequestIdentity::TrustedNetwork);
            return next.run(req).await;
        }
    }

    // Credential acceptance: Bearer -> cookie -> stream query token.
    let identity = {
        if let Some(tok) = bearer_token(&req).or_else(|| query_token(&req)) {
            rt.store
                .validate_api_token(&tok)
                .await
                .ok()
                .flatten()
                .map(RequestIdentity::User)
        } else if let Some(cookie) = cookie_token(&req) {
            rt.store
                .validate_session(&cookie, policy.session_ttl_days)
                .await
                .ok()
                .flatten()
                .map(RequestIdentity::User)
        } else {
            None
        }
    };

    if let Some(id) = identity {
        req.extensions_mut().insert(id);
        return next.run(req).await;
    }

    // Health stays reachable for liveness probes; the handler trims the
    // body for anonymous callers based on the extension we set here.
    if is_health(&path) {
        req.extensions_mut().insert(RequestIdentity::Anonymous);
        return next.run(req).await;
    }

    if wants_html(&req) {
        return Redirect::temporary("/login").into_response();
    }
    unauthorized_api()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_exemptions() {
        assert!(is_public_asset("/pkg/localsky.wasm"));
        assert!(is_public_asset("/sw.js"));
        assert!(is_public_asset("/brand-mark.svg"));
        assert!(is_public_asset("/manifest.webmanifest"));
        assert!(is_public_asset("/favicon.ico"));
        assert!(!is_public_asset("/site/photos/zone.jpg"));
        assert!(!is_public_asset("/api/v1/snapshot"));
        assert!(!is_public_asset("/zones"));
        // Nested paths with asset extensions are not root files.
        assert!(!is_public_asset("/site/photos/x.png"));
    }

    #[test]
    fn endpoint_classification() {
        assert!(is_auth_endpoint("/api/v1/auth/login"));
        assert!(is_auth_endpoint("/login"));
        assert!(!is_auth_endpoint("/api/v1/auth/tokens"));
        assert!(is_info("/api/v1/info"));
        assert!(is_health("/api/v1/health"));
        assert!(is_wizard_surface("/setup/controllers"));
        assert!(is_wizard_surface("/api/v1/wizard/draft"));
    }

    #[test]
    fn policy_parses_cidrs() {
        let cfg = crate::config::schema::AuthConfig {
            mode: AuthMode::Required,
            session_ttl_days: 30,
            trusted_networks: vec!["10.1.2.0/24".into(), "garbage".into()],
            trusted_proxies: vec!["172.18.0.0/16".into(), "nonsense".into()],
            trusted_origins: vec!["https://dash.example.com".into()],
        };
        let p = AuthRuntime::policy_from_cfg(&cfg);
        assert!(p.required);
        assert_eq!(p.trusted.len(), 1);
        assert!(p.trusted[0].contains(&"10.1.2.50".parse::<IpAddr>().unwrap()));
        assert!(!p.trusted[0].contains(&"172.16.0.1".parse::<IpAddr>().unwrap()));
        assert_eq!(p.trusted_proxies.len(), 1);
        assert_eq!(p.trusted_origins.len(), 1);
    }

    fn req_with_peer(peer: &str, xff: Option<&str>) -> Request<Body> {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/auth/login")
            .body(Body::empty())
            .unwrap();
        if let Some(xff) = xff {
            req.headers_mut()
                .insert("x-forwarded-for", HeaderValue::from_str(xff).unwrap());
        }
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
                peer.parse().unwrap(),
                40000,
            )));
        req
    }

    #[test]
    fn spoofed_xff_from_untrusted_peer_is_ignored() {
        // Attacker on the WAN claims to be on the trusted LAN via XFF.
        let req = req_with_peer("203.0.113.5", Some("10.0.0.10"));
        let ip = client_ip(&req, &[]).unwrap();
        assert_eq!(ip, "203.0.113.5".parse::<IpAddr>().unwrap());
        // Even with proxies configured, a peer outside the list keeps
        // its own address.
        let proxies = vec!["172.18.0.0/16".parse::<ipnet::IpNet>().unwrap()];
        let ip = client_ip(&req, &proxies).unwrap();
        assert_eq!(ip, "203.0.113.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn xff_honored_from_trusted_proxy_takes_last_untrusted_hop() {
        let proxies = vec!["172.18.0.0/16".parse::<ipnet::IpNet>().unwrap()];
        // client -> evil-claimed hop -> real client -> our proxy chain.
        let req = req_with_peer("172.18.0.2", Some("10.9.9.9, 198.51.100.7, 172.18.0.3"));
        let ip = client_ip(&req, &proxies).unwrap();
        // 172.18.0.3 is a trusted hop (skipped); 198.51.100.7 is the
        // last untrusted hop; 10.9.9.9 is client-forgeable and ignored.
        assert_eq!(ip, "198.51.100.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn trusted_proxy_without_xff_falls_back_to_peer() {
        let proxies = vec!["172.18.0.0/16".parse::<ipnet::IpNet>().unwrap()];
        let req = req_with_peer("172.18.0.2", None);
        let ip = client_ip(&req, &proxies).unwrap();
        assert_eq!(ip, "172.18.0.2".parse::<IpAddr>().unwrap());
        // Garbage XFF also falls back to the peer.
        let req = req_with_peer("172.18.0.2", Some("not-an-ip"));
        let ip = client_ip(&req, &proxies).unwrap();
        assert_eq!(ip, "172.18.0.2".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn origin_check_blocks_cross_origin_writes() {
        let none: &[String] = &[];
        // Same-origin passes (typical browser request).
        assert!(origin_allowed(
            "http://10.0.0.20:3000",
            Some("10.0.0.20:3000"),
            None,
            none
        ));
        // Cross-origin (hostile site, DNS rebinding) rejected.
        assert!(!origin_allowed(
            "http://evil.example",
            Some("10.0.0.20:3000"),
            None,
            none
        ));
        // Opaque "null" origin rejected.
        assert!(!origin_allowed("null", Some("10.0.0.20:3000"), None, none));
        // Reverse proxy: X-Forwarded-Host carries the public name.
        assert!(origin_allowed(
            "https://sky.example.com",
            Some("10.0.0.20:3000"),
            Some("sky.example.com"),
            none
        ));
        // Explicit trusted origin passes despite the Host mismatch.
        let trusted = vec!["https://dash.example.com".to_string()];
        assert!(origin_allowed(
            "https://dash.example.com",
            Some("10.0.0.20:3000"),
            None,
            &trusted
        ));
    }
}
