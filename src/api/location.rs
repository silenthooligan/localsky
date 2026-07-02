// GET /api/v1/location, the configured map center (lat/lon/zoom).
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
        .route("/timezone", get(timezone))
        .route("/elevation", get(elevation))
        .with_state(cfg_store)
}

#[derive(serde::Deserialize)]
struct LatLonQuery {
    lat: f64,
    lon: f64,
}

/// GET /api/v1/location/timezone?lat=..&lon=.. -> { timezone } via the
/// offline tzf dataset. The wizard's Location step autofills with it.
async fn timezone(
    axum::extract::Query(q): axum::extract::Query<LatLonQuery>,
) -> Json<serde_json::Value> {
    Json(json!({ "timezone": crate::timeutil::tz_name_for(q.lat, q.lon) }))
}

/// GET /api/v1/location/elevation?lat=..&lon=.. -> { elevation_m } via the
/// Open-Meteo elevation API. The wizard's Location step prefills the
/// (manually overridable) elevation field with it. The value is in meters,
/// matching the `deployment.location.elevation_m` config field.
///
/// On any upstream/parse failure this returns 502 with a trimmed category;
/// the client ignores the error and leaves the field at manual entry.
async fn elevation(
    axum::extract::Query(q): axum::extract::Query<LatLonQuery>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    let url = format!(
        "https://api.open-meteo.com/v1/elevation?latitude={}&longitude={}",
        q.lat, q.lon
    );
    let client = reqwest::Client::new();
    let res = client.get(&url).send().await;
    match res {
        Ok(r) => match r.json::<serde_json::Value>().await {
            // Open-Meteo returns {"elevation":[123.0]} (meters).
            Ok(v) => match v
                .get("elevation")
                .and_then(|e| e.as_array())
                .and_then(|a| a.first())
                .and_then(|m| m.as_f64())
            {
                Some(meters) => Json(json!({ "elevation_m": meters })).into_response(),
                None => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "elevation_parse_error" })),
                )
                    .into_response(),
            },
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "elevation_parse_error" })),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "elevation_transport_error",
                "detail": crate::net::reqwest_error_category(&e).to_string(),
            })),
        )
            .into_response(),
    }
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
