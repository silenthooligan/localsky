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

/// The resolved map view: real coordinates when the install has any, an
/// honest continent-scale default when it has none.
pub struct MapCenter {
    pub lat: f64,
    pub lon: f64,
    pub zoom: u32,
    /// False when neither the config nor the legacy env vars carry a
    /// location: the center below is a PLACEHOLDER view, and the UI must
    /// say so instead of rendering it as the user's surroundings.
    pub located: bool,
}

/// Resolve the map center once for every consumer (the radar panel's SSR
/// attributes and GET /api/v1/location): config location first, then the
/// legacy WEATHER_APP_LAT/LON env vars (an explicit operator choice, so it
/// counts as located), else a continental-US overview.
///
/// The old fallback was (40.0, -75.0) at zoom 8: a neighborhood-scale view
/// of the Delaware Valley, station marker included, on every install with no
/// location. A beta tester in another state read it as "my location updates
/// are not being applied" (issue #7 thread). An unlocated install now gets
/// the CONUS centroid at zoom 4, which reads as a map of the country, and
/// `located=false` so the client suppresses the marker and says what is
/// going on.
pub fn resolve_map_center(cfg_loc: Option<(f64, f64)>) -> MapCenter {
    let env_loc = || {
        let lat: f64 = std::env::var("WEATHER_APP_LAT").ok()?.parse().ok()?;
        let lon: f64 = std::env::var("WEATHER_APP_LON").ok()?.parse().ok()?;
        Some((lat, lon))
    };
    let located = cfg_loc.filter(|(lat, lon)| !(*lat == 0.0 && *lon == 0.0));
    let (lat, lon, located) = match located.or_else(env_loc) {
        Some((lat, lon)) => (lat, lon, true),
        None => (39.8, -98.6, false),
    };
    let zoom: u32 = std::env::var("WEATHER_APP_ZOOM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if located { 8 } else { 4 });
    MapCenter {
        lat,
        lon,
        zoom,
        located,
    }
}

async fn location(State(store): State<Arc<FileConfigStore>>) -> Json<serde_json::Value> {
    let from_cfg = store
        .load()
        .await
        .ok()
        .map(|c| (c.deployment.location.lat, c.deployment.location.lon));
    let c = resolve_map_center(from_cfg);
    Json(json!({ "lat": c.lat, "lon": c.lon, "zoom": c.zoom, "located": c.located }))
}

#[cfg(test)]
mod center_tests {
    use super::resolve_map_center;

    #[test]
    fn unlocated_installs_get_an_honest_continental_view() {
        // No config location (or the 0,0 sentinel): continent-scale center,
        // flagged unlocated so the client can say so. (Assumes the test env
        // does not set the legacy WEATHER_APP_* vars, which nothing in the
        // suite does.)
        for loc in [None, Some((0.0, 0.0))] {
            let c = resolve_map_center(loc);
            assert!(!c.located);
            assert_eq!((c.lat, c.lon, c.zoom), (39.8, -98.6, 4));
        }
        // A real location is itself, neighborhood zoom, located.
        let c = resolve_map_center(Some((29.9, -81.3)));
        assert!(c.located);
        assert_eq!((c.lat, c.lon, c.zoom), (29.9, -81.3, 8));
    }
}
