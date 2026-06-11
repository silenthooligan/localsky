// Devices API. Exposes the DeviceRegistry as the MA-style device view:
// every gateway / hub / controller / cloud account LocalSky knows about,
// each with the sensors or zones it provides. Read-only in Phase D; native
// discovery (E) and HA import (F) enrich the same shape.
//
// Mounted at /api/v1/devices by api::router.

use crate::devices::{Device, DeviceRegistry};
use crate::discovery::{discover_ecowitt, DiscoveredGateway};
use axum::{extract::State, response::Json, routing::get, Router};
use std::time::Duration;

pub fn router(registry: DeviceRegistry) -> Router {
    Router::new()
        .route("/", get(list))
        .route("/discover", get(discover))
        .with_state(registry)
}

/// GET /api/v1/devices, every known device, sorted by id.
async fn list(State(registry): State<DeviceRegistry>) -> Json<Vec<Device>> {
    Json(registry.all())
}

/// GET /api/v1/devices/discover, broadcast LAN discovery (Ecowitt for now)
/// and return the gateways found, each with a suggested host the UI's "Add"
/// button pre-fills into an ecowitt_gw_poll source. ~3s while it listens.
async fn discover() -> Json<Vec<DiscoveredGateway>> {
    Json(discover_ecowitt(Duration::from_secs(3)).await)
}
