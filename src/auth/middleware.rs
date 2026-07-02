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
//     zero users exist; enforced in the handler). NOTE the token-admin
//     routes (POST /auth/tokens, DELETE /auth/tokens/{id}) are the
//     opposite: ALWAYS authenticated-owner-only, even in Disabled mode
//     and never via a trusted-network match (LS-API-06).
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

    /// Decide the next AuthPolicy from a config load result (ACCT-02).
    /// `Some(p)` means store `p`; `None` means keep the current policy.
    ///   - Ok(cfg)        -> policy from cfg.auth
    ///   - Err(NotFound)  -> disabled (explicit revert: no on-disk config
    ///                       can back a required policy, so an in-memory
    ///                       required must not persist past the file's
    ///                       disappearance)
    ///   - Err(other)     -> None (transient Io/parse: keep last good so a
    ///                       flaky read never silently drops auth)
    pub fn policy_for_load_result(
        load: Result<crate::config::schema::Config, crate::ports::config_store::ConfigStoreError>,
    ) -> Option<AuthPolicy> {
        use crate::ports::config_store::ConfigStoreError;
        match load {
            Ok(cfg) => Some(Self::policy_from_cfg(&cfg.auth)),
            Err(ConfigStoreError::NotFound) => Some(Self::policy_from_cfg(
                &crate::config::schema::AuthConfig::default(),
            )),
            Err(_) => None,
        }
    }

    /// Spawn the policy refresher: re-reads the config + user count on a
    /// 10s cadence. Cheap (one small TOML parse + one COUNT(*)).
    pub fn spawn_refresh(self: &Arc<Self>, cfg_store: Arc<FileConfigStore>) {
        let rt = self.clone();
        tokio::spawn(async move {
            loop {
                // ACCT-02: map the load result to the next policy. A
                // present config rebuilds policy from it; a NotFound config
                // EXPLICITLY reverts to disabled (nothing on disk can back
                // required mode); a transient read error keeps the last
                // good policy so a flaky read never silently drops auth.
                if let Some(policy) = Self::policy_for_load_result(cfg_store.load().await) {
                    rt.policy.store(Arc::new(policy));
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

/// P4-1: the Prometheus exposition endpoint. Public like /api/health: it carries
/// only aggregate operational counters (verdict mix, refresh/degraded counts,
/// controller/cloud error counts, last-fetch latency) -- no secrets, config, or
/// PII -- so a scraper (the monitoring host) reaches it without credentials. A deployment
/// that wants it private firewalls /metrics at the proxy.
fn is_metrics(path: &str) -> bool {
    path == "/metrics"
}

/// Bundled documentation, served same-origin at /docs (see
/// docs_serve.rs). Public so in-app help is reachable pre-login and on
/// fresh installs (a locked-out operator still needs the setup guide).
/// Static doc HTML + assets carry no secrets. Precise prefix match so
/// only /docs and paths beneath it are exempt, not e.g. /docsomething.
fn is_docs(path: &str) -> bool {
    path == "/docs" || path.starts_with("/docs/")
}

fn is_wizard_surface(path: &str) -> bool {
    path == "/setup"
        || path.starts_with("/setup/")
        || path.starts_with("/api/wizard")
        || path.starts_with("/api/v1/wizard")
}

/// True for a private-range or loopback client address: the LAN LocalSky
/// already trusts in its default (proxy/isolated-LAN) posture. Covers IPv4
/// loopback + RFC1918 (10/8, 172.16/12, 192.168/16) and IPv6 loopback +
/// unique-local (fc00::/7). Used ONLY by the disabled-mode privileged gate
/// to admit the reverse proxy / LAN browser while still refusing a public
/// internet caller. NOT a substitute for trusted_networks in Required mode.
pub(crate) fn is_private_or_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        // map ::ffff:a.b.c.d to its v4 view first.
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_loopback() || v4.is_private();
            }
            // fc00::/7 unique-local.
            v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Privileged config/backup surface that must ALWAYS require an
/// authenticated identity OR a trusted-network / loopback caller, even when
/// global AuthMode is Disabled (security wave 3). On the shipped default
/// (Disabled) the rest of the API is intentionally LAN-friendly and
/// anonymous, but these specific routes either WRITE config or expose the
/// full config surface (including the raw TOML + downloadable backup), so
/// an unauthenticated, untrusted caller must be refused here regardless of
/// mode. The ordinary redacted GET /api/config and the normal read /
/// snapshot / stream surface are deliberately NOT included so the default
/// posture stays frictionless for non-sensitive reads.
///
/// The set, normalized across the /api and /api/v1 prefixes:
///   - any state-changing method on /api*/config* (PUT/POST/DELETE):
///     config writes, preview, rollback, raw PUT
///   - GET /api*/config/raw: the verbatim/redacted TOML editor read
///   - ALL methods on /api*/backup*: the bundle download stages secrets +
///     a DB copy off the box; restore writes config + stages a DB swap
/// (The rollback + snapshot WRITE routes live under /config, so they are
/// covered by the state-changing /config rule.)
///   - the wizard's ALTERNATE config-write paths (LS-API-12). The wizard
///     persists the SAME localsky.toml as /api/config, just by another
///     route, so it has to clear the identical bar:
///       - POST   /api*/wizard/apply        : writes the whole config
///       - PUT    /api*/wizard/draft        : stages the full config
///       - DELETE /api*/wizard/draft        : clears the staged config
///       - POST   /api*/wizard/seed_current : mirrors the live config into
///                                            a draft
///     Only these four are swept in. The wizard's outbound probe/test/scan/
///     discover endpoints are deliberately NOT here: they keep ProbeGuard,
///     which admits a private/LAN client so a fresh self-hoster can drive
///     onboarding from their own LAN. Adding them to the privileged gate
///     would be redundant with ProbeGuard and identical in outcome, so the
///     split is by responsibility (config-write gate vs SSRF/probe guard).
/// Normalize the /api/v1/... prefix to /api/... so one rule set covers
/// both the canonical and legacy API mounts.
fn normalize_api_prefix(path: &str) -> std::borrow::Cow<'_, str> {
    match path.strip_prefix("/api/v1/") {
        Some(rest) => std::borrow::Cow::Owned(format!("/api/{rest}")),
        None => std::borrow::Cow::Borrowed(path),
    }
}

/// API-token administration surface that must ALWAYS require a real
/// authenticated owner identity (LS-API-06), regardless of global
/// AuthMode and regardless of where the caller sits on the network. This
/// is a STRICTER bar than [`is_privileged_path`]: minting or revoking a
/// long-lived API token is the keys-to-the-kingdom operation, so a
/// trusted-network / loopback / private-LAN match is NOT sufficient here
/// (unlike the config/backup privileged gate). Only a valid session or
/// API token attributed to an owner account passes.
///
/// The set, normalized across the /api and /api/v1 prefixes:
///   - POST   /api*/auth/tokens        : mint a new token
///   - DELETE /api*/auth/tokens/{id}   : revoke a token
/// (GET /api*/auth/tokens lists token metadata only, no secret; it stays
/// on the ordinary auth path. The mutating mint/revoke routes are the
/// ones that must never be reachable unauthenticated, even in Disabled
/// mode where the rest of the API is intentionally LAN-anonymous.)
fn is_token_admin_path(method: &Method, path: &str) -> bool {
    let path = normalize_api_prefix(path);
    let tokens_root = path == "/api/auth/tokens";
    let tokens_child = path.starts_with("/api/auth/tokens/");
    match *method {
        // Mint a token: POST to the collection root.
        Method::POST => tokens_root,
        // Revoke a token: DELETE /tokens/{id}.
        Method::DELETE => tokens_child,
        _ => false,
    }
}

/// The wizard's ALTERNATE config-write surface (LS-API-12). These routes
/// persist or stage the SAME localsky.toml that /api/config writes, just
/// via the setup/edit flow, so they must clear the identical privileged
/// bar. Exactly four routes, normalized across /api and /api/v1:
///   - POST   /api*/wizard/apply        : write the whole config
///   - PUT    /api*/wizard/draft        : stage the full config
///   - DELETE /api*/wizard/draft        : clear the staged config
///   - POST   /api*/wizard/seed_current : mirror the live config to a draft
/// The probe/test/scan/discover endpoints are intentionally EXCLUDED here
/// (they keep ProbeGuard for LAN onboarding); only these config-write paths
/// are gated. Path is already prefix-normalized by the caller.
fn is_wizard_config_write(method: &Method, normalized_path: &str) -> bool {
    match *method {
        Method::POST => {
            normalized_path == "/api/wizard/apply" || normalized_path == "/api/wizard/seed_current"
        }
        Method::PUT | Method::DELETE => normalized_path == "/api/wizard/draft",
        _ => false,
    }
}

/// Whether a credential-less caller is vouched for on the privileged
/// config/backup/wizard-config-write gate, by network position alone. This
/// is the exact admission rule the gate applies before falling back to a
/// real credential, extracted as a pure function so the fresh-install LAN
/// path can be unit-tested without standing up a router + DB.
///
///   - loopback or a configured trusted_networks CIDR: always vouched (in
///     BOTH modes), the same-box owner / explicit-trust case.
///   - in the DISABLED default posture ONLY, any private/LAN address
///     (RFC1918 / ULA / loopback) is vouched: this is the product's
///     "behind a reverse proxy / on an isolated trusted LAN" model and is
///     exactly what a fresh-install LAN owner sends (private client IP,
///     auth Disabled, no trusted_networks) when driving the wizard. We
///     refuse only the INTERNET-anonymous caller here.
///   - when the operator opts into Required mode the bar rises: only an
///     explicit trusted_networks / loopback match passes without a
///     credential; a bare private IP no longer suffices.
pub(crate) fn privileged_caller_vouched(ip: &IpAddr, policy: &AuthPolicy) -> bool {
    ip.is_loopback()
        || policy.trusted.iter().any(|net| net.contains(ip))
        || (!policy.required && is_private_or_loopback(ip))
}

fn is_privileged_path(method: &Method, path: &str) -> bool {
    // Normalize /api/v1/... -> /api/... so one rule set covers both.
    let path = normalize_api_prefix(path);
    let path = path.as_ref();

    // All backup routes are privileged in every method (download + restore
    // + snapshot listing): the download alone exfiltrates config + DB.
    if path == "/api/backup" || path.starts_with("/api/backup/") {
        return true;
    }

    // Irrigation actuation: POST /api/irrigation/action runs / stops / pauses /
    // skips physical valves. It clears the same anonymous-internet bar as a config
    // write; an unauthenticated caller in disabled mode must never open a valve.
    // /simulate is a read-only dry-run preview and intentionally stays open.
    if path == "/api/irrigation/action"
        && !matches!(*method, Method::HEAD | Method::OPTIONS | Method::GET)
    {
        return true;
    }

    // The wizard's alternate config-write paths (apply / draft PUT+DELETE /
    // seed_current) persist or stage the same localsky.toml as /api/config,
    // so they clear the same bar. Probe/test/scan/discover are NOT here.
    if is_wizard_config_write(method, path) {
        return true;
    }

    // AuthMode-Disabled hardening (LS-REC-05): in the shipped default an
    // anonymous internet caller could otherwise seed push subscriptions
    // (POST /push/subscribe) or fill disk with photo uploads (POST
    // /zones/photo). Neither is a benign read, so both clear the same
    // anonymous-internet bar as a config write. A LAN/loopback/trusted
    // caller is still vouched by IP (the gate's IP branch), so a normal
    // self-hoster's own browser keeps working; only the public-anonymous
    // caller is refused. State-changing methods only (a GET never reaches
    // these handlers).
    if !matches!(*method, Method::HEAD | Method::OPTIONS | Method::GET) {
        let is_push_sub = path == "/api/push/subscribe" || path == "/api/push/unsubscribe";
        let is_photo_upload = path == "/api/zones/photo";
        if is_push_sub || is_photo_upload {
            return true;
        }
    }

    let is_config = path == "/api/config" || path.starts_with("/api/config/");
    if !is_config {
        return false;
    }
    // Raw TOML read is privileged even as a GET (it is the editor's
    // full-surface read; redaction protects the body, gating protects the
    // route from an anonymous internet caller in disabled mode).
    if *method == Method::GET {
        return path == "/api/config/raw";
    }
    // Every state-changing method under /config: PUT /, POST /preview,
    // POST /rollback, PUT /raw, and any future mutating route.
    !matches!(*method, Method::HEAD | Method::OPTIONS)
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
    client_ip_parts(req.extensions(), req.headers(), trusted_proxies)
}

/// Same client-IP derivation as [`client_ip`], but reading from request
/// `Parts` (extensions + headers) so `FromRequestParts` extractors (e.g.
/// the wizard probe guard) can make the identical trusted-proxy/XFF
/// decision without owning a full `Request<Body>`.
pub fn client_ip_parts(
    extensions: &axum::http::Extensions,
    headers: &header::HeaderMap,
    trusted_proxies: &[ipnet::IpNet],
) -> Option<IpAddr> {
    let peer = extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())?;
    if !trusted_proxies.iter().any(|net| net.contains(&peer)) {
        return Some(peer);
    }
    let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) else {
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

/// One-shot guard for the P4-6 exposure warning (see `enforce`): the
/// "public IP while auth Disabled" warning fires at most once per process.
static EXPOSED_WHILE_OPEN_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Store-independent gate for the no-history-DB boot (LS-REC-05 fail-closed).
///
/// When no history DB is mounted there is no [`AuthStore`], so the full
/// [`enforce`] middleware (which needs the store to validate credentials) was
/// previously NOT layered at all, and the Origin check + the privileged
/// config/backup/push/photo/actuation gate silently disappeared: every
/// state-changing route became reachable cross-origin and unauthenticated.
/// This struct carries only the hot [`AuthPolicy`] (no store) so the
/// structural protections still layer and FAIL CLOSED: with no store, a
/// privileged route admits ONLY an IP-vouched caller (loopback / trusted_
/// networks / private-LAN-in-disabled), and a credential can never be
/// presented, so a public/anonymous caller is refused outright. Identity is
/// always Anonymous here (there is no store to attribute a User to).
pub struct NoStoreGate {
    pub policy: arc_swap::ArcSwap<AuthPolicy>,
}

impl NoStoreGate {
    pub fn new() -> Self {
        Self {
            policy: arc_swap::ArcSwap::from_pointee(AuthPolicy::default()),
        }
    }

    /// Refresh the policy from the config file on the same 10s cadence as
    /// [`AuthRuntime::spawn_refresh`]. No user-count poll (there is no store);
    /// trusted_networks/proxies/origins still come from the config so a LAN
    /// owner's Origin + trusted-IP rules apply even without persistence.
    pub fn spawn_refresh(self: &Arc<Self>, cfg_store: Arc<FileConfigStore>) {
        let gate = self.clone();
        tokio::spawn(async move {
            loop {
                if let Some(policy) = AuthRuntime::policy_for_load_result(cfg_store.load().await) {
                    gate.policy.store(Arc::new(policy));
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
    }
}

impl Default for NoStoreGate {
    fn default() -> Self {
        Self::new()
    }
}

/// The structural half of [`enforce`] for the no-store boot: Origin check on
/// state-changing requests + the privileged / token-admin gate, with NO
/// credential validation (there is no store). Fails closed:
///   - Origin check: identical to `enforce` (cross-origin writes rejected in
///     both modes).
///   - token-admin (mint/revoke API token): always refused, no store to
///     authenticate an owner.
///   - privileged (config/backup/push/photo/actuation/wizard-config-write):
///     an IP-vouched caller (loopback / trusted_networks / private-LAN in the
///     disabled default) passes; everyone else is refused (no credential is
///     possible without a store).
/// Every other path is admitted Anonymous, matching the disabled-default
/// posture for ordinary reads. Demo mode is handled by the outer demo_guard,
/// so this gate does not special-case it.
pub async fn enforce_no_store(
    State(gate): State<Arc<NoStoreGate>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let policy = gate.policy.load_full();
    let path = req.uri().path().to_string();
    // No store -> auth can never be "required"; report Disabled so /info and
    // the health trim behave as in the open posture.
    req.extensions_mut().insert(AuthRequired(false));

    // Origin check on state-changing requests (CSRF / DNS-rebinding), the
    // ONLY thing stopping a hostile site from firing cross-origin writes at a
    // LAN instance with auth disabled. Identical to `enforce`. /ingest is
    // exempt (weather hardware POSTs; per-source secrets gate those).
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

    // Token admin (mint/revoke) requires a real owner identity, which needs a
    // store we do not have: refuse unconditionally. (No tokens can exist
    // without a store anyway.)
    if is_token_admin_path(req.method(), &path) {
        if wants_html(&req) {
            let login = format!("{}/login", crate::base::from_headers(req.headers()));
            return Redirect::temporary(&login).into_response();
        }
        return unauthorized_api();
    }

    // Privileged gate, IP-vouching only (no credential possible without a
    // store). This is the fail-closed core: config/backup/push/photo/
    // actuation/wizard-config-write are refused for a non-vouched caller.
    if is_privileged_path(req.method(), &path) {
        let client = client_ip(&req, &policy.trusted_proxies);
        if client.map(|ip| privileged_caller_vouched(&ip, &policy)) == Some(true) {
            req.extensions_mut().insert(RequestIdentity::TrustedNetwork);
            return next.run(req).await;
        }
        if wants_html(&req) {
            let login = format!("{}/login", crate::base::from_headers(req.headers()));
            return Redirect::temporary(&login).into_response();
        }
        return unauthorized_api();
    }

    // Everything else is an ordinary anonymous request (the disabled-default
    // posture for non-sensitive reads + the open app surface).
    req.extensions_mut().insert(RequestIdentity::Anonymous);
    next.run(req).await
}

pub async fn enforce(
    State(rt): State<Arc<AuthRuntime>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let policy = rt.policy.load_full();
    let path = req.uri().path().to_string();
    req.extensions_mut().insert(AuthRequired(policy.required));

    // P4-6 posture nudge: on the shipped default (auth Disabled), a request that
    // arrives from a PUBLIC source IP means this instance is very likely exposed
    // to the internet with no login. Warn once per process so the operator sees
    // it without spamming a line per request. After it fires, the steady-state
    // cost is a single relaxed atomic load. Required mode is already protected,
    // so it is exempt.
    if !policy.required && !EXPOSED_WHILE_OPEN_WARNED.load(std::sync::atomic::Ordering::Relaxed) {
        // client_ip resolves the real client only when the socket peer is a
        // configured trusted_proxy; otherwise it returns the peer itself. Two
        // exposure shapes to catch:
        //   - direct: the peer is a PUBLIC IP (direct internet bind).
        //   - proxied: the peer is private BUT a forwarding header is present and
        //     NO trusted_proxies are configured -- i.e. an (unconfigured) reverse
        //     proxy is in front, which may well be internet-facing. This is the
        //     dominant deployment and the prior version missed it entirely
        //     (peer = the proxy's own private IP, so the public test never fired).
        let peer = client_ip(&req, &policy.trusted_proxies);
        let direct_public = matches!(peer, Some(ip) if !is_private_or_loopback(&ip));
        let has_fwd = req.headers().contains_key("x-forwarded-for")
            || req.headers().contains_key("forwarded");
        let proxied_unconfigured = policy.trusted_proxies.is_empty()
            && has_fwd
            && matches!(peer, Some(ip) if is_private_or_loopback(&ip));
        if direct_public || proxied_unconfigured {
            EXPOSED_WHILE_OPEN_WARNED.store(true, std::sync::atomic::Ordering::Relaxed);
            if direct_public {
                if let Some(ip) = peer {
                    tracing::warn!(
                        client_ip = %ip,
                        "LocalSky served a request from a PUBLIC IP while authentication is \
                         DISABLED. If this instance is reachable from the internet, enable a login \
                         under Settings -> Account, or restrict access at your firewall / reverse \
                         proxy. LAN-only use needs no login."
                    );
                }
            } else {
                tracing::warn!(
                    "LocalSky received a forwarded request (proxy header present) while \
                     authentication is DISABLED and no trusted_proxies are configured. If the \
                     reverse proxy in front is internet-facing without its own login, enable a \
                     login under Settings -> Account, or set auth.trusted_proxies so real client \
                     IPs (and this exposure check) work. LAN-only use needs no login."
                );
            }
        }
    }

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

    // Token-admin gate (LS-API-06). Minting (POST /auth/tokens) or
    // revoking (DELETE /auth/tokens/{id}) an API token must ALWAYS require
    // a real authenticated owner identity, in BOTH auth modes and from any
    // network position. This is deliberately STRICTER than the
    // config/backup privileged gate below: a trusted-network / loopback /
    // private-LAN match is NOT sufficient to mint or revoke a long-lived
    // API token, because that token is itself a credential that would then
    // work from anywhere. So even in the shipped default (AuthMode::
    // Disabled), where the rest of the API is intentionally LAN-anonymous,
    // these two routes require a valid session or API token attributed to
    // an owner account. The demo already 403s them via demo_guard, so skip
    // there (demo has no real owner to authenticate as).
    if is_token_admin_path(req.method(), &path) && !super::demo_guard::is_demo() {
        let user = if let Some(tok) = bearer_token(&req).or_else(|| query_token(&req)) {
            rt.store.validate_api_token(&tok).await.ok().flatten()
        } else if let Some(cookie) = cookie_token(&req) {
            rt.store
                .validate_session(&cookie, policy.session_ttl_days)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        match user {
            Some(id) => {
                req.extensions_mut().insert(RequestIdentity::User(id));
                return next.run(req).await;
            }
            // No real credential: refuse, regardless of mode or network.
            // Token admin is never reachable by a trusted-network caller.
            None => {
                if wants_html(&req) {
                    let login = format!("{}/login", crate::base::from_headers(req.headers()));
                    return Redirect::temporary(&login).into_response();
                }
                return unauthorized_api();
            }
        }
    }

    // Privileged config/backup gate (security wave 3). Runs in BOTH auth
    // modes, BEFORE the disabled-mode short-circuit below, so config writes
    // + the raw-config read + the whole backup surface always require an
    // authenticated identity OR a trusted-network / loopback caller, even
    // on the shipped default (AuthMode::Disabled). This protects a
    // self-hoster in the default posture without forcing full auth, and
    // does not touch the LAN-friendly default for ordinary reads.
    // The public read-only demo is intentionally anonymous-readable and is
    // already write-locked at the outermost layer by auth::demo_guard
    // (every config/backup MUTATION returns 403 there). Its config GETs are
    // redacted demo data, so the privileged READ gate would only break the
    // demo's "kick the tires" browsing without adding protection. Skip the
    // gate in demo mode; demo_guard remains the demo's mutation boundary.
    if is_privileged_path(req.method(), &path) && !super::demo_guard::is_demo() {
        // Loopback (same-box owner via same-host proxy, CLI, healthcheck)
        // and a configured trusted_networks client are vouched for without
        // credentials, mirroring the wizard ProbeGuard's allowance.
        let client = client_ip(&req, &policy.trusted_proxies);
        let trusted_ip = client.map(|ip| privileged_caller_vouched(&ip, &policy));
        if trusted_ip == Some(true) {
            req.extensions_mut().insert(RequestIdentity::TrustedNetwork);
            return next.run(req).await;
        }
        // Otherwise require a valid credential (Bearer / cookie / stream
        // token), regardless of global mode.
        let user = if let Some(tok) = bearer_token(&req).or_else(|| query_token(&req)) {
            rt.store.validate_api_token(&tok).await.ok().flatten()
        } else if let Some(cookie) = cookie_token(&req) {
            rt.store
                .validate_session(&cookie, policy.session_ttl_days)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        if let Some(id) = user {
            req.extensions_mut().insert(RequestIdentity::User(id));
            return next.run(req).await;
        }
        // Unauthenticated, untrusted caller on a privileged route: refuse.
        // HTML GET (the raw-config editor opened directly) gets a login
        // redirect; everything else (API/JSON, writes) gets 401.
        if wants_html(&req) {
            let login = format!("{}/login", crate::base::from_headers(req.headers()));
            return Redirect::temporary(&login).into_response();
        }
        return unauthorized_api();
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
        || is_metrics(&path)
        || path.starts_with("/ingest")
        || path.starts_with("/api/v1/ingest")
        || is_docs(&path)
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
        // Prefix-aware: under HA ingress (or any prefix proxy setting
        // X-Ingress-Path) the browser-facing login URL carries the prefix.
        let login = format!("{}/login", crate::base::from_headers(req.headers()));
        return Redirect::temporary(&login).into_response();
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
    fn privileged_path_classification() {
        // Config writes are privileged on every state-changing method, both
        // prefixes.
        assert!(is_privileged_path(&Method::PUT, "/api/config"));
        assert!(is_privileged_path(&Method::PUT, "/api/v1/config"));
        assert!(is_privileged_path(&Method::PUT, "/api/config/raw"));
        assert!(is_privileged_path(&Method::POST, "/api/config/rollback"));
        assert!(is_privileged_path(&Method::POST, "/api/v1/config/preview"));
        // GET /config/raw is privileged (full-surface editor read).
        assert!(is_privileged_path(&Method::GET, "/api/config/raw"));
        assert!(is_privileged_path(&Method::GET, "/api/v1/config/raw"));
        // ALL backup methods are privileged: the download exfiltrates
        // config + DB, restore writes.
        assert!(is_privileged_path(&Method::GET, "/api/v1/backup"));
        assert!(is_privileged_path(&Method::POST, "/api/v1/backup/restore"));
        assert!(is_privileged_path(&Method::GET, "/api/v1/backup/snapshots"));
        assert!(is_privileged_path(&Method::GET, "/api/backup"));

        // Irrigation actuation is privileged: POST /irrigation/action opens valves.
        assert!(is_privileged_path(&Method::POST, "/api/irrigation/action"));
        assert!(is_privileged_path(
            &Method::POST,
            "/api/v1/irrigation/action"
        ));

        // Push subscribe/unsubscribe + photo upload are privileged in the
        // disabled default (anonymous internet must not seed subscriptions or
        // fill disk). Both prefixes; POST only.
        assert!(is_privileged_path(&Method::POST, "/api/push/subscribe"));
        assert!(is_privileged_path(&Method::POST, "/api/v1/push/subscribe"));
        assert!(is_privileged_path(&Method::POST, "/api/push/unsubscribe"));
        assert!(is_privileged_path(&Method::POST, "/api/zones/photo"));
        assert!(is_privileged_path(&Method::POST, "/api/v1/zones/photo"));
        // The vapid-key READ stays public (the frontend needs it before any
        // subscription exists).
        assert!(!is_privileged_path(&Method::GET, "/api/push/vapid-key"));
        assert!(!is_privileged_path(&Method::GET, "/api/push/subscribe"));

        // NOT privileged: the redacted ordinary config GET + the normal
        // read/snapshot surface stay LAN-friendly.
        assert!(!is_privileged_path(&Method::GET, "/api/config"));
        assert!(!is_privileged_path(&Method::GET, "/api/v1/config"));
        // The dry-run preview does not actuate and stays open.
        assert!(!is_privileged_path(
            &Method::POST,
            "/api/irrigation/simulate"
        ));
        assert!(!is_privileged_path(&Method::GET, "/api/irrigation/action"));
        assert!(!is_privileged_path(&Method::GET, "/api/config/schema"));
        assert!(!is_privileged_path(&Method::GET, "/api/config/snapshots"));
        assert!(!is_privileged_path(&Method::GET, "/api/config/validate"));
        assert!(!is_privileged_path(&Method::GET, "/api/v1/snapshot"));
        assert!(!is_privileged_path(
            &Method::GET,
            "/api/v1/irrigation/stream"
        ));
        assert!(!is_privileged_path(&Method::GET, "/zones"));
        // HEAD/OPTIONS on config are not state-changing.
        assert!(!is_privileged_path(&Method::OPTIONS, "/api/config"));
        assert!(!is_privileged_path(&Method::HEAD, "/api/config"));
        // A lookalike path is not the backup surface.
        assert!(!is_privileged_path(&Method::GET, "/api/backupsomething"));
    }

    #[test]
    fn wizard_config_write_paths_are_privileged() {
        // The wizard's three config-write surfaces persist/stage the same
        // localsky.toml as /api/config, so they clear the same bar. Both
        // /api and /api/v1 prefixes.
        // apply: POST writes the whole config.
        assert!(is_privileged_path(&Method::POST, "/api/wizard/apply"));
        assert!(is_privileged_path(&Method::POST, "/api/v1/wizard/apply"));
        // draft: PUT stages, DELETE clears.
        assert!(is_privileged_path(&Method::PUT, "/api/wizard/draft"));
        assert!(is_privileged_path(&Method::PUT, "/api/v1/wizard/draft"));
        assert!(is_privileged_path(&Method::DELETE, "/api/wizard/draft"));
        assert!(is_privileged_path(&Method::DELETE, "/api/v1/wizard/draft"));
        // seed_current: POST mirrors live config into a draft.
        assert!(is_privileged_path(
            &Method::POST,
            "/api/wizard/seed_current"
        ));
        assert!(is_privileged_path(
            &Method::POST,
            "/api/v1/wizard/seed_current"
        ));

        // NOT privileged: GET draft + GET state are ordinary reads (the
        // wizard UI loads them); they stay LAN-friendly.
        assert!(!is_privileged_path(&Method::GET, "/api/wizard/draft"));
        assert!(!is_privileged_path(&Method::GET, "/api/v1/wizard/draft"));
        assert!(!is_privileged_path(&Method::GET, "/api/wizard/state"));

        // NOT privileged here: the probe/test/scan/discover endpoints keep
        // ProbeGuard for LAN onboarding, so the privileged gate must NOT
        // sweep them in (that would be redundant and could double-gate).
        assert!(!is_privileged_path(
            &Method::POST,
            "/api/wizard/test_source"
        ));
        assert!(!is_privileged_path(
            &Method::POST,
            "/api/wizard/test_controller"
        ));
        assert!(!is_privileged_path(&Method::POST, "/api/wizard/test_llm"));
        assert!(!is_privileged_path(&Method::POST, "/api/wizard/scan_zones"));
        assert!(!is_privileged_path(&Method::POST, "/api/wizard/probe_soil"));
        assert!(!is_privileged_path(&Method::GET, "/api/wizard/discover"));
        assert!(!is_privileged_path(&Method::GET, "/api/wizard/geocode"));

        // Wrong method on a config-write path is not privileged: POST /draft
        // is not a draft route (the draft router has no POST), and PUT
        // /apply / PUT /seed_current are not real routes either.
        assert!(!is_privileged_path(&Method::POST, "/api/wizard/draft"));
        assert!(!is_privileged_path(&Method::PUT, "/api/wizard/apply"));
        assert!(!is_privileged_path(&Method::HEAD, "/api/wizard/draft"));
        // A lookalike path is not the wizard config-write surface.
        assert!(!is_privileged_path(&Method::POST, "/api/wizard/applyx"));
        assert!(!is_privileged_path(&Method::POST, "/api/wizardly/apply"));
    }

    #[test]
    fn fresh_install_lan_wizard_admitted_public_refused() {
        // Models the shipped fresh-install default: AuthMode Disabled, no
        // trusted_networks configured. The gate that runs on the wizard's
        // config-write routes is: is_privileged_path() && (vouched-by-IP ||
        // valid credential). Here we assert the IP branch directly, since
        // that is what a credential-less fresh-install owner relies on.
        let fresh = AuthPolicy {
            required: false,
            session_ttl_days: 30,
            trusted: vec![],
            trusted_proxies: vec![],
            trusted_origins: vec![],
        };

        // The wizard config-write routes a fresh-install owner drives.
        let write_routes = [
            (Method::POST, "/api/wizard/apply"),
            (Method::PUT, "/api/wizard/draft"),
            (Method::DELETE, "/api/wizard/draft"),
            (Method::POST, "/api/wizard/seed_current"),
            (Method::POST, "/api/v1/wizard/apply"),
            (Method::PUT, "/api/v1/wizard/draft"),
        ];

        let lan_owner: IpAddr = "10.0.0.50".parse().unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let public: IpAddr = "203.0.113.5".parse().unwrap();

        for (method, path) in &write_routes {
            // The route is privileged (so the gate engages)...
            assert!(
                is_privileged_path(method, path),
                "{method} {path} must be privileged"
            );
            // ...and a private-LAN owner IP is vouched without a credential,
            // so the fresh-install LAN wizard still works.
            assert!(
                privileged_caller_vouched(&lan_owner, &fresh),
                "fresh-install LAN owner must drive {method} {path}"
            );
            // Loopback (same-box proxy / CLI) is vouched too.
            assert!(privileged_caller_vouched(&loopback, &fresh));
            // A public anonymous caller is REFUSED by IP: it then falls
            // through to the credential check (which it has none for).
            assert!(
                !privileged_caller_vouched(&public, &fresh),
                "public anonymous caller must be refused on {method} {path}"
            );
        }
    }

    #[test]
    fn required_mode_raises_bar_on_wizard_writes() {
        // Once the operator opts into Required mode, a bare private-LAN IP no
        // longer suffices on the wizard config-write gate: only an explicit
        // trusted_networks / loopback match passes without a credential
        // (mirroring /api/config). A real credential is still accepted by the
        // gate's fallback (not modeled here; this asserts the IP branch).
        let required_no_trust = AuthPolicy {
            required: true,
            session_ttl_days: 30,
            trusted: vec![],
            trusted_proxies: vec![],
            trusted_origins: vec![],
        };
        let lan: IpAddr = "10.0.0.50".parse().unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        // Bare LAN IP is NOT vouched in Required mode without trusted_networks.
        assert!(!privileged_caller_vouched(&lan, &required_no_trust));
        // Loopback still passes (same-box owner).
        assert!(privileged_caller_vouched(&loopback, &required_no_trust));

        // With the LAN explicitly trusted, it is vouched again even in
        // Required mode.
        let required_trusted = AuthPolicy {
            required: true,
            session_ttl_days: 30,
            trusted: vec!["10.0.0.0/24".parse().unwrap()],
            trusted_proxies: vec![],
            trusted_origins: vec![],
        };
        assert!(privileged_caller_vouched(&lan, &required_trusted));
    }

    #[test]
    fn token_admin_path_classification() {
        // Mint: POST to the collection root, both prefixes.
        assert!(is_token_admin_path(&Method::POST, "/api/auth/tokens"));
        assert!(is_token_admin_path(&Method::POST, "/api/v1/auth/tokens"));
        // Revoke: DELETE /tokens/{id}, both prefixes.
        assert!(is_token_admin_path(&Method::DELETE, "/api/auth/tokens/7"));
        assert!(is_token_admin_path(
            &Method::DELETE,
            "/api/v1/auth/tokens/7"
        ));
        // GET (list metadata) is NOT token-admin: it stays on the ordinary
        // auth path. Only mint/revoke are the keys-to-the-kingdom routes.
        assert!(!is_token_admin_path(&Method::GET, "/api/auth/tokens"));
        assert!(!is_token_admin_path(&Method::GET, "/api/v1/auth/tokens"));
        // DELETE on the bare collection (no id) is not a revoke route.
        assert!(!is_token_admin_path(&Method::DELETE, "/api/auth/tokens"));
        // POST to a child path is not the mint route.
        assert!(!is_token_admin_path(&Method::POST, "/api/auth/tokens/7"));
        // Other auth endpoints are untouched by this gate.
        assert!(!is_token_admin_path(&Method::POST, "/api/auth/login"));
        assert!(!is_token_admin_path(&Method::POST, "/api/auth/setup"));
        // A lookalike path is not the token surface.
        assert!(!is_token_admin_path(
            &Method::POST,
            "/api/auth/tokensomething"
        ));
    }

    #[test]
    fn private_or_loopback_recognition() {
        // Loopback + RFC1918 are "the LAN we trust in disabled mode".
        assert!(is_private_or_loopback(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_or_loopback(&"172.16.5.20".parse().unwrap()));
        assert!(is_private_or_loopback(&"172.16.5.9".parse().unwrap()));
        assert!(is_private_or_loopback(&"10.0.0.20".parse().unwrap())); // a private reverse-proxy hop
        assert!(is_private_or_loopback(&"::1".parse().unwrap()));
        assert!(is_private_or_loopback(&"fc00::1234".parse().unwrap()));
        assert!(is_private_or_loopback(&"::ffff:10.0.0.50".parse().unwrap()));
        // Public internet addresses are NOT trusted: the gate refuses them.
        assert!(!is_private_or_loopback(&"203.0.113.5".parse().unwrap()));
        assert!(!is_private_or_loopback(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_or_loopback(&"172.32.0.1".parse().unwrap())); // just outside 172.16/12
        assert!(!is_private_or_loopback(&"2606:4700::1".parse().unwrap()));
    }

    #[test]
    fn endpoint_classification() {
        assert!(is_auth_endpoint("/api/v1/auth/login"));
        assert!(is_auth_endpoint("/login"));
        assert!(!is_auth_endpoint("/api/v1/auth/tokens"));
        assert!(is_info("/api/v1/info"));
        assert!(is_health("/api/v1/health"));
        assert!(is_metrics("/metrics"));
        assert!(!is_metrics("/metrics/x"));
        assert!(!is_metrics("/api/metrics"));
        assert!(is_wizard_surface("/setup/controllers"));
        assert!(is_wizard_surface("/api/v1/wizard/draft"));
        // Bundled docs are public; the exemption is a precise prefix.
        assert!(is_docs("/docs"));
        assert!(is_docs("/docs/"));
        assert!(is_docs("/docs/controllers"));
        assert!(is_docs("/docs/css/general.css"));
        assert!(!is_docs("/docsomething"));
        assert!(!is_docs("/api/docs"));
    }

    #[test]
    fn refresher_reverts_to_disabled_on_notfound() {
        use crate::config::schema::{AuthConfig, AuthMode, Config};
        use crate::ports::config_store::ConfigStoreError;

        // Present config with required mode -> policy required.
        let mut cfg = Config::default();
        cfg.auth.mode = AuthMode::Required;
        let p =
            AuthRuntime::policy_for_load_result(Ok(cfg)).expect("present config rebuilds policy");
        assert!(p.required);

        // NotFound -> explicit revert to disabled (the ACCT-02 invariant):
        // an in-memory required must not survive the config disappearing.
        let p = AuthRuntime::policy_for_load_result(Err(ConfigStoreError::NotFound))
            .expect("NotFound yields a (disabled) policy, not a keep");
        assert!(!p.required, "NotFound must revert to disabled");

        // Transient error -> keep last good (None), never silently drop auth.
        assert!(
            AuthRuntime::policy_for_load_result(Err(ConfigStoreError::Io("flaky".into())))
                .is_none(),
            "transient read error must keep the last good policy"
        );

        // Sanity: a disabled config maps to disabled.
        let mut disabled = Config::default();
        disabled.auth = AuthConfig::default();
        let p = AuthRuntime::policy_for_load_result(Ok(disabled)).unwrap();
        assert!(!p.required);
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
