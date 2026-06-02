// Forecast API: snapshot + SSE stream for the 7-day daily +
// 48-hour hourly Open-Meteo feed, plus the learned per-month forecast
// bias multiplier (when enough observations are recorded).

use crate::engine::forecast_bias::{BiasModel, DEFAULT_WINDOW_DAYS, MIN_OBSERVATIONS};
use crate::forecast::ForecastStore;
use crate::persistence::ForecastObservationsStore;
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
    routing::get,
    Router,
};
use chrono::{Datelike, Local};
use futures::stream::Stream;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::json;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
struct ForecastApiState {
    store: Arc<ForecastStore>,
    observations: Option<ForecastObservationsStore>,
}

pub fn router(store: Arc<ForecastStore>, db: Option<Arc<Mutex<Connection>>>) -> Router {
    let observations = db.map(ForecastObservationsStore::new);
    let state = ForecastApiState {
        store,
        observations,
    };
    Router::new()
        .route("/snapshot", get(snapshot))
        .route("/stream", get(stream))
        .route("/bias", get(bias))
        .with_state(state)
}

async fn snapshot(
    State(state): State<ForecastApiState>,
) -> Json<crate::forecast::snapshot::ForecastSnapshot> {
    let s = state.store.snapshot();
    Json((*s).clone())
}

async fn stream(
    State(state): State<ForecastApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.store.subscribe();
    let s = WatchStream::new(rx).map(|snap| {
        let payload = serde_json::to_string(&*snap).unwrap_or_else(|_| "{}".into());
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

    let today = Local::now().date_naive();
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
