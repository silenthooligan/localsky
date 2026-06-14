// GET /api/v1/radar/precip?bbox=minLon,minLat,maxLon,maxLat
//
// Short-range precipitation forecast grid for the radar map. Returns a
// stack of 8 future 15-minute frames (the next 2 hours), each a flat
// row-major array of 64 mm-per-15-min values starting at the NW corner,
// plus a `max_mm` scale hint. The frontend animates these as a
// nowcast-style precipitation overlay.
//
// Built server-side from ONE batched Open-Meteo call, modeled exactly on
// windgrid: comma-separated latitude/longitude lists return one result
// object per point in request order. The grid is clamped to 8x8 = 64
// points (about 1 KB of 2-decimal coords) and responses are cached ~15
// minutes keyed on the rounded grid + model so map panning does not
// hammer upstream.
//
// Open-Meteo `minutely_15=precipitation` gives millimetres per 15 min on
// a 15-minute time axis; we keep timezone=GMT (the upstream default when
// no timezone parameter is sent) so "now" math is a simple lexical
// compare. The configured open_meteo model is honored (`&models=`
// appended for non-default, same rule as the forecast refresher); the
// two hard-regional models return HTTP 400 outside their domain, which
// surfaces here as a 502 the frontend degrades on silently.

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

/// Grid points per axis: 8x8 = 64 upstream locations, comfortably inside
/// both verified Open-Meteo ceilings (URL length and free-tier quota
/// burn) while dense enough for a readable precipitation field. Matches
/// windgrid so the two overlays register cell-for-cell.
const GRID_N: usize = 8;
/// Number of future 15-minute frames returned (the next 2 hours).
const FRAMES: usize = 8;
/// Cache TTL. The 15-minute nowcast refreshes faster than the hourly
/// wind field, so this is shorter than windgrid's 30 minutes.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
/// Cache entry ceiling; beyond this, expired entries are pruned and, if
/// every entry is still live, the cache is cleared outright. Keeps a
/// bbox-scanning client from growing the map unboundedly.
const CACHE_MAX: usize = 64;

type CacheKey = (i64, i64, i64, i64, String);
type Cache = Mutex<HashMap<CacheKey, (Instant, Arc<serde_json::Value>)>>;

#[derive(Clone)]
pub struct PrecipState {
    client: reqwest::Client,
    cfg_store: Option<Arc<FileConfigStore>>,
    cache: Arc<Cache>,
    /// Serializes upstream fetches (simple lock, not per-key
    /// single-flight: one map, one upstream, contention is rare).
    /// Concurrent cold-cache requests for the same grid would each burn
    /// 64 free-tier calls without it; waiters re-check the cache after
    /// acquiring and find the winner's entry instead.
    fetch_lock: Arc<tokio::sync::Mutex<()>>,
}

impl PrecipState {
    pub fn new(cfg_store: Option<Arc<FileConfigStore>>) -> Self {
        // Same HTTP discipline as windgrid: bounded timeout, identifying
        // user agent. 15s because the batched 64-point call is heavier
        // than the single-point forecast.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("localsky/precip")
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

/// Fresh-entry cache lookup, shared by the fast path and the post-lock
/// re-check. The std mutex is only ever held for the map access itself,
/// never across an await.
fn cache_get(cache: &Cache, key: &CacheKey) -> Option<Arc<serde_json::Value>> {
    let cache = cache.lock().ok()?;
    let (at, body) = cache.get(key)?;
    (at.elapsed() < CACHE_TTL).then(|| body.clone())
}

pub fn router(state: PrecipState) -> Router {
    Router::new()
        .route("/precip", get(precip))
        .with_state(state)
}

#[derive(Deserialize)]
struct PrecipQuery {
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
    // mercator limit Leaflet itself renders to, so a fully zoomed-out map
    // still produces a valid grid instead of polar nonsense.
    let min_lon = round2(v[0].clamp(-180.0, 180.0));
    let min_lat = round2(v[1].clamp(-85.0, 85.0));
    let max_lon = round2(v[2].clamp(-180.0, 180.0));
    let max_lat = round2(v[3].clamp(-85.0, 85.0));
    // Degenerate after clamp+rounding (inverted, empty, or thinner than
    // the 2-decimal coordinate resolution) is a client error.
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

/// Grid geometry: la1/lo1 = NW corner, la2/lo2 = SE corner, dx/dy =
/// positive cell size in degrees, points emitted row-major from the NW
/// corner. Identical convention to windgrid so the overlays align.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Grid {
    la1: f64,
    lo1: f64,
    la2: f64,
    lo2: f64,
    dx: f64,
    dy: f64,
}

impl Grid {
    fn from_bbox(b: &Bbox) -> Self {
        Self {
            la1: b.max_lat,
            lo1: b.min_lon,
            la2: b.min_lat,
            lo2: b.max_lon,
            dx: (b.max_lon - b.min_lon) / (GRID_N - 1) as f64,
            dy: (b.max_lat - b.min_lat) / (GRID_N - 1) as f64,
        }
    }

    /// Comma-separated latitude/longitude lists in row-major order from
    /// the NW corner: rows north to south, columns west to east. The
    /// batched Open-Meteo response preserves this order, so the per-point
    /// results map 1:1 onto the values arrays.
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
        // Times come back in GMT (the upstream default when no timezone
        // parameter is sent), which is what the now-index math assumes.
        // forecast_minutely_15 covers ~2 hours of 15-min steps; we only
        // read the first 8 future steps but ask for a day's worth to be
        // safe across the hour boundary.
        let mut url = format!(
            "https://api.open-meteo.com/v1/forecast?\
             latitude={lat_csv}&longitude={lon_csv}&\
             minutely_15=precipitation&\
             forecast_minutely_15=96&forecast_days=1"
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

/// Round mm to 2 decimals to keep the 512-value payload small and free
/// of 17-digit float noise.
fn round2_mm(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// First 15-minute step strictly after `now_iso` in an Open-Meteo
/// minutely_15 time array ("2026-06-12T14:15" strings, GMT). ISO strings
/// sort lexically. Returns None if the array ends at or before now.
fn first_future_index(times: &[String], now_iso: &str) -> Option<usize> {
    times.iter().position(|t| t.as_str() > now_iso)
}

// Batched Open-Meteo response: a top-level JSON array of per-location
// results (elements past the first also carry a location_id field, which
// we don't need: order matches the request lists).

#[derive(Deserialize)]
struct RawPoint {
    minutely_15: RawPrecip,
}

#[derive(Deserialize)]
struct RawPrecip {
    time: Vec<String>,
    precipitation: Vec<Option<f64>>,
}

/// One animation frame: a unix-second timestamp, a lead time in minutes,
/// and the 64 mm/15-min values in NW-row-major order (same cell order as
/// windgrid's data array).
#[derive(Serialize)]
struct Frame {
    time: i64,
    lead_min: u32,
    values: Vec<f64>,
}

#[derive(Serialize)]
struct PrecipResponse {
    bounds: Bounds,
    rows: usize,
    cols: usize,
    interval_min: u32,
    max_mm: f64,
    frames: Vec<Frame>,
}

#[derive(Serialize)]
struct Bounds {
    north: f64,
    south: f64,
    west: f64,
    east: f64,
}

/// Parse an Open-Meteo GMT minutely_15 timestamp ("2026-06-12T14:15") to
/// unix seconds. Returns None on a malformed string.
fn iso_gmt_to_unix(s: &str) -> Option<i64> {
    use chrono::{NaiveDateTime, TimeZone, Utc};
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").ok()?;
    Some(Utc.from_utc_datetime(&naive).timestamp())
}

/// Build the 8-frame response from the 64 per-point precipitation series.
/// `points` must already be length GRID_N*GRID_N in grid order. Returns
/// an error string if the upstream minutely_15 window has no future
/// steps. Pulled out of the handler so it is unit-testable.
fn build_response(
    bbox: &Bbox,
    points: &[RawPoint],
    now_iso: &str,
) -> Result<PrecipResponse, String> {
    let times = &points[0].minutely_15.time;
    let start = first_future_index(times, now_iso)
        .ok_or_else(|| "open-meteo minutely_15 window has no future steps".to_string())?;

    let mut max_mm = 0.0f64;
    let mut frames = Vec::with_capacity(FRAMES);
    for f in 0..FRAMES {
        let step = start + f;
        // The minutely_15 window should comfortably cover 8 future
        // steps; if it somehow runs short, stop early rather than fail.
        let Some(t) = times.get(step) else { break };
        let unix = iso_gmt_to_unix(t)
            .ok_or_else(|| format!("open-meteo minutely_15 time '{t}' is not GMT ISO"))?;
        let mut values = Vec::with_capacity(GRID_N * GRID_N);
        for p in points {
            // A null (model hole at one grid point) becomes dry rather
            // than failing the whole frame.
            let mm = p
                .minutely_15
                .precipitation
                .get(step)
                .copied()
                .flatten()
                .unwrap_or(0.0)
                .max(0.0);
            let mm = round2_mm(mm);
            if mm > max_mm {
                max_mm = mm;
            }
            values.push(mm);
        }
        frames.push(Frame {
            time: unix,
            lead_min: ((f + 1) * 15) as u32,
            values,
        });
    }

    Ok(PrecipResponse {
        bounds: Bounds {
            north: bbox.max_lat,
            south: bbox.min_lat,
            west: bbox.min_lon,
            east: bbox.max_lon,
        },
        rows: GRID_N,
        cols: GRID_N,
        interval_min: 15,
        max_mm: round2_mm(max_mm),
        frames,
    })
}

fn error_json(status: StatusCode, msg: String) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

async fn precip(State(st): State<PrecipState>, Query(q): Query<PrecipQuery>) -> Response {
    let bbox = match parse_bbox(&q.bbox) {
        Ok(b) => b,
        Err(msg) => return error_json(StatusCode::BAD_REQUEST, msg),
    };
    let grid = Grid::from_bbox(&bbox);

    // Honor the configured open_meteo model, same resolution rule as the
    // forecast refresher (first open_meteo source entry).
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
            tracing::warn!(error = %msg, "precip upstream fetch failed");
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

    let now_iso = chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string();
    let resp = match build_response(&bbox, &points, &now_iso) {
        Ok(r) => r,
        Err(msg) => return error_json(StatusCode::BAD_GATEWAY, msg),
    };

    let body = match serde_json::to_value(&resp) {
        Ok(b) => b,
        Err(e) => {
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("precip serialize: {e}"),
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
        .map_err(|e| format!("GET open-meteo precip: {e}"))?
        .error_for_status()
        .map_err(|e| format!("open-meteo non-2xx: {e}"))?
        .json()
        .await
        .map_err(|e| format!("decode open-meteo precip json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
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
            "-81.3,28.3,-82.0,29.0",
            "-82.0,29.0,-81.3,28.3",
            "-81.4,28.5,-81.4,28.5",
            "-81.401,28.5,-81.399,29.0",
        ] {
            assert!(parse_bbox(bad).is_err(), "'{bad}' should be rejected");
        }
    }

    #[test]
    fn grid_geometry_is_consistent_with_the_bbox() {
        let b = parse_bbox("-82.0,28.3,-81.3,29.0").unwrap();
        let g = Grid::from_bbox(&b);
        assert_eq!((g.la1, g.lo1), (29.0, -82.0));
        assert_eq!((g.la2, g.lo2), (28.3, -81.3));
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
    fn url_carries_minutely_15_and_models_only_when_pinned() {
        let g = Grid::from_bbox(&parse_bbox("-82.0,28.3,-81.3,29.0").unwrap());
        let plain = g.open_meteo_url("best_match");
        assert!(!plain.contains("models="));
        assert!(plain.contains("minutely_15=precipitation"));
        assert!(plain.contains("forecast_minutely_15=96"));
        let pinned = g.open_meteo_url("gfs_seamless");
        assert_eq!(pinned, format!("{plain}&models=gfs_seamless"));
    }

    #[test]
    fn first_future_index_finds_first_step_after_now() {
        // A clean 15-min ladder across the hour boundary.
        let times: Vec<String> = vec![
            "2026-06-12T14:00".into(),
            "2026-06-12T14:15".into(),
            "2026-06-12T14:30".into(),
            "2026-06-12T14:45".into(),
            "2026-06-12T15:00".into(),
        ];
        // now exactly on a step -> next step is strictly greater.
        assert_eq!(first_future_index(&times, "2026-06-12T14:15"), Some(2));
        // now between steps -> first step after.
        assert_eq!(first_future_index(&times, "2026-06-12T14:20"), Some(2));
        // now before the window -> first element.
        assert_eq!(first_future_index(&times, "2026-06-12T13:00"), Some(0));
        // now after the window -> none.
        assert_eq!(first_future_index(&times, "2026-06-12T16:00"), None);
    }

    #[test]
    fn iso_gmt_to_unix_matches_known_epoch() {
        // 2026-06-12T14:15Z = 1781273700 (verified via epoch math).
        assert_eq!(iso_gmt_to_unix("2026-06-12T14:15"), Some(1781273700));
        assert_eq!(iso_gmt_to_unix("garbage"), None);
    }

    /// Synthetic 2-point grid would not satisfy the GRID_N*GRID_N
    /// handler check, but build_response only needs points[0] for the
    /// time axis and iterates whatever points it is given, so we feed it
    /// a full 64-point grid where each point carries the same series and
    /// assert the frame assembly, lead times, and max_mm.
    #[test]
    fn build_response_assembles_eight_future_frames() {
        // A 15-min ladder of 12 steps starting at 14:00. "now" is
        // 14:07, so the first future step is index 1 (14:15) and we take
        // 8 frames: 14:15 .. 16:00.
        let times: Vec<String> = (0..12)
            .map(|i| {
                let mins = i * 15;
                let h = 14 + mins / 60;
                let m = mins % 60;
                format!("2026-06-12T{h:02}:{m:02}")
            })
            .collect();
        // precipitation: ramp 0.0, 0.1, 0.2, ... so frame f (step 1+f)
        // for every point is (1+f)*0.1 mm.
        let precip: Vec<Option<f64>> = (0..12).map(|i| Some((i as f64) * 0.1)).collect();
        let mk_point = || RawPoint {
            minutely_15: RawPrecip {
                time: times.clone(),
                precipitation: precip.clone(),
            },
        };
        let points: Vec<RawPoint> = (0..GRID_N * GRID_N).map(|_| mk_point()).collect();
        let bbox = parse_bbox("-82.0,28.3,-81.3,29.0").unwrap();

        let r = build_response(&bbox, &points, "2026-06-12T14:07").unwrap();
        assert_eq!(r.rows, 8);
        assert_eq!(r.cols, 8);
        assert_eq!(r.interval_min, 15);
        assert_eq!(r.frames.len(), 8);
        // bounds mirror the bbox.
        assert!(close(r.bounds.north, 29.0) && close(r.bounds.south, 28.3));
        assert!(close(r.bounds.west, -82.0) && close(r.bounds.east, -81.3));
        // lead_min ladder.
        let leads: Vec<u32> = r.frames.iter().map(|f| f.lead_min).collect();
        assert_eq!(leads, vec![15, 30, 45, 60, 75, 90, 105, 120]);
        // Each frame carries 64 values, all equal to that step's ramp
        // value: frame 0 = step 1 = 0.1 mm, frame 7 = step 8 = 0.8 mm.
        for (f, frame) in r.frames.iter().enumerate() {
            assert_eq!(frame.values.len(), 64);
            let expected = round2_mm(((f + 1) as f64) * 0.1);
            for v in &frame.values {
                assert!(close(*v, expected), "frame {f}: {v} != {expected}");
            }
        }
        // First frame timestamp is 14:15Z.
        assert_eq!(
            r.frames[0].time,
            iso_gmt_to_unix("2026-06-12T14:15").unwrap()
        );
        // max_mm is the 8th step value, 0.8.
        assert!(close(r.max_mm, 0.8), "max_mm was {}", r.max_mm);
    }

    #[test]
    fn build_response_nulls_become_dry() {
        let times: Vec<String> = (0..10)
            .map(|i| {
                let mins = i * 15;
                let h = 14 + mins / 60;
                let m = mins % 60;
                format!("2026-06-12T{h:02}:{m:02}")
            })
            .collect();
        // All null -> all dry, max_mm 0.
        let precip: Vec<Option<f64>> = vec![None; 10];
        let mk_point = || RawPoint {
            minutely_15: RawPrecip {
                time: times.clone(),
                precipitation: precip.clone(),
            },
        };
        let points: Vec<RawPoint> = (0..GRID_N * GRID_N).map(|_| mk_point()).collect();
        let bbox = parse_bbox("-82.0,28.3,-81.3,29.0").unwrap();
        let r = build_response(&bbox, &points, "2026-06-12T13:00").unwrap();
        assert!(close(r.max_mm, 0.0));
        assert!(r.frames.iter().all(|f| f.values.iter().all(|v| *v == 0.0)));
    }

    #[test]
    fn build_response_errors_when_no_future_steps() {
        let times: Vec<String> = vec!["2026-06-12T14:00".into(), "2026-06-12T14:15".into()];
        let precip: Vec<Option<f64>> = vec![Some(1.0), Some(1.0)];
        let mk_point = || RawPoint {
            minutely_15: RawPrecip {
                time: times.clone(),
                precipitation: precip.clone(),
            },
        };
        let points: Vec<RawPoint> = (0..GRID_N * GRID_N).map(|_| mk_point()).collect();
        let bbox = parse_bbox("-82.0,28.3,-81.3,29.0").unwrap();
        // "now" past the window.
        assert!(build_response(&bbox, &points, "2026-06-12T18:00").is_err());
    }

    #[test]
    fn cache_key_is_stable_across_subrounding_pans() {
        let a = Grid::from_bbox(&parse_bbox("-82.001,28.299,-81.302,29.004").unwrap());
        let b = Grid::from_bbox(&parse_bbox("-81.996,28.301,-81.297,28.996").unwrap());
        assert_eq!(a.cache_key("best_match"), b.cache_key("best_match"));
        assert_ne!(a.cache_key("best_match"), a.cache_key("gfs_seamless"));
    }
}
