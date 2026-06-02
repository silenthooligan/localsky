// GET /api/v1/location — the configured map center (lat/lon/zoom).
//
// The radar renders its center from #radar-map data-* attrs, which are
// correct on a server-rendered load but fall back to a hardcoded default
// on client-side (SPA) navigation. radar.js fetches this on init and
// recenters, so the true location shows immediately however the page was
// reached. Honors deployment.location from config first (so Settings ->
// Location flows through), falling back to the WEATHER_APP_LAT/LON/ZOOM
// env vars.

use std::sync::Arc;

use axum::{extract::State, response::Json, routing::get, Router};
use serde_json::json;

use crate::config::FileConfigStore;
use crate::ports::config_store::ConfigStore;

pub fn router(cfg_store: Arc<FileConfigStore>) -> Router {
    Router::new()
        .route("/", get(location))
        .with_state(cfg_store)
}

async fn location(State(store): State<Arc<FileConfigStore>>) -> Json<serde_json::Value> {
    let from_cfg = store
        .load()
        .await
        .ok()
        .map(|c| (c.deployment.location.lat, c.deployment.location.lon))
        .filter(|(lat, lon)| !(*lat == 0.0 && *lon == 0.0));

    let (lat, lon) = from_cfg.unwrap_or_else(|| {
        let lat = std::env::var("WEATHER_APP_LAT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(40.0);
        let lon = std::env::var("WEATHER_APP_LON")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(-75.0);
        (lat, lon)
    });
    let zoom: u32 = std::env::var("WEATHER_APP_ZOOM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);

    Json(json!({ "lat": lat, "lon": lon, "zoom": zoom }))
}
