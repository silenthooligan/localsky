// HTTP API mounted at /api and /api/v1 on the SSR server.
//
// /api/v1 is the canonical, stable contract. The bare /api/* aliases are
// kept for backwards-compat with v0.1 clients and the in-app radar/dashboard
// fetches; new clients (e.g. the HACS integration) should use /api/v1/*.
//
// Endpoints (under /api/v1):
//   GET  /info                       service + API version
//   GET  /snapshot                   current Tempest snapshot
//   GET  /stream                     SSE feed of Tempest snapshots
//   GET  /irrigation/snapshot        current irrigation snapshot
//   GET  /irrigation/stream          SSE feed of irrigation snapshots
//   GET  /irrigation/history         365-day SQLite history
//   POST /irrigation/action          run/stop/pause/skip a zone
//   GET  /forecast/snapshot          current Open-Meteo forecast snapshot
//   GET  /forecast/stream            SSE feed of forecast snapshots
//   GET  /weather/history            observed-weather series (24h sparklines)
//   GET  /llm/explanation            LLM advisor verdict-explanation
//   GET  /llm/anomalies              LLM advisor anomaly summary
//   GET  /config                     read current localsky.toml (secrets redacted)
//   PUT  /config                     write localsky.toml
//   POST /config/rollback?to=<v>     restore a prior config snapshot
//   GET  /config/schema              JSON Schema for tooling
//   GET  /wizard/draft               first-run wizard draft state
//   PUT  /wizard/draft               update draft
//   POST /wizard/apply               write the draft as the live config
//   GET  /health                     per-source freshness + controller summary
//   GET  /sensors/manifest           declarative entity inventory for HACS
//   GET  /sources/openmeteo/models   Open-Meteo forecast model catalog
//   GET  /radar/windgrid             U/V wind grid for the map's velocity layer
//                                    (wired in boot::api: it needs the config
//                                    store for the model + its own cache)
//   GET  /radar/tropical             all-basin tropical cyclone GeoJSON
//                                    (NHC/CPHC + JMA + JTWC normalized
//                                    server-side; wired in boot::api with
//                                    its own 10-minute cache)

pub mod auth;
pub mod backup;
pub mod config;
pub mod devices;
pub mod diagnostics;
pub mod error_boundary;
pub mod forecast;
pub mod health;
pub mod info;
pub mod ingest;
pub mod irrigation;
pub mod location;
pub mod manifest;
pub mod photos;
pub mod precip;
pub mod sensors;
pub mod sources;
pub mod system;
pub mod tropical;
pub mod weather;
pub mod windgrid;
pub mod wizard;

#[cfg(test)]
mod snapshot_tests;

use crate::forecast::ForecastStore;
use crate::llm::AdvisorState;
use crate::refresher::{IrrigationStore, SnapshotSource};
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

/// Everything the core API routers read, built once by the boot and
/// mounted at both `/api` and `/api/v1` from one `router()` call.
#[derive(Clone)]
pub struct ApiState {
    pub tempest: Arc<TempestStore>,
    pub irrigation: Arc<IrrigationStore>,
    pub forecast: Arc<ForecastStore>,
    pub advisor: AdvisorState,
    pub history: Option<Arc<Mutex<Connection>>>,
    /// Routes the POST /action vacation-pause + one-day override to local
    /// state (native) vs HA helpers (HA).
    pub source: SnapshotSource,
    /// Device topology (gateways/controllers + their sensors/zones) for
    /// the MA-style /devices view.
    pub devices: crate::devices::DeviceRegistry,
    /// HA controller entity prefix for the POST /action handler.
    pub sprinkler_prefix: String,
    /// Config store for the manifest's capability gates and for POST
    /// /action's threshold writes.
    pub cfg_store: Arc<crate::config::FileConfigStore>,
    /// The live watering policy: POST /action reads the adoption markers
    /// off the same handle the refresher reads them from.
    pub watering_policy: Arc<arc_swap::ArcSwap<crate::refresher::WateringPolicy>>,
    /// Manual zone dispatch plumbing; None in demo mode, where the zone
    /// actions answer 503.
    pub dispatch: Option<irrigation::DispatchState>,
    /// The unified sensor inventory's zone bindings and flow meters;
    /// None in demo mode.
    pub inventory: Option<sensors::InventoryState>,
    /// Tuning report generation; None without a history database.
    pub tuning: Option<Arc<crate::tuning::TuningHandles>>,
}

pub fn router(st: ApiState) -> Router {
    let ApiState {
        tempest,
        irrigation,
        forecast: forecast_store,
        advisor,
        history,
        source,
        devices,
        sprinkler_prefix,
        cfg_store,
        watering_policy,
        dispatch,
        inventory,
        tuning,
    } = st;
    let tempest_routes = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/stream", get(stream))
        .with_state(tempest);

    // Manifest needs the live irrigation snapshot to enumerate per-zone
    // entities, so it borrows the IrrigationStore Arc before we hand it
    // off to the irrigation routes' nested router.
    let manifest_router = manifest::router(irrigation.clone(), cfg_store.clone());

    let mut router = tempest_routes
        .nest(
            "/irrigation",
            irrigation::router(
                irrigation,
                advisor,
                history.clone(),
                source,
                sprinkler_prefix,
                cfg_store,
                watering_policy,
                dispatch,
                tuning,
            ),
        )
        .nest(
            "/forecast",
            forecast::router(forecast_store, history.clone()),
        )
        .nest("/devices", devices::router(devices, history.clone()))
        .merge(info::router())
        .merge(sources::router())
        .merge(manifest_router);

    // Observed-weather history (sparklines), only when persistence is mounted.
    if let Some(h) = history {
        router = router
            .nest("/weather", weather::router(h.clone()))
            .nest("/sensors", sensors::router(h, inventory));
    }
    router
}

async fn snapshot(State(store): State<Arc<TempestStore>>) -> Json<crate::tempest::state::Snapshot> {
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
