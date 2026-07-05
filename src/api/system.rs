// /api/system: process-level operations. One route today:
//
//   POST /system/restart -> restart LocalSky from inside the app, so a
//   "restart required" config change never sends the user off to manage the
//   container by hand.
//
// Mechanism, auto-detected per deployment:
//   - HAOS add-on (SUPERVISOR_TOKEN present): POST the Supervisor's
//     /addons/self/restart, the supported add-on self-restart path. Falls
//     back to process exit if the call fails (the add-on's watchdog +
//     boot=auto still bring it back).
//   - Everything else: reply first, then a clean process exit. Every shipped
//     install path runs under a restart policy (compose/docker-run snippets
//     are `--restart unless-stopped`; runit/systemd docs use auto-restart),
//     so exit IS restart. A bare `cargo run` user gets an honest note in the
//     UI confirm instead of a dead promise.
//
// Safety: the route sits behind the privileged gate (same bar as a config
// write; see auth::middleware::is_privileged_path), and it REFUSES while any
// zone is actively watering unless `force: true` (the shut-off backstops +
// boot valve reconciliation make even a forced restart safe, but nobody
// should interrupt a running cycle by accident).

use std::sync::Arc;

use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::persistence::active_runs::ActiveRunsStore;

/// Exit code used for the self-restart path. Non-zero on purpose: it also
/// satisfies `on-failure` restart policies, while `always`/`unless-stopped`
/// (the documented defaults) restart regardless.
const RESTART_EXIT_CODE: i32 = 64;

pub fn router(db: Option<Arc<Mutex<Connection>>>) -> Router {
    Router::new()
        .route("/restart", post(restart))
        .with_state(db)
}

#[derive(serde::Deserialize, Default)]
struct RestartReq {
    /// Restart even while a zone is actively watering. The backstops close
    /// valves across restarts, but this is never the default.
    #[serde(default)]
    force: bool,
}

#[derive(serde::Serialize)]
struct RestartResp {
    /// "supervisor" (HAOS add-on API restart) or "exit" (process exit; the
    /// container/service restart policy brings it back).
    mode: &'static str,
}

async fn restart(
    axum::extract::State(db): axum::extract::State<Option<Arc<Mutex<Connection>>>>,
    body: Option<Json<RestartReq>>,
) -> impl IntoResponse {
    let req = body.map(|Json(b)| b).unwrap_or_default();

    // Active-watering guard: interrupting a commanded-on zone needs intent.
    if !req.force {
        if let Some(db) = db.as_ref() {
            // due(i64::MAX) = every armed run regardless of deadline: the
            // store has no plain list, and "armed at all" is the guard's
            // question, not "overdue".
            let running = ActiveRunsStore::new(db.clone())
                .due(i64::MAX)
                .await
                .unwrap_or_default();
            if !running.is_empty() {
                let zones: Vec<String> = running.into_iter().map(|r| r.zone_slug).collect();
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "watering_in_progress",
                        "detail": format!(
                            "zone(s) currently watering: {}. Stop them first, or pass force=true \
                             (valves are closed by the shut-off backstop and reconciled at boot).",
                            zones.join(", ")
                        ),
                        "zones": zones,
                    })),
                )
                    .into_response();
            }
        }
    }

    // HAOS add-on: ask the Supervisor to restart us (the supported path).
    // Detected by the token the Supervisor injects; run.sh does not forward
    // it into LocalSky's own config, so its presence == running as the add-on.
    if let Ok(token) = std::env::var("SUPERVISOR_TOKEN") {
        tokio::spawn(async move {
            // Give the 202 below time to flush before the Supervisor tears us
            // down mid-response.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let client = reqwest::Client::new();
            let resp = client
                .post("http://supervisor/addons/self/restart")
                .bearer_auth(&token)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    tracing::info!("supervisor accepted the add-on restart");
                }
                Ok(r) => {
                    tracing::warn!(
                        status = %r.status(),
                        "supervisor refused the restart; falling back to process exit \
                         (watchdog restarts the add-on)"
                    );
                    graceful_exit().await;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "supervisor unreachable; falling back to process exit"
                    );
                    graceful_exit().await;
                }
            }
        });
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::to_value(RestartResp { mode: "supervisor" }).unwrap()),
        )
            .into_response();
    }

    // Container / service-manager path: reply, then exit cleanly. The restart
    // policy (unless-stopped in every documented install) brings us back.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        graceful_exit().await;
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::to_value(RestartResp { mode: "exit" }).unwrap()),
    )
        .into_response()
}

/// Best-effort clean shutdown, then exit with the restart code. State that
/// must survive is already durable by design: the forecast cache persists on
/// every write, SQLite runs in WAL and recovers cleanly, valves are
/// reconciled at boot. A brief pause lets in-flight writes settle.
async fn graceful_exit() -> ! {
    tracing::info!("in-app restart: exiting (restart policy / watchdog brings LocalSky back)");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    std::process::exit(RESTART_EXIT_CODE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn store_with_run(zone: &str) -> Arc<Mutex<Connection>> {
        let mut c = Connection::open_in_memory().unwrap();
        crate::persistence::runner::run(&mut c).unwrap();
        let db = Arc::new(Mutex::new(c));
        let now = chrono::Utc::now().timestamp();
        ActiveRunsStore::new(db.clone())
            .arm(
                zone.to_string(),
                "opensprinkler".to_string(),
                now,
                now + 600,
            )
            .await
            .unwrap();
        db
    }

    // The guard is the only part of the handler a test can safely exercise
    // (the happy path exits the process). A zone actively watering must 409
    // without force.
    #[tokio::test]
    async fn restart_refuses_while_watering() {
        let db = store_with_run("back_yard").await;
        let app = router(Some(db));
        let resp = app
            .oneshot(
                Request::post("/restart")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"], "watering_in_progress");
        assert!(v["detail"].as_str().unwrap().contains("back_yard"));
    }
}
