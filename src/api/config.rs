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
//   POST /api/config/rollback?to=<v> -> restore a snapshot (Phase 4)
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

use crate::config::schema::Config;
use crate::config::FileConfigStore;
use crate::ports::config_store::{ConfigStore, ConfigStoreError};

pub type ConfigApiState = Arc<FileConfigStore>;

pub fn router(state: ConfigApiState) -> Router {
    Router::new()
        .route("/", get(get_config).put(put_config))
        .route("/validate", get(get_validate))
        .route("/schema", get(get_schema))
        .route("/preview", post(preview_config))
        .route("/rollback", post(post_rollback))
        .route("/raw", get(get_raw_toml).put(put_raw_toml))
        .with_state(state)
}

/// Return the raw TOML bytes of /data/localsky.toml as text/plain so the
/// Advanced settings page can render a textarea editor. Secrets are NOT
/// redacted here (unlike GET /); this endpoint is operator-only and
/// gated behind the existing app auth surface. Empty 200 when the file
/// hasn't been written yet so the wizard can pre-populate via PUT.
async fn get_raw_toml(State(store): State<ConfigApiState>) -> impl IntoResponse {
    match tokio::fs::read_to_string(store.path()).await {
        Ok(s) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            s,
        )
            .into_response(),
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

/// Replace /data/localsky.toml with the supplied TOML body verbatim,
/// after a round-trip validation that the text parses to a Config
/// matching the schema invariants. On success the FileConfigStore's
/// in-memory cache is invalidated by the next load() call.
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
    if let Err(e) = tokio::fs::write(store.path(), body.as_bytes()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "raw_write_failed".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response();
    }
    Json(serde_json::json!({ "ok": true, "bytes": body.len() })).into_response()
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

fn redact_secrets(v: &mut serde_json::Value) {
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
                    || lk == "access_token";
                if is_secret {
                    if let Value::String(s) = val {
                        if !s.is_empty() {
                            *s = SECRET_REDACTED_SENTINEL.to_string();
                        }
                    }
                } else {
                    redact_secrets(val);
                }
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

/// Inverse of redact_secrets: walks the candidate config alongside the
/// stored config, and any place the candidate contains the sentinel,
/// substitutes the original value back in. Lets clients PUT a redacted
/// JSON without losing the secret.
fn unredact_secrets(candidate: &mut serde_json::Value, original: &serde_json::Value) {
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
            for (i, c_v) in c.iter_mut().enumerate() {
                if let Some(o_v) = o.get(i) {
                    unredact_secrets(c_v, o_v);
                }
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

#[derive(Debug, Deserialize)]
struct RollbackQuery {
    to: u32,
}

async fn post_rollback(
    State(store): State<ConfigApiState>,
    Query(q): Query<RollbackQuery>,
) -> impl IntoResponse {
    // Phase 4 DB wiring will make this functional. Until then it returns
    // 404 because no snapshots are persisted.
    match store.rollback(q.to).await {
        Ok(cfg) => Json(cfg).into_response(),
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
}
