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
use tower_http::limit::RequestBodyLimitLayer;
use tracing::warn;

use crate::sources::{EcowittLocal, HttpWebhook};

#[derive(Clone)]
pub struct IngestState {
    pub ecowitt: Vec<Arc<EcowittLocal>>,
    pub webhooks: Vec<Arc<HttpWebhook>>,
    /// When set, received readings are recorded so the source shows live
    /// freshness + values on the Sensors page (validation), independent of
    /// the not-yet-consumed event bus.
    pub sensor_history: Option<crate::persistence::SensorHistoryStore>,
}

/// Upper bound on an ingest POST (LS-API-09). /ingest/* is the only
/// always-public write surface (weather hardware + webhooks cannot
/// authenticate; per-source secrets are the gate), so an anonymous caller
/// can reach it. An Ecowitt form or a webhook JSON reading set is a few
/// KiB; 256 KiB is a generous ceiling that still refuses a memory-
/// exhaustion body from an unauthenticated client.
const INGEST_BODY_LIMIT: usize = 256 * 1024;

pub fn router(state: IngestState) -> Router {
    Router::new()
        .route("/ecowitt", post(ecowitt_handler))
        .route("/webhook/{id}", post(webhook_handler))
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(INGEST_BODY_LIMIT))
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
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "ecowitt source not configured",
        )
            .into_response();
    }
    for adapter in &state.ecowitt {
        adapter.handle_post(&form);
    }

    // Record every numeric field (incl. soilmoisture1..N) to sensor_history
    // keyed by its Ecowitt name, so the Sensors page can show this source as
    // live + display the actual values the gateway is posting. Fire-and-
    // forget so the gateway always gets its prompt 200.
    //
    // SC-08: gate this parallel write on the SAME shared-secret check the
    // adapter's handle_post applies. The history attribution uses the first
    // adapter's id, so verify the first adapter's secret here; if it does not
    // match (and a secret is configured) we skip the insert entirely, instead
    // of recording readings the adapter itself rejected. With no secret
    // configured `secret_matches` returns true, preserving open-by-default
    // ingest. Without this gate an attacker who knows only the source id could
    // POST forged readings that bypassed the secret on the event bus but still
    // landed in sensor_history through this second door.
    let first_accepted = state
        .ecowitt
        .first()
        .map(|src| src.secret_matches(&form))
        .unwrap_or(false);
    if let (Some(store), Some(src), true) = (
        state.sensor_history.as_ref(),
        state.ecowitt.first(),
        first_accepted,
    ) {
        let source_id = src.id().to_string();
        let epoch = chrono::Utc::now().timestamp();
        let readings: Vec<_> = form
            .iter()
            .filter_map(|(k, v)| {
                v.parse::<f64>()
                    .ok()
                    .map(|value| crate::persistence::sensor_history::Reading {
                        epoch,
                        source_id: source_id.clone(),
                        key: k.clone(),
                        value,
                    })
            })
            .collect();
        if !readings.is_empty() {
            let store = store.clone();
            tokio::spawn(async move {
                if let Err(e) = store.insert_many(readings).await {
                    warn!("ecowitt sensor_history record failed: {e}");
                }
            });
        }
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
    let provided_token = q.token.as_deref().or_else(|| {
        headers
            .get("x-localsky-token")
            .and_then(|v| v.to_str().ok())
    });
    let emitted = adapter.handle_post(&body, provided_token);
    if !emitted {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "no observation emitted (token or payload)",
        )
            .into_response();
    }
    StatusCode::OK.into_response()
}

// EcowittLocal + HttpWebhook are concrete types but the Arc<Self>
// access here needs `id()` from the WeatherSource trait scope.
use crate::ports::weather_source::WeatherSource;
