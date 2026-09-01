// /api/wizard router. Drives the first-run setup flow.
//
// Endpoints:
//   GET    /api/wizard/draft           -> current draft (or default)
//   PUT    /api/wizard/draft           -> save draft
//   DELETE /api/wizard/draft           -> clear draft (cancel + restart)
//   POST   /api/wizard/apply           -> validate + write /data/localsky.toml
//   POST   /api/wizard/test_source     -> dispatch through source adapter (Phase 6)
//   POST   /api/wizard/test_controller -> dispatch through controller adapter (Phase 5)
//   POST   /api/wizard/scan_zones      -> controller zone probe (Phase 5)
//   POST   /api/wizard/test_llm        -> probe an LLM endpoint
//   POST   /api/wizard/probe_soil      -> live Ecowitt soil enumeration
//   GET    /api/wizard/discover        -> LAN device sweep
//   GET    /api/wizard/state           -> { config_present, draft_present }
//   POST   /api/wizard/seed_current    -> draft pre-filled from live config
//   GET    /api/wizard/geocode?q=...   -> server-side Nominatim proxy
//
// MOUNTING (was: "only mounted before /data/localsky.toml exists"; that
// was never true and is the comment this header corrects). This router is
// mounted UNCONDITIONALLY, on purpose, because the surface serves two
// lifecycles, not just first-run:
//   1. First-run setup: a fresh install (is_initialized()==false) walks
//      /setup/* and finishes with POST /apply, which writes the config.
//   2. Post-setup REUSE on a configured instance:
//        - Settings -> Controllers and the Devices hub share
//          ControllerEditorPanel, which calls POST /test_controller +
//          /scan_zones to test/list a controller's stations live without
//          saving first. Settings -> Sources / the Sensors page reuse
//          /test_source, /probe_soil, /discover, /geocode the same way.
//        - The wizard is re-enterable as an EDITOR over the live config
//          ("Modify current setup"): GET /state -> POST /seed_current ->
//          draft GET/PUT -> POST /apply. So /apply + the draft + /state +
//          /seed_current are all valid AFTER the instance is configured.
// Gating this router (or any of these handlers) on is_initialized() would
// break those live Settings/Devices probes and re-entry editing on the
// owner's prod instance and on every self-hoster, so it is deliberately
// NOT gated that way.
//
// SECURITY. The hardening for this surface is authorization + demo lock,
// not a mount gate:
//   - ProbeGuard re-asserts authz INSIDE each outbound handler
//     (test_*/scan_zones/probe_soil/discover/geocode): a probe runs only
//     for an authenticated identity, a trusted-network client, or a
//     private/LAN client IP (loopback + RFC1918 + IPv6 ULA, which is what
//     a fresh self-hoster onboarding from their own LAN sends with auth
//     off and no trusted_networks set). A bare internet-anonymous caller
//     is refused regardless of global AuthMode or the pre-setup exemption
//     (was an unauthenticated SSRF trigger before). Geocode additionally
//     carries a per-IP rate limit: it relays to the shared Nominatim
//     service, whose usage policy bans bulk callers.
//   - The public read-only demo (LOCALSKY_DEMO=1) 403s every wizard
//     mutation + the outbound /discover GET at the outermost layer (see
//     auth::demo_guard), so demo.localsky.io cannot be driven or used to
//     probe.
// The owner-setup path for a genuine fresh install is untouched by both.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{FromRequestParts, Query, State},
    http::request::Parts,
    http::{Request, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use tower_http::limit::RequestBodyLimitLayer;

use crate::auth::middleware::client_ip_parts;
use crate::auth::RequestIdentity;
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
    /// Live runtime handles for config hot-reload. Present when a live engine
    /// is wired (main.rs boot); `None` in unit tests with no running engine. On
    /// apply, the engine-tunable subset of the new config is re-applied here so
    /// re-entering the wizard as an editor takes effect without a restart, the
    /// same as PUT /api/config.
    pub runtime: Option<crate::runtime::RuntimeHandles>,
}

/// Upper bound on a wizard body (LS-API-09). The largest is the draft
/// (PUT /draft), which mirrors the config shape; the probe/test bodies are
/// tiny. 2 MiB matches the config write cap and refuses an over-large body
/// before it is buffered.
const WIZARD_BODY_LIMIT: usize = 2 * 1024 * 1024;

pub fn router(state: WizardApiState) -> Router {
    Router::new()
        .route("/draft", get(get_draft).put(put_draft))
        .route("/draft", delete(delete_draft))
        .route("/apply", post(post_apply))
        .route("/test_source", post(post_test_source))
        .route("/test_controller", post(post_test_controller))
        .route("/test_llm", post(post_test_llm))
        .route("/scan_zones", post(post_scan_zones))
        .route("/probe_soil", post(post_probe_soil))
        .route("/discover", get(get_discover))
        .route("/state", get(get_state))
        .route("/seed_current", post(post_seed_current))
        .route("/geocode", get(get_geocode))
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(WIZARD_BODY_LIMIT))
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

// ---- Live-probe authorization guard. ----
//
// The wizard's live-probe / test / scan / discover endpoints make
// OUTBOUND requests to a caller-named host. Before this guard they were
// reachable by a bare anonymous internet caller, because global auth
// defaults to Disabled (middleware short-circuits to Anonymous for every
// path) AND the wizard surface is exempt pre-setup. That made them an
// unauthenticated SSRF trigger.
//
// This guard re-asserts authorization INSIDE the handler, independent of
// global AuthMode and of the pre-setup exemption. A probe runs only when
// the caller is one of:
//   - an authenticated identity (User), or a trusted-network match the
//     middleware already vouched for (TrustedNetwork), OR
//   - a PRIVATE/LAN client IP: loopback, RFC1918 (10/8, 172.16/12,
//     192.168/16) or IPv6 ULA (fc00::/7). This covers the same-box owner
//     (loopback: their own browser through a same-host proxy, healthchecks,
//     CLI) AND a fresh self-hoster onboarding from their LAN with auth off
//     and NO trusted_networks configured (their browser's client IP is a
//     private address), OR
//   - a client IP inside auth.trusted_networks (an explicitly trusted LAN,
//     still honored even if it sits outside the RFC1918/ULA ranges).
// A bare Anonymous caller from a PUBLIC address gets 403. This does not
// change the happy path: the owner (authenticated, or on a trusted LAN, or
// same-host behind oauth2-proxy) and a fresh self-hoster onboarding from
// their LAN both pass; only the internet-anonymous case is cut. The public
// demo is safe because the real client IP there is a public internet
// address (Cloudflare -> Caddy), so it is not admitted, and demo_guard 403s
// these endpoints outright regardless.
pub struct ProbeGuard;

impl FromRequestParts<WizardApiState> for ProbeGuard {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &WizardApiState,
    ) -> Result<Self, Self::Rejection> {
        // Identity the enforcement middleware attached to this request.
        let identity = parts
            .extensions
            .get::<RequestIdentity>()
            .copied()
            .unwrap_or(RequestIdentity::Anonymous);
        if matches!(
            identity,
            RequestIdentity::User(_) | RequestIdentity::TrustedNetwork
        ) {
            return Ok(ProbeGuard);
        }

        // Anonymous: admit a loopback caller, a configured trusted-network
        // client IP, OR any client whose address is itself a PRIVATE/LAN
        // address (RFC1918 10/8, 172.16/12, 192.168/16, IPv6 ULA fc00::/7,
        // plus loopback). The third clause is what keeps a fresh self-hoster
        // working: on their LAN, auth Disabled, with NO trusted_networks
        // configured, the owner's browser sends a private client IP and so
        // the live device probes (test_controller/scan_zones/probe_soil/
        // test_llm/discover) run. This stays safe on the PUBLIC demo: there
        // the client IP is a public internet address (Cloudflare -> Caddy
        // sets the real client IP), so it is NOT admitted here, and the
        // demo_guard 403s these endpoints outright anyway.
        //
        // The client IP comes from the SAME trusted-proxy/XFF derivation the
        // privileged gate uses (client_ip_parts): a raw X-Forwarded-For from
        // an untrusted peer is ignored, so a public attacker cannot forge a
        // private address by spoofing the header; only the socket peer (or a
        // hop vouched for by a configured trusted proxy) is believed.
        let policy = state.auth_rt.as_ref().map(|rt| rt.policy.load_full());
        let trusted_proxies = policy
            .as_ref()
            .map(|p| p.trusted_proxies.as_slice())
            .unwrap_or(&[]);
        if let Some(ip) = client_ip_parts(&parts.extensions, &parts.headers, trusted_proxies) {
            if crate::auth::middleware::is_private_or_loopback(&ip) {
                return Ok(ProbeGuard);
            }
            if let Some(p) = &policy {
                if p.trusted.iter().any(|net| net.contains(&ip)) {
                    return Ok(ProbeGuard);
                }
            }
        }

        Err((
            StatusCode::FORBIDDEN,
            Json(ApiError {
                error: "forbidden".into(),
                detail: Some(
                    "live device probes require an authenticated session or a request from a trusted network".into(),
                ),
            }),
        )
            .into_response())
    }
}

/// GET the draft with secrets in `draft.config` REDACTED to the sentinel,
/// matching GET /api/config and the backup/raw read paths (security wave
/// 3). The draft file lives in /data and, in the shipped default posture
/// (AuthMode::Disabled), the draft GET was an unauthenticated cleartext
/// leak of any secret typed into earlier wizard steps. The setup UI loads
/// the whole draft on mount and PUTs it back wholesale on each step, so
/// `put_draft` round-trips the sentinel via unredact_secrets against the
/// stored draft, leaving the happy path (and the just-fixed license +
/// notification persistence) unchanged.
async fn get_draft(State(s): State<WizardApiState>) -> impl IntoResponse {
    let store = s.draft_store.clone();
    let res = tokio::task::spawn_blocking(move || store.load()).await;
    let draft = match res {
        Ok(Ok(d)) => d,
        Ok(Err(WizardError::NotPresent)) => WizardDraft::default(),
        Ok(Err(e)) => return wizard_err(e).into_response(),
        Err(e) => return wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    };
    // Serialize, redact only the nested config block, return.
    let mut v = match serde_json::to_value(&draft) {
        Ok(v) => v,
        Err(e) => {
            return wizard_err(WizardError::Io(format!("serialize draft: {e}"))).into_response()
        }
    };
    if let Some(cfg) = v.get_mut("config") {
        crate::api::config::redact_secrets(cfg);
    }
    Json(v).into_response()
}

async fn put_draft(
    State(s): State<WizardApiState>,
    Json(mut candidate): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Restore any redacted secret in `config` from the stored draft so a
    // wholesale PUT of a draft that was fetched (and therefore redacted)
    // does not persist the literal sentinel as the secret. Mirrors
    // put_config's unredact + leftover-sentinel guard exactly.
    let store = s.draft_store.clone();
    let load_store = store.clone();
    let original_draft = tokio::task::spawn_blocking(move || load_store.load()).await;
    // Capture both the original config (for unredaction) and the stored
    // draft's last_updated_epoch (for optimistic concurrency below).
    let (original_cfg, stored_epoch): (Option<serde_json::Value>, Option<i64>) =
        match &original_draft {
            Ok(Ok(d)) => (
                serde_json::to_value(&d.config).ok(),
                Some(d.last_updated_epoch),
            ),
            // No stored draft yet (or a transient load issue): nothing to
            // restore from and no version to conflict with. A sentinel that
            // survives is then caught below.
            _ => (None, None),
        };
    // Optimistic concurrency (LS-API): the client echoes back the
    // last_updated_epoch it fetched on GET. If a draft is already stored and
    // its epoch differs, another writer saved in between, so refuse with 409
    // rather than silently clobbering their save (last-write-wins). A first
    // PUT against no stored draft (stored_epoch == None) always proceeds.
    if let Some(stored) = stored_epoch {
        let client_epoch = candidate.get("last_updated_epoch").and_then(|v| v.as_i64());
        // A client that omits the token entirely (older UI) is treated as
        // "unconditional" and allowed through, preserving backward compat;
        // a client that DOES send a token must match the stored version.
        if let Some(client) = client_epoch {
            if client != stored {
                return (
                    StatusCode::CONFLICT,
                    Json(ApiError {
                        error: "draft_conflict".into(),
                        detail: Some(format!(
                            "the setup draft changed since you loaded it (you have {client}, current is {stored}); reload the wizard and re-apply your edits"
                        )),
                    }),
                )
                    .into_response();
            }
        }
    }
    if let (Some(cfg), Some(orig)) = (candidate.get_mut("config"), original_cfg.as_ref()) {
        crate::api::config::unredact_secrets(cfg, orig);
    }
    // Reject any sentinel that had no stored counterpart (a brand-new
    // secret left as the placeholder), so it is never saved as the literal
    // "***redacted***" string.
    let mut leftover = Vec::new();
    if let Some(cfg) = candidate.get("config") {
        crate::api::config::remaining_sentinels(cfg, "$.config", &mut leftover);
    }
    if !leftover.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "unmatched_redacted_secret".into(),
                detail: Some(format!(
                    "redacted placeholder(s) with no stored value at: {}; supply the real secret",
                    leftover.join(", ")
                )),
            }),
        )
            .into_response();
    }
    let draft: WizardDraft = match serde_json::from_value(candidate) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "draft_decode_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response()
        }
    };
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
    // Fill the defaults the wizard promises for skipped steps (sources,
    // timezone, units), auto-mark the sole controller default, and provision
    // a VAPID keypair if Web Push was enabled with no keys. This runs BEFORE
    // validation so the validator sees the finalized config (e.g. the
    // now-marked default controller), matching what gets written to disk.
    WizardStore::finalize_for_apply(&mut draft);
    // The previous on-disk config: consulted by the region normalizer below
    // (which sources are NEW) and by the hot-reload restart-required diff.
    // `None` on a true fresh install (no prior config), which the normalizer's
    // contract handles as "every source is new".
    // Hold the read-modify-write guard across this whole load, mutate and
    // save, the way PUT /api/config and the backup restore do. The migration
    // ledger carry-forward below is a load/save sequence, so without the
    // guard it can interleave with the one-time adoption commit and write
    // back the pre-commit (empty) `ha_adoption`, un-retiring all seven helper
    // reads with nothing left to re-mark them until a restart.
    let _write_guard = s.config_store.begin_write().await;
    let prev_cfg = s.config_store.load().await.ok();
    // Region-rank the cloud sources this apply INTRODUCES, exactly like the
    // PUT /api/config path: the wizard's cloud toggles ride the draft (they
    // cannot PUT the live config mid-wizard, issue #7), so this is where a
    // draft-added cloud entry gets its researched region priority instead of
    // shipping at the flat schema default until the next boot's repair pass.
    // Runs AFTER finalize (the draft's location is in place for region
    // resolution) and BEFORE validation, matching what gets written to disk.
    crate::config::region::normalize_new_cloud_sources(prev_cfg.as_ref(), &mut draft.config);
    // Pre-apply checks against the finalized draft.
    if let Err(e) = s.draft_store.validate_for_apply(&draft) {
        return wizard_err(e).into_response();
    }
    // The Account step creates the owner directly in SQLite; reflect it
    // in the written policy so login is required from first boot.
    if let Some(rt) = &s.auth_rt {
        if rt.setup_complete.load(std::sync::atomic::Ordering::Relaxed) {
            draft.config.auth.mode = crate::config::schema::AuthMode::Required;
        }
    }
    // `ha_adoption` and `seeded_source_ids` are migration LEDGERS, not
    // tunables: they describe what happened on THIS instance, not what the
    // draft holds. The wizard is re-enterable as an editor, and its "start
    // fresh" branch builds from `WizardDraft::default()` rather than a copy of
    // the live config, so both are empty on that branch. Persisting that would
    // un-retire all seven helper reads against helpers the migration notice
    // invited the owner to delete, and `apply_runtime_config` below arc-swaps
    // the rebuilt policy immediately, so the vacation pause reads
    // `.unwrap_or(0)` on the next tick with nothing to re-mark it until a
    // restart. Same carry-forward as `post_rollback` and `post_restore`: the
    // running config's adoption record wins outright, and its seeded ids are
    // unioned in so a source the owner deleted is not re-added at the next
    // boot. `prev_cfg` is `None` on a true first install, which correctly
    // carries nothing.
    if let Some(prev) = prev_cfg.as_ref() {
        draft.config.ha_adoption = prev.ha_adoption.clone();
        for id in &prev.seeded_source_ids {
            if !draft.config.seeded_source_ids.contains(id) {
                draft.config.seeded_source_ids.push(id.clone());
            }
        }
    }
    // Write the config atomically.
    match s.config_store.save(&draft.config).await {
        Ok(v) => {
            // Genuine hot-reload: re-apply the engine-tunable subset of the new
            // config (source priorities, per-field overrides, forecast provider,
            // watering policy) to the LIVE running system so a wizard edit takes
            // effect now, not at the next restart. Mirrors PUT /api/config.
            let apply_outcome = match &s.runtime {
                Some(h) => {
                    crate::runtime::apply_runtime_config(h, prev_cfg.as_ref(), &draft.config)
                }
                None => crate::runtime::ConfigApplyOutcome::default(),
            };
            // Now that auth.mode=required is PERSISTED (ACCT-02), flip the
            // live policy to match so required enforcement starts at once
            // instead of waiting for the next refresher tick. This is the
            // only place that flips in-memory required on a fresh install:
            // post_setup deliberately defers it until the config exists, so
            // required mode and its on-disk backing always appear together.
            if let Some(rt) = &s.auth_rt {
                if draft.config.auth.mode == crate::config::schema::AuthMode::Required {
                    let policy =
                        crate::auth::middleware::AuthRuntime::policy_from_cfg(&draft.config.auth);
                    rt.policy.store(Arc::new(policy));
                }
            }
            // Drop the draft. Best-effort; if cleanup fails the next boot
            // can still resume from the draft, which is harmless.
            let ds = s.draft_store.clone();
            let _ = tokio::task::spawn_blocking(move || ds.clear()).await;
            Json(serde_json::json!({
                "saved": v,
                "restart_required": apply_outcome.restart_required,
                "restart_reasons": apply_outcome.restart_reasons,
            }))
            .into_response()
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

/// Non-secret config fields that name WHERE a probe connects. When a stored
/// secret is restored into a probe entry, these must come from the STORED
/// entry, never the client: honoring a client-supplied address alongside a
/// server-restored credential would send that credential to an arbitrary
/// host (token exfiltration; the probe surface is reachable LAN-anonymous
/// by design). Lowercase; matching is case-insensitive on the key name.
const TRANSPORT_FIELD_KEYS: &[&str] = &["base_url", "host", "broker_host", "port", "broker_port"];

/// Walk the candidate entry alongside its stored counterpart and PIN every
/// transport-named field to the stored value, recording the JSON paths whose
/// client value differed (the caller rejects on any mismatch rather than
/// silently probing a different address than the client asked for). A
/// transport field the stored entry does not carry is pinned to null.
fn pin_transport_fields(
    candidate: &mut serde_json::Value,
    stored: &serde_json::Value,
    path: &str,
    mismatched: &mut Vec<String>,
) {
    use serde_json::Value;
    match (candidate, stored) {
        (Value::Object(c), Value::Object(o)) => {
            for (k, c_val) in c.iter_mut() {
                let p = format!("{path}.{k}");
                if TRANSPORT_FIELD_KEYS.contains(&k.to_lowercase().as_str()) {
                    match o.get(k) {
                        Some(o_val) => {
                            if c_val != o_val {
                                mismatched.push(p);
                                *c_val = o_val.clone();
                            }
                        }
                        None => {
                            if !c_val.is_null() {
                                mismatched.push(p);
                                *c_val = Value::Null;
                            }
                        }
                    }
                } else if let Some(o_val) = o.get(k) {
                    pin_transport_fields(c_val, o_val, &p, mismatched);
                }
            }
        }
        (Value::Array(c), Value::Array(o)) => {
            for (i, c_v) in c.iter_mut().enumerate() {
                if let Some(o_v) = o.get(i) {
                    pin_transport_fields(c_v, o_v, &format!("{path}[{i}]"), mismatched);
                }
            }
        }
        _ => {}
    }
}

/// Resolve redacted-secret sentinels in a probe's controller entry against
/// the stored config (and, failing that, the wizard draft), matched BY ID,
/// exactly like PUT /api/config. The settings editor and the wizard both
/// hold the entry as served by their GET, which redacts secrets to the
/// sentinel; posting that entry to test_controller/scan_zones verbatim
/// would send the literal sentinel to the vendor cloud and fail auth. A
/// sentinel that cannot be resolved (new entry, renamed id) is a plain 400
/// telling the caller to re-enter the secret.
///
/// SECURITY: whenever a stored secret is restored here, the entry's
/// transport fields (base_url/host/port and friends) are pinned to the
/// stored entry's values, and a probe whose transport differs from stored
/// is refused with a plain 400. Without this, a LAN-anonymous caller could
/// post `{api_token: "***redacted***", base_url: "http://their-host"}` and
/// have the server deliver the real stored token to their host. Probes that
/// carry the real secret (no sentinel) are untouched: the caller already
/// knows the credential, so directing it is not an escalation.
async fn resolve_probe_entry_secrets(
    s: &WizardApiState,
    entry: crate::config::schema::ControllerEntry,
) -> Result<crate::config::schema::ControllerEntry, Box<Response>> {
    use crate::api::config::{remaining_sentinels, unredact_secrets};

    let mut entry_json = match serde_json::to_value(&entry) {
        Ok(v) => v,
        Err(_) => return Ok(entry), // nothing sensible to do; probe as-is
    };
    let mut leftover = Vec::new();
    remaining_sentinels(&entry_json, "$", &mut leftover);
    if leftover.is_empty() {
        return Ok(entry);
    }

    // A stored controller entry with the same id (live config first, wizard
    // draft second) supplies the real secret values. The FIRST stored entry
    // found is also the transport-pinning authority below.
    let stored_entry_json = |controllers: serde_json::Value| -> Option<serde_json::Value> {
        controllers.as_array().and_then(|arr| {
            arr.iter()
                .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(entry.id.as_str()))
                .cloned()
        })
    };
    let mut pin_source: Option<serde_json::Value> = None;
    if let Ok(cfg) = s.config_store.load().await {
        if let Ok(v) = serde_json::to_value(&cfg.controllers) {
            if let Some(stored) = stored_entry_json(v) {
                unredact_secrets(&mut entry_json, &stored);
                pin_source = Some(stored);
            }
        }
    }
    leftover.clear();
    remaining_sentinels(&entry_json, "$", &mut leftover);
    if !leftover.is_empty() {
        let store = s.draft_store.clone();
        if let Ok(Ok(draft)) = tokio::task::spawn_blocking(move || store.load()).await {
            if let Ok(v) = serde_json::to_value(&draft.config.controllers) {
                if let Some(stored) = stored_entry_json(v) {
                    unredact_secrets(&mut entry_json, &stored);
                    if pin_source.is_none() {
                        pin_source = Some(stored);
                    }
                }
            }
        }
    }
    leftover.clear();
    remaining_sentinels(&entry_json, "$", &mut leftover);
    if !leftover.is_empty() {
        // Boxed: Response is large and Ok is the overwhelmingly common
        // path (clippy::result_large_err).
        return Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: "unmatched_redacted_secret".into(),
                    detail: Some(
                        "this controller's secret is shown redacted and no stored value matches \
                         it; re-enter the secret and try again"
                            .into(),
                    ),
                }),
            )
                .into_response(),
        ));
    }

    // Secrets were restored from a stored entry: pin the transport fields
    // to that entry and refuse the probe when the client's differed.
    if let Some(stored) = &pin_source {
        let mut mismatched = Vec::new();
        pin_transport_fields(&mut entry_json, stored, "$", &mut mismatched);
        if !mismatched.is_empty() {
            return Err(Box::new(
                (
                    StatusCode::BAD_REQUEST,
                    Json(ApiError {
                        error: "transport_field_mismatch".into(),
                        detail: Some(format!(
                            "this controller's stored secret is only sent to its saved address, \
                             and the probe changed {}. Save the address change first, or re-enter \
                             the secret to probe a different address",
                            mismatched.join(", ")
                        )),
                    }),
                )
                    .into_response(),
            ));
        }
    }

    match serde_json::from_value(entry_json) {
        Ok(resolved) => Ok(resolved),
        Err(_) => Ok(entry),
    }
}

async fn post_test_controller(
    _guard: ProbeGuard,
    State(s): State<WizardApiState>,
    Json(body): Json<TestControllerBody>,
) -> impl IntoResponse {
    let entry = match resolve_probe_entry_secrets(&s, body.controller).await {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    // Rachio runs through its concrete adapter so the probe can surface
    // what the trait object cannot: device auto-discovery from just the
    // token, and the cloud's remaining daily request budget.
    if let crate::config::schema::ControllerKind::Rachio(rc) = &entry.controller {
        return test_rachio_controller(&entry.id, rc).await;
    }
    match crate::runtime::build_test_controller(&entry) {
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
            // Do NOT reflect the raw upstream/transport error string: it
            // embeds the operator-supplied target URL + OS/TLS text and would
            // turn this probe into an SSRF/exfil oracle. The ControllerError
            // already carries a trimmed category (its adapters wrap reqwest via
            // net::reqwest_error_category), so surface that, not the upstream's
            // own bytes. Consistent with the Wave-1 body-trim.
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "controller_unreachable".into(),
                    detail: Some(controller_error_detail(&e)),
                }),
            )
                .into_response(),
        },
        // build_test_controller's error is our own "unsupported kind" message,
        // not upstream text, so it is safe to surface verbatim.
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

/// Rachio-specific controller test. Two additions over the generic path:
///
/// 1. Device auto-discovery: with an api_token but NO device_id, resolve
///    the account's first device (GET /person/info -> GET /person/{id}),
///    run the status probe against it, and REPORT it in the result as
///    `discovered_device` so the form can offer it. Config is never
///    written silently.
/// 2. `rate_limit_remaining` carries the cloud's X-RateLimit-Remaining
///    header when it was present, so the operator can watch the daily
///    request budget from the test button.
async fn test_rachio_controller(id: &str, rc: &crate::config::schema::RachioConfig) -> Response {
    use crate::controllers::rachio::Rachio;
    // Method-call syntax on the CONCRETE adapter still resolves through the
    // trait, which must be in scope.
    use crate::ports::irrigation_controller::IrrigationController as _;

    fn unreachable_response(e: &crate::ports::irrigation_controller::ControllerError) -> Response {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "controller_unreachable".into(),
                detail: Some(controller_error_detail(e)),
            }),
        )
            .into_response()
    }

    let mut cfg = rc.clone();
    let mut discovered: Option<serde_json::Value> = None;
    if cfg.device_id.trim().is_empty() && !cfg.api_token.trim().is_empty() {
        let probe = match Rachio::new(format!("{id}_probe"), cfg.clone()) {
            Ok(p) => p,
            Err(e) => return unreachable_response(&e),
        };
        match probe.resolve_device_id().await {
            Ok(found) => {
                cfg.device_id = found.device_id.clone();
                discovered = Some(serde_json::json!({
                    "device_id": found.device_id,
                    "name": found.name,
                    "device_count": found.device_count,
                }));
            }
            Err(e) => return unreachable_response(&e),
        }
    }
    let adapter = match Rachio::new(id.to_string(), cfg) {
        Ok(a) => a,
        Err(e) => return unreachable_response(&e),
    };
    match adapter.status().await {
        Ok(st) => Json(serde_json::json!({
            "ok": true,
            "reachable": st.reachable,
            "master_enabled": st.master_enabled,
            "water_level_pct": st.water_level_pct,
            "zone_count": st.zone_states.len(),
            "firmware": st.firmware,
            "discovered_device": discovered,
            "rate_limit_remaining": adapter.last_rate_limit_remaining(),
        }))
        .into_response(),
        Err(e) => unreachable_response(&e),
    }
}

/// Map a `ControllerError` to a caller-safe detail string. Auth/zone/rate
/// failures are our own labels and safe verbatim; the Transport/Init/Remote
/// variants may carry upstream/transport text (the adapters now feed them a
/// trimmed category, but be defensive here too), so collapse them to a fixed
/// category rather than reflecting whatever string they hold.
fn controller_error_detail(e: &crate::ports::irrigation_controller::ControllerError) -> String {
    use crate::ports::irrigation_controller::ControllerError as CE;
    match e {
        CE::Offline => "controller offline".into(),
        CE::ZoneUnknown(z) => format!("zone unknown: {z}"),
        CE::RateLimited => "rate limited".into(),
        CE::AuthFailed => "authentication failed".into(),
        CE::Unsupported(_) => "operation not supported by this controller".into(),
        // These can carry upstream/transport text; do not reflect it.
        CE::Remote(_) => "controller returned an error".into(),
        CE::Transport(_) => "could not reach the controller".into(),
        CE::Init(_) => "controller client could not be initialized".into(),
    }
}

#[derive(Debug, Deserialize)]
struct TestLlmBody {
    pub llm: crate::config::schema::LlmConfig,
}

async fn post_test_llm(
    _guard: ProbeGuard,
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
        // Same as the controller probe: the LlmError can carry the upstream
        // transport/parse text (which embeds the operator-supplied base_url),
        // so surface a trimmed category instead of e.to_string().
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "llm_unreachable".into(),
                detail: Some(llm_error_detail(&e)),
            }),
        )
            .into_response(),
    }
}

/// Caller-safe detail for an `LlmError`. Mirrors `controller_error_detail`:
/// the Remote/Transport/Parse/ModelUnavailable variants may carry upstream
/// text, so collapse them to a fixed category rather than reflecting it.
fn llm_error_detail(e: &crate::ports::llm_provider::LlmError) -> String {
    use crate::ports::llm_provider::LlmError as LE;
    match e {
        LE::Offline => "provider offline".into(),
        LE::AuthFailed => "authentication failed".into(),
        LE::ModelUnavailable(_) => "model unavailable".into(),
        LE::RateLimited => "rate limited".into(),
        LE::Remote(_) => "provider returned an error".into(),
        LE::Transport(_) => "could not reach the provider".into(),
        LE::Parse(_) => "provider response could not be parsed".into(),
    }
}

async fn post_scan_zones(
    _guard: ProbeGuard,
    State(s): State<WizardApiState>,
    Json(body): Json<TestControllerBody>,
) -> impl IntoResponse {
    // Editing an EXISTING cloud controller serves its api_token/password as
    // the redaction sentinel; restore the stored secret by entry id (the
    // put_config pattern) so "Scan zones" works from the editor without
    // retyping the token. Unresolvable sentinels are a plain 400.
    let entry = match resolve_probe_entry_secrets(&s, body.controller).await {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    match crate::runtime::build_test_controller(&entry) {
        Ok(c) => match c.discover_zones().await {
            Ok(zones) => Json(serde_json::json!({ "zones": zones })).into_response(),
            // Trimmed category, not the raw upstream/transport string (the
            // discover call hits the operator-supplied controller host).
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "zone_scan_failed".into(),
                    detail: Some(controller_error_detail(&e)),
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

// ---- Live soil enumeration for the Sensors step. ----
//
// The wizard draft is NOT applied while the wizard runs, so no
// `ecowitt_gw_poll` source is actually polling and `/api/v1/sensors/inventory`
// lists nothing for a gateway the user just added in the Weather step. To
// still show real probes, this endpoint queries the gateway's local API
// directly over the LAN (`GET http://<host>/get_livedata_info`) and reuses the
// exact same parser the live poller uses, so the channel ids it returns
// (`source:<src>:soilmoisture<N>`) are byte-identical to what the engine
// resolves post-apply.

#[derive(Debug, Deserialize)]
struct ProbeSoilBody {
    /// Gateway IP or hostname on the LAN (from the draft source's config.host).
    host: String,
    /// Draft source id, used to build canonical `source:<id>:soilmoisture<N>`
    /// channel ids that match the eventual live binding. Defaults to
    /// "ecowitt_gw" (the id the discovery "Add" button uses).
    #[serde(default = "default_probe_source_id")]
    source_id: String,
}

fn default_probe_source_id() -> String {
    "ecowitt_gw".to_string()
}

#[derive(Debug, Serialize)]
struct ProbeChannel {
    /// Channel number as the gateway reports it ("1".."N").
    channel: String,
    /// Canonical engine address, identical to a live `soil_sensor_id`:
    /// `source:<source_id>:soilmoisture<channel>`.
    id: String,
    /// Live moisture %, when the gateway reported it.
    moisture_pct: Option<f64>,
    /// Soil battery %, when present (already scaled 0..100 by the parser).
    battery_pct: Option<f64>,
    /// Soil temperature F, when present.
    temp_f: Option<f64>,
    /// Soil EC, when present.
    ec: Option<f64>,
}

/// POST /api/wizard/probe_soil { host, source_id? } -> the gateway's current
/// soil channels read live off its local API. User-initiated (the Sensors
/// step calls it per configured gateway). A few seconds at worst.
async fn post_probe_soil(
    _guard: ProbeGuard,
    State(_s): State<WizardApiState>,
    Json(body): Json<ProbeSoilBody>,
) -> impl IntoResponse {
    let host = body.host.trim();
    if host.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "missing_host".into(),
                detail: Some("gateway host is required".into()),
            }),
        )
            .into_response();
    }
    let url = format!("http://{host}/get_livedata_info");
    // SSRF-hardened client: rejects loopback/metadata/link-local/multicast
    // targets, allows private LAN ranges (the gateway lives on the LAN),
    // pins the resolved IP (anti DNS-rebinding) and disables redirects.
    // Keeps the existing 8s budget.
    let (client, safe_url) =
        match crate::net::safe_fetch::build_safe_client(&url, std::time::Duration::from_secs(8))
            .await
        {
            Ok(pair) => pair,
            Err(crate::net::safe_fetch::SafeFetchError::BlockedTarget)
            | Err(crate::net::safe_fetch::SafeFetchError::UnsupportedScheme) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(ApiError {
                        error: "invalid_target".into(),
                        detail: Some("gateway host is not a permitted device address".into()),
                    }),
                )
                    .into_response()
            }
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: "gateway_unreachable".into(),
                        detail: Some("could not resolve or reach the gateway host".into()),
                    }),
                )
                    .into_response()
            }
        };
    let body_json = match client
        .get(safe_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        // Capped body read (never reqwest's unbounded .json()): an
        // operator-named target streaming an endless body must not be able
        // to OOM the service (see safe_fetch::MAX_RESPONSE_BODY_BYTES).
        Ok(r) => match crate::net::safe_fetch::read_json_capped::<serde_json::Value>(r).await {
            Ok(v) => v,
            // Do NOT reflect the raw parse error / upstream body: it would
            // turn this into an SSRF exfil oracle. A category is enough.
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: "gateway_parse_error".into(),
                        detail: Some("gateway response was not the expected livedata JSON".into()),
                    }),
                )
                    .into_response()
            }
        },
        // Same here: surface reachability, not the upstream's own message.
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "gateway_unreachable".into(),
                    detail: Some("gateway did not return a successful response".into()),
                }),
            )
                .into_response()
        }
    };

    let channels = soil_channels_from_livedata(&body_json, &body.source_id);
    Json(serde_json::json!({ "ok": true, "channels": channels })).into_response()
}

/// Reduce a `/get_livedata_info` body to one ProbeChannel per soil-moisture
/// channel, gathering the battery/temp/EC siblings the parser emits under
/// `soil{batt,temp,ec}<N>`. Reuses `ecowitt_gw_poll::parse_livedata` so the
/// keys (and therefore the channel ids) match the live poller exactly.
fn soil_channels_from_livedata(body: &serde_json::Value, source_id: &str) -> Vec<ProbeChannel> {
    use std::collections::BTreeMap;
    // epoch is irrelevant here (we only read keys/values), pass 0.
    let readings = crate::sources::ecowitt_gw_poll::parse_livedata(body, source_id, 0);

    // Collect by channel number parsed off the key suffix.
    let mut by_ch: BTreeMap<String, ProbeChannel> = BTreeMap::new();
    let ensure = |map: &mut BTreeMap<String, ProbeChannel>, ch: &str| {
        map.entry(ch.to_string()).or_insert_with(|| ProbeChannel {
            channel: ch.to_string(),
            id: format!("source:{source_id}:soilmoisture{ch}"),
            moisture_pct: None,
            battery_pct: None,
            temp_f: None,
            ec: None,
        });
    };

    for r in &readings {
        if let Some(ch) = r.key.strip_prefix("soilmoisture") {
            ensure(&mut by_ch, ch);
            by_ch.get_mut(ch).unwrap().moisture_pct = Some(r.value);
        } else if let Some(ch) = r.key.strip_prefix("soilbatt") {
            ensure(&mut by_ch, ch);
            by_ch.get_mut(ch).unwrap().battery_pct = Some(r.value);
        } else if let Some(rest) = r.key.strip_prefix("soiltemp") {
            // key shape is soiltemp<N>f.
            let ch = rest.trim_end_matches('f');
            ensure(&mut by_ch, ch);
            by_ch.get_mut(ch).unwrap().temp_f = Some(r.value);
        } else if let Some(ch) = r.key.strip_prefix("soilec") {
            ensure(&mut by_ch, ch);
            by_ch.get_mut(ch).unwrap().ec = Some(r.value);
        }
    }

    // Only surface channels that actually reported a moisture reading (a bare
    // battery/temp sibling without moisture is not a usable probe to bind).
    by_ch
        .into_values()
        .filter(|c| c.moisture_pct.is_some())
        .collect()
}

/// Count the live soil-moisture channels a discovered Ecowitt gateway is
/// carrying, by fetching its `/get_livedata_info` through the SAME
/// SSRF-hardened `build_safe_client` that `post_probe_soil` uses and running
/// `soil_channels_from_livedata`. Best-effort enrichment for discovery: any
/// failure (blocked target, unreachable, non-JSON body, no soil) yields 0 and
/// NEVER errors the scan. The per-gateway fetch is capped well under the
/// discover handler's ~8s budget so a slow or absent gateway shortens the
/// sweep rather than hanging it; the source_id passed to the parser is
/// irrelevant to the count (channels are keyed off the livedata keys), so a
/// stable placeholder is used.
async fn ecowitt_soil_channels(ip: &str) -> u32 {
    // Tight per-gateway cap: a real gateway answers in well under this on a
    // LAN, so the timeout only shortens dead/slow probes and keeps the whole
    // enrichment under the discover handler's budget.
    const ECOWITT_LIVEDATA_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
    let url = format!("http://{ip}/get_livedata_info");
    let (client, safe_url) =
        match crate::net::safe_fetch::build_safe_client(&url, ECOWITT_LIVEDATA_BUDGET).await {
            Ok(pair) => pair,
            // Blocked/unsupported/unreachable target: not a soil gateway as far
            // as discovery is concerned. Leave the count at 0, do not error.
            Err(_) => return 0,
        };
    let body_json = match client
        .get(safe_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        // Capped read, same rationale as post_probe_soil.
        Ok(r) => match crate::net::safe_fetch::read_json_capped::<serde_json::Value>(r).await {
            Ok(v) => v,
            Err(_) => return 0,
        },
        Err(_) => return 0,
    };
    soil_channels_from_livedata(&body_json, "ecowitt_gw").len() as u32
}

// ---- Re-entry support: state probe + seed-from-current. ----

/// GET /api/wizard/state -> { config_present, draft_present }. The setup
/// shell uses it to offer "modify current setup" vs "start fresh" when
/// the wizard is re-entered on an already-configured instance.
async fn get_state(State(s): State<WizardApiState>) -> impl IntoResponse {
    let draft_present = s.draft_store.exists();
    let config_present = s.config_store.is_initialized();
    Json(serde_json::json!({
        "config_present": config_present,
        "draft_present": draft_present,
    }))
}

/// POST /api/wizard/seed_current -> start a draft pre-filled from the
/// live config (license already accepted on the original run), so the
/// wizard becomes an editor over the existing setup instead of a wipe.
async fn post_seed_current(State(s): State<WizardApiState>) -> impl IntoResponse {
    let cfg = match s.config_store.load().await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(ApiError {
                    error: "no_config".into(),
                    detail: Some(format!("nothing to modify: {e}")),
                }),
            )
                .into_response()
        }
    };
    let draft = WizardDraft {
        current_step: crate::config::wizard::WizardStep::Location,
        config: cfg,
        license_accepted: true,
        telemetry_choice: Some(false),
        // Seeded from the LIVE config: the supply question was answered (or
        // skipped) long ago, so the draft starts unanswered and the config's
        // own interleave_cycles value carries the earlier decision.
        water_supply: None,
        last_updated_epoch: chrono::Utc::now().timestamp(),
    };
    let store = s.draft_store.clone();
    match tokio::task::spawn_blocking(move || store.save(&draft)).await {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => wizard_err(e).into_response(),
        Err(e) => wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    }
}

// ---- Network discovery. One aggregated sweep for the wizard. ----

/// GET /api/wizard/discover -> everything findable on the LAN right now:
/// passive Tempest (any hub already broadcasting on UDP 50222), Ecowitt
/// gateways (UDP broadcast probe), OpenSprinkler controllers (HTTP /24
/// sweep). User-initiated only; total wall time a few seconds.
async fn get_discover(_guard: ProbeGuard, State(s): State<WizardApiState>) -> impl IntoResponse {
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

    // The OpenSprinkler sweep probes every host of every local /24. On a
    // host-network container that means every network interface + docker bridge the host
    // sees, and the per-host timeout is NOT a total bound -- so without an
    // overall deadline the request can run for minutes and the wizard spinner
    // hangs with no error (the reported bug). Hard-cap the sweep so it always
    // returns within a few seconds; ecowitt keeps its own 3s UDP window, so
    // wrapping only the opensprinkler future preserves ecowitt's result even if
    // the subnet sweep is the slow one.
    // 8s overall cap with a short 400ms per-host connect: a real controller
    // answers in well under that, so the timeout only shortens dead-IP probes
    // and the whole sweep finishes in a few seconds on a typical multi-subnet
    // host -- fast enough to actually find the controller before the cap, not
    // just fast enough to stop hanging.
    const DISCOVER_OPENSPRINKLER_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);
    let (ecowitt, opensprinkler) = tokio::join!(
        async {
            // The UDP sweep returns identity only; it cannot tell a soil-
            // bearing gateway from a weather-only one. Enrich each gateway
            // with a live soil-channel count so the wizard recognizes it as a
            // soil gateway at scan time (not only later in the Sensors step).
            let mut gateways =
                crate::discovery::ecowitt::discover_ecowitt(std::time::Duration::from_secs(3))
                    .await;
            for gw in &mut gateways {
                gw.soil_channels = ecowitt_soil_channels(&gw.ip).await;
            }
            gateways
        },
        async {
            tokio::time::timeout(
                DISCOVER_OPENSPRINKLER_BUDGET,
                crate::discovery::opensprinkler::discover_opensprinkler(
                    std::time::Duration::from_millis(400),
                ),
            )
            .await
            .unwrap_or_default()
        },
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

/// Geocode limiter budget: max 10 requests per fixed one-minute window per
/// client IP, the same budget AuthRuntime::allow_login_attempt grants
/// login/setup. Generous for a human (the UI fires ONE request per Search
/// click and serializes them behind a `searching` flag) and far under
/// Nominatim's absolute 1 req/s ceiling.
const GEOCODE_MAX_PER_MIN: u32 = 10;

/// Geocode limiter state: ip -> (attempts, window_start_epoch). Mirrors
/// AuthRuntime::login_attempts but is deliberately its OWN bucket: sharing
/// the login map would let address searches consume login attempts (and
/// failed logins block address search), coupling two unrelated surfaces.
static GEOCODE_ATTEMPTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (u32, i64)>>,
> = std::sync::LazyLock::new(Default::default);

/// Fixed-window limiter for the geocode relay, same shape as
/// AuthRuntime::allow_login_attempt (count per IP, window reset after 60s,
/// opportunistic shrink so the map cannot grow unbounded).
fn allow_geocode_attempt(ip: std::net::IpAddr) -> bool {
    let now = chrono::Utc::now().timestamp();
    let mut map = GEOCODE_ATTEMPTS.lock().expect("geocode limiter lock");
    let entry = map.entry(ip).or_insert((0, now));
    if now - entry.1 >= 60 {
        *entry = (0, now);
    }
    entry.0 += 1;
    let allowed = entry.0 <= GEOCODE_MAX_PER_MIN;
    if map.len() > 4096 {
        map.retain(|_, (_, start)| now - *start < 60);
    }
    allowed
}

/// GET /api/wizard/geocode?q= -> Nominatim search relay. ProbeGuard-gated
/// like every other outbound wizard handler: without it this was an
/// unauthenticated, un-rate-limited relay any internet-anonymous caller
/// (the public demo above all) could drive, and Nominatim bans bulk
/// callers BY User-Agent, so one abuser could break address search for
/// every LocalSky deployment sharing the agent string.
async fn get_geocode(
    _guard: ProbeGuard,
    State(s): State<WizardApiState>,
    Query(q): Query<GeocodeQuery>,
    req: Request<Body>,
) -> impl IntoResponse {
    // Per-IP rate limit on top of the guard: even an admitted LAN client
    // must not be able to turn this instance into a bulk geocoding relay
    // (Nominatim usage policy: absolute max 1 req/s, no bulk). Client IP
    // via the same trusted-proxy/XFF derivation the guard + login limiter
    // use, so a spoofed X-Forwarded-For cannot mint fresh buckets. No
    // derivable IP (no ConnectInfo, e.g. some test harnesses) skips the
    // limit, matching the login limiter's posture in auth.rs.
    let policy = s.auth_rt.as_ref().map(|rt| rt.policy.load_full());
    let trusted_proxies = policy
        .as_ref()
        .map(|p| p.trusted_proxies.as_slice())
        .unwrap_or(&[]);
    if let Some(ip) = crate::auth::middleware::client_ip(&req, trusted_proxies) {
        if !allow_geocode_attempt(ip) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ApiError {
                    error: "rate_limited".into(),
                    detail: Some("too many geocode requests; wait a minute".into()),
                }),
            )
                .into_response();
        }
    }

    let url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=json&limit=5",
        urlencode(&q.q)
    );
    // Bounded budget: Client::new() has NO total timeout, so a stalled
    // upstream would pin handler tasks for as long as it likes.
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "geocode_transport_error".into(),
                    detail: Some("client build failed".into()),
                }),
            )
                .into_response()
        }
    };
    let res = client
        .get(&url)
        // Nominatim ToS requires a meaningful User-Agent identifying the
        // operator. The deployment name + URL is reasonable; users can
        // override via wizard customization.
        .header("User-Agent", "LocalSky setup wizard")
        .send()
        .await;
    // Trimmed categories, not the raw upstream/transport string, consistent
    // with the other probe handlers (Nominatim is a fixed cloud host so the
    // SSRF-oracle risk is low, but keep the no-raw-upstream-text rule uniform).
    match res {
        // Capped body read, like the other outbound handlers: a fixed host
        // is still an unbounded-buffer risk behind a broken CDN/proxy.
        Ok(r) => match crate::net::safe_fetch::read_json_capped::<Vec<GeocodeResult>>(r).await {
            Ok(results) => Json(results).into_response(),
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "geocode_parse_error".into(),
                    detail: Some("geocoder response could not be parsed".into()),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "geocode_transport_error".into(),
                detail: Some(crate::net::reqwest_error_category(&e).to_string()),
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

#[cfg(test)]
mod draft_redaction_tests {
    use super::*;
    use crate::config::schema::*;

    fn state_in(dir: &std::path::Path) -> WizardApiState {
        WizardApiState {
            draft_store: Arc::new(WizardStore::new(dir.join("wizard-draft.json"))),
            config_store: Arc::new(FileConfigStore::new(dir.join("localsky.toml"))),
            auth_rt: None,
            tempest_store: None,
            runtime: None,
        }
    }

    fn draft_with_secret() -> WizardDraft {
        let mut cfg = Config::default();
        cfg.sources.push(SourceEntry {
            id: "ha_pass".into(),
            priority: 30,
            enabled: true,
            max_age_s: None,
            source: SourceKind::HaPassthrough(HaPassthroughConfig {
                base_url: "http://ha.local:8123".into(),
                bearer_token: "supersecret_ha_token_xyz".into(),
                field_map: Default::default(),
                soil_zone_map: Default::default(),
            }),
        });
        WizardDraft {
            current_step: crate::config::wizard::WizardStep::Sources,
            config: cfg,
            license_accepted: true,
            telemetry_choice: Some(false),
            water_supply: None,
            last_updated_epoch: 0,
        }
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn get_draft_redacts_and_put_round_trips_the_sentinel() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-wizard-test-{}-redact",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = state_in(&dir);

        // Seed a draft carrying a real secret.
        s.draft_store.save(&draft_with_secret()).unwrap();

        // GET redacts the secret in draft.config.
        let got = body_json(get_draft(State(s.clone())).await.into_response()).await;
        let serialized = serde_json::to_string(&got).unwrap();
        assert!(
            !serialized.contains("supersecret_ha_token_xyz"),
            "draft GET leaked the HA bearer token"
        );
        assert!(
            serialized.contains(crate::api::config::SECRET_REDACTED_SENTINEL),
            "sentinel must appear in the redacted draft"
        );
        // Non-secret context still rides along (UI needs it).
        assert!(serialized.contains("ha.local:8123"));
        assert!(serialized.contains("license_accepted"));

        // Simulate the setup UI: it loaded the (redacted) draft, edited a
        // NON-secret field, and PUTs the WHOLE thing back.
        let mut candidate = got.clone();
        candidate["current_step"] = serde_json::json!("controllers");
        let put = put_draft(State(s.clone()), Json(candidate))
            .await
            .into_response();
        assert_eq!(put.status(), StatusCode::NO_CONTENT, "PUT must succeed");

        // The stored draft kept the REAL secret (sentinel was unredacted),
        // and the edited field persisted.
        let stored = s.draft_store.load().unwrap();
        let SourceKind::HaPassthrough(ha) = &stored.config.sources[0].source else {
            panic!("expected ha_passthrough source");
        };
        assert_eq!(
            ha.bearer_token, "supersecret_ha_token_xyz",
            "round-trip must restore the real secret, not persist the sentinel"
        );
        assert_eq!(
            stored.current_step,
            crate::config::wizard::WizardStep::Controllers
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_draft_rejects_new_secret_left_as_sentinel() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-wizard-test-{}-newsecret",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = state_in(&dir);

        // No stored draft. A PUT whose config carries a literal sentinel for
        // a secret has nothing to restore from, so it must be rejected
        // rather than persisting "***redacted***" as the token.
        let mut draft = draft_with_secret();
        if let SourceKind::HaPassthrough(ha) = &mut draft.config.sources[0].source {
            ha.bearer_token = crate::api::config::SECRET_REDACTED_SENTINEL.to_string();
        }
        let candidate = serde_json::to_value(&draft).unwrap();
        let resp = put_draft(State(s.clone()), Json(candidate))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // And nothing got written.
        assert!(s.draft_store.load().is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn put_draft_optimistic_concurrency() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-wizard-test-{}-concurrency",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = state_in(&dir);

        // First PUT against no stored draft always succeeds (no version to
        // conflict with).
        let mut draft = WizardDraft::default();
        draft.license_accepted = true;
        let v0 = serde_json::to_value(&draft).unwrap();
        let r0 = put_draft(State(s.clone()), Json(v0)).await.into_response();
        assert_eq!(
            r0.status(),
            StatusCode::NO_CONTENT,
            "first PUT must succeed"
        );

        // The save bumped last_updated_epoch; capture the stored token a GET
        // would return.
        let stored_epoch = s.draft_store.load().unwrap().last_updated_epoch;

        // A PUT echoing a STALE token (someone else saved in between) is a 409.
        let mut stale = serde_json::to_value(&draft).unwrap();
        stale["last_updated_epoch"] = serde_json::json!(stored_epoch - 1);
        let r_conflict = put_draft(State(s.clone()), Json(stale))
            .await
            .into_response();
        assert_eq!(
            r_conflict.status(),
            StatusCode::CONFLICT,
            "a stale last_updated_epoch must 409, not silently last-write-win"
        );

        // A PUT echoing the CURRENT token proceeds.
        let mut fresh = serde_json::to_value(&draft).unwrap();
        fresh["last_updated_epoch"] = serde_json::json!(stored_epoch);
        let r_ok = put_draft(State(s.clone()), Json(fresh))
            .await
            .into_response();
        assert_eq!(
            r_ok.status(),
            StatusCode::NO_CONTENT,
            "a matching token must save"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod draft_cloud_apply_tests {
    use super::*;

    fn state_in(dir: &std::path::Path) -> WizardApiState {
        WizardApiState {
            draft_store: Arc::new(WizardStore::new(dir.join("wizard-draft.json"))),
            config_store: Arc::new(FileConfigStore::new(dir.join("localsky.toml"))),
            auth_rt: None,
            tempest_store: None,
            runtime: None,
        }
    }

    #[tokio::test]
    async fn draft_added_cloud_source_survives_apply_and_reload() {
        // The wizard sources step's DRAFT-mode cloud toggle (issue #7) writes
        // the draft exactly like this: location set on the location step, then
        // an open_meteo entry spliced into config.sources as {id, kind,
        // enabled, config} (no priority; the schema default carries it), PUT
        // through the real draft handler. Prove the whole pipeline holds:
        // PUT /draft -> POST /apply (finalize_for_apply + validate_for_apply
        // + save) -> the stored config contains the source AND loads back
        // cleanly.
        let dir = std::env::temp_dir().join(format!(
            "localsky-wizard-test-{}-cloudapply",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = state_in(&dir);

        let mut draft = WizardDraft::default();
        draft.license_accepted = true;
        draft.telemetry_choice = Some(false);
        draft.config.deployment.location.lat = 29.9;
        draft.config.deployment.location.lon = -81.3;
        let mut candidate = serde_json::to_value(&draft).unwrap();
        candidate["config"]["sources"] = serde_json::json!([{
            "id": "open_meteo",
            "kind": "open_meteo",
            "enabled": true,
            "config": {
                "forecast_days": 7,
                "forecast_hours": 48,
                "past_days": 1,
                "include_radar": true,
            },
        }, {
            // A kind whose researched US region rank (75) differs from the
            // flat schema default (50): apply must region-rank draft-added
            // clouds via normalize_new_cloud_sources, not ship them at 50
            // until the next boot's repair pass.
            "id": "noaa_mrms",
            "kind": "noaa_mrms",
            "enabled": true,
            "config": {},
        }]);
        let put = put_draft(State(s.clone()), Json(candidate))
            .await
            .into_response();
        assert_eq!(
            put.status(),
            StatusCode::NO_CONTENT,
            "a draft carrying a spliced cloud source must save"
        );

        let apply = post_apply(State(s.clone())).await.into_response();
        assert_eq!(
            apply.status(),
            StatusCode::OK,
            "apply must accept a draft whose cloud source came from the wizard toggle"
        );

        // The stored config carries the source and loads back cleanly.
        let cfg = s
            .config_store
            .load()
            .await
            .expect("the applied config loads back");
        let om = cfg
            .sources
            .iter()
            .find(|src| src.id == "open_meteo")
            .expect("the draft-added cloud source survived finalize + validate + save");
        assert!(om.enabled, "the enabled flag survives apply");
        assert!(
            matches!(om.source, crate::config::schema::SourceKind::OpenMeteo(_)),
            "the kind + config block deserialized into the typed source"
        );
        // Region ranking happened AT APPLY: the draft entries carried no
        // priority, and NOAA MRMS's researched US rank is 75, not the flat 50.
        let mrms = cfg
            .sources
            .iter()
            .find(|src| src.id == "noaa_mrms")
            .expect("the second draft-added cloud source survived apply");
        assert_eq!(
            mrms.priority, 75,
            "apply region-ranks draft-added clouds (normalize_new_cloud_sources)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Re-entering the wizard as an editor and taking the "start fresh" branch
    // builds from WizardDraft::default(), which carries no .
    // Saving that verbatim would un-retire all seven helper reads against
    // helpers the migration notice invited the owner to delete, and apply
    // hot-swaps the rebuilt policy, so it lands on the next tick with nothing
    // to re-mark it until a restart.
    #[tokio::test]
    async fn apply_keeps_the_migration_ledger_the_draft_does_not_carry() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-wizard-test-{}-ledger",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = state_in(&dir);

        // A migrated install already on disk.
        let mut stored = crate::config::schema::Config::default();
        stored.deployment.location.lat = 29.9;
        stored.deployment.location.lon = -81.3;
        for id in crate::ha_adopt::ENTITIES {
            stored
                .ha_adoption
                .push(crate::ha::snapshot::HaAdoptedHelper {
                    entity: id.to_string(),
                    outcome: crate::ha_adopt::OUTCOME_ADOPTED.to_string(),
                    target: crate::ha_adopt::target_of(id).to_string(),
                    adopted_value: Some("0".into()),
                    observed_value: None,
                    previous_value: Some("0".into()),
                    epoch: 1,
                });
        }
        stored.seeded_source_ids.push("nws".into());
        s.config_store.save(&stored).await.unwrap();

        // "Start fresh": a default draft with only what the wizard collects.
        let mut draft = WizardDraft::default();
        draft.license_accepted = true;
        draft.telemetry_choice = Some(false);
        draft.config.deployment.location.lat = 29.9;
        draft.config.deployment.location.lon = -81.3;
        assert!(draft.config.ha_adoption.is_empty());
        s.draft_store.save(&draft).unwrap();

        let apply = post_apply(State(s.clone())).await.into_response();
        assert_eq!(apply.status(), StatusCode::OK);

        let after = s.config_store.load().await.unwrap();
        assert_eq!(
            after.ha_adoption.len(),
            crate::ha_adopt::ENTITIES.len(),
            "every migration marker survives a start-fresh apply"
        );
        for id in crate::ha_adopt::ENTITIES {
            assert!(
                crate::refresher::WateringPolicy::from_config(&after).ha_read_retired(id),
                "{id} must not go back to reading a helper the owner was told to delete"
            );
        }
        assert!(after.seeded_source_ids.contains(&"nws".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod probe_guard_tests {
    use super::*;
    use crate::auth::middleware::{AuthPolicy, AuthRuntime};
    use crate::auth::AuthStore;
    use axum::extract::ConnectInfo;
    use axum::http::Method;
    use std::net::SocketAddr;
    use std::sync::Mutex as StdMutex;

    // Build a WizardApiState whose auth_rt carries the given policy. The
    // store sits on an in-memory SQLite (the guard never touches it for the
    // anonymous IP path, but AuthRuntime::new requires a store).
    fn state_with_policy(policy: AuthPolicy) -> WizardApiState {
        // A unique temp dir per call keeps the FileConfigStore/WizardStore
        // paths isolated; the guard does not read them.
        static SEQ: StdMutex<u64> = StdMutex::new(0);
        let n = {
            let mut g = SEQ.lock().unwrap();
            *g += 1;
            *g
        };
        let dir =
            std::env::temp_dir().join(format!("localsky-probeguard-{}-{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let db = Arc::new(tokio::sync::Mutex::new(conn));
        let rt = AuthRuntime::new(AuthStore::new(db));
        rt.policy.store(Arc::new(policy));
        WizardApiState {
            draft_store: Arc::new(WizardStore::new(dir.join("wizard-draft.json"))),
            config_store: Arc::new(FileConfigStore::new(dir.join("localsky.toml"))),
            auth_rt: Some(Arc::new(rt)),
            tempest_store: None,
            runtime: None,
        }
    }

    // An anonymous request from `peer`, optional X-Forwarded-For, with no
    // authenticated identity attached (the disabled-mode short-circuit
    // leaves RequestIdentity::Anonymous, which the guard also defaults to).
    fn anon_parts(peer: &str, xff: Option<&str>) -> axum::http::request::Parts {
        let mut req = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/api/wizard/test_controller")
            .body(())
            .unwrap();
        if let Some(xff) = xff {
            req.headers_mut().insert(
                "x-forwarded-for",
                axum::http::HeaderValue::from_str(xff).unwrap(),
            );
        }
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer.parse().unwrap(), 40000)));
        req.into_parts().0
    }

    async fn guard_allows(parts: &mut axum::http::request::Parts, state: &WizardApiState) -> bool {
        ProbeGuard::from_request_parts(parts, state).await.is_ok()
    }

    #[tokio::test]
    async fn rfc1918_client_with_no_trusted_networks_is_allowed() {
        // The regression: a fresh self-hoster on their LAN, auth Disabled,
        // NO trusted_networks configured. Their browser's client IP is a
        // private address, so the probe must run.
        let state = state_with_policy(AuthPolicy::default());
        for ip in [
            "10.0.0.50",
            "172.16.5.20",
            "172.16.5.9",
            "127.0.0.1",
            "fc00::1234",
        ] {
            let mut parts = anon_parts(ip, None);
            assert!(
                guard_allows(&mut parts, &state).await,
                "private/LAN client {ip} with no trusted_networks must be allowed"
            );
        }
    }

    #[tokio::test]
    async fn public_client_is_refused() {
        // On the public demo the real client IP is a public internet
        // address; the guard must refuse it (defense in depth behind the
        // demo_guard, and the correct call for any non-LAN anonymous hit).
        let state = state_with_policy(AuthPolicy::default());
        for ip in ["203.0.113.5", "8.8.8.8", "2606:4700::1", "172.32.0.1"] {
            let mut parts = anon_parts(ip, None);
            assert!(
                !guard_allows(&mut parts, &state).await,
                "public client {ip} must be refused (403)"
            );
        }
    }

    #[tokio::test]
    async fn spoofed_xff_from_untrusted_peer_uses_socket_peer() {
        // A public peer claims a LAN address via X-Forwarded-For. With no
        // trusted_proxies configured the header is ignored, the socket peer
        // (public) is used, and the probe is refused. The forged private
        // address must NOT grant access.
        let state = state_with_policy(AuthPolicy::default());
        let mut parts = anon_parts("203.0.113.5", Some("10.0.0.10"));
        assert!(
            !guard_allows(&mut parts, &state).await,
            "spoofed XFF from an untrusted peer must not forge a LAN address"
        );
    }

    #[tokio::test]
    async fn trusted_proxy_xff_private_hop_is_allowed() {
        // When the peer IS a configured trusted proxy, the last untrusted
        // XFF hop wins. A private real-client hop behind the proxy is the
        // self-hoster behind their own reverse proxy and must be allowed.
        let policy = AuthPolicy {
            trusted_proxies: vec!["172.18.0.0/16".parse().unwrap()],
            ..AuthPolicy::default()
        };
        let state = state_with_policy(policy);
        let mut parts = anon_parts("172.18.0.2", Some("10.0.0.10"));
        assert!(
            guard_allows(&mut parts, &state).await,
            "private client behind a trusted proxy must be allowed"
        );
        // And a public real-client hop behind the same proxy is refused.
        let policy = AuthPolicy {
            trusted_proxies: vec!["172.18.0.0/16".parse().unwrap()],
            ..AuthPolicy::default()
        };
        let state = state_with_policy(policy);
        let mut parts = anon_parts("172.18.0.2", Some("203.0.113.9"));
        assert!(
            !guard_allows(&mut parts, &state).await,
            "public client behind a trusted proxy must be refused"
        );
    }

    #[tokio::test]
    async fn explicit_trusted_network_outside_rfc1918_is_allowed() {
        // trusted_networks still honored for a public range the operator
        // explicitly trusts (e.g. a tunnel egress), even though it is not
        // RFC1918/ULA.
        let policy = AuthPolicy {
            trusted: vec!["203.0.113.0/24".parse().unwrap()],
            ..AuthPolicy::default()
        };
        let state = state_with_policy(policy);
        let mut parts = anon_parts("203.0.113.5", None);
        assert!(
            guard_allows(&mut parts, &state).await,
            "an explicit trusted_networks match must be allowed"
        );
    }

    #[tokio::test]
    async fn authenticated_user_always_allowed() {
        let state = state_with_policy(AuthPolicy::default());
        let mut parts = anon_parts("203.0.113.5", None);
        // Attach an authenticated identity: a public IP no longer matters.
        parts.extensions.insert(RequestIdentity::User(1));
        assert!(
            guard_allows(&mut parts, &state).await,
            "an authenticated User identity must always pass the guard"
        );
    }

    #[tokio::test]
    async fn geocode_refuses_public_anonymous_caller() {
        // get_geocode relays outbound (Nominatim) and sits behind the SAME
        // ProbeGuard as the other outbound wizard handlers. A public
        // anonymous caller must get 403 from the extractor, BEFORE the
        // handler body runs, so this test never touches the network.
        use tower::ServiceExt;
        let state = state_with_policy(AuthPolicy::default());
        let app = router(state);
        let mut req = axum::http::Request::builder()
            .method(Method::GET)
            .uri("/geocode?q=paris")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            "203.0.113.5".parse().unwrap(),
            40000,
        )));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "public anonymous geocode must be refused by ProbeGuard"
        );
    }
}

#[cfg(test)]
mod geocode_rate_limit_tests {
    use super::*;

    #[test]
    fn fixed_window_allows_then_refuses_per_ip() {
        // Unique TEST-NET IPs so parallel tests sharing the static map
        // cannot collide; the limiter buckets strictly per IP.
        let hot: std::net::IpAddr = "198.51.100.77".parse().unwrap();
        let cold: std::net::IpAddr = "198.51.100.78".parse().unwrap();
        for i in 0..GEOCODE_MAX_PER_MIN {
            assert!(
                allow_geocode_attempt(hot),
                "attempt {i} within the budget must be allowed"
            );
        }
        assert!(
            !allow_geocode_attempt(hot),
            "the attempt past the budget must be refused"
        );
        // Another client is unaffected (per-IP buckets, not global).
        assert!(allow_geocode_attempt(cold));
    }
}

#[cfg(test)]
mod probe_soil_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reduces_livedata_to_bindable_channels() {
        // A real EC-soil gateway shape: two probes with moisture + siblings.
        let body = json!({
            "ch_ec": [
                {"channel": "1", "name": "Back Yard", "battery": "5", "humidity": "32%", "temp": "79.2", "unit": "F", "ec": "40 uS/cm"},
                {"channel": "3", "name": "Side Yard", "battery": "4", "humidity": "65%", "temp": "82.0", "unit": "F", "ec": "170 uS/cm"}
            ]
        });
        let mut chans = soil_channels_from_livedata(&body, "ecowitt_gw");
        chans.sort_by(|a, b| a.channel.cmp(&b.channel));
        assert_eq!(chans.len(), 2);

        let c1 = &chans[0];
        assert_eq!(c1.channel, "1");
        // The id must be byte-identical to a live `soil_sensor_id` binding.
        assert_eq!(c1.id, "source:ecowitt_gw:soilmoisture1");
        assert_eq!(c1.moisture_pct, Some(32.0));
        // Ecowitt battery level 5 -> 100% (the parser scales x20, clamped).
        assert_eq!(c1.battery_pct, Some(100.0));
        assert_eq!(c1.temp_f, Some(79.2));
        assert_eq!(c1.ec, Some(40.0));

        let c3 = &chans[1];
        assert_eq!(c3.channel, "3");
        assert_eq!(c3.id, "source:ecowitt_gw:soilmoisture3");
        assert_eq!(c3.moisture_pct, Some(65.0));
    }

    #[test]
    fn ignores_non_soil_and_uses_source_id() {
        // Classic WH51 ch_soil plus weather noise that must not surface.
        let body = json!({
            "common_list": [{"id": "0x02", "val": "71.6"}],
            "ch_soil": [{"channel": "2", "name": "Front", "battery": "3", "humidity": "45%"}]
        });
        let chans = soil_channels_from_livedata(&body, "my_gw");
        assert_eq!(chans.len(), 1, "only the soil channel is bindable");
        assert_eq!(chans[0].id, "source:my_gw:soilmoisture2");
        assert_eq!(chans[0].moisture_pct, Some(45.0));
        // ch_soil carries no temp/EC.
        assert_eq!(chans[0].temp_f, None);
        assert_eq!(chans[0].ec, None);
    }

    #[test]
    fn empty_body_has_no_channels() {
        assert!(soil_channels_from_livedata(&json!({}), "gw").is_empty());
    }

    #[test]
    fn livedata_count_drives_discovered_gateway_soil_channels() {
        // The enrichment in get_discover sets DiscoveredGateway.soil_channels
        // to the channel count this reduction yields, and the wizard UI reads
        // that field off the JSON. Assert N soil channels -> soil_channels=N
        // and that the field serializes under that exact name.
        let body = json!({
            "ch_soil": [
                {"channel": "1", "name": "Back Yard", "battery": "5", "humidity": "32%"},
                {"channel": "2", "name": "Side Yard", "battery": "4", "humidity": "65%"},
                {"channel": "4", "name": "Front Yard", "battery": "3", "humidity": "21%"}
            ]
        });
        // Mirror ecowitt_soil_channels: the count is the bindable-channel total.
        let n = soil_channels_from_livedata(&body, "ecowitt_gw").len() as u32;
        assert_eq!(n, 3);

        let gw = crate::discovery::ecowitt::DiscoveredGateway {
            vendor: "ecowitt".to_string(),
            mac: "00:11:22:33:44:55".to_string(),
            ip: "192.0.2.61".to_string(),
            port: 45000,
            model: "GW2000A".to_string(),
            suggested_host: "192.0.2.61".to_string(),
            soil_channels: n,
        };
        let v = serde_json::to_value(&gw).expect("serializes");
        assert_eq!(v["soil_channels"], json!(3));
    }
}

#[cfg(test)]
mod scan_zones_tests {
    use super::*;
    use crate::config::schema::ControllerEntry;

    fn state_in(dir: &std::path::Path) -> WizardApiState {
        WizardApiState {
            draft_store: Arc::new(WizardStore::new(dir.join("wizard-draft.json"))),
            config_store: Arc::new(FileConfigStore::new(dir.join("localsky.toml"))),
            auth_rt: None,
            tempest_store: None,
            runtime: None,
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("localsky-wizard-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(v: serde_json::Value) -> ControllerEntry {
        serde_json::from_value(v).expect("test controller entry deserializes")
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // The offline scan path issue #8's fix rides on: a dry_run controller
    // reports its three sample stations with no hardware and no network.
    #[tokio::test]
    async fn scan_zones_dry_run_returns_sample_stations() {
        let dir = temp_dir("scan-dryrun");
        let s = state_in(&dir);
        let body = TestControllerBody {
            controller: entry(serde_json::json!({
                "id": "sim",
                "default": true,
                "enabled": true,
                "kind": "dry_run",
                "config": { "simulate_runs": false },
            })),
        };
        let resp = post_scan_zones(ProbeGuard, State(s), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let zones = v["zones"].as_array().expect("zones array");
        assert_eq!(zones.len(), 3, "dry_run reports 3 sample stations");
        assert_eq!(zones[0]["station_id"], "1");
        assert!(zones[0]["name"].as_str().is_some_and(|n| !n.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scan_zones_unprobeable_kind_is_422() {
        let dir = temp_dir("scan-422");
        let s = state_in(&dir);
        let body = TestControllerBody {
            controller: entry(serde_json::json!({
                "id": "mq",
                "default": false,
                "enabled": true,
                "kind": "mqtt_command",
                "config": { "broker_host": "broker.local" },
            })),
        };
        let resp = post_scan_zones(ProbeGuard, State(s), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "controller_unsupported");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Editing an EXISTING cloud controller serves api_token as the redaction
    // sentinel; the scan must resolve it against the stored config by entry
    // id before probing (the put_config pattern).
    #[tokio::test]
    async fn probe_entry_sentinel_resolves_against_stored_config_by_id() {
        use crate::api::config::SECRET_REDACTED_SENTINEL;
        use crate::config::schema::{Config, ControllerKind};

        let dir = temp_dir("scan-unredact");
        let s = state_in(&dir);

        // Store a config holding the real token.
        let mut cfg = Config::default();
        cfg.controllers.push(entry(serde_json::json!({
            "id": "rachio_main",
            "default": true,
            "enabled": true,
            "kind": "rachio",
            "config": {
                "api_token": "stored-example-token",
                "device_id": "device-0001",
                "zone_uuid_map": {},
            },
        })));
        s.config_store.save(&cfg).await.unwrap();

        // The client posts the entry as served: token redacted.
        let posted = entry(serde_json::json!({
            "id": "rachio_main",
            "default": true,
            "enabled": true,
            "kind": "rachio",
            "config": {
                "api_token": SECRET_REDACTED_SENTINEL,
                "device_id": "device-0001",
                "zone_uuid_map": {},
            },
        }));
        let resolved = resolve_probe_entry_secrets(&s, posted)
            .await
            .expect("sentinel resolves against the stored entry");
        match &resolved.controller {
            ControllerKind::Rachio(rc) => {
                assert_eq!(rc.api_token, "stored-example-token");
                assert_eq!(rc.device_id, "device-0001");
            }
            other => panic!("expected rachio entry, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The WIZARD path: the entry lives only in the draft (no live config
    // yet), served redacted by GET /draft. The resolver falls back to the
    // draft store by entry id.
    #[tokio::test]
    async fn probe_entry_sentinel_resolves_against_the_wizard_draft() {
        use crate::api::config::SECRET_REDACTED_SENTINEL;
        use crate::config::schema::ControllerKind;

        let dir = temp_dir("scan-unredact-draft");
        let s = state_in(&dir);

        let mut draft = WizardDraft::default();
        draft.config.controllers.push(entry(serde_json::json!({
            "id": "rachio_main",
            "default": true,
            "enabled": true,
            "kind": "rachio",
            "config": {
                "api_token": "draft-example-token",
                "device_id": "device-0001",
                "zone_uuid_map": {},
            },
        })));
        s.draft_store.save(&draft).unwrap();

        let posted = entry(serde_json::json!({
            "id": "rachio_main",
            "default": true,
            "enabled": true,
            "kind": "rachio",
            "config": {
                "api_token": SECRET_REDACTED_SENTINEL,
                "device_id": "device-0001",
                "zone_uuid_map": {},
            },
        }));
        let resolved = resolve_probe_entry_secrets(&s, posted)
            .await
            .expect("sentinel resolves against the draft entry");
        match &resolved.controller {
            ControllerKind::Rachio(rc) => assert_eq!(rc.api_token, "draft-example-token"),
            other => panic!("expected rachio entry, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A sentinel with NO stored counterpart (fresh add, renamed id) must be
    // a plain 400, not a doomed probe against the vendor cloud with the
    // literal sentinel as the credential.
    #[tokio::test]
    async fn scan_zones_unresolvable_sentinel_is_400() {
        use crate::api::config::SECRET_REDACTED_SENTINEL;

        let dir = temp_dir("scan-unmatched");
        let s = state_in(&dir);
        let body = TestControllerBody {
            controller: entry(serde_json::json!({
                "id": "rachio_new",
                "default": false,
                "enabled": true,
                "kind": "rachio",
                "config": {
                    "api_token": SECRET_REDACTED_SENTINEL,
                    "device_id": "device-0002",
                    "zone_uuid_map": {},
                },
            })),
        };
        let resp = post_scan_zones(ProbeGuard, State(s), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "unmatched_redacted_secret");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // SECURITY: a probe that rides the sentinel restoration cannot redirect
    // the restored token. Stored entry has the default endpoint (base_url
    // null); the posted entry points base_url at another host. The probe
    // must be refused with a plain 400 and NO request may reach that host.
    #[tokio::test]
    async fn scan_zones_sentinel_with_changed_transport_is_400_and_sends_nothing() {
        use crate::api::config::SECRET_REDACTED_SENTINEL;
        use crate::config::schema::Config;
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let attacker = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(0) // the restored token must never travel here
            .mount(&attacker)
            .await;

        let dir = temp_dir("scan-transport-pin");
        let s = state_in(&dir);
        let mut cfg = Config::default();
        cfg.controllers.push(entry(serde_json::json!({
            "id": "rachio_main",
            "default": true,
            "enabled": true,
            "kind": "rachio",
            "config": {
                "api_token": "stored-example-token",
                "device_id": "device-0001",
                "zone_uuid_map": {},
                // base_url absent: the stored entry uses the built-in
                // production endpoint.
            },
        })));
        s.config_store.save(&cfg).await.unwrap();

        let body = TestControllerBody {
            controller: entry(serde_json::json!({
                "id": "rachio_main",
                "default": true,
                "enabled": true,
                "kind": "rachio",
                "config": {
                    "api_token": SECRET_REDACTED_SENTINEL,
                    "device_id": "device-0001",
                    "zone_uuid_map": {},
                    "base_url": attacker.uri(),
                },
            })),
        };
        let resp = post_scan_zones(ProbeGuard, State(s.clone()), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "transport_field_mismatch");

        // Same guard on the test flow (the other probe that restores secrets).
        let body = TestControllerBody {
            controller: entry(serde_json::json!({
                "id": "rachio_main",
                "default": true,
                "enabled": true,
                "kind": "rachio",
                "config": {
                    "api_token": SECRET_REDACTED_SENTINEL,
                    "device_id": "device-0001",
                    "zone_uuid_map": {},
                    "base_url": attacker.uri(),
                },
            })),
        };
        let resp = post_test_controller(ProbeGuard, State(s), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "transport_field_mismatch");

        attacker.verify().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // End to end: the editor posts a redacted rachio entry, the handler
    // restores the stored token by id and the scan runs against the cloud
    // (mocked) with the REAL token, returning the device's zones. Doubles
    // as the transport-pin ALLOW case: the posted base_url equals the
    // stored one, so the sentinel probe proceeds.
    #[tokio::test]
    async fn scan_zones_rachio_sentinel_end_to_end() {
        use crate::api::config::SECRET_REDACTED_SENTINEL;
        use crate::config::schema::Config;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/device/device-0001"))
            .and(header("authorization", "Bearer stored-example-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "device-0001",
                "status": "ONLINE",
                "zones": [
                    { "id": "1f00aa00-0000-4000-8000-000000000001", "zoneNumber": 1,
                      "name": "Front Lawn", "enabled": true },
                    { "id": "1f00aa00-0000-4000-8000-000000000002", "zoneNumber": 2,
                      "name": "Back Lawn", "enabled": true },
                ],
            })))
            .expect(1)
            .mount(&server)
            .await;

        let dir = temp_dir("scan-sentinel-e2e");
        let s = state_in(&dir);
        let mut cfg = Config::default();
        cfg.controllers.push(entry(serde_json::json!({
            "id": "rachio_main",
            "default": true,
            "enabled": true,
            "kind": "rachio",
            "config": {
                "api_token": "stored-example-token",
                "device_id": "device-0001",
                "zone_uuid_map": {},
                "base_url": server.uri(),
            },
        })));
        s.config_store.save(&cfg).await.unwrap();

        let body = TestControllerBody {
            controller: entry(serde_json::json!({
                "id": "rachio_main",
                "default": true,
                "enabled": true,
                "kind": "rachio",
                "config": {
                    "api_token": SECRET_REDACTED_SENTINEL,
                    "device_id": "device-0001",
                    "zone_uuid_map": {},
                    "base_url": server.uri(),
                },
            })),
        };
        let resp = post_scan_zones(ProbeGuard, State(s), Json(body))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let zones = v["zones"].as_array().expect("zones array");
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0]["name"], "Front Lawn");
        server.verify().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
