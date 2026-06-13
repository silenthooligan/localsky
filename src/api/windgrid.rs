// GET /api/v1/radar/windgrid?bbox=minLon,minLat,maxLon,maxLat
//
// Wind field for the radar map's leaflet-velocity layer (the "wind"
// entry in radar_catalog). Returns the grib2json-style payload the
// plugin parses: a JSON array of two records, U then V, each with a
// GRIB-style header (parameterCategory 2, parameterNumber 2 = U /
// 3 = V, grid geometry) and a flat row-major data array of m/s
// components starting at the NW corner (la1 north, lo1 west).
//
// Built server-side from ONE batched Open-Meteo call: comma-separated
// latitude/longitude lists return one result object per point
// (verified 2026-06-12 up to 484 points; 414 URI-too-long past ~8 KB,
// and each point burns one free-tier call so a rapid burst can 429).
// The grid is therefore clamped to 8x8 = 64 points (about 1 KB of
// 2-decimal coords) and responses are cached ~30 minutes keyed on the
// rounded grid + model so map panning does not hammer upstream.
//
// Open-Meteo wind is km/h with meteorological direction (where the
// wind comes FROM); the conversion to U/V components in m/s is
//   u = -speed * sin(dir), v = -speed * cos(dir)
// using the CURRENT hour's values. The configured open_meteo model is
// honored (`&models=` appended for non-default, same rule as the
// forecast refresher); note the two hard-regional models return HTTP
// 400 outside their domain, which surfaces here as a 502 the frontend
// degrades on silently.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::FileConfigStore;
use crate::forecast::model_catalog::DEFAULT_MODEL;
use crate::forecast::refresher::configured_open_meteo_model;
use crate::ports::config_store::ConfigStore;

/// Grid points per axis: 8x8 = 64 upstream locations, comfortably
/// inside both verified Open-Meteo ceilings (URL length and free-tier
/// quota burn) while dense enough for a readable particle field.
const GRID_N: usize = 8;
/// Cache TTL. Open-Meteo's own model runs update hourly at best; 30
/// minutes matches the forecast refresher cadence.
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
/// Cache entry ceiling; beyond this, expired entries are pruned and,
/// if every entry is still live, the cache is cleared outright. Keeps
/// a bbox-scanning client from growing the map unboundedly.
const CACHE_MAX: usize = 64;

type CacheKey = (i64, i64, i64, i64, String);
type Cache = Mutex<HashMap<CacheKey, (Instant, Arc<serde_json::Value>)>>;

#[derive(Clone)]
pub struct WindGridState {
    client: reqwest::Client,
    cfg_store: Option<Arc<FileConfigStore>>,
    cache: Arc<Cache>,
    /// Serializes upstream fetches (simple lock, not per-key
    /// single-flight: one map, one upstream, contention is rare).
    /// Concurrent cold-cache requests for the same grid would each
    /// burn 64 free-tier calls without it; waiters re-check the cache
    /// after acquiring and find the winner's entry instead.
    fetch_lock: Arc<tokio::sync::Mutex<()>>,
}

impl WindGridState {
    pub fn new(cfg_store: Option<Arc<FileConfigStore>>) -> Self {
        // Same HTTP discipline as the forecast refresher: bounded
        // timeout, identifying user agent. 15s because the batched
        // 64-point call is heavier than the single-point forecast.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("localsky/windgrid")
            .build()
            .unwrap_or_default();
        Self {
            client,
            cfg_store,
            cache: Arc::new(Mutex::new(HashMap::new())),
            fetch_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

/// Fresh-entry cache lookup, shared by the fast path and the
/// post-lock re-check. The std mutex is only ever held for the map
/// access itself, never across an await.
fn cache_get(cache: &Cache, key: &CacheKey) -> Option<Arc<serde_json::Value>> {
    let cache = cache.lock().ok()?;
    let (at, body) = cache.get(key)?;
    (at.elapsed() < CACHE_TTL).then(|| body.clone())
}

pub fn router(state: WindGridState) -> Router {
    Router::new()
        .route("/windgrid", get(windgrid))
        .with_state(state)
}

#[derive(Deserialize)]
struct WindGridQuery {
    bbox: String,
}

/// Parsed + sanity-clamped bbox. Corners are rounded to 2 decimals
/// (about 1.1 km), which keeps the upstream URL short and makes the
/// cache key stable across sub-rounding map pans.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bbox {
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 6-decimal rounding for the header's dx/dy: keeps the JSON free of
/// 17-digit float-division noise. The worst-case drift this introduces
/// across the whole grid is 7 * 5e-7 degrees, far below the 2-decimal
/// coordinate resolution the points themselves are fetched at.
fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

fn parse_bbox(s: &str) -> Result<Bbox, String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err("bbox must be minLon,minLat,maxLon,maxLat".into());
    }
    let mut v = [0f64; 4];
    for (i, p) in parts.iter().enumerate() {
        let n: f64 = p
            .trim()
            .parse()
            .map_err(|_| format!("bbox component '{}' is not a number", p.trim()))?;
        if !n.is_finite() {
            return Err(format!("bbox component '{}' is not finite", p.trim()));
        }
        v[i] = n;
    }
    // Sanity-clamp to plausible degrees. Latitude stops at the web
    // mercator limit Leaflet itself renders to, so a fully zoomed-out
    // map still produces a valid grid instead of polar nonsense.
    let min_lon = round2(v[0].clamp(-180.0, 180.0));
    let min_lat = round2(v[1].clamp(-85.0, 85.0));
    let max_lon = round2(v[2].clamp(-180.0, 180.0));
    let max_lat = round2(v[3].clamp(-85.0, 85.0));
    // Degenerate after clamp+rounding (inverted, empty, or thinner
    // than the 2-decimal coordinate resolution) is a client error.
    if max_lon - min_lon < 0.01 || max_lat - min_lat < 0.01 {
        return Err("bbox is empty or inverted after clamping".into());
    }
    Ok(Bbox {
        min_lon,
        min_lat,
        max_lon,
        max_lat,
    })
}

/// Grid geometry in the GRIB header convention leaflet-velocity
/// expects: la1/lo1 = NW corner, la2/lo2 = SE corner, dx/dy = positive
/// cell size in degrees, data row-major from the NW corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Grid {
    pub(crate) la1: f64,
    pub(crate) lo1: f64,
    pub(crate) la2: f64,
    pub(crate) lo2: f64,
    pub(crate) dx: f64,
    pub(crate) dy: f64,
}

impl Grid {
    fn from_bbox(b: &Bbox) -> Self {
        Self {
            la1: b.max_lat,
            lo1: b.min_lon,
            la2: b.min_lat,
            lo2: b.max_lon,
            dx: round6((b.max_lon - b.min_lon) / (GRID_N - 1) as f64),
            dy: round6((b.max_lat - b.min_lat) / (GRID_N - 1) as f64),
        }
    }

    /// Comma-separated latitude/longitude lists in row-major order
    /// from the NW corner: rows north to south, columns west to east.
    /// The batched Open-Meteo response preserves this order, so the
    /// per-point results map 1:1 onto the data arrays.
    fn coord_csv(&self) -> (String, String) {
        let mut lats = Vec::with_capacity(GRID_N * GRID_N);
        let mut lons = Vec::with_capacity(GRID_N * GRID_N);
        for row in 0..GRID_N {
            let lat = self.la1 - self.dy * row as f64;
            for col in 0..GRID_N {
                lats.push(format!("{:.2}", lat));
                lons.push(format!("{:.2}", self.lo1 + self.dx * col as f64));
            }
        }
        (lats.join(","), lons.join(","))
    }

    fn open_meteo_url(&self, model: &str) -> String {
        let (lat_csv, lon_csv) = self.coord_csv();
        // Times come back in GMT (the upstream default when no
        // timezone parameter is sent), which is what the current-hour
        // index math assumes. Wind stays in the default km/h and is
        // converted to m/s components here.
        let mut url = format!(
            "https://api.open-meteo.com/v1/forecast?\
             latitude={lat_csv}&longitude={lon_csv}&\
             hourly=wind_speed_10m,wind_direction_10m&\
             forecast_days=1"
        );
        if model != DEFAULT_MODEL {
            url.push_str("&models=");
            url.push_str(model);
        }
        url
    }

    /// Cache key: corners scaled to 2-decimal integers + the model, so
    /// every pan landing on the same rounded grid hits the same entry.
    fn cache_key(&self, model: &str) -> CacheKey {
        (
            (self.la1 * 100.0).round() as i64,
            (self.lo1 * 100.0).round() as i64,
            (self.la2 * 100.0).round() as i64,
            (self.lo2 * 100.0).round() as i64,
            model.to_string(),
        )
    }
}

/// Met convention to vector components: direction is where the wind
/// comes FROM, so a north wind (0 deg) blows southward, u = 0 and
/// v = -speed. Input km/h (the Open-Meteo default), output m/s.
fn wind_to_uv(speed_kmh: f64, dir_deg: f64) -> (f64, f64) {
    let ms = speed_kmh / 3.6;
    let rad = dir_deg.to_radians();
    (-ms * rad.sin(), -ms * rad.cos())
}

/// Round a component to 3 decimals (about 1 mm/s); keeps the 128-value
/// payload free of 17-digit float noise.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Pick the index of the current UTC hour in an Open-Meteo hourly time
/// array ("2026-06-12T14:00" strings, GMT). Exact match first, then
/// the last entry not after now (ISO strings sort lexically), then 0.
fn current_hour_index(times: &[String], now_iso: &str) -> usize {
    times
        .iter()
        .position(|t| t.as_str() == now_iso)
        .or_else(|| times.iter().rposition(|t| t.as_str() <= now_iso))
        .unwrap_or(0)
}

// Batched Open-Meteo response: a top-level JSON array of per-location
// results (elements past the first also carry a location_id field,
// which we don't need: order matches the request lists).

#[derive(Deserialize)]
struct RawPoint {
    hourly: RawWind,
}

#[derive(Deserialize)]
struct RawWind {
    time: Vec<String>,
    wind_speed_10m: Vec<Option<f64>>,
    wind_direction_10m: Vec<Option<f64>>,
}

/// One grib2json-style record. leaflet-velocity keys off
/// parameterCategory + parameterNumber (2,2 = U; 2,3 = V) and reads
/// the grid geometry from the header; serde renames everything to the
/// camelCase keys the plugin parses.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WindHeader {
    pub(crate) parameter_category: u32,
    pub(crate) parameter_number: u32,
    pub(crate) parameter_unit: &'static str,
    pub(crate) parameter_number_name: &'static str,
    pub(crate) nx: usize,
    pub(crate) ny: usize,
    pub(crate) lo1: f64,
    pub(crate) la1: f64,
    pub(crate) lo2: f64,
    pub(crate) la2: f64,
    pub(crate) dx: f64,
    pub(crate) dy: f64,
    pub(crate) ref_time: String,
    pub(crate) forecast_time: u32,
}

#[derive(Serialize)]
pub(crate) struct WindRecord {
    pub(crate) header: WindHeader,
    pub(crate) data: Vec<f64>,
}

/// Assemble the two-record [U, V] payload for a grid.
pub(crate) fn make_records(
    grid: &Grid,
    ref_time: &str,
    u: Vec<f64>,
    v: Vec<f64>,
) -> Vec<WindRecord> {
    let header = |number: u32, name: &'static str| WindHeader {
        parameter_category: 2,
        parameter_number: number,
        parameter_unit: "m.s-1",
        parameter_number_name: name,
        nx: GRID_N,
        ny: GRID_N,
        lo1: grid.lo1,
        la1: grid.la1,
        lo2: grid.lo2,
        la2: grid.la2,
        dx: grid.dx,
        dy: grid.dy,
        ref_time: ref_time.to_string(),
        forecast_time: 0,
    };
    vec![
        WindRecord {
            header: header(2, "eastward_wind"),
            data: u,
        },
        WindRecord {
            header: header(3, "northward_wind"),
            data: v,
        },
    ]
}

/// Deterministic grid for the cross-module shape-lock snapshot test
/// (snapshot_tests::radar_windgrid_v1_shape).
#[cfg(test)]
pub(crate) fn test_fixture_grid() -> Grid {
    Grid::from_bbox(&parse_bbox("-82.0,28.3,-81.3,29.0").expect("fixture bbox"))
}

fn error_json(status: StatusCode, msg: String) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

async fn windgrid(State(st): State<WindGridState>, Query(q): Query<WindGridQuery>) -> Response {
    let bbox = match parse_bbox(&q.bbox) {
        Ok(b) => b,
        Err(msg) => return error_json(StatusCode::BAD_REQUEST, msg),
    };
    let grid = Grid::from_bbox(&bbox);

    // Honor the configured open_meteo model, same resolution rule as
    // the forecast refresher (first open_meteo source entry).
    let model = match &st.cfg_store {
        Some(cs) => match cs.load().await {
            Ok(cfg) => configured_open_meteo_model(&cfg),
            Err(_) => DEFAULT_MODEL.to_string(),
        },
        None => DEFAULT_MODEL.to_string(),
    };

    let key = grid.cache_key(&model);
    if let Some(body) = cache_get(&st.cache, &key) {
        return Json((*body).clone()).into_response();
    }

    // Cold cache: take the fetch lock, then re-check. A concurrent
    // request for the same grid that lost the race finds the winner's
    // cached body here instead of stampeding Open-Meteo.
    let _fetch_guard = st.fetch_lock.lock().await;
    if let Some(body) = cache_get(&st.cache, &key) {
        return Json((*body).clone()).into_response();
    }

    let url = grid.open_meteo_url(&model);
    let points: Vec<RawPoint> = match fetch_points(&st.client, &url).await {
        Ok(p) => p,
        Err(msg) => {
            tracing::warn!(error = %msg, "windgrid upstream fetch failed");
            return error_json(StatusCode::BAD_GATEWAY, msg);
        }
    };
    if points.len() != GRID_N * GRID_N {
        return error_json(
            StatusCode::BAD_GATEWAY,
            format!(
                "open-meteo returned {} points, expected {}",
                points.len(),
                GRID_N * GRID_N
            ),
        );
    }

    let now_iso = chrono::Utc::now().format("%Y-%m-%dT%H:00").to_string();
    let times = &points[0].hourly.time;
    let idx = current_hour_index(times, &now_iso);
    let ref_time = times
        .get(idx)
        .map(|t| format!("{t}:00Z"))
        .unwrap_or_else(|| format!("{now_iso}:00Z"));

    let mut u = Vec::with_capacity(points.len());
    let mut v = Vec::with_capacity(points.len());
    for p in &points {
        // Nulls (a model hole at one grid point) become calm air
        // rather than failing the whole field.
        let speed = p.hourly.wind_speed_10m.get(idx).copied().flatten();
        let dir = p.hourly.wind_direction_10m.get(idx).copied().flatten();
        let (uu, vv) = match (speed, dir) {
            (Some(s), Some(d)) => wind_to_uv(s, d),
            _ => (0.0, 0.0),
        };
        u.push(round3(uu));
        v.push(round3(vv));
    }

    let body = match serde_json::to_value(make_records(&grid, &ref_time, u, v)) {
        Ok(b) => b,
        Err(e) => {
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("windgrid serialize: {e}"),
            )
        }
    };

    if let Ok(mut cache) = st.cache.lock() {
        if cache.len() >= CACHE_MAX {
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            if cache.len() >= CACHE_MAX {
                cache.clear();
            }
        }
        cache.insert(key, (Instant::now(), Arc::new(body.clone())));
    }
    Json(body).into_response()
}

async fn fetch_points(client: &reqwest::Client, url: &str) -> Result<Vec<RawPoint>, String> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET open-meteo windgrid: {e}"))?
        .error_for_status()
        .map_err(|e| format!("open-meteo non-2xx: {e}"))?
        .json()
        .await
        .map_err(|e| format!("decode open-meteo windgrid json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn north_wind_blows_southward() {
        // 10 km/h FROM the north: no eastward component, v = -2.778 m/s.
        let (u, v) = wind_to_uv(10.0, 0.0);
        assert!(close(u, 0.0), "u was {u}");
        assert!(close(v, -2.778), "v was {v}");
    }

    #[test]
    fn east_wind_blows_westward() {
        // 18 km/h FROM the east: u = -5 m/s, no northward component.
        let (u, v) = wind_to_uv(18.0, 90.0);
        assert!(close(u, -5.0), "u was {u}");
        assert!(close(v, 0.0), "v was {v}");
    }

    #[test]
    fn south_and_west_winds_flip_sign() {
        let (u, v) = wind_to_uv(36.0, 180.0); // from the south
        assert!(close(u, 0.0) && close(v, 10.0), "got ({u}, {v})");
        let (u, v) = wind_to_uv(36.0, 270.0); // from the west
        assert!(close(u, 10.0) && close(v, 0.0), "got ({u}, {v})");
    }

    #[test]
    fn bbox_parses_and_rounds() {
        let b = parse_bbox("-82.004,28.296,-81.3,29.0").unwrap();
        assert_eq!(
            b,
            Bbox {
                min_lon: -82.0,
                min_lat: 28.3,
                max_lon: -81.3,
                max_lat: 29.0,
            }
        );
    }

    #[test]
    fn bbox_clamps_to_plausible_degrees() {
        // A zoomed-out world map hands Leaflet's unwrapped bounds in;
        // clamp instead of rejecting.
        let b = parse_bbox("-400,-95,400,95").unwrap();
        assert_eq!(
            b,
            Bbox {
                min_lon: -180.0,
                min_lat: -85.0,
                max_lon: 180.0,
                max_lat: 85.0,
            }
        );
    }

    #[test]
    fn malformed_bboxes_are_rejected() {
        for bad in [
            "",
            "1,2,3",
            "1,2,3,4,5",
            "a,2,3,4",
            "NaN,2,3,4",
            "inf,2,3,4",
            // Inverted and empty extents.
            "-81.3,28.3,-82.0,29.0",
            "-82.0,29.0,-81.3,28.3",
            "-81.4,28.5,-81.4,28.5",
            // Thinner than the 2-decimal rounding resolution.
            "-81.401,28.5,-81.399,29.0",
        ] {
            assert!(parse_bbox(bad).is_err(), "'{bad}' should be rejected");
        }
    }

    #[test]
    fn grid_geometry_is_consistent_with_the_bbox() {
        let b = parse_bbox("-82.0,28.3,-81.3,29.0").unwrap();
        let g = Grid::from_bbox(&b);
        // NW corner / SE corner.
        assert_eq!((g.la1, g.lo1), (29.0, -82.0));
        assert_eq!((g.la2, g.lo2), (28.3, -81.3));
        // 8 points per axis -> 7 cells spanning the full extent.
        assert!(close(g.dx, 0.1) && close(g.dy, 0.1), "({}, {})", g.dx, g.dy);
        assert!(close(g.la1 - g.dy * (GRID_N - 1) as f64, g.la2));
        assert!(close(g.lo1 + g.dx * (GRID_N - 1) as f64, g.lo2));
    }

    #[test]
    fn coord_csv_is_row_major_from_nw() {
        let b = parse_bbox("-82.0,28.3,-81.3,29.0").unwrap();
        let g = Grid::from_bbox(&b);
        let (lats, lons) = g.coord_csv();
        let lats: Vec<&str> = lats.split(',').collect();
        let lons: Vec<&str> = lons.split(',').collect();
        assert_eq!(lats.len(), 64);
        assert_eq!(lons.len(), 64);
        // First point NW, end of first row NE, last point SE.
        assert_eq!((lats[0], lons[0]), ("29.00", "-82.00"));
        assert_eq!((lats[7], lons[7]), ("29.00", "-81.30"));
        assert_eq!((lats[63], lons[63]), ("28.30", "-81.30"));
    }

    #[test]
    fn url_carries_models_param_only_when_pinned() {
        let g = Grid::from_bbox(&parse_bbox("-82.0,28.3,-81.3,29.0").unwrap());
        let plain = g.open_meteo_url("best_match");
        assert!(!plain.contains("models="));
        assert!(plain.contains("hourly=wind_speed_10m,wind_direction_10m"));
        assert!(plain.contains("forecast_days=1"));
        let pinned = g.open_meteo_url("gfs_seamless");
        assert_eq!(pinned, format!("{plain}&models=gfs_seamless"));
    }

    #[test]
    fn record_headers_carry_grib_identity_and_geometry() {
        let g = Grid::from_bbox(&parse_bbox("-82.0,28.3,-81.3,29.0").unwrap());
        let recs = make_records(&g, "2026-06-12T14:00:00Z", vec![0.0; 64], vec![0.0; 64]);
        assert_eq!(recs.len(), 2);
        let (u, v) = (&recs[0], &recs[1]);
        for r in [u, v] {
            assert_eq!(r.header.parameter_category, 2);
            assert_eq!((r.header.nx, r.header.ny), (GRID_N, GRID_N));
            assert_eq!(r.data.len(), GRID_N * GRID_N);
            assert!(r.header.la1 > r.header.la2, "la1 must be the north edge");
            assert!(r.header.lo1 < r.header.lo2, "lo1 must be the west edge");
            assert!(r.header.dx > 0.0 && r.header.dy > 0.0);
            assert_eq!(r.header.ref_time, "2026-06-12T14:00:00Z");
        }
        assert_eq!(u.header.parameter_number, 2);
        assert_eq!(v.header.parameter_number, 3);
    }

    #[test]
    fn current_hour_index_matches_then_falls_back() {
        let times: Vec<String> = (0..4).map(|h| format!("2026-06-12T{h:02}:00")).collect();
        assert_eq!(current_hour_index(&times, "2026-06-12T02:00"), 2);
        // Past the array (clock just ticked into the next day): last.
        assert_eq!(current_hour_index(&times, "2026-06-13T00:00"), 3);
        // Before the array (clock skew): first.
        assert_eq!(current_hour_index(&times, "2026-06-11T23:00"), 0);
    }

    #[test]
    fn cache_key_is_stable_across_subrounding_pans() {
        let a = Grid::from_bbox(&parse_bbox("-82.001,28.299,-81.302,29.004").unwrap());
        let b = Grid::from_bbox(&parse_bbox("-81.996,28.301,-81.297,28.996").unwrap());
        assert_eq!(a.cache_key("best_match"), b.cache_key("best_match"));
        assert_ne!(a.cache_key("best_match"), a.cache_key("gfs_seamless"));
    }
}
