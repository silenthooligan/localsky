// /ingest/* routes. POST receivers for the sensor sources that work by
// device-pushed HTTP instead of LocalSky-pulled polls. Wires the
// Ecowitt local + HTTP webhook adapters constructed by
// Runtime::build_receiver_sources into the live Axum router.
//
// Mounted by main.rs when v2 is enabled. Always returns 200 on
// successful parse (per the Ecowitt + generic-webhook conventions)
// so a misconfigured downstream doesn't make a sensor's onboard
// retry-storm escalate.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Form, Router,
};
use serde::Deserialize;
use std::collections::HashMap;
use tracing::warn;

use crate::sources::{EcowittLocal, HttpWebhook};

#[derive(Clone)]
pub struct IngestState {
    pub ecowitt: Vec<Arc<EcowittLocal>>,
    pub webhooks: Vec<Arc<HttpWebhook>>,
}

pub fn router(state: IngestState) -> Router {
    Router::new()
        .route("/ecowitt", post(ecowitt_handler))
        .route("/webhook/{id}", post(webhook_handler))
        .with_state(state)
}

/// Ecowitt POST handler. The gateway sends form-encoded data; Axum's
/// Form extractor parses it. We tee the payload to every configured
/// EcowittLocal adapter; the adapter's optional shared_secret check
/// gates per-adapter ingestion.
async fn ecowitt_handler(
    State(state): State<IngestState>,
    Form(form): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    if state.ecowitt.is_empty() {
        warn!("ecowitt POST received but no EcowittLocal source is configured");
        return (StatusCode::SERVICE_UNAVAILABLE, "ecowitt source not configured").into_response();
    }
    for adapter in &state.ecowitt {
        adapter.handle_post(&form);
    }
    // Ecowitt gateways expect a 200 with empty body. Anything else
    // triggers their retry-storm.
    StatusCode::OK.into_response()
}

#[derive(Deserialize)]
struct WebhookQuery {
    token: Option<String>,
}

/// Generic webhook handler. Path `/webhook/<id>` picks which configured
/// HttpWebhook adapter receives this POST. Body is raw bytes.
async fn webhook_handler(
    State(state): State<IngestState>,
    Path(id): Path<String>,
    Query(q): Query<WebhookQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let Some(adapter) = state.webhooks.iter().find(|w| w.id() == id) else {
        return (StatusCode::NOT_FOUND, "webhook id not configured").into_response();
    };
    let provided_token = q
        .token
        .as_deref()
        .or_else(|| headers.get("x-localsky-token").and_then(|v| v.to_str().ok()));
    let emitted = adapter.handle_post(&body, provided_token);
    if !emitted {
        return (StatusCode::UNPROCESSABLE_ENTITY, "no observation emitted (token or payload)")
            .into_response();
    }
    StatusCode::OK.into_response()
}

// EcowittLocal + HttpWebhook are concrete types but the Arc<Self>
// access here needs `id()` from the WeatherSource trait scope.
use crate::ports::weather_source::WeatherSource;
