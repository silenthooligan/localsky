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

use crate::config::FileConfigStore;
use crate::history::types::WeatherHistory;
use crate::persistence::sensor_history::{Reading, SensorHistoryStore};
use crate::ports::config_store::ConfigStore;

#[derive(Clone)]
struct WeatherState {
    db: Arc<Mutex<Connection>>,
    /// Judges which recorded soil channels are retired pre-1.1.0 zone
    /// readings. None when no config store is mounted.
    cfg_store: Option<Arc<FileConfigStore>>,
}

pub fn router(db: Arc<Mutex<Connection>>, cfg_store: Option<Arc<FileConfigStore>>) -> Router {
    Router::new()
        .route("/history", get(history))
        .route("/readings", get(readings))
        .with_state(WeatherState { db, cfg_store })
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
    State(st): State<WeatherState>,
    Query(q): Query<ReadingsQuery>,
) -> Json<Vec<SourceReading>> {
    let store = SensorHistoryStore::new(st.db);
    let now = chrono::Utc::now().timestamp();
    let rows = store.latest_for_source(q.source).await.unwrap_or_default();
    let cfg = match st.cfg_store.as_ref() {
        Some(store) => store.load().await.ok(),
        None => None,
    };
    Json(
        current_readings(cfg.as_ref(), rows)
            .into_iter()
            .map(|r| SourceReading {
                key: r.key,
                value: r.value,
                age_s: (now - r.epoch).max(0),
            })
            .collect(),
    )
}

/// Drop the retired pre-1.1.0 zone soil channels (`soilmoisture_<zone>`
/// for a zone whose mappings now record one channel per probe). Nothing
/// writes them after the upgrade, so listing them would show a frozen value
/// under a header that says the source updated just now.
fn current_readings(cfg: Option<&crate::config::Config>, rows: Vec<Reading>) -> Vec<Reading> {
    rows.into_iter()
        .filter(|r| {
            !cfg.is_some_and(|c| crate::config::soil::legacy_mapped_key(c, &r.source_id, &r.key))
        })
        .collect()
}

async fn history(
    State(st): State<WeatherState>,
    Query(q): Query<HistoryQuery>,
) -> Json<WeatherHistory> {
    let store = SensorHistoryStore::new(st.db);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(source_id: &str, key: &str) -> Reading {
        Reading {
            epoch: 0,
            source_id: source_id.to_string(),
            key: key.to_string(),
            value: 1.0,
        }
    }

    /// A source's live readings leave out the retired shared zone channel,
    /// which no longer updates, and keep its per-probe channels and the rest.
    #[test]
    fn retired_zone_soil_channels_are_not_live_readings() {
        let mut cfg = crate::config::Config::default();
        cfg.sources.push(
            serde_json::from_value::<crate::config::SourceEntry>(serde_json::json!({
                "id": "mqtt", "kind": "mqtt", "config": {
                    "broker_host": "localhost", "subscriptions": [
                        {"topic": "soil/1", "field": "rh_pct", "zone_slug": "bed"},
                        {"topic": "air", "field": "air_temp_f"}
                    ]
                }
            }))
            .unwrap(),
        );
        let probe = crate::config::soil::probe_key("mqtt", &["soil/1", ""]);
        let rows = vec![
            reading("mqtt", "air_temp_f"),
            reading("mqtt", "soilmoisture_bed"),
            reading("mqtt", &probe),
        ];
        let keys: Vec<String> = current_readings(Some(&cfg), rows.clone())
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(keys, ["air_temp_f".to_string(), probe]);
        assert_eq!(current_readings(None, rows).len(), 3);
    }
}
