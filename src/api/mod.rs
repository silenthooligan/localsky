// HTTP API mounted at /api on the SSR server.
//
// /api/snapshot              — JSON of the current TempestStore snapshot.
// /api/stream                — Server-Sent Events feed; one event per
//                              tempest state mutation.
// /api/irrigation/snapshot   — JSON of the current IrrigationStore snapshot.
// /api/irrigation/stream     — SSE feed for irrigation state changes.

pub mod config;
pub mod forecast;
pub mod health;
pub mod ingest;
pub mod irrigation;
pub mod wizard;

use crate::forecast::ForecastStore;
use crate::ha::IrrigationStore;
use crate::llm::AdvisorState;
use crate::tempest::state::TempestStore;
use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        Json,
    },
    routing::get,
    Router,
};
use futures::stream::Stream;
use rusqlite::Connection;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

pub fn router(
    tempest: Arc<TempestStore>,
    irrigation: Arc<IrrigationStore>,
    forecast_store: Arc<ForecastStore>,
    advisor: AdvisorState,
    history: Option<Arc<Mutex<Connection>>>,
) -> Router {
    let tempest_routes = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/stream", get(stream))
        .with_state(tempest);

    tempest_routes
        .nest("/irrigation", irrigation::router(irrigation, advisor, history))
        .nest("/forecast", forecast::router(forecast_store))
}

async fn snapshot(
    State(store): State<Arc<TempestStore>>,
) -> Json<crate::tempest::state::Snapshot> {
    let s = store.snapshot();
    Json((*s).clone())
}

async fn stream(
    State(store): State<Arc<TempestStore>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = store.subscribe();
    let s = WatchStream::new(rx).map(|snap| {
        let payload = serde_json::to_string(&*snap).unwrap_or_else(|_| "{}".into());
        Ok(Event::default().event("snapshot").data(payload))
    });
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
