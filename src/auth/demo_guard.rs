// Demo read-only gate. The public demo (demo.localsky.io, LOCALSKY_DEMO=1)
// shows a fully populated instance for screenshots + kicking the tires,
// but it is reachable by anonymous internet callers. LOCALSKY_DEMO has
// historically only swapped the data feeder, so every state-changing and
// outbound-probe API was still live and anonymous there.
//
// This layer makes the demo genuinely read-only. It runs as a tower
// middleware over the ENTIRE router and is DEFAULT-DENY for the API: every
// state-changing (non-GET/HEAD/OPTIONS) request whose normalized path
// starts with /api/ is refused with 403, so ANY current or future mutating
// API route is locked on the demo without having to enumerate it (an
// enumerated allowlist previously let POST /api/zones/photo and
// POST /api/push/subscribe slip through). The model is:
//
//   - BLOCK every non-GET/HEAD/OPTIONS request under /api/ ...
//   - ... EXCEPT a tiny explicit ALLOWLIST of demo-legit writes:
//       POST /api/auth/login   (the demo's auth UI is explorable)
//       POST /api/auth/logout
//     (+ their /api/v1/* forms). No account can be created and no token
//     minted: POST /api/auth/setup and the token CRUD are NOT allowlisted,
//     so they remain blocked by the default-deny rule.
//   - ALSO block a few sensitive GETs that read secrets or probe outward,
//     since default-deny only covers writes:
//       GET /api/backup            (config + DB download)
//       GET /api/config/raw        (un-redacted-capable read)
//       GET /api/wizard/discover   (outbound LAN sweep)
//     (+ their /api/v1/* forms).
//
// Everything else (GET browsing, SSE streams, the read-only auth status,
// health, info, docs, static assets) passes untouched, so the demo stays
// fully functional for read browsing. Non-/api paths are NEVER touched:
// /ingest/* (the demo's own data feed), /pkg, /docs, static, and SSE all
// pass, so the demo keeps populating and browsing.
//
// Outside demo mode the layer is a no-op (one bool check then pass-through),
// so prod, the owner's live instance, and a self-hoster are never touched.

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// True when this process is the public read-only demo. The source of
/// truth is the LOCALSKY_DEMO env var (==1), the same signal main.rs reads
/// into its `demo_mode` bool; exposed here so any gate can ask without
/// threading the bool through every router.
pub fn is_demo() -> bool {
    std::env::var("LOCALSKY_DEMO").ok().as_deref() == Some("1")
}

/// 403 body for a blocked demo mutation. Coarse + friendly: the demo is
/// read-only by design, not an error the caller can fix.
fn demo_forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": "demo_read_only",
            "detail": "this is the public read-only demo; changes and device probes are disabled",
        })),
    )
        .into_response()
}

/// True for a request that must be refused on the demo. DEFAULT-DENY for
/// the API: any state-changing call under /api/ is blocked unless it is on a
/// small explicit allowlist; a handful of sensitive GETs are also blocked.
/// Pure classification on method + path so it is unit-testable without a
/// running server.
fn is_blocked_demo_request(method: &Method, path: &str) -> bool {
    // Normalize the version prefix so /api/... and /api/v1/... share one
    // rule set. Strip a leading /api/v1 -> /api, leaving everything else
    // (Leptos pages, /docs, /pkg, /ingest, ...) untouched.
    let p = path
        .strip_prefix("/api/v1/")
        .map(|rest| format!("/api/{rest}"));
    let path = p.as_deref().unwrap_or(path);

    let is_write = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);

    // Sensitive GET reads that must still be blocked on the demo. The demo
    // deliberately skips the privileged-path gate (the Caddy hop makes the
    // socket peer look like a trusted LAN IP), so these reads are gated here
    // instead: the outbound /discover probe, the full-fidelity backup
    // download (config + DB), and the raw (un-redacted-capable) config read.
    if *method == Method::GET
        && (path == "/api/wizard/discover" || path == "/api/backup" || path == "/api/config/raw")
    {
        return true;
    }

    // Non-/api requests (Leptos pages, /ingest/* data feed, /pkg, /docs,
    // static assets, SSE streams) are NEVER touched, so the demo keeps
    // populating + browsing. Only the /api surface is default-denied below.
    if !path.starts_with("/api/") {
        return false;
    }

    // Reads (GET/HEAD/OPTIONS) under /api are open for browsing (the few
    // sensitive ones were already caught above).
    if !is_write {
        return false;
    }

    // DEFAULT-DENY: every state-changing /api request is blocked unless it
    // is on the explicit demo-legit allowlist below. This means a brand-new
    // mutating route (e.g. POST /api/foo) is locked on the demo the moment
    // it is added, with no edit here required (closes the enumeration gap
    // that left POST /api/zones/photo + POST /api/push/subscribe live).
    //
    // ALLOWLIST: only login + logout, so the demo's auth UI stays
    // explorable. Account creation (POST /api/auth/setup) and token CRUD
    // are deliberately NOT here, so they remain blocked. Path is already
    // version-normalized, so /api/v1/auth/login matches /api/auth/login.
    let allowed_write = path == "/api/auth/login" || path == "/api/auth/logout";
    !allowed_write
}

/// Tower middleware: when LOCALSKY_DEMO=1, refuse state-changing +
/// outbound-probe requests with 403; otherwise pass straight through.
/// Layered outermost in main so it short-circuits before auth/handlers.
pub async fn block_when_demo(req: Request<Body>, next: Next) -> Response {
    if is_demo() && is_blocked_demo_request(req.method(), req.uri().path()) {
        return demo_forbidden();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(method: Method, path: &str) -> bool {
        is_blocked_demo_request(&method, path)
    }

    #[test]
    fn blocks_config_writes_both_prefixes() {
        assert!(blocked(Method::PUT, "/api/config"));
        assert!(blocked(Method::PUT, "/api/v1/config"));
        assert!(blocked(Method::PUT, "/api/config/raw"));
        assert!(blocked(Method::PUT, "/api/v1/config/raw"));
        assert!(blocked(Method::POST, "/api/config/rollback"));
        assert!(blocked(Method::POST, "/api/v1/config/rollback"));
    }

    #[test]
    fn allows_config_reads() {
        assert!(!blocked(Method::GET, "/api/config"));
        assert!(!blocked(Method::GET, "/api/v1/config"));
        assert!(!blocked(Method::GET, "/api/config/schema"));
        assert!(!blocked(Method::GET, "/api/v1/config/snapshots"));
        assert!(!blocked(Method::GET, "/api/config/validate"));
        // Raw config read is sensitive (can reveal secrets); blocked on demo.
        assert!(blocked(Method::GET, "/api/config/raw"));
        assert!(blocked(Method::GET, "/api/v1/config/raw"));
    }

    #[test]
    fn blocks_every_wizard_mutation() {
        for path in [
            "/api/wizard/apply",
            "/api/wizard/test_source",
            "/api/wizard/test_controller",
            "/api/wizard/test_llm",
            "/api/wizard/scan_zones",
            "/api/wizard/probe_soil",
            "/api/wizard/seed_current",
            "/api/wizard/draft",
            "/api/v1/wizard/apply",
            "/api/v1/wizard/draft",
        ] {
            assert!(blocked(Method::POST, path), "POST {path} must be blocked");
        }
        assert!(blocked(Method::PUT, "/api/wizard/draft"));
        assert!(blocked(Method::DELETE, "/api/wizard/draft"));
    }

    #[test]
    fn blocks_outbound_discover_get() {
        // The one outbound GET that must still be denied.
        assert!(blocked(Method::GET, "/api/wizard/discover"));
        assert!(blocked(Method::GET, "/api/v1/wizard/discover"));
    }

    #[test]
    fn allows_wizard_reads() {
        // Read-only wizard surface stays browsable (state, geocode, draft GET).
        assert!(!blocked(Method::GET, "/api/wizard/draft"));
        assert!(!blocked(Method::GET, "/api/wizard/state"));
        assert!(!blocked(Method::GET, "/api/wizard/geocode"));
        assert!(!blocked(Method::GET, "/api/v1/wizard/state"));
    }

    #[test]
    fn blocks_backup_download_and_restore() {
        assert!(blocked(Method::POST, "/api/v1/backup/restore"));
        // The full-fidelity download (config + DB) is blocked on demo; the
        // snapshot list (metadata only) stays open for browsing.
        assert!(blocked(Method::GET, "/api/v1/backup"));
        assert!(blocked(Method::GET, "/api/backup"));
        assert!(!blocked(Method::GET, "/api/v1/backup/snapshots"));
    }

    #[test]
    fn blocks_irrigation_action() {
        assert!(blocked(Method::POST, "/api/irrigation/action"));
        assert!(blocked(Method::POST, "/api/v1/irrigation/action"));
        // Reads + the SSE stream + simulate-as-GET stay open.
        assert!(!blocked(Method::GET, "/api/irrigation/snapshot"));
        assert!(!blocked(Method::GET, "/api/v1/irrigation/stream"));
    }

    #[test]
    fn blocks_auth_setup_and_tokens_not_login() {
        assert!(blocked(Method::POST, "/api/auth/setup"));
        assert!(blocked(Method::POST, "/api/v1/auth/setup"));
        assert!(blocked(Method::POST, "/api/auth/tokens"));
        assert!(blocked(Method::DELETE, "/api/auth/tokens/3"));
        assert!(blocked(Method::DELETE, "/api/v1/auth/tokens/3"));
        // Auth UI exploration stays open.
        assert!(!blocked(Method::POST, "/api/auth/login"));
        assert!(!blocked(Method::POST, "/api/auth/logout"));
        assert!(!blocked(Method::GET, "/api/auth/status"));
        assert!(!blocked(Method::GET, "/api/auth/tokens"));
    }

    #[test]
    fn lets_general_browsing_through() {
        // Pages, snapshots, streams, ingest, docs, assets: never blocked.
        assert!(!blocked(Method::GET, "/"));
        assert!(!blocked(Method::GET, "/zones"));
        assert!(!blocked(Method::GET, "/api/v1/snapshot"));
        assert!(!blocked(Method::GET, "/api/v1/info"));
        assert!(!blocked(Method::POST, "/ingest/ecowitt"));
        assert!(!blocked(Method::GET, "/docs/getting-started"));
        assert!(!blocked(Method::GET, "/pkg/localsky.wasm"));
    }

    #[test]
    fn default_deny_blocks_previously_leaked_writes() {
        // The two routes the old enumerated list missed: a default-deny
        // model blocks them now (regression guard for the audit finding).
        assert!(blocked(Method::POST, "/api/zones/photo"));
        assert!(blocked(Method::POST, "/api/v1/zones/photo"));
        assert!(blocked(Method::POST, "/api/push/subscribe"));
        assert!(blocked(Method::POST, "/api/v1/push/subscribe"));
        // Other methods on those surfaces are blocked too.
        assert!(blocked(Method::DELETE, "/api/push/subscribe"));
        assert!(blocked(Method::PUT, "/api/zones/photo"));
    }

    #[test]
    fn default_deny_blocks_a_hypothetical_new_route() {
        // The whole point of default-deny: a brand-new mutating /api route
        // is blocked on the demo with no edit to this file required.
        assert!(blocked(Method::POST, "/api/foo"));
        assert!(blocked(Method::POST, "/api/v1/foo"));
        assert!(blocked(Method::PUT, "/api/foo/bar"));
        assert!(blocked(Method::PATCH, "/api/anything/new"));
        assert!(blocked(Method::DELETE, "/api/some/future/route"));
        // ...but a GET on the same new route is still browsable.
        assert!(!blocked(Method::GET, "/api/foo"));
    }

    #[test]
    fn allowlist_only_admits_login_logout() {
        // The sole demo-legit writes: login + logout, both prefixes.
        assert!(!blocked(Method::POST, "/api/auth/login"));
        assert!(!blocked(Method::POST, "/api/v1/auth/login"));
        assert!(!blocked(Method::POST, "/api/auth/logout"));
        assert!(!blocked(Method::POST, "/api/v1/auth/logout"));
        // Everything else under /api/auth that mutates is still blocked.
        assert!(blocked(Method::POST, "/api/auth/setup"));
        assert!(blocked(Method::POST, "/api/auth/tokens"));
        assert!(blocked(Method::DELETE, "/api/auth/tokens/9"));
    }

    #[test]
    fn non_api_paths_are_never_blocked() {
        // Non-/api writes pass: the demo's data feed (/ingest), the service
        // worker, and any static/page POST-like surface stay untouched.
        assert!(!blocked(Method::POST, "/ingest/ecowitt"));
        assert!(!blocked(Method::POST, "/ingest/tempest"));
        assert!(!blocked(Method::POST, "/some/leptos/server/fn"));
        assert!(!blocked(Method::GET, "/sw.js"));
        // A path that merely contains "api" but is not under /api/ is safe.
        assert!(!blocked(Method::POST, "/apiary"));
    }
}
