// Forecast API: snapshot + SSE stream for the 7-day daily +
// 48-hour hourly Open-Meteo feed. Same shape as the tempest +
// irrigation APIs.

use crate::forecast::ForecastStore;
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
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio_stream::wrappers::WatchStream;
use tokio_stream::StreamExt;

pub fn router(store: Arc<ForecastStore>) -> Router {
    Router::new()
        .route("/snapshot", get(snapshot))
        .route("/stream", get(stream))
        .with_state(store)
}

async fn snapshot(
    State(store): State<Arc<ForecastStore>>,
) -> Json<crate::forecast::snapshot::ForecastSnapshot> {
    let s = store.snapshot();
    Json((*s).clone())
}

async fn stream(
    State(store): State<Arc<ForecastStore>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = store.subscribe();
    let s = WatchStream::new(rx).map(|snap| {
        let payload = serde_json::to_string(&*snap).unwrap_or_else(|_| "{}".into());
        Ok(Event::default().event("snapshot").data(payload))
    });
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}
