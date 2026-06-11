// Observed-weather history. GET /api/v1/weather/history?hours=24 returns
// recent series (oldest -> newest) for the headline fields, read from the
// sensor_history table the weather sampler populates. Powers the Weather
// home telemetry sparklines.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::Json,
    routing::get,
    Router,
};
use rusqlite::Connection;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::history::types::WeatherHistory;
use crate::persistence::sensor_history::SensorHistoryStore;

pub fn router(db: Arc<Mutex<Connection>>) -> Router {
    Router::new()
        .route("/history", get(history))
        .route("/readings", get(readings))
        .with_state(db)
}

#[derive(Deserialize)]
struct HistoryQuery {
    hours: Option<i64>,
}

#[derive(Deserialize)]
struct ReadingsQuery {
    source: String,
}

#[derive(serde::Serialize)]
struct SourceReading {
    key: String,
    value: f64,
    age_s: i64,
}

/// GET /api/v1/weather/readings?source=ID, the latest value per key a
/// source has reported, newest first. Powers the Sensors page's
/// per-integration live-data view: "is it posting, and what?" Used to
/// validate that a local source (Ecowitt, Tempest, webhook) is actually
/// ingesting after it's been added.
async fn readings(
    State(db): State<Arc<Mutex<Connection>>>,
    Query(q): Query<ReadingsQuery>,
) -> Json<Vec<SourceReading>> {
    let store = SensorHistoryStore::new(db);
    let now = chrono::Utc::now().timestamp();
    let rows = store.latest_for_source(q.source).await.unwrap_or_default();
    Json(
        rows.into_iter()
            .map(|r| SourceReading {
                key: r.key,
                value: r.value,
                age_s: (now - r.epoch).max(0),
            })
            .collect(),
    )
}

async fn history(
    State(db): State<Arc<Mutex<Connection>>>,
    Query(q): Query<HistoryQuery>,
) -> Json<WeatherHistory> {
    let store = SensorHistoryStore::new(db);
    let hours = q.hours.unwrap_or(24).clamp(1, 168);
    let now = chrono::Utc::now().timestamp();
    let from = now - hours * 3600;

    // series() returns most-recent-first; reverse to oldest-first for charts.
    let series = |key: &'static str| {
        let store = store.clone();
        async move {
            let mut rows = store
                .series(key.to_string(), from, now + 1, 5000)
                .await
                .unwrap_or_default();
            rows.reverse();
            rows.into_iter().map(|r| r.value).collect::<Vec<f64>>()
        }
    };

    Json(WeatherHistory {
        air_temp_f: series("air_temp_f").await,
        rh_pct: series("rh_pct").await,
        wind_avg_mph: series("wind_avg_mph").await,
        pressure_inhg: series("pressure_inhg").await,
        solar_w_m2: series("solar_w_m2").await,
        uv_index: series("uv_index").await,
    })
}
