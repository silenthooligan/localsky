// Forecast API: snapshot + SSE stream for the 7-day daily +
// 48-hour hourly Open-Meteo feed, plus the learned per-month forecast
// bias multiplier (when enough observations are recorded).

use crate::engine::forecast_bias::{BiasModel, DEFAULT_WINDOW_DAYS, MIN_OBSERVATIONS};
use crate::forecast::ForecastStore;
use crate::persistence::ForecastObservationsStore;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::get,
    Router,
};
use chrono::Datelike;
use futures::stream::Stream;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_stream::wrappers::{IntervalStream, WatchStream};
use tokio_stream::StreamExt;

#[derive(Clone)]
struct ForecastApiState {
    store: Arc<ForecastStore>,
    observations: Option<ForecastObservationsStore>,
    archive: Option<crate::persistence::forecast_archive::ForecastArchiveStore>,
}

pub fn router(store: Arc<ForecastStore>, db: Option<Arc<Mutex<Connection>>>) -> Router {
    let archive_store = db
        .clone()
        .map(crate::persistence::forecast_archive::ForecastArchiveStore::new);
    let observations = db.map(ForecastObservationsStore::new);
    let state = ForecastApiState {
        store,
        observations,
        archive: archive_store,
    };
    Router::new()
        .route("/snapshot", get(snapshot))
        .route("/stream", get(stream))
        .route("/bias", get(bias))
        .route("/window", get(window))
        .route("/tracks", get(tracks))
        .route("/archive", get(archive))
        .with_state(state)
}

#[derive(Deserialize)]
struct WindowQuery {
    #[serde(default = "merged_track")]
    track: String,
    from: i64,
    to: i64,
}

fn merged_track() -> String {
    "merged".into()
}

#[derive(Deserialize)]
struct ArchiveParams {
    #[serde(default = "merged_track")]
    track: String,
    from: i64,
    to: i64,
    lead_h: Option<u32>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn archive(
    State(state): State<ForecastApiState>,
    Query(query): Query<ArchiveParams>,
    headers: HeaderMap,
) -> axum::response::Response {
    use crate::persistence::forecast_archive::{ArchiveQuery, MAX_PAGE_SIZE, RETENTION_DAYS};
    let bad_request =
        |error: &str| (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response();
    if let Err(error) =
        crate::forecast::window::validate_range(query.from, query.to, RETENTION_DAYS * 86400)
    {
        return bad_request(error);
    }
    let limit = query.limit.unwrap_or(1000);
    if limit == 0 || limit > MAX_PAGE_SIZE || query.lead_h.is_some_and(|lead| lead > 47) {
        return bad_request("limit must be 1..5000 and lead_h must be 0..47");
    }
    let after = match query.cursor.as_deref() {
        None => None,
        Some(cursor) => match cursor
            .split_once(':')
            .and_then(|(a, b)| Some((a.parse::<i64>().ok()?, b.parse::<i64>().ok()?)))
        {
            Some((a, b)) if a >= 0 && b >= 0 => Some((a, b)),
            _ => return bad_request("invalid archive cursor"),
        },
    };
    let Some(store) = state.archive else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"forecast archive requires persistent storage"})),
        )
            .into_response();
    };
    let page = match store
        .query(ArchiveQuery {
            track: query.track,
            from: query.from,
            to: query.to,
            lead_h: query.lead_h,
            after,
            limit,
        })
        .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::warn!(%error, "forecast archive read failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"forecast archive read failed"})),
            )
                .into_response();
        }
    };
    if headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/csv"))
    {
        let mut csv =
            String::from("track,provider,model,target_epoch,lead_h,pop_pct,precip_in,fetched_at\n");
        for row in &page.rows {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                csv_text(&row.track),
                csv_text(&row.provider),
                csv_text(row.model.as_deref().unwrap_or("")),
                row.target_epoch,
                row.lead_h,
                row.pop_pct.map(|v| v.to_string()).unwrap_or_default(),
                row.precip_in.map(|v| v.to_string()).unwrap_or_default(),
                row.fetched_at
            ));
        }
        let mut response = (
            [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
            csv,
        )
            .into_response();
        if let Some(cursor) = page.next_cursor.and_then(|v| v.parse().ok()) {
            response.headers_mut().insert("x-next-cursor", cursor);
        }
        response
    } else {
        Json(page).into_response()
    }
}

fn csv_text(value: &str) -> String {
    // Provider labels can come from configured adapters. Quote CSV structure
    // and neutralize spreadsheet formulas without changing stored provenance.
    let formula = value.trim_start().starts_with(['=', '+', '-', '@']);
    format!(
        "\"{}{}\"",
        if formula { "'" } else { "" },
        value.replace('"', "\"\"")
    )
}

async fn window(
    State(state): State<ForecastApiState>,
    Query(query): Query<WindowQuery>,
) -> axum::response::Response {
    let (model, snapshot) = if query.track == "merged" {
        (None, state.store.snapshot().as_ref().clone())
    } else {
        let Some((model, snapshot)) = state.store.tracks.snapshot(&query.track) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Unknown forecast track" })),
            )
                .into_response();
        };
        (Some(model), snapshot)
    };
    match crate::forecast::window::query(
        &snapshot,
        &query.track,
        model.as_deref(),
        query.from,
        query.to,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(result) => Json(result).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response(),
    }
}

async fn tracks(
    State(state): State<ForecastApiState>,
) -> Json<Vec<crate::forecast::tracks::TrackStatus>> {
    Json(state.store.tracks.status(chrono::Utc::now().timestamp()))
}

async fn snapshot(
    State(state): State<ForecastApiState>,
) -> Json<crate::forecast::snapshot::ForecastSnapshot> {
    let s = state.store.snapshot();
    Json(current_calendar_view(&s))
}

fn current_calendar_view(
    s: &crate::forecast::snapshot::ForecastSnapshot,
) -> crate::forecast::snapshot::ForecastSnapshot {
    let cal = crate::timeutil::deployment_calendar();
    match cal.date_of(chrono::Utc::now().timestamp()) {
        Some(day) => s.for_day(cal, day),
        None => s.clone(),
    }
}

async fn stream(
    State(state): State<ForecastApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.store.subscribe();
    // A quiet provider must not leave an open browser on yesterday's labels.
    // Re-project the cached facts periodically without refreshing their age.
    let interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    let rollover = IntervalStream::new(interval).map(move |_| state.store.snapshot());
    let s = WatchStream::new(rx).merge(rollover).map(|snap| {
        let payload =
            serde_json::to_string(&current_calendar_view(&snap)).unwrap_or_else(|_| "{}".into());
        Ok(Event::default().event("snapshot").data(payload))
    });
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}

#[derive(Serialize)]
struct BiasResponse {
    /// Multiplier currently active given today's month. Apply to a
    /// raw forecast rain amount before passing it into the skip-rule
    /// inputs.
    current_month_multiplier: f64,
    current_month: u32,
    min_observations_required: usize,
    window_days: i64,
    months: Vec<MonthBiasRow>,
}

#[derive(Serialize)]
struct MonthBiasRow {
    month: u32,
    multiplier: f64,
    samples: usize,
    description: String,
}

async fn bias(State(state): State<ForecastApiState>) -> impl IntoResponse {
    let Some(observations_store) = state.observations else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "history database not mounted; forecast-bias requires /data persistence",
            })),
        )
            .into_response();
    };

    let observations = match observations_store.recent(DEFAULT_WINDOW_DAYS).await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("forecast observation read failed: {e}") })),
            )
                .into_response();
        }
    };

    // Configured-timezone date, matching the calendar the observation rows
    // are keyed by (the writer stamps configured-tz days).
    let today = crate::timeutil::now_local().date_naive();
    let model = BiasModel::from_observations(&observations, today, None);
    let current_month = today.month();

    let months: Vec<MonthBiasRow> = (1..=12u32)
        .map(|m| MonthBiasRow {
            month: m,
            multiplier: model.multiplier_for(m),
            samples: model.sample_count_for(m),
            description: model.describe_month(m),
        })
        .collect();

    Json(BiasResponse {
        current_month_multiplier: model.multiplier_for(current_month),
        current_month,
        min_observations_required: MIN_OBSERVATIONS,
        window_days: DEFAULT_WINDOW_DAYS,
        months,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn windows_use_the_same_router_for_merged_named_missing_and_invalid_queries() {
        let store = Arc::new(ForecastStore::new());
        store.store(crate::forecast::snapshot::ForecastSnapshot {
            last_refresh_epoch: 1000,
            hourly: vec![crate::forecast::snapshot::HourlyEntry {
                time_epoch: 3600,
                precip_in: Some(0.0),
                ..Default::default()
            }],
            ..Default::default()
        });
        let config = crate::config::Config {
            forecast_tracks: vec![crate::config::ForecastTrack {
                id: "nbm".into(),
                model: "ncep_nbm_conus".into(),
            }],
            ..Default::default()
        };
        store.tracks.configure(&config);
        let app = router(store, None);
        for (url, status) in [
            ("/window?from=3600&to=3600", StatusCode::OK),
            ("/window?track=nbm&from=3600&to=3600", StatusCode::OK),
            (
                "/window?track=absent&from=3600&to=3600",
                StatusCode::NOT_FOUND,
            ),
            ("/window?from=3600&to=0", StatusCode::BAD_REQUEST),
            ("/window?from=0&to=172801", StatusCode::BAD_REQUEST),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{url}");
            if url == "/window?from=3600&to=3600" {
                let body: serde_json::Value =
                    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap())
                        .unwrap();
                assert_eq!(body["fetched_at"], 1000);
                assert_eq!(body["precip_sum_in"], 0.0);
                assert_eq!(body["pop_max_pct"], serde_json::Value::Null);
            }
        }
    }

    #[tokio::test]
    async fn archive_reads_json_csv_and_bounds_bad_queries() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
        let db = Arc::new(Mutex::new(conn));
        let archive = crate::persistence::forecast_archive::ForecastArchiveStore::new(db.clone());
        let snapshot = crate::forecast::snapshot::ForecastSnapshot {
            source_label: "NWS".into(),
            last_refresh_epoch: 1000,
            hourly: vec![crate::forecast::snapshot::HourlyEntry {
                time_epoch: 3600,
                precip_in: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        archive.record("merged", None, &snapshot).await.unwrap();
        let app = router(Arc::new(ForecastStore::new()), Some(db));
        for accept in ["application/json", "text/csv"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/archive?from=0&to=7200")
                        .header("accept", accept)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 65536).await.unwrap();
            if accept == "application/json" {
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert!(value["rows"][0]["precip_in"].is_null());
                assert_eq!(value["rows"][0]["provider"], "NWS");
            } else {
                let body = std::str::from_utf8(&body).unwrap();
                assert!(body.contains("target_epoch,lead_h,pop_pct,precip_in,fetched_at"));
                assert!(body.contains("3600,1,,,1000"));
            }
        }
        for query in [
            "from=2&to=1",
            "from=0&to=34560001",
            "from=0&to=7200&lead_h=48",
            "from=0&to=7200&limit=5001",
            "from=0&to=7200&cursor=bad",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/archive?{query}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        }
        assert_eq!(
            csv_text("=formula,\"quoted\""),
            "\"'=formula,\"\"quoted\"\"\""
        );
    }
}
