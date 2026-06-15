// /api/config router. Reads + writes /data/localsky.toml via FileConfigStore.
//
// Endpoints:
//   GET  /api/config              -> current Config, secrets replaced with
//                                    SECRET_REDACTED_SENTINEL by redact_secrets()
//   PUT  /api/config              -> validate + save; restores any field still
//                                    set to the sentinel from the stored value
//                                    via unredact_secrets() so partial edits work
//   GET  /api/config/schema       -> JsonSchema for the settings UI forms
//   POST /api/config/preview      -> dry-run validation against a candidate
//   GET  /api/config/snapshots    -> file snapshots (<config_dir>/snapshots)
//   POST /api/config/rollback     -> {"ts": <snapshot ts>} restore (also
//                                    accepts legacy ?to=<ts>)
//
// Not wired into the main api router yet. Phase 5 composition root passes
// a constructed FileConfigStore via state.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use tower_http::limit::RequestBodyLimitLayer;

use crate::config::schema::Config;
use crate::config::FileConfigStore;
use crate::ports::config_store::{ConfigStore, ConfigStoreError};

pub type ConfigApiState = Arc<FileConfigStore>;

/// Upper bound on a config write (LS-API-09). A full localsky.toml with
/// many zones/sources/rules is a few tens of KiB at most; 2 MiB is a
/// comfortable ceiling that still refuses an over-large body before it is
/// buffered. Applies to PUT / (JSON), PUT /raw (TOML text), POST /preview
/// and POST /rollback. The route is privileged-gated already; this cap is
/// defense-in-depth.
const CONFIG_BODY_LIMIT: usize = 2 * 1024 * 1024;

pub fn router(state: ConfigApiState) -> Router {
    Router::new()
        .route("/", get(get_config).put(put_config))
        .route("/validate", get(get_validate))
        .route("/schema", get(get_schema))
        .route("/preview", post(preview_config))
        .route("/snapshots", get(get_snapshots))
        .route("/rollback", post(post_rollback))
        .route("/raw", get(get_raw_toml).put(put_raw_toml))
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(CONFIG_BODY_LIMIT))
}

#[derive(Debug, Deserialize, Default)]
struct RawQuery {
    /// Opt in to full-fidelity (unredacted) TOML. Honored only for an
    /// authenticated owner identity; ignored otherwise.
    #[serde(default)]
    reveal: Option<bool>,
}

/// Return the TOML of /data/localsky.toml as text/plain so the Advanced
/// settings page can render a textarea editor.
///
/// REDACTION + GATING (security wave 3): secrets are redacted to the
/// sentinel by default, matching GET / and the backup/draft read paths, so
/// this endpoint never leaks a cleartext token even in the shipped default
/// posture (AuthMode::Disabled). The route itself is additionally treated
/// as PRIVILEGED in `auth::middleware`: an unauthenticated, non-trusted
/// caller is refused BEFORE reaching this handler, even with auth disabled.
///
/// Full fidelity (real secrets) is opt-in via `?reveal=1` AND only for a
/// caller the privileged gate already vouched for: an authenticated owner
/// (session/API-token User) OR a trusted-network caller. The latter is a
/// LAN owner the operator trusts (loopback / RFC1918 / ULA / an explicit
/// trusted_networks match in the disabled-default posture); honoring reveal
/// for them lets a LAN owner in Disabled mode (who has no session) read
/// their own raw config in the Advanced editor. A bare public/anonymous
/// caller never reaches this handler (the gate refuses it). Redaction is
/// still the DEFAULT; reveal must be explicitly requested. The editor PUT
/// also round-trips the sentinel via `unredact_secrets`, so saving a
/// redacted edit preserves untouched secrets.
///
/// Empty 200 when the file hasn't been written yet so the wizard can
/// pre-populate via PUT.
async fn get_raw_toml(
    State(store): State<ConfigApiState>,
    Query(q): Query<RawQuery>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Full fidelity is granted on an explicit opt-in to a caller the
    // privileged gate already vouched for: an authenticated owner (User) OR
    // a trusted-network caller. The privileged gate in auth::middleware
    // refuses a bare public/anonymous caller before reaching this handler in
    // BOTH auth modes, so a TrustedNetwork here is a LAN owner the operator
    // trusts (loopback / RFC1918 / ULA / trusted_networks in the disabled
    // default). Honoring ?reveal=1 for them lets a LAN owner in Disabled
    // mode (who has no session) read their own raw config in the Advanced
    // editor. Redacted stays the default; reveal is strictly opt-in.
    let is_owner = matches!(
        req.extensions().get::<crate::auth::RequestIdentity>(),
        Some(crate::auth::RequestIdentity::User(_) | crate::auth::RequestIdentity::TrustedNetwork)
    );
    let reveal = q.reveal.unwrap_or(false) && is_owner;
    match tokio::fs::read_to_string(store.path()).await {
        Ok(s) => {
            let body = if reveal {
                s
            } else {
                // Withhold (empty) rather than ship raw bytes if the file
                // somehow fails to parse for redaction: never leak.
                redact_toml_str(&s).unwrap_or_default()
            };
            (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                body,
            )
                .into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            String::new(),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "raw_read_failed".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

/// Replace /data/localsky.toml with the supplied TOML body, after parsing
/// + validating it against the schema invariants.
///
/// REDACTION ROUND-TRIP (security wave 3): GET /config/raw now returns
/// REDACTED TOML by default (the Advanced editor textarea shows the
/// sentinel for each secret, exactly like the form-based settings UI). So
/// the body that comes back here may contain the sentinel for any secret
/// the operator did not retype. We restore those from the stored config
/// (same unredact_secrets pass as PUT /api/config) before saving, and
/// reject any sentinel that has no stored counterpart so the literal
/// "***redacted***" is never persisted as a secret. An operator who opened
/// the editor with ?reveal=1 and typed real secrets simply has no sentinels
/// to restore, so this is a no-op for them.
async fn put_raw_toml(State(store): State<ConfigApiState>, body: String) -> impl IntoResponse {
    // Validate by parsing through the same path as the loader. Reuses
    // the Validate step in src/config/loader.rs::validate.
    let parsed: Config = match toml::from_str(&body) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "toml_parse_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response();
        }
    };

    // Restore redacted secrets from the stored config, then reject any
    // unmatched sentinel (a new secret left as the placeholder).
    let mut candidate_json = match serde_json::to_value(&parsed) {
        Ok(v) => v,
        Err(e) => {
            return store_err(ConfigStoreError::Io(format!("serialize candidate: {e}")))
                .into_response();
        }
    };
    let original = match store.load().await {
        Ok(cfg) => serde_json::to_value(&cfg).ok(),
        Err(ConfigStoreError::NotFound) => None,
        Err(e) => return store_err(e).into_response(),
    };
    if let Some(orig) = original.as_ref() {
        unredact_secrets(&mut candidate_json, orig);
    }
    let mut leftover = Vec::new();
    remaining_sentinels(&candidate_json, "$", &mut leftover);
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
    let parsed: Config = match serde_json::from_value(candidate_json) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "config_decode_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response();
        }
    };

    if let Err(e) = crate::config::loader::validate(&parsed) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "config_validation_error".into(),
                detail: Some(format!("{e}")),
            }),
        )
            .into_response();
    }
    // Same structural validation the wizard preflight + PUT / run:
    // errors block the save, warnings ride along in the success body.
    let report = crate::config::validate::validate(&parsed);
    if !report.ok() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "config_invalid",
                "validation": report,
            })),
        )
            .into_response();
    }
    // Store-managed typed write: snapshots the previous file + fsyncs. We
    // save the unredacted Config (not the raw text) so restored secrets land
    // on disk; the store serializes via to_string_pretty exactly like the
    // form-based PUT, so the on-disk shape is identical either way.
    match store.save(&parsed).await {
        Ok(_) => Json(serde_json::json!({ "ok": true, "validation": report })).into_response(),
        Err(e) => store_err(e).into_response(),
    }
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
    detail: Option<String>,
}

fn store_err(e: ConfigStoreError) -> (StatusCode, Json<ApiError>) {
    let code = match &e {
        ConfigStoreError::NotFound => StatusCode::NOT_FOUND,
        ConfigStoreError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
        ConfigStoreError::RollbackTargetMissing(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        code,
        Json(ApiError {
            error: "config_store_error".into(),
            detail: Some(e.to_string()),
        }),
    )
}

async fn get_config(State(store): State<ConfigApiState>) -> impl IntoResponse {
    match store.load().await {
        Ok(cfg) => {
            // Redact secrets before returning. The JSON wire format
            // never exposes API keys, bearer tokens, MD5 passwords, or
            // VAPID privates; clients display a sentinel and PUT-side
            // logic on the operator's edit-form preserves the existing
            // value when the sentinel is sent back unchanged.
            let mut v = match serde_json::to_value(&cfg) {
                Ok(v) => v,
                Err(e) => {
                    return store_err(ConfigStoreError::Io(format!("serialize: {e}")))
                        .into_response();
                }
            };
            redact_secrets(&mut v);
            Json(v).into_response()
        }
        Err(e) => store_err(e).into_response(),
    }
}

/// In-place mutation that replaces every known secret-bearing string
/// with a SECRET_REDACTED_SENTINEL. Conservative: false positives are
/// preferable to leaking a token. The PUT handler accepts the sentinel
/// and preserves the existing stored value.
pub const SECRET_REDACTED_SENTINEL: &str = "***redacted***";

pub(crate) fn redact_secrets(v: &mut serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let lk = k.to_lowercase();
                let is_secret = lk == "password_md5"
                    || lk == "bearer_token"
                    || lk == "api_key"
                    || lk == "api_token"
                    || lk == "password"
                    || lk == "auth_token"
                    || lk == "vapid_private_path"
                    || lk == "vapid_private"
                    || lk == "webhook_url"
                    || lk == "token"
                    || lk == "shared_secret"
                    || lk == "access_token"
                    || lk == "app_key"
                    || lk == "client_secret"
                    || lk == "refresh_token"
                    // SMTP credential (EmailConfig.username); the MQTT
                    // source/command/notification username fields are the
                    // other half of a broker credential pair, so redacting
                    // every `username` is both correct and conservative.
                    || lk == "username"
                    // Cloud-controller / cloud-source ACCOUNT EMAIL: the
                    // username half of a credential pair whose password half
                    // is already redacted above. B-hyve (BhyveConfig.email),
                    // Rain Bird (RainbirdConfig.email) and LaCrosse
                    // (LacrosseConfig.email) all authenticate with
                    // account-email + password; leaving the email in the
                    // clear half-leaked the credential. The notification
                    // EmailConfig uses `from_address`/`to_address` (not
                    // `email`) and `vapid_subject` is a mailto: contact, so
                    // those legitimate addresses are untouched. The `email`
                    // KEY on the notifications struct points at an OBJECT
                    // (EmailConfig), not a string. We only redact a secret-named
                    // key when its value is a STRING leaf (the cloud-controller
                    // account emails are strings); when it is an object/array we
                    // must still RECURSE into it, or marking `email` secret would
                    // skip the whole notifications.email subtree and leak its
                    // smtp username/password. See the string-vs-recurse handling
                    // below.
                    || lk == "email";
                // Redact only when the secret-named key holds a STRING value;
                // otherwise (object/array under a secret-named key, e.g. the
                // notifications `email` EmailConfig object) fall through to
                // recursion so nested secrets are still redacted.
                if is_secret {
                    if let Value::String(s) = val {
                        if !s.is_empty() {
                            *s = SECRET_REDACTED_SENTINEL.to_string();
                        }
                        continue;
                    }
                }
                redact_secrets(val);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_secrets(v);
            }
        }
        _ => {}
    }
}

/// Redact secrets in a localsky.toml TEXT blob, returning sanitized TOML.
///
/// Used by the sibling read paths (GET /backup, GET /config/raw) that ship
/// the on-disk config instead of the JSON-serialized one. The file is
/// always store-written via `toml::to_string_pretty(&Config)`, so parsing
/// it back into `Config`, running the SAME `redact_secrets()` pass over its
/// JSON form, and re-serializing to TOML preserves every field the loader
/// and restore path read while replacing each secret with the sentinel.
/// The wizard/config PUT side already round-trips the sentinel back to the
/// stored value via `unredact_secrets`, so a redacted backup re-imports
/// without losing secrets when restored onto the SAME instance.
///
/// Parse/serialize failures return `None`; the caller decides whether to
/// withhold the field rather than risk shipping raw bytes.
pub(crate) fn redact_toml_str(raw: &str) -> Option<String> {
    let cfg: Config = toml::from_str(raw).ok()?;
    let mut v = serde_json::to_value(&cfg).ok()?;
    redact_secrets(&mut v);
    let redacted: Config = serde_json::from_value(v).ok()?;
    toml::to_string_pretty(&redacted).ok()
}

/// Inverse of redact_secrets: walks the candidate config alongside the
/// stored config, and any place the candidate contains the sentinel,
/// substitutes the original value back in. Lets clients PUT a redacted
/// JSON without losing the secret.
///
/// Arrays whose elements carry an `id` field (sources, controllers) are
/// matched BY ID, not by index: a reorder or delete in the candidate
/// must not attach one entry's stored secret to a different entry.
/// Id-less arrays still match positionally.
pub(crate) fn unredact_secrets(candidate: &mut serde_json::Value, original: &serde_json::Value) {
    use serde_json::Value;
    match (candidate, original) {
        (Value::Object(c), Value::Object(o)) => {
            for (k, c_val) in c.iter_mut() {
                if let Some(o_val) = o.get(k) {
                    if let Value::String(s) = c_val {
                        if s == SECRET_REDACTED_SENTINEL {
                            *c_val = o_val.clone();
                            continue;
                        }
                    }
                    unredact_secrets(c_val, o_val);
                }
            }
        }
        (Value::Array(c), Value::Array(o)) => {
            // The stored side decides the matching mode: it is always
            // server-serialized, so sources/controllers reliably carry
            // string ids there. Candidate entries without an id (or
            // with an unknown id) simply get nothing restored; any
            // sentinel left in them is rejected by the caller.
            let id_keyed = !o.is_empty()
                && o.iter()
                    .all(|v| v.get("id").map(|id| id.is_string()).unwrap_or(false));
            if id_keyed {
                for c_v in c.iter_mut() {
                    let id = c_v.get("id").and_then(|v| v.as_str()).map(str::to_owned);
                    let Some(id) = id else { continue };
                    if let Some(o_v) = o
                        .iter()
                        .find(|ov| ov.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                    {
                        unredact_secrets(c_v, o_v);
                    }
                }
            } else {
                for (i, c_v) in c.iter_mut().enumerate() {
                    if let Some(o_v) = o.get(i) {
                        unredact_secrets(c_v, o_v);
                    }
                }
            }
        }
        _ => {}
    }
}

/// JSON paths of every string still equal to the sentinel. A non-empty
/// result after unredact_secrets means a redacted placeholder had no
/// stored counterpart (new/renamed entry); saving it would persist the
/// literal sentinel as the secret, so the PUT handler rejects instead.
pub(crate) fn remaining_sentinels(v: &serde_json::Value, path: &str, out: &mut Vec<String>) {
    use serde_json::Value;
    match v {
        Value::String(s) if s == SECRET_REDACTED_SENTINEL => out.push(path.to_string()),
        Value::Object(map) => {
            for (k, val) in map {
                remaining_sentinels(val, &format!("{path}.{k}"), out);
            }
        }
        Value::Array(arr) => {
            for (i, val) in arr.iter().enumerate() {
                // Prefer the element id in the path when present.
                let seg = val
                    .get("id")
                    .and_then(|id| id.as_str())
                    .map(|id| format!("{path}[id={id}]"))
                    .unwrap_or_else(|| format!("{path}[{i}]"));
                remaining_sentinels(val, &seg, out);
            }
        }
        _ => {}
    }
}

async fn put_config(
    State(store): State<ConfigApiState>,
    Json(mut candidate_json): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Load the current Config so we can restore any redacted secrets.
    let original = match store.load().await {
        Ok(cfg) => match serde_json::to_value(&cfg) {
            Ok(v) => v,
            Err(e) => {
                return store_err(ConfigStoreError::Io(format!("serialize current: {e}")))
                    .into_response();
            }
        },
        Err(ConfigStoreError::NotFound) => serde_json::Value::Null,
        Err(e) => return store_err(e).into_response(),
    };
    if !original.is_null() {
        unredact_secrets(&mut candidate_json, &original);
    }
    // Any sentinel that survived has no stored counterpart (new entry,
    // renamed id, or no config on disk). Saving would persist the
    // literal "***redacted***" as the secret; reject instead.
    let mut leftover = Vec::new();
    remaining_sentinels(&candidate_json, "$", &mut leftover);
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
    let cfg: Config = match serde_json::from_value(candidate_json) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "config_decode_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response();
        }
    };
    // Structural validation: errors block the save (the report rides in
    // the 422 body so the UI can show field-level issues); warnings are
    // returned alongside the success body.
    let report = crate::config::validate::validate(&cfg);
    if !report.ok() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "config_invalid",
                "validation": report,
            })),
        )
            .into_response();
    }
    match store.save(&cfg).await {
        Ok(v) => Json(serde_json::json!({
            "saved": v,
            "validation": report,
        }))
        .into_response(),
        Err(e) => store_err(e).into_response(),
    }
}

/// GET /api/v1/config/validate -> the structured report for the config
/// as currently on disk. The settings overview surfaces warnings.
async fn get_validate(State(store): State<ConfigApiState>) -> impl IntoResponse {
    match store.load().await {
        Ok(cfg) => Json(serde_json::json!({
            "validation": crate::config::validate::validate(&cfg)
        }))
        .into_response(),
        Err(ConfigStoreError::NotFound) => Json(serde_json::json!({
            "validation": { "errors": [], "warnings": [] },
            "note": "no config yet (wizard pending)",
        }))
        .into_response(),
        Err(e) => store_err(e).into_response(),
    }
}

async fn get_schema() -> impl IntoResponse {
    let schema = schema_for!(Config);
    Json(schema)
}

#[derive(Debug, Deserialize)]
struct PreviewBody {
    candidate: Config,
}

#[derive(Debug, Serialize)]
struct PreviewResult {
    ok: bool,
    errors: Vec<String>,
}

async fn preview_config(
    State(_store): State<ConfigApiState>,
    Json(body): Json<PreviewBody>,
) -> impl IntoResponse {
    let mut errors = Vec::new();
    if let Err(e) = crate::config::loader::validate(&body.candidate) {
        errors.push(e.to_string());
    }
    Json(PreviewResult {
        ok: errors.is_empty(),
        errors,
    })
}

/// GET /api/v1/config/snapshots -> the on-disk snapshot history
/// (<config_dir>/snapshots/<ts>.toml), newest first.
async fn get_snapshots(State(store): State<ConfigApiState>) -> impl IntoResponse {
    match store.list_snapshots().await {
        Ok(list) => {
            let snapshots: Vec<_> = list
                .into_iter()
                .map(|v| {
                    serde_json::json!({
                        "ts": v.version,
                        "applied_at_epoch": v.applied_at_epoch,
                        "schema_version": v.schema_version,
                        "note": v.note,
                    })
                })
                .collect();
            Json(serde_json::json!({ "snapshots": snapshots })).into_response()
        }
        Err(e) => store_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
struct RollbackQuery {
    to: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RollbackBody {
    ts: u32,
}

/// POST /api/v1/config/rollback with {"ts": <snapshot ts>} (or the
/// legacy ?to=<ts> query). Validates the snapshot parses before the
/// swap; the pre-rollback config is snapshotted first.
async fn post_rollback(
    State(store): State<ConfigApiState>,
    Query(q): Query<RollbackQuery>,
    body: Option<Json<RollbackBody>>,
) -> impl IntoResponse {
    let Some(ts) = body.map(|Json(b)| b.ts).or(q.to) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "rollback_target_missing".into(),
                detail: Some("send {\"ts\": <snapshot ts>} or ?to=<ts>".into()),
            }),
        )
            .into_response();
    };
    match store.rollback(ts).await {
        Ok(cfg) => {
            // Same redaction contract as GET /: secrets never ride the
            // JSON wire format.
            let mut v = serde_json::to_value(&cfg).unwrap_or(serde_json::Value::Null);
            redact_secrets(&mut v);
            Json(serde_json::json!({ "ok": true, "restored_ts": ts, "config": v })).into_response()
        }
        Err(e) => store_err(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_secrets() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "deployment": {
                "location": { "lat": 28.5, "lon": -81.4 },
                "units": "imperial",
                "display_name": "Yard"
            },
            "sources": [{
                "id": "ha_pass",
                "priority": 30,
                "enabled": true,
                "kind": "ha_passthrough",
                "config": {
                    "base_url": "http://ha.local:8123",
                    "bearer_token": "supersecret_ha_token_xyz",
                    "field_map": {}
                }
            }, {
                "id": "mqtt_sensors",
                "priority": 80,
                "enabled": true,
                "kind": "mqtt",
                "config": {
                    "broker_host": "broker.local",
                    "broker_port": 1883,
                    "username": "user1",
                    "password": "mqtt_password_123",
                    "subscriptions": [{
                        "topic": "soil/+",
                        "field": "soil_moisture",
                        "scale": 1.0,
                        "offset": 0.0
                    }]
                }
            }],
            "controllers": [{
                "id": "os_main",
                "default": true,
                "enabled": true,
                "kind": "opensprinkler_direct",
                "config": {
                    "host": "10.0.0.10",
                    "port": 80,
                    "password_md5": "abc123md5hash",
                    "poll_interval_s": 10
                }
            }],
            "zones": {},
            "llm": {
                "provider": "openai_compat",
                "config": {
                    "base_url": "https://api.openai.com",
                    "model": "gpt-4o-mini",
                    "api_key": "sk-proj-very-real-looking-key"
                },
                "timeout_s": 20,
                "explanation_ttl_s": 300,
                "anomaly_ttl_s": 3600
            },
            "notifications": {
                "web_push": {
                    "vapid_public": "BPublicKey",
                    "vapid_private_path": "/keys/vapid-private.pem",
                    "vapid_subject": "mailto:ops@example.com"
                },
                "slack": {
                    "webhook_url": "https://hooks.slack.com/services/SECRET"
                }
            },
            "features": {},
            "engine": {}
        })
    }

    #[test]
    fn redact_replaces_every_known_secret() {
        let mut v = cfg_with_secrets();
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        // Sanitize-grep: no secret value should survive
        assert!(!s.contains("supersecret_ha_token_xyz"), "HA bearer leaked");
        assert!(!s.contains("mqtt_password_123"), "MQTT password leaked");
        assert!(!s.contains("abc123md5hash"), "OS password_md5 leaked");
        assert!(
            !s.contains("sk-proj-very-real-looking-key"),
            "API key leaked"
        );
        assert!(
            !s.contains("hooks.slack.com/services/SECRET"),
            "Slack webhook leaked"
        );
        assert!(
            !s.contains("/keys/vapid-private.pem"),
            "VAPID private path leaked"
        );
        // Sentinel should appear
        assert!(s.contains(SECRET_REDACTED_SENTINEL));
        // Non-secret fields should remain
        assert!(
            s.contains("ha.local:8123"),
            "base_url unexpectedly redacted"
        );
        assert!(s.contains("os_main"), "controller id unexpectedly redacted");
        assert!(s.contains("28.5"), "lat unexpectedly redacted");
    }

    #[test]
    fn redact_covers_smtp_username_and_password() {
        // EmailConfig.username is an SMTP credential; it must be redacted
        // alongside the password. (The MQTT username fields ride the same
        // `username` rule for free, which is correct: it's half of a
        // broker credential pair.)
        let mut v = serde_json::json!({
            "notifications": {
                "email": {
                    "smtp_host": "smtp.example.com",
                    "smtp_port": 587,
                    "username": "smtp_user_secret",
                    "password": "smtp_pass_secret",
                    "from_address": "alerts@example.com",
                    "to_address": "me@example.com",
                    "starttls": true
                }
            }
        });
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("smtp_user_secret"), "SMTP username leaked");
        assert!(!s.contains("smtp_pass_secret"), "SMTP password leaked");
        // Non-secret SMTP fields stay visible so the form still renders.
        assert!(
            s.contains("smtp.example.com"),
            "smtp_host unexpectedly redacted"
        );
        assert!(s.contains("alerts@example.com"), "from_address redacted");
    }

    #[test]
    fn redact_toml_str_sanitizes_a_real_config_file() {
        // The backup + raw read paths re-serialize the on-disk TOML through
        // this helper. Build a full Config, write it the same way the store
        // does, then prove the redacted TOML still parses AND carries no
        // cleartext secret.
        use crate::config::schema::*;
        let mut cfg = Config::default();
        cfg.deployment.location = Location {
            lat: 28.5,
            lon: -81.4,
            elevation_m: None,
        };
        cfg.sources.push(SourceEntry {
            id: "ha_pass".into(),
            priority: 30,
            enabled: true,
            max_age_s: None,
            source: SourceKind::HaPassthrough(HaPassthroughConfig {
                base_url: "http://ha.local:8123".into(),
                bearer_token: "supersecret_ha_token_xyz".into(),
                field_map: Default::default(),
            }),
        });
        cfg.controllers.push(ControllerEntry {
            id: "os_main".into(),
            default: true,
            enabled: true,
            controller: ControllerKind::OpensprinklerDirect(OpenSprinklerDirectConfig {
                host: "10.0.0.10".into(),
                port: 80,
                password_md5: "abc123md5hash".into(),
                poll_interval_s: 10,
            }),
        });
        cfg.notifications.email = Some(EmailConfig {
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            username: "smtp_user_secret".into(),
            password: "smtp_pass_secret".into(),
            from_address: "a@example.com".into(),
            to_address: "b@example.com".into(),
            starttls: true,
        });

        // Store-style serialization (matches FileConfigStore::save).
        let raw = toml::to_string_pretty(&cfg).unwrap();
        // Sanity: the RAW file does contain the secrets (this is the leak
        // the backup/raw paths used to ship).
        assert!(raw.contains("supersecret_ha_token_xyz"));

        let redacted = redact_toml_str(&raw).expect("redaction parses + re-serializes");
        // No cleartext secret survives.
        assert!(
            !redacted.contains("supersecret_ha_token_xyz"),
            "HA token leaked in backup TOML"
        );
        assert!(
            !redacted.contains("abc123md5hash"),
            "OS password_md5 leaked in backup TOML"
        );
        assert!(
            !redacted.contains("smtp_user_secret"),
            "SMTP username leaked in backup TOML"
        );
        assert!(
            !redacted.contains("smtp_pass_secret"),
            "SMTP password leaked in backup TOML"
        );
        assert!(
            redacted.contains(SECRET_REDACTED_SENTINEL),
            "sentinel present"
        );
        // The redacted output is still valid, restorable TOML.
        let reparsed: Config =
            toml::from_str(&redacted).expect("redacted TOML re-parses to Config");
        assert_eq!(reparsed.controllers[0].id, "os_main");
    }

    #[test]
    fn redact_covers_cloud_controller_account_email() {
        // The cloud controllers (B-hyve, Rain Bird) and the LaCrosse cloud
        // source authenticate with account-email + password. The password half
        // was already redacted; this proves the email (the username half) is
        // too, while a legitimate notification address (from_address /
        // to_address / vapid_subject mailto:) is NOT redacted.
        let mut v = serde_json::json!({
            "controllers": [{
                "id": "bhyve_main",
                "kind": "bhyve",
                "config": {
                    "email": "owner.account@example.com",
                    "password": "bhyve_pw_secret",
                    "device_id": "dev-123"
                }
            }, {
                "id": "rainbird_main",
                "kind": "rainbird",
                "config": {
                    "email": "rainbird.account@example.com",
                    "password": "rb_pw_secret",
                    "controller_id": "ctl-9"
                }
            }],
            "sources": [{
                "id": "lacrosse_main",
                "kind": "lacrosse",
                "config": {
                    "email": "lacrosse.account@example.com",
                    "password": "lc_pw_secret",
                    "device_id": "LTV-WSDTH04"
                }
            }],
            "notifications": {
                "email": {
                    "smtp_host": "smtp.example.com",
                    "from_address": "alerts@example.com",
                    "to_address": "me@example.com",
                    "username": "smtp_user",
                    "password": "smtp_pw"
                },
                "web_push": {
                    "vapid_subject": "mailto:ops@example.com"
                }
            }
        });
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        // Account emails (the credential username half) must NOT survive.
        assert!(
            !s.contains("owner.account@example.com"),
            "B-hyve account email leaked"
        );
        assert!(
            !s.contains("rainbird.account@example.com"),
            "Rain Bird account email leaked"
        );
        assert!(
            !s.contains("lacrosse.account@example.com"),
            "LaCrosse account email leaked"
        );
        // The password halves stay redacted as before.
        assert!(!s.contains("bhyve_pw_secret"), "B-hyve password leaked");
        assert!(!s.contains("rb_pw_secret"), "Rain Bird password leaked");
        assert!(!s.contains("lc_pw_secret"), "LaCrosse password leaked");
        // Legitimate NOTIFICATION addresses are untouched: from/to_address are
        // not credentials, and vapid_subject is a contact mailto:.
        assert!(
            s.contains("alerts@example.com"),
            "from_address must NOT be redacted"
        );
        assert!(
            s.contains("me@example.com"),
            "to_address must NOT be redacted"
        );
        assert!(
            s.contains("mailto:ops@example.com"),
            "vapid_subject must NOT be redacted"
        );
        // Non-secret device identifiers stay visible so the forms render.
        assert!(s.contains("dev-123"), "device_id unexpectedly redacted");
        assert!(s.contains("LTV-WSDTH04"), "device_id unexpectedly redacted");
    }

    #[test]
    fn redact_empty_strings_left_alone() {
        let mut v = serde_json::json!({
            "config": {
                "api_key": ""
            }
        });
        redact_secrets(&mut v);
        // Empty stays empty (so the UI can distinguish "no token set" from "redacted")
        assert_eq!(v["config"]["api_key"], "");
    }

    #[test]
    fn unredact_restores_original_secret_when_sentinel_present() {
        let original = cfg_with_secrets();
        let mut redacted = original.clone();
        redact_secrets(&mut redacted);
        // Simulate the user submitting the redacted form unchanged
        let mut candidate = redacted.clone();
        unredact_secrets(&mut candidate, &original);
        // The candidate now matches the original
        assert_eq!(candidate, original, "unredact failed to restore secrets");
    }

    #[test]
    fn unredact_keeps_user_edit() {
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        candidate["llm"]["config"]["api_key"] = serde_json::json!("new-api-key");
        unredact_secrets(&mut candidate, &original);
        // Edited value preserved (it wasn't the sentinel)
        assert_eq!(candidate["llm"]["config"]["api_key"], "new-api-key");
    }

    #[test]
    fn unredact_reordered_sources_keeps_secrets_on_the_right_id() {
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        redact_secrets(&mut candidate);
        // User reordered the sources array in the settings UI.
        let arr = candidate["sources"].as_array_mut().unwrap();
        arr.reverse();
        unredact_secrets(&mut candidate, &original);
        let mqtt = candidate["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "mqtt_sensors")
            .unwrap();
        assert_eq!(
            mqtt["config"]["password"], "mqtt_password_123",
            "mqtt entry must get the mqtt password, not the HA token"
        );
        let ha = candidate["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "ha_pass")
            .unwrap();
        assert_eq!(ha["config"]["bearer_token"], "supersecret_ha_token_xyz");
    }

    #[test]
    fn unredact_after_delete_does_not_shift_secrets() {
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        redact_secrets(&mut candidate);
        // User deleted the FIRST source; index 0 is now mqtt_sensors.
        candidate["sources"].as_array_mut().unwrap().remove(0);
        unredact_secrets(&mut candidate, &original);
        let sources = candidate["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["id"], "mqtt_sensors");
        assert_eq!(
            sources[0]["config"]["password"], "mqtt_password_123",
            "deletion must not hand mqtt the deleted entry's secret"
        );
        // And nothing still carries the sentinel.
        let mut leftover = Vec::new();
        remaining_sentinels(&candidate, "$", &mut leftover);
        assert!(leftover.is_empty(), "leftover sentinels: {leftover:?}");
    }

    #[test]
    fn redact_and_roundtrip_new_source_oauth_secrets() {
        // The OAuth-style source secrets (Ambient Weather app_key, Netatmo /
        // YoLink / Tuya client_secret + refresh_token) must be redacted on the
        // GET path and round-trip back on a PUT that sends the sentinel
        // unchanged. client_id is a PUBLIC identifier and must NOT be redacted.
        let original = serde_json::json!({
            "schema_version": 1,
            "sources": [{
                "id": "netatmo_main",
                "priority": 40,
                "enabled": true,
                "kind": "netatmo",
                "config": {
                    "client_id": "63abc_public_client_id",
                    "client_secret": "very_secret_client_secret_value",
                    "refresh_token": "rt_super_secret_refresh_token",
                    "device_id": "70:ee:50:00:11:22"
                }
            }, {
                "id": "ambient_main",
                "priority": 50,
                "enabled": true,
                "kind": "ambient_weather",
                "config": {
                    "app_key": "ambient_secret_app_key_zzz",
                    "api_key": "ambient_secret_api_key_yyy",
                    "mac_address": "AA:BB:CC:DD:EE:FF"
                }
            }]
        });

        // GET path: redaction hides every new secret but leaves client_id +
        // non-secret fields visible.
        let mut redacted = original.clone();
        redact_secrets(&mut redacted);
        let s = serde_json::to_string(&redacted).unwrap();
        assert!(
            !s.contains("very_secret_client_secret_value"),
            "client_secret leaked"
        );
        assert!(
            !s.contains("rt_super_secret_refresh_token"),
            "refresh_token leaked"
        );
        assert!(!s.contains("ambient_secret_app_key_zzz"), "app_key leaked");
        assert!(!s.contains("ambient_secret_api_key_yyy"), "api_key leaked");
        // client_id is public: it must survive verbatim.
        assert!(
            s.contains("63abc_public_client_id"),
            "client_id must NOT be redacted (public identifier)"
        );
        assert!(
            s.contains("70:ee:50:00:11:22"),
            "device_id unexpectedly redacted"
        );

        // PUT path: client sends the redacted JSON unchanged; unredact restores
        // every stored secret by sentinel match, leaving no sentinel behind.
        let mut candidate = redacted.clone();
        unredact_secrets(&mut candidate, &original);
        assert_eq!(
            candidate, original,
            "sentinel round-trip failed to restore new source secrets"
        );
        let mut leftover = Vec::new();
        remaining_sentinels(&candidate, "$", &mut leftover);
        assert!(leftover.is_empty(), "leftover sentinels: {leftover:?}");
    }

    #[test]
    fn new_entry_with_sentinel_is_flagged_not_silently_saved() {
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        redact_secrets(&mut candidate);
        // User added a brand-new source but left the secret field as
        // the redaction placeholder.
        candidate["sources"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "brand_new",
                "priority": 10,
                "enabled": true,
                "kind": "mqtt",
                "config": { "broker_host": "x", "broker_port": 1883,
                            "username": "u", "password": SECRET_REDACTED_SENTINEL,
                            "subscriptions": [] }
            }));
        unredact_secrets(&mut candidate, &original);
        let mut leftover = Vec::new();
        remaining_sentinels(&candidate, "$", &mut leftover);
        assert_eq!(leftover.len(), 1, "exactly the new entry's secret flagged");
        assert!(
            leftover[0].contains("brand_new"),
            "path names the entry: {leftover:?}"
        );
    }
}
