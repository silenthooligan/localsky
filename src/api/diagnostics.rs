// GET /api/v1/diagnostics: the bundle the bug template asks for, in one
// request. Health, info, the last few hundred log lines, the config with
// its secrets redacted, and the current decision trace. Everything in it
// is scrubbed against the config's own secret values, log lines included,
// so it can be pasted into an issue. Privileged like the config surface.

use std::sync::Arc;

use axum::{extract::State, response::Json, routing::get, Router};

use crate::config::FileConfigStore;
use crate::refresher::IrrigationStore;

#[derive(Clone)]
pub struct DiagnosticsState {
    pub cfg_store: Arc<FileConfigStore>,
    pub irrigation_store: Option<Arc<IrrigationStore>>,
    pub health: crate::api::health::HealthState,
    /// The in-memory tail of the log.
    pub logs: crate::logring::Ring,
}

pub fn router(state: DiagnosticsState) -> Router {
    Router::new().route("/", get(diagnostics)).with_state(state)
}

/// How many log lines ride in the bundle.
pub const LOG_LINES: usize = 300;

/// Replace every occurrence of a secret value with the redaction sentinel.
pub fn scrub(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s, crate::api::config::SECRET_REDACTED_SENTINEL);
        }
    }
    out
}

/// The bundle, as JSON. `log_lines` is injected so the assembly is
/// testable with a known log; the handler passes the ring buffer.
pub async fn build_bundle(
    state: &DiagnosticsState,
    log_lines: Vec<String>,
    auth_required: bool,
) -> serde_json::Value {
    use crate::ports::config_store::ConfigStore;
    // Secrets come from the raw document first, so a config that fails to
    // load (the case a bug report is most likely about) still scrubs, and
    // from the typed config too, whose `${VAR}` references are expanded.
    let mut secrets = Vec::new();
    let raw_doc: Option<serde_json::Value> = tokio::fs::read_to_string(state.cfg_store.path())
        .await
        .ok()
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
        .and_then(|t| serde_json::to_value(t).ok());
    if let Some(raw) = raw_doc.as_ref() {
        secrets.extend(crate::api::config::secret_values(raw));
    }
    let config = match state.cfg_store.load().await {
        Ok(cfg) => {
            let mut v = serde_json::to_value(&cfg).unwrap_or(serde_json::Value::Null);
            secrets.extend(crate::api::config::secret_values(&v));
            crate::api::config::redact_secrets(&mut v);
            v
        }
        Err(e) => {
            let mut v = raw_doc.unwrap_or(serde_json::Value::Null);
            crate::api::config::redact_secrets(&mut v);
            serde_json::json!({ "load_error": e.to_string(), "document": v })
        }
    };
    secrets.sort();
    secrets.dedup();
    let health = crate::api::health::health_report(state.health.clone(), true).await;
    let info = crate::api::info::build_info(auth_required).await;
    let snapshot = state.irrigation_store.as_ref().map(|s| s.snapshot());
    let decision_trace = snapshot
        .as_ref()
        .and_then(|s| s.decision_trace.clone())
        .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null));
    let logs: Vec<String> = log_lines.iter().map(|l| scrub(l, &secrets)).collect();
    let mut bundle = serde_json::json!({
        "generated_at_epoch": chrono::Utc::now().timestamp(),
        "info": info,
        "health": health,
        "config": config,
        "decision_trace": decision_trace,
        "logs": logs,
    });
    // Belt and braces: whatever else carried a secret string (a health
    // note, a trace reason) is scrubbed as text too.
    if !secrets.is_empty() {
        let text = scrub(&bundle.to_string(), &secrets);
        bundle = serde_json::from_str(&text).unwrap_or(bundle);
    }
    bundle
}

async fn diagnostics(
    State(state): State<DiagnosticsState>,
    req: axum::http::Request<axum::body::Body>,
) -> Json<serde_json::Value> {
    let auth_required = req
        .extensions()
        .get::<crate::auth::middleware::AuthRequired>()
        .map(|a| a.0)
        .unwrap_or(false);
    Json(build_bundle(&state, state.logs.recent(LOG_LINES), auth_required).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config full of secrets, a log line that leaked one: the bundle
    /// carries none of them, and says so where each was.
    #[tokio::test]
    async fn the_bundle_contains_no_secret_from_the_redaction_list() {
        let dir = std::env::temp_dir().join(format!("localsky-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localsky.toml");
        std::fs::write(
            &path,
            r#"
schema_version = 2
[deployment.location]
lat = 29.65
lon = -82.32

[[sources]]
id = "ha_bridge"
kind = "ha_passthrough"
[sources.config]
base_url = "http://10.0.0.10:8123"
bearer_token = "eyJ-bearer-SECRET-token-1234"

[[sources]]
id = "pirate"
kind = "pirate_weather"
[sources.config]
api_key = "pirate-API-KEY-5678"

[notifications.ntfy]
base_url = "https://ntfy.sh"
topic = "yard"
token = "tk_ntfy-SECRET-9012"
"#,
        )
        .unwrap();
        let cfg_store = Arc::new(FileConfigStore::new(&path));
        let state = DiagnosticsState {
            logs: crate::logring::Ring::new(),
            cfg_store: cfg_store.clone(),
            irrigation_store: None,
            health: crate::api::health::HealthState {
                started_at: std::time::Instant::now(),
                config_store: Some(cfg_store),
                sensor_history: None,
                tempest_store: None,
                forecast_store: None,
                irrigation_store: None,
                source_last_seen: None,
                source_reachable: None,
                active_runs: None,
            },
        };
        let logs = vec![
            "2026-09-07T04:00:00Z  INFO localsky: fetch http://10.0.0.10:8123/api/states bearer eyJ-bearer-SECRET-token-1234".to_string(),
            "2026-09-07T04:00:01Z  WARN localsky: key pirate-API-KEY-5678 rejected".to_string(),
        ];
        let bundle = build_bundle(&state, logs, false).await;
        let text = bundle.to_string();
        for secret in [
            "eyJ-bearer-SECRET-token-1234",
            "pirate-API-KEY-5678",
            "tk_ntfy-SECRET-9012",
        ] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert!(text.contains(crate::api::config::SECRET_REDACTED_SENTINEL));
        assert_eq!(bundle["logs"].as_array().map(Vec::len), Some(2));
        assert!(bundle["health"]["status"].is_string());
        assert_eq!(bundle["info"]["service"], "localsky");
        assert!(
            bundle["config"]["deployment"]["location"]["lat"].is_number(),
            "the config loaded: {}",
            bundle["config"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A config that does not load (here: an unset `${VAR}` reference) is
    /// the one a bug report is about; its secrets are still scrubbed, from
    /// the raw document.
    #[tokio::test]
    async fn a_broken_config_still_scrubs() {
        let dir = std::env::temp_dir().join(format!("localsky-diag-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localsky.toml");
        std::fs::write(
            &path,
            "schema_version = 2
[[sources]]
id = \"x\"
kind = \"ha_passthrough\"
[sources.config]
base_url = \"${LOCALSKY_DIAG_UNSET_VAR}\"
bearer_token = \"broken-SECRET-4321\"
",
        )
        .unwrap();
        let cfg_store = Arc::new(FileConfigStore::new(&path));
        let state = DiagnosticsState {
            logs: crate::logring::Ring::new(),
            cfg_store: cfg_store.clone(),
            irrigation_store: None,
            health: crate::api::health::HealthState {
                started_at: std::time::Instant::now(),
                config_store: Some(cfg_store),
                sensor_history: None,
                tempest_store: None,
                forecast_store: None,
                irrigation_store: None,
                source_last_seen: None,
                source_reachable: None,
                active_runs: None,
            },
        };
        let bundle =
            build_bundle(&state, vec!["token broken-SECRET-4321 seen".into()], false).await;
        let text = bundle.to_string();
        assert!(!text.contains("broken-SECRET-4321"), "{text}");
        assert!(bundle["config"]["load_error"].is_string());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
