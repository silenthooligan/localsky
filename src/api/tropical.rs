// GET /api/v1/radar/tropical
//
// Basin-aware tropical cyclone tracking for the radar map's
// "hurricanes" feature (radar_catalog::tropical_basins). The browser
// consumes exactly ONE uniform endpoint regardless of basin; this
// handler fetches every verified agency feed, normalizes them all
// into one GeoJSON FeatureCollection, and caches the result for 10
// minutes with windgrid-style single-flight so concurrent cold-cache
// requests cannot stampede the upstreams.
//
// Verified sources normalized here (2026-06-12 recon; key-free, valid
// quiet-basin states confirmed):
//   - NOAA NHC + CPHC: CurrentStorms.json (positions; CP1-CP5 bins
//     carry Central Pacific storms, verified against the archived
//     2025-08-11 production copy showing ep082025 Henriette as "CP3")
//     plus the summary MapServer aggregate forecast-track (layer 6)
//     and cone (layer 7) GeoJSON queries.
//   - JMA / RSMC Tokyo: bosai typhoon JSON (targetTc.json enumerator,
//     pastTracks.json, per-storm forecast.json). CAUTION: JMA
//     coordinates are [lat, lon] (NOT GeoJSON order) and radii are
//     METERS; both are converted here. The 70% probability circles
//     become a cone polygon via a convex hull over sampled circle
//     points.
//   - JTWC: RSS index plus per-storm fixed-width .tcw warnings, used
//     ONLY for the basins without a verified RSMC machine feed
//     (io-sector = North Indian Ocean, sh-sector = Southern
//     Hemisphere incl. the Australian region, where BOM's sanctioned
//     channel is FTP and reqwest is HTTP-only). wp/ep/cp sector
//     products are deliberately skipped: JMA and NHC are the primary
//     issuers there and duplicates would double-render.
//
// Response shape (locked by snapshot_tests::radar_tropical_v1_shape):
// a FeatureCollection whose features carry a uniform property bag
// (kind position|track|forecast_track|cone, id, name, term, agency,
// basin, classification, intensity_kt, pressure_mb, movement,
// updated) over Point / LineString / Polygon geometry, plus a
// `sources` array reporting per-agency fetch health so the frontend
// can tell "quiet globe" from "feed down" without failing the layer.
//
// HTTP discipline: bounded timeout, polite User-Agent identifying the
// project URL only (no personal data), graceful per-agency
// degradation (one dead feed never blanks the others).

use axum::{
    extract::State,
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::radar_catalog::tropical_basin_by_id;

/// Cache TTL. Advisory cadence is hours (NHC 6h, JMA 3-6h, JTWC
/// 6-12h); 10 minutes keeps the layer fresh across advisory drops
/// while a dashboard full of clients costs at most one upstream sweep
/// per interval.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Per-request upstream timeout.
const FETCH_TIMEOUT: Duration = Duration::from_secs(12);
/// Per-agency active-storm fan-out ceiling (the per-storm JMA/JTWC
/// product fetches). Real seasons peak well below this.
const MAX_STORMS_PER_AGENCY: usize = 8;
/// Polite UA: project URL only, no personal data (house rule).
const USER_AGENT: &str = "localsky-tropical/1.0 (+https://github.com/silenthooligan/localsky)";

const NHC_CURRENT_STORMS: &str = "https://www.nhc.noaa.gov/CurrentStorms.json";
const NHC_TRACK_QUERY: &str = "https://mapservices.weather.noaa.gov/tropical/rest/services/tropical/NHC_tropical_weather_summary/MapServer/6/query?where=1%3D1&outFields=*&f=geojson";
const NHC_CONE_QUERY: &str = "https://mapservices.weather.noaa.gov/tropical/rest/services/tropical/NHC_tropical_weather_summary/MapServer/7/query?where=1%3D1&outFields=*&f=geojson";
const JMA_TARGET_TC: &str = "https://www.jma.go.jp/bosai/typhoon/data/targetTc.json";
const JMA_PAST_TRACKS: &str = "https://www.jma.go.jp/bosai/typhoon/data/pastTracks.json";
const JTWC_RSS: &str = "https://www.metoc.navy.mil/jtwc/rss/jtwc.rss";

type Cache = Mutex<Option<(Instant, Arc<Value>)>>;

#[derive(Clone)]
pub struct TropicalState {
    client: reqwest::Client,
    /// Single global entry: the endpoint is bbox-free (all active
    /// storms worldwide), so there is exactly one cacheable body.
    cache: Arc<Cache>,
    /// Single-flight for cold-cache fills, same pattern as windgrid:
    /// losers of the race re-check the cache after acquiring and find
    /// the winner's entry instead of re-sweeping three agencies.
    fetch_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Default for TropicalState {
    fn default() -> Self {
        Self::new()
    }
}

impl TropicalState {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .unwrap_or_default();
        Self {
            client,
            cache: Arc::new(Mutex::new(None)),
            fetch_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

pub fn router(state: TropicalState) -> Router {
    Router::new()
        .route("/tropical", get(tropical))
        .with_state(state)
}

/// Fresh-entry lookup; the std mutex is only held for the Option
/// access itself, never across an await.
fn cache_get(cache: &Cache) -> Option<Arc<Value>> {
    let cache = cache.lock().ok()?;
    let (at, body) = cache.as_ref()?;
    (at.elapsed() < CACHE_TTL).then(|| body.clone())
}

async fn tropical(State(st): State<TropicalState>) -> Response {
    if let Some(body) = cache_get(&st.cache) {
        return Json((*body).clone()).into_response();
    }
    let _fetch_guard = st.fetch_lock.lock().await;
    if let Some(body) = cache_get(&st.cache) {
        return Json((*body).clone()).into_response();
    }

    let (body, any_ok) = build_collection(&st.client).await;
    // An all-agencies-down sweep is served (the frontend reads the
    // per-source health) but NOT cached, so the next request retries
    // instead of pinning an outage for the full TTL.
    if any_ok {
        if let Ok(mut cache) = st.cache.lock() {
            *cache = Some((Instant::now(), Arc::new(body.clone())));
        }
    }
    Json(body).into_response()
}

// ---- Normalized output types ----------------------------------------------

/// Uniform per-feature property bag. Every key is always present
/// (null when unknown) so radar.js reads `properties.term` etc.
/// without existence checks; `term` + `name` drive the localized
/// tooltip ("Typhoon Hagibis", "Cyclone Tracy").
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StormProps {
    /// position | track | forecast_track | cone
    pub(crate) kind: &'static str,
    /// Agency storm id (al052025, TC2105, sh0226...).
    pub(crate) id: String,
    pub(crate) name: String,
    /// Local term for the storm's CURRENT basin.
    pub(crate) term: &'static str,
    /// Issuing agency of the normalized data.
    pub(crate) agency: &'static str,
    /// radar_catalog basin id.
    pub(crate) basin: &'static str,
    /// Agency-native classification (HU, TS, TY, STS...).
    pub(crate) classification: Option<String>,
    pub(crate) intensity_kt: Option<f64>,
    pub(crate) pressure_mb: Option<f64>,
    /// Human movement summary ("WNW at 10 kt").
    pub(crate) movement: Option<String>,
    /// Agency issue time, ISO 8601.
    pub(crate) updated: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Feature {
    #[serde(rename = "type")]
    pub(crate) typ: &'static str,
    pub(crate) geometry: Value,
    pub(crate) properties: StormProps,
}

impl Feature {
    fn new(geometry: Value, properties: StormProps) -> Self {
        Self {
            typ: "Feature",
            geometry,
            properties,
        }
    }
}

/// Per-agency fetch health, surfaced as a foreign member on the
/// FeatureCollection so the frontend can distinguish a quiet globe
/// from a dead feed.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SourceStatus {
    pub(crate) agency: &'static str,
    pub(crate) ok: bool,
    /// Count of position features this agency contributed.
    pub(crate) storms: usize,
}

pub(crate) fn collection_value(
    features: &[Feature],
    sources: &[SourceStatus],
    generated_at: &str,
) -> Value {
    json!({
        "type": "FeatureCollection",
        "features": features,
        "sources": sources,
        "generated_at": generated_at,
    })
}

fn position_count(features: &[Feature]) -> usize {
    features
        .iter()
        .filter(|f| f.properties.kind == "position")
        .count()
}

/// Sweep all three agencies concurrently; per-agency failure degrades
/// to an ok:false source entry instead of failing the response.
/// Returns the collection plus whether ANY agency succeeded (the
/// cacheability signal).
async fn build_collection(client: &reqwest::Client) -> (Value, bool) {
    let (nhc, jma, jtwc) = tokio::join!(fetch_nhc(client), fetch_jma(client), fetch_jtwc(client));
    let mut features: Vec<Feature> = Vec::new();
    let mut sources: Vec<SourceStatus> = Vec::new();
    let mut any_ok = false;
    for (agency, result) in [
        ("NOAA NHC + CPHC", nhc),
        ("JMA / RSMC Tokyo", jma),
        ("JTWC", jtwc),
    ] {
        match result {
            Ok(feats) => {
                any_ok = true;
                sources.push(SourceStatus {
                    agency,
                    ok: true,
                    storms: position_count(&feats),
                });
                features.extend(feats);
            }
            Err(msg) => {
                tracing::warn!(agency, error = %msg, "tropical upstream fetch failed");
                sources.push(SourceStatus {
                    agency,
                    ok: false,
                    storms: 0,
                });
            }
        }
    }
    let generated_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    (collection_value(&features, &sources, &generated_at), any_ok)
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("non-2xx from {url}: {e}"))?
        .text()
        .await
        .map_err(|e| format!("read body from {url}: {e}"))
}

// ---- Tolerant Value access -------------------------------------------------
// Upstream schemas drift (the official NHC reference PDF still predates
// CP-bin integration); every extraction below prefers being lenient and
// skipping a field over failing a storm, and skipping a storm over
// failing the agency.

/// First present key as &str.
fn vstr<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(k).and_then(Value::as_str))
}

/// First present key as f64, accepting numbers or numeric strings
/// (NHC serves intensity/pressure as strings, JMA serves pressure as
/// strings).
fn vnum(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        let f = v.get(k)?;
        f.as_f64()
            .or_else(|| f.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

/// A JMA-style [lat, lon] pair (their documented order, NOT GeoJSON).
fn latlon_pair(v: &Value) -> Option<(f64, f64)> {
    let arr = v.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    let lat = arr[0].as_f64()?;
    let lon = arr[1].as_f64()?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// 3-decimal rounding (about 110 m) for derived geometry (cone hull
/// samples); keeps the payload free of float-division noise.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// 16-point compass label for a meteorological direction.
fn compass(dir_deg: f64) -> &'static str {
    const PTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    PTS[((dir_deg.rem_euclid(360.0) / 22.5).round() as usize) % 16]
}

fn movement_label(dir_deg: Option<f64>, speed_kt: Option<f64>) -> Option<String> {
    match (dir_deg, speed_kt) {
        (Some(d), Some(s)) => Some(format!("{} at {:.0} kt", compass(d), s)),
        _ => None,
    }
}

fn point_geometry(lat: f64, lon: f64) -> Value {
    json!({ "type": "Point", "coordinates": [lon, lat] })
}

/// (lat, lon) waypoint list to a GeoJSON LineString.
fn line_geometry(points: &[(f64, f64)]) -> Value {
    let coords: Vec<[f64; 2]> = points.iter().map(|(lat, lon)| [*lon, *lat]).collect();
    json!({ "type": "LineString", "coordinates": coords })
}

// ---- NOAA NHC + CPHC --------------------------------------------------------

/// One storm from CurrentStorms.json with its resolved basin
/// identity. binNumber reflects CURRENT agency responsibility (an
/// East Pacific storm crossing 140W keeps its ep id but moves to a CP
/// bin), so the bin, not the id prefix, picks the basin.
struct NhcStorm {
    id: String,
    name: String,
    basin: &'static str,
    agency: &'static str,
    term: &'static str,
    lat: f64,
    lon: f64,
    classification: Option<String>,
    intensity_kt: Option<f64>,
    pressure_mb: Option<f64>,
    movement: Option<String>,
    updated: Option<String>,
}

/// Basin id from a CurrentStorms binNumber ("CP3") or, as fallback,
/// a storm/ATCF id prefix ("ep082025").
fn nhc_basin(token: &str) -> &'static str {
    let t = token.to_ascii_lowercase();
    if t.starts_with("cp") {
        "central_pacific"
    } else if t.starts_with("ep") {
        "east_pacific"
    } else {
        // "at" bins and "al" ids; also the safe default.
        "north_atlantic"
    }
}

fn parse_nhc_current(body: &str) -> Result<Vec<NhcStorm>, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("decode CurrentStorms.json: {e}"))?;
    let storms = v
        .get("activeStorms")
        .and_then(Value::as_array)
        .ok_or_else(|| "CurrentStorms.json missing activeStorms".to_string())?;
    let mut out = Vec::new();
    for s in storms {
        // Numeric lat/lon are required (verified fields); a storm
        // without them cannot render and is skipped, not fatal.
        let (Some(lat), Some(lon)) = (
            vnum(s, &["latitudeNumeric", "latitude"]),
            vnum(s, &["longitudeNumeric", "longitude"]),
        ) else {
            continue;
        };
        let id = vstr(s, &["id"]).unwrap_or_default().to_string();
        let bin = vstr(s, &["binNumber"]).unwrap_or(&id);
        let basin = nhc_basin(bin);
        let basin_meta = tropical_basin_by_id(basin).expect("nhc basin in catalog");
        out.push(NhcStorm {
            name: vstr(s, &["name"]).unwrap_or("Unnamed").to_string(),
            basin,
            agency: basin_meta.agency,
            term: basin_meta.term,
            lat,
            lon,
            classification: vstr(s, &["classification"]).map(str::to_string),
            intensity_kt: vnum(s, &["intensity"]),
            pressure_mb: vnum(s, &["pressure"]),
            movement: movement_label(vnum(s, &["movementDir"]), vnum(s, &["movementSpeed"])),
            updated: vstr(s, &["lastUpdate"]).map(str::to_string),
            id,
        });
    }
    Ok(out)
}

fn nhc_position_features(storms: &[NhcStorm]) -> Vec<Feature> {
    storms
        .iter()
        .map(|s| {
            Feature::new(
                point_geometry(s.lat, s.lon),
                StormProps {
                    kind: "position",
                    id: s.id.clone(),
                    name: s.name.clone(),
                    term: s.term,
                    agency: s.agency,
                    basin: s.basin,
                    classification: s.classification.clone(),
                    intensity_kt: s.intensity_kt,
                    pressure_mb: s.pressure_mb,
                    movement: s.movement.clone(),
                    updated: s.updated.clone(),
                },
            )
        })
        .collect()
}

/// Re-tag the aggregate MapServer track/cone GeoJSON with the uniform
/// property bag, matching features back to CurrentStorms storms by
/// name so a CP-bin storm's cone carries the CPHC identity.
fn nhc_overlay_features(body: &str, kind: &'static str, storms: &[NhcStorm]) -> Vec<Feature> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(feats) = v.get("features").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for f in feats {
        let Some(geometry) = f.get("geometry").filter(|g| g.is_object()) else {
            continue;
        };
        let props = f.get("properties").cloned().unwrap_or(Value::Null);
        let name = vstr(&props, &["STORMNAME", "stormName", "stormname", "NAME"])
            .unwrap_or_default()
            .to_string();
        let matched = storms
            .iter()
            .find(|s| !name.is_empty() && s.name.eq_ignore_ascii_case(&name));
        let (id, term, agency, basin) = match matched {
            Some(s) => (s.id.clone(), s.term, s.agency, s.basin),
            None => {
                // Unmatched overlays still render with NHC defaults
                // (basin from a BASIN/STORMID prop when present).
                let basin = nhc_basin(
                    vstr(&props, &["BASIN", "basin", "STORMID", "stormid"]).unwrap_or(""),
                );
                let meta = tropical_basin_by_id(basin).expect("nhc basin in catalog");
                (String::new(), meta.term, meta.agency, basin)
            }
        };
        out.push(Feature::new(
            geometry.clone(),
            StormProps {
                kind,
                id,
                name: matched.map(|s| s.name.clone()).unwrap_or(name),
                term,
                agency,
                basin,
                classification: None,
                intensity_kt: None,
                pressure_mb: None,
                movement: None,
                updated: None,
            },
        ));
    }
    out
}

async fn fetch_nhc(client: &reqwest::Client) -> Result<Vec<Feature>, String> {
    let body = fetch_text(client, NHC_CURRENT_STORMS).await?;
    let storms = parse_nhc_current(&body)?;
    let mut feats = nhc_position_features(&storms);
    // Quiet basins: skip the MapServer queries entirely (zero extra
    // upstream load for the common no-storm case).
    if storms.is_empty() {
        return Ok(feats);
    }
    for (url, kind) in [
        (NHC_TRACK_QUERY, "forecast_track"),
        (NHC_CONE_QUERY, "cone"),
    ] {
        match fetch_text(client, url).await {
            Ok(body) => feats.extend(nhc_overlay_features(&body, kind, &storms)),
            // Track/cone are enrichment; positions alone still render.
            Err(e) => tracing::warn!(error = %e, kind, "tropical: NHC overlay fetch failed"),
        }
    }
    Ok(feats)
}

// ---- JMA / RSMC Tokyo -------------------------------------------------------

/// targetTc.json enumerates active storm directories. Quiet basin is
/// a valid empty array; elements have been observed as plain strings
/// and as objects keyed `tropicalCyclone` (TC{yy}{seq}, which also
/// covers pre-typhoon TDs; typhoonNumber can be a placeholder until
/// numbered, so the directory key is the identity).
fn parse_jma_target_tc(body: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("decode targetTc.json: {e}"))?;
    let arr = v
        .as_array()
        .ok_or_else(|| "targetTc.json is not an array".to_string())?;
    let mut out = Vec::new();
    for e in arr {
        let id = e
            .as_str()
            .map(str::to_string)
            .or_else(|| vstr(e, &["tropicalCyclone"]).map(str::to_string));
        if let Some(id) = id {
            if !id.is_empty() && !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// Past-track waypoints for one storm out of pastTracks.json:
/// find the element whose tropicalCyclone matches, then collect
/// [lat, lon] pairs (preferring an explicit `track` member, falling
/// back to any plausible pair list under the element).
fn jma_past_track(past_tracks: &Value, tc_id: &str) -> Vec<(f64, f64)> {
    let Some(arr) = past_tracks.as_array() else {
        return Vec::new();
    };
    let Some(entry) = arr
        .iter()
        .find(|e| vstr(e, &["tropicalCyclone"]).is_some_and(|id| id.eq_ignore_ascii_case(tc_id)))
    else {
        return Vec::new();
    };
    let scope = entry.get("track").unwrap_or(entry);
    let mut out = Vec::new();
    collect_latlon_pairs(scope, &mut out);
    out
}

fn collect_latlon_pairs(v: &Value, out: &mut Vec<(f64, f64)>) {
    if let Some(p) = latlon_pair(v) {
        out.push(p);
        return;
    }
    match v {
        Value::Array(a) => a.iter().for_each(|e| collect_latlon_pairs(e, out)),
        Value::Object(o) => o.values().for_each(|e| collect_latlon_pairs(e, out)),
        _ => {}
    }
}

/// Normalize one JMA bosai per-storm forecast.json (an array of
/// "part" objects: a title part with typhoonNumber + jp/en name, an
/// analysis part with center/pressure/winds, then forecast parts with
/// advancedHours + center + 70% probabilityCircle).
fn normalize_jma_storm(tc_id: &str, forecast: &Value, past_tracks: Option<&Value>) -> Vec<Feature> {
    let Some(parts) = forecast.as_array() else {
        return Vec::new();
    };
    let basin = "west_pacific";
    let meta = tropical_basin_by_id(basin).expect("west_pacific in catalog");

    // Identity from the title part (name.en may be empty for an
    // unnamed TD; fall back to the directory key).
    let name = parts
        .iter()
        .find_map(|p| p.get("name").and_then(|n| vstr(n, &["en"])))
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| tc_id.to_uppercase());
    let updated = parts
        .iter()
        .find_map(|p| p.get("issue").and_then(|i| vstr(i, &["UTC", "utc"])))
        .map(str::to_string);

    let props = |kind: &'static str| StormProps {
        kind,
        id: tc_id.to_string(),
        name: name.clone(),
        term: meta.term,
        agency: meta.agency,
        basin,
        classification: None,
        intensity_kt: None,
        pressure_mb: None,
        movement: None,
        updated: updated.clone(),
    };

    let mut out = Vec::new();

    // Analysis part: has a center but no advancedHours.
    let analysis = parts
        .iter()
        .find(|p| p.get("center").is_some() && p.get("advancedHours").is_none());
    let analysis_center = analysis.and_then(|p| p.get("center")).and_then(latlon_pair);
    if let (Some(part), Some((lat, lon))) = (analysis, analysis_center) {
        let mut p = props("position");
        p.classification = part
            .get("category")
            .and_then(|c| vstr(c, &["en"]))
            .map(str::to_string);
        p.intensity_kt = part
            .get("maximumWind")
            .and_then(|w| w.get("sustained"))
            .and_then(|s| vnum(s, &["knots"]));
        p.pressure_mb = vnum(part, &["pressure"]);
        out.push(Feature::new(point_geometry(lat, lon), p));
    }

    // Past track from pastTracks.json ([lat, lon] waypoints).
    if let Some(past) = past_tracks {
        let track = jma_past_track(past, tc_id);
        if track.len() >= 2 {
            out.push(Feature::new(line_geometry(&track), props("track")));
        }
    }

    // Forecast parts, in advancedHours order.
    let mut fparts: Vec<(f64, &Value)> = parts
        .iter()
        .filter_map(|p| vnum(p, &["advancedHours"]).map(|h| (h, p)))
        .collect();
    fparts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut track: Vec<(f64, f64)> = analysis_center.into_iter().collect();
    // (lat, lon, radius m) probability circles for the cone hull.
    let mut circles: Vec<(f64, f64, f64)> = Vec::new();
    for (_h, part) in &fparts {
        let Some((lat, lon)) = part.get("center").and_then(latlon_pair) else {
            continue;
        };
        track.push((lat, lon));
        if let Some(pc) = part.get("probabilityCircle") {
            let (clat, clon) = pc.get("center").and_then(latlon_pair).unwrap_or((lat, lon));
            if let Some(r) = vnum(pc, &["radius"]).filter(|r| *r > 0.0) {
                circles.push((clat, clon, r));
            }
        }
    }
    if track.len() >= 2 {
        out.push(Feature::new(line_geometry(&track), props("forecast_track")));
    }
    if let Some(ring) = cone_ring(analysis_center, &circles) {
        out.push(Feature::new(
            json!({ "type": "Polygon", "coordinates": [ring] }),
            props("cone"),
        ));
    }
    out
}

/// JMA publishes 70% probability circles, not a ready-made cone; the
/// cone is the convex hull over the circles (JMA's own renderer does
/// the same). Returns a closed [lon, lat] ring, or None when there is
/// nothing to hull. Planar approximation; fine at storm scales away
/// from the antimeridian (a dateline-straddling hull would render
/// wrapped, the known and accepted limitation).
fn cone_ring(origin: Option<(f64, f64)>, circles: &[(f64, f64, f64)]) -> Option<Vec<[f64; 2]>> {
    if circles.is_empty() {
        return None;
    }
    let mut pts: Vec<(f64, f64)> = Vec::new();
    if let Some((lat, lon)) = origin {
        pts.push((lon, lat));
    }
    for (lat, lon, radius_m) in circles {
        let dlat = radius_m / 111_320.0;
        let dlon = radius_m / (111_320.0 * lat.to_radians().cos().max(0.01));
        for i in 0..16 {
            let a = i as f64 * std::f64::consts::TAU / 16.0;
            pts.push((round3(lon + dlon * a.cos()), round3(lat + dlat * a.sin())));
        }
    }
    let hull = convex_hull(pts);
    if hull.len() < 3 {
        return None;
    }
    let mut ring: Vec<[f64; 2]> = hull.iter().map(|(x, y)| [*x, *y]).collect();
    ring.push(ring[0]);
    Some(ring)
}

/// Andrew's monotone chain over (x, y) points; returns the hull
/// counter-clockwise, unclosed.
fn convex_hull(mut pts: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };
    let mut lower: Vec<(f64, f64)> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<(f64, f64)> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

async fn fetch_jma(client: &reqwest::Client) -> Result<Vec<Feature>, String> {
    let target = fetch_text(client, JMA_TARGET_TC).await?;
    let ids = parse_jma_target_tc(&target)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // Past tracks are one shared file; enrichment only.
    let past_tracks = match fetch_text(client, JMA_PAST_TRACKS).await {
        Ok(body) => serde_json::from_str::<Value>(&body).ok(),
        Err(e) => {
            tracing::warn!(error = %e, "tropical: JMA pastTracks fetch failed");
            None
        }
    };
    let mut feats = Vec::new();
    for id in ids.iter().take(MAX_STORMS_PER_AGENCY) {
        let url = format!("https://www.jma.go.jp/bosai/typhoon/data/{id}/forecast.json");
        match fetch_text(client, &url).await {
            Ok(body) => match serde_json::from_str::<Value>(&body) {
                Ok(v) => feats.extend(normalize_jma_storm(id, &v, past_tracks.as_ref())),
                Err(e) => {
                    tracing::warn!(error = %e, storm = %id, "tropical: bad JMA forecast.json")
                }
            },
            // One storm failing never blanks the basin.
            Err(e) => tracing::warn!(error = %e, storm = %id, "tropical: JMA storm fetch failed"),
        }
    }
    Ok(feats)
}

// ---- JTWC -------------------------------------------------------------------

/// Scan the RSS body for sh/io-sector product ids ({sh|io}NNYY, as in
/// products/sh0226.tcw). wp/ep/cp sectors are intentionally NOT
/// matched: JMA and NHC are the primary issuers there and JTWC's
/// mirrors would double-render the same storms.
fn jtwc_sector_ids(rss: &str) -> Vec<String> {
    // Byte-wise scan: `i` walks every byte offset, so str slicing here
    // would panic on a non-char-boundary the moment the live RSS
    // carries any multi-byte UTF-8 (a "Réunion" in an advisory title).
    // The id itself is ASCII by construction (sector + 4 digits).
    let b = rss.as_bytes();
    let mut out: Vec<String> = Vec::new();
    if b.len() < 6 {
        return out;
    }
    for i in 0..=b.len() - 6 {
        let sector = &b[i..i + 2];
        if sector != b"sh" && sector != b"io" {
            continue;
        }
        // Standalone token: no alphanumeric immediately before.
        if i > 0 && b[i - 1].is_ascii_alphanumeric() {
            continue;
        }
        let digits = &b[i + 2..i + 6];
        if !digits.iter().all(u8::is_ascii_digit) {
            continue;
        }
        let id = String::from_utf8_lossy(&b[i..i + 6]).into_owned();
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Best-effort storm name from the RSS item that links the product:
/// JTWC titles carry it parenthesized ("Tropical Cyclone 02S
/// (Anahita) Warning #08").
fn jtwc_storm_name(rss: &str, id: &str) -> Option<String> {
    let item = rss
        .split("<item>")
        .find(|block| block.contains(id))?
        .split("</item>")
        .next()?;
    let title = item.split("<title>").nth(1)?.split("</title>").next()?;
    let name = title.split('(').nth(1)?.split(')').next()?.trim();
    (!name.is_empty() && name.len() <= 24).then(|| name.to_string())
}

/// One waypoint from a fixed-width .tcw T-line (T000 = analysis,
/// T012/T024/... = forecast taus).
struct TcwPoint {
    tau: u32,
    lat: f64,
    lon: f64,
    intensity_kt: Option<f64>,
}

/// Parse a .tcw coordinate token ("121S", "0633E", "18.7N"): trailing
/// hemisphere letter, magnitude either explicit-decimal or ATCF
/// tenths-without-a-point.
fn tcw_coord(token: &str, pos: char, neg: char) -> Option<f64> {
    let last = token.chars().last()?;
    let sign = if last == pos {
        1.0
    } else if last == neg {
        -1.0
    } else {
        return None;
    };
    let digits = &token[..token.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit() || c == b'.') {
        return None;
    }
    let mag: f64 = if digits.contains('.') {
        digits.parse().ok()?
    } else {
        digits.parse::<f64>().ok()? / 10.0
    };
    Some(sign * mag)
}

fn parse_tcw(text: &str) -> Vec<TcwPoint> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        let Some(head) = tokens.next() else { continue };
        // T-lines only: 'T' + tau digits (T000, T012, T120...).
        let Some(tau) = head
            .strip_prefix('T')
            .filter(|d| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit()))
            .and_then(|d| d.parse::<u32>().ok())
        else {
            continue;
        };
        let mut lat = None;
        let mut lon = None;
        let mut intensity = None;
        for tok in tokens {
            if lat.is_none() {
                if let Some(v) = tcw_coord(tok, 'N', 'S') {
                    lat = Some(v);
                    continue;
                }
            }
            if lon.is_none() {
                if let Some(v) = tcw_coord(tok, 'E', 'W') {
                    lon = Some(v);
                    continue;
                }
            }
            // First bare integer after the position is the intensity;
            // R034/R050 radii tokens are letter-prefixed and skipped.
            if lat.is_some() && lon.is_some() && intensity.is_none() {
                if let Ok(v) = tok.parse::<f64>() {
                    intensity = Some(v);
                }
            }
        }
        if let (Some(lat), Some(lon)) = (lat, lon) {
            out.push(TcwPoint {
                tau,
                lat,
                lon,
                intensity_kt: intensity,
            });
        }
    }
    out.sort_by_key(|p| p.tau);
    out
}

/// Basin for a JTWC storm: io-sector is the North Indian Ocean;
/// sh-sector splits by longitude across the three Southern Hemisphere
/// basins (SW Indian to 90E, Australian region 90E-160E, South
/// Pacific east of 160E incl. west of the dateline).
fn jtwc_basin(id: &str, lon: f64) -> &'static str {
    if id.starts_with("io") {
        "north_indian"
    } else if (90.0..160.0).contains(&lon) {
        "australian"
    } else if (20.0..90.0).contains(&lon) {
        "southwest_indian"
    } else {
        "south_pacific"
    }
}

fn normalize_jtwc_storm(id: &str, name: Option<&str>, tcw: &str) -> Vec<Feature> {
    let points = parse_tcw(tcw);
    let Some(current) = points.first() else {
        return Vec::new();
    };
    let basin = jtwc_basin(id, current.lon);
    // Term follows the basin's local language; agency stays JTWC (the
    // actual issuer of the parsed data), even where the RSMC term
    // owner is someone else (BOM/IMD).
    let term = tropical_basin_by_id(basin)
        .expect("jtwc basin in catalog")
        .term;
    let name = name
        .map(str::to_string)
        .unwrap_or_else(|| id.to_uppercase());
    let props = |kind: &'static str, intensity_kt: Option<f64>| StormProps {
        kind,
        id: id.to_string(),
        name: name.clone(),
        term,
        agency: "JTWC",
        basin,
        classification: None,
        intensity_kt,
        pressure_mb: None,
        movement: None,
        updated: None,
    };
    let mut out = vec![Feature::new(
        point_geometry(current.lat, current.lon),
        props("position", current.intensity_kt),
    )];
    if points.len() >= 2 {
        let track: Vec<(f64, f64)> = points.iter().map(|p| (p.lat, p.lon)).collect();
        out.push(Feature::new(
            line_geometry(&track),
            props("forecast_track", None),
        ));
    }
    out
}

async fn fetch_jtwc(client: &reqwest::Client) -> Result<Vec<Feature>, String> {
    let rss = fetch_text(client, JTWC_RSS).await?;
    let ids = jtwc_sector_ids(&rss);
    let mut feats = Vec::new();
    for id in ids.iter().take(MAX_STORMS_PER_AGENCY) {
        let url = format!("https://www.metoc.navy.mil/jtwc/products/{id}.tcw");
        match fetch_text(client, &url).await {
            Ok(body) => {
                feats.extend(normalize_jtwc_storm(
                    id,
                    jtwc_storm_name(&rss, id).as_deref(),
                    &body,
                ));
            }
            Err(e) => tracing::warn!(error = %e, storm = %id, "tropical: JTWC storm fetch failed"),
        }
    }
    Ok(feats)
}

// ---- Test fixtures ----------------------------------------------------------

/// Embedded recon fixtures: one realistic payload snippet per
/// verified source, reconstructed from the 2026-06-12 recon captures
/// (the archived 2025-08-11 CurrentStorms.json with Henriette in a CP
/// bin; the TC2105 bosai field notes; the Jul 2025 JTWC RSS/.tcw
/// column notes). Unit tests and the radar_tropical_v1 API snapshot
/// build from these so the normalizers are locked against real-world
/// shapes, not idealized ones.
#[cfg(test)]
pub(crate) mod fixtures {
    /// Archived production shape, 2025-08-11: ep082025 Henriette
    /// carried binNumber "CP3" after crossing 140W into CPHC
    /// responsibility (id keeps the genesis prefix). intensity and
    /// pressure are strings in production, lat/lon numeric fields are
    /// numbers.
    pub(crate) const NHC_CURRENT_STORMS: &str = r#"{
      "activeStorms": [
        {
          "id": "ep082025",
          "binNumber": "CP3",
          "name": "Henriette",
          "classification": "HU",
          "intensity": "75",
          "pressure": "988",
          "latitude": "16.1N",
          "longitude": "144.5W",
          "latitudeNumeric": 16.1,
          "longitudeNumeric": -144.5,
          "movementDir": 285,
          "movementSpeed": 10,
          "lastUpdate": "2025-08-11T15:00:00.000Z"
        },
        {
          "id": "al052025",
          "binNumber": "AT1",
          "name": "Erin",
          "classification": "TS",
          "intensity": "60",
          "pressure": "997",
          "latitudeNumeric": 24.8,
          "longitudeNumeric": -61.2,
          "movementDir": 300,
          "movementSpeed": 14,
          "lastUpdate": "2025-08-11T15:00:00.000Z"
        }
      ]
    }"#;

    /// Aggregate forecast-track query (summary MapServer layer 6).
    pub(crate) const NHC_TRACK_GEOJSON: &str = r#"{
      "type": "FeatureCollection",
      "features": [
        {
          "type": "Feature",
          "geometry": { "type": "LineString",
            "coordinates": [[-144.5, 16.1], [-146.2, 16.9], [-148.0, 17.6]] },
          "properties": { "STORMNAME": "Henriette", "STORMTYPE": "HU", "ADVISNUM": "23" }
        }
      ]
    }"#;

    /// Aggregate cone query (summary MapServer layer 7); lowercase
    /// property spelling exercises the tolerant name lookup.
    pub(crate) const NHC_CONE_GEOJSON: &str = r#"{
      "type": "FeatureCollection",
      "features": [
        {
          "type": "Feature",
          "geometry": { "type": "Polygon",
            "coordinates": [[[-144.0, 15.6], [-148.6, 16.9], [-148.2, 18.4], [-143.9, 16.4], [-144.0, 15.6]]] },
          "properties": { "stormName": "Henriette", "basin": "cp" }
        }
      ]
    }"#;

    /// Quiet-basin states, exactly as fetched live on 2026-06-12.
    pub(crate) const NHC_QUIET: &str = r#"{"activeStorms":[]}"#;
    pub(crate) const JMA_TARGET_TC_QUIET: &str = "[]";

    /// targetTc.json with the object element shape.
    pub(crate) const JMA_TARGET_TC_ACTIVE: &str =
        r#"[{"tropicalCyclone":"TC2105","typhoonNumber":"2106"}]"#;

    /// Per-storm bosai forecast.json (TC2105 IN-FA, reconstructed
    /// from the recon parse of the archived payload): title part with
    /// jp/en name, analysis part (center [lat, lon]!, hPa pressure as
    /// a string, winds in m/s + kt, gale radius in METERS), then
    /// forecast parts with advancedHours + 70% probabilityCircle.
    pub(crate) const JMA_FORECAST: &str = r#"[
      {
        "issue": { "JST": "2021-07-21T22:10:00+09:00", "UTC": "2021-07-21T13:10:00Z" },
        "typhoonNumber": "2106",
        "tropicalCyclone": "TC2105",
        "name": { "jp": "インファ", "en": "IN-FA" }
      },
      {
        "validtime": { "JST": "2021-07-21T21:00:00+09:00", "UTC": "2021-07-21T12:00:00Z" },
        "category": { "jp": "台風", "en": "TY" },
        "center": [23.3, 126.6],
        "pressure": "950",
        "maximumWind": { "sustained": { "mps": 41, "knots": 80 } },
        "galeWarningArea": { "center": [23.3, 126.6], "radius": 330000 }
      },
      {
        "advancedHours": 12,
        "validtime": { "JST": "2021-07-22T09:00:00+09:00", "UTC": "2021-07-22T00:00:00Z" },
        "category": { "jp": "台風", "en": "TY" },
        "center": [23.9, 124.9],
        "pressure": "945",
        "maximumWind": { "sustained": { "mps": 43, "knots": 85 } },
        "probabilityCircle": { "center": [23.9, 124.9], "radius": 70000 }
      },
      {
        "advancedHours": 24,
        "validtime": { "UTC": "2021-07-22T12:00:00Z" },
        "category": { "jp": "台風", "en": "TY" },
        "center": [24.6, 122.9],
        "pressure": "940",
        "probabilityCircle": { "center": [24.6, 122.9], "radius": 110000 }
      }
    ]"#;

    /// Shared past-track file: [lat, lon] waypoints per storm key.
    pub(crate) const JMA_PAST_TRACKS: &str = r#"[
      { "tropicalCyclone": "TC2105",
        "track": [[21.4, 131.9], [22.0, 130.1], [22.7, 128.4], [23.3, 126.6]] },
      { "tropicalCyclone": "TC2104",
        "track": [[18.0, 140.0], [18.5, 139.0]] }
    ]"#;

    /// JTWC RSS shape per the archived Jul 2025 index: one item per
    /// active storm, parenthesized name in the title, per-storm
    /// product links in the description. The wp item must be SKIPPED
    /// (JMA is primary in the West Pacific).
    pub(crate) const JTWC_RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel>
<title>JTWC Tropical Warnings</title>
<item>
<title>Tropical Cyclone 02S (Anahita) Warning #08</title>
<description><![CDATA[<a href="https://www.metoc.navy.mil/jtwc/products/sh0226.tcw">TC Warning Graphic Overlay</a> <a href="https://www.metoc.navy.mil/jtwc/products/sh0226web.txt">TC Warning Text</a>]]></description>
</item>
<item>
<title>Tropical Cyclone 03A (Three) Warning #02</title>
<description><![CDATA[<a href="https://www.metoc.navy.mil/jtwc/products/io0326.tcw">TC Warning Graphic Overlay</a>]]></description>
</item>
<item>
<title>Typhoon 10W (Krosa) Warning #15</title>
<description><![CDATA[<a href="https://www.metoc.navy.mil/jtwc/products/wp1025.tcw">TC Warning Graphic Overlay</a>]]></description>
</item>
</channel></rss>"#;

    /// Fixed-width .tcw warning (columns per the recon notes on
    /// wp1025.tcw): T-lines with tenths-coded lat/lon, intensity kt,
    /// and R034/R050 quadrant radii to be skipped.
    pub(crate) const JTWC_TCW: &str = "SH0226 02S ANAHITA 20260612T0600Z\n\
T000 121S 0633E 045\n\
T012 128S 0621E 055 R034 NE060 SE050 SW040 NW055\n\
T024 137S 0607E 065 R034 NE070 SE060 SW050 NW065 R050 NE030 SE025 SW020 NW025\n\
T048 155S 0585E 070\n";
}

/// Deterministic FeatureCollection built from the embedded recon
/// fixtures, exercising all three normalizers; the cross-module
/// shape-lock snapshot (snapshot_tests::radar_tropical_v1_shape)
/// serializes exactly this.
#[cfg(test)]
pub(crate) fn test_fixture_collection() -> Value {
    let storms = parse_nhc_current(fixtures::NHC_CURRENT_STORMS).expect("nhc fixture parses");
    let mut nhc = nhc_position_features(&storms);
    nhc.extend(nhc_overlay_features(
        fixtures::NHC_TRACK_GEOJSON,
        "forecast_track",
        &storms,
    ));
    nhc.extend(nhc_overlay_features(
        fixtures::NHC_CONE_GEOJSON,
        "cone",
        &storms,
    ));

    let forecast: Value = serde_json::from_str(fixtures::JMA_FORECAST).expect("jma fixture");
    let past: Value = serde_json::from_str(fixtures::JMA_PAST_TRACKS).expect("jma past fixture");
    let jma = normalize_jma_storm("TC2105", &forecast, Some(&past));

    let jtwc = normalize_jtwc_storm("sh0226", Some("Anahita"), fixtures::JTWC_TCW);

    let sources = vec![
        SourceStatus {
            agency: "NOAA NHC + CPHC",
            ok: true,
            storms: position_count(&nhc),
        },
        SourceStatus {
            agency: "JMA / RSMC Tokyo",
            ok: true,
            storms: position_count(&jma),
        },
        SourceStatus {
            agency: "JTWC",
            ok: true,
            storms: position_count(&jtwc),
        },
    ];
    let features: Vec<Feature> = nhc.into_iter().chain(jma).chain(jtwc).collect();
    collection_value(&features, &sources, "2026-06-12T00:00:00Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_kind<'a>(feats: &'a [Feature], kind: &str) -> Vec<&'a Feature> {
        feats.iter().filter(|f| f.properties.kind == kind).collect()
    }

    // ---- NHC ---------------------------------------------------------

    #[test]
    fn nhc_positions_resolve_basin_from_the_bin_not_the_id() {
        let storms = parse_nhc_current(fixtures::NHC_CURRENT_STORMS).unwrap();
        let feats = nhc_position_features(&storms);
        assert_eq!(feats.len(), 2);
        // Henriette: ep-genesis id, CP3 bin -> CPHC identity.
        let h = &feats[0].properties;
        assert_eq!(h.id, "ep082025");
        assert_eq!(h.basin, "central_pacific");
        assert_eq!(h.agency, "NOAA CPHC");
        assert_eq!(h.term, "hurricane");
        assert_eq!(h.classification.as_deref(), Some("HU"));
        assert_eq!(h.intensity_kt, Some(75.0)); // string "75" upstream
        assert_eq!(h.pressure_mb, Some(988.0));
        assert_eq!(h.movement.as_deref(), Some("WNW at 10 kt"));
        assert_eq!(h.updated.as_deref(), Some("2025-08-11T15:00:00.000Z"));
        // GeoJSON order: [lon, lat].
        assert_eq!(
            feats[0].geometry,
            serde_json::json!({"type": "Point", "coordinates": [-144.5, 16.1]})
        );
        // Erin: AT bin -> NHC Atlantic.
        let e = &feats[1].properties;
        assert_eq!((e.basin, e.agency), ("north_atlantic", "NOAA NHC"));
    }

    #[test]
    fn nhc_overlays_inherit_the_matched_storm_identity() {
        let storms = parse_nhc_current(fixtures::NHC_CURRENT_STORMS).unwrap();
        let track = nhc_overlay_features(fixtures::NHC_TRACK_GEOJSON, "forecast_track", &storms);
        assert_eq!(track.len(), 1);
        assert_eq!(track[0].properties.kind, "forecast_track");
        assert_eq!(track[0].properties.id, "ep082025");
        assert_eq!(track[0].properties.agency, "NOAA CPHC");
        assert_eq!(track[0].geometry["type"], "LineString");
        // Cone matches via the lowercase stormName spelling.
        let cone = nhc_overlay_features(fixtures::NHC_CONE_GEOJSON, "cone", &storms);
        assert_eq!(cone.len(), 1);
        assert_eq!(cone[0].properties.kind, "cone");
        assert_eq!(cone[0].properties.name, "Henriette");
        assert_eq!(cone[0].properties.basin, "central_pacific");
        assert_eq!(cone[0].geometry["type"], "Polygon");
    }

    #[test]
    fn nhc_quiet_basin_yields_no_features() {
        let storms = parse_nhc_current(fixtures::NHC_QUIET).unwrap();
        assert!(storms.is_empty());
        assert!(nhc_position_features(&storms).is_empty());
    }

    #[test]
    fn nhc_garbage_is_an_error_not_a_panic() {
        assert!(parse_nhc_current("not json").is_err());
        assert!(parse_nhc_current("{\"wrong\":[]}").is_err());
    }

    // ---- JMA ---------------------------------------------------------

    #[test]
    fn jma_target_tc_accepts_strings_objects_and_quiet() {
        assert!(parse_jma_target_tc(fixtures::JMA_TARGET_TC_QUIET)
            .unwrap()
            .is_empty());
        assert_eq!(
            parse_jma_target_tc(fixtures::JMA_TARGET_TC_ACTIVE).unwrap(),
            ["TC2105"]
        );
        assert_eq!(
            parse_jma_target_tc(r#"["TC2601","TC2601"]"#).unwrap(),
            ["TC2601"]
        );
        assert!(parse_jma_target_tc("{}").is_err());
    }

    #[test]
    fn jma_storm_normalizes_position_tracks_and_cone() {
        let forecast: Value = serde_json::from_str(fixtures::JMA_FORECAST).unwrap();
        let past: Value = serde_json::from_str(fixtures::JMA_PAST_TRACKS).unwrap();
        let feats = normalize_jma_storm("TC2105", &forecast, Some(&past));

        let pos = by_kind(&feats, "position");
        assert_eq!(pos.len(), 1);
        let p = &pos[0].properties;
        assert_eq!(p.name, "IN-FA");
        assert_eq!((p.term, p.agency), ("typhoon", "JMA / RSMC Tokyo"));
        assert_eq!(p.basin, "west_pacific");
        assert_eq!(p.classification.as_deref(), Some("TY"));
        assert_eq!(p.intensity_kt, Some(80.0));
        assert_eq!(p.pressure_mb, Some(950.0));
        assert_eq!(p.updated.as_deref(), Some("2021-07-21T13:10:00Z"));
        // JMA [lat, lon] flipped to GeoJSON [lon, lat].
        assert_eq!(
            pos[0].geometry,
            serde_json::json!({"type": "Point", "coordinates": [126.6, 23.3]})
        );

        // Past track: 4 waypoints from pastTracks.json, flipped.
        let track = by_kind(&feats, "track");
        assert_eq!(track.len(), 1);
        assert_eq!(
            track[0].geometry["coordinates"][0],
            serde_json::json!([131.9, 21.4])
        );

        // Forecast track: analysis center + the two forecast centers.
        let ftrack = by_kind(&feats, "forecast_track");
        assert_eq!(ftrack.len(), 1);
        let coords = ftrack[0].geometry["coordinates"].as_array().unwrap();
        assert_eq!(coords.len(), 3);
        assert_eq!(coords[2], serde_json::json!([122.9, 24.6]));

        // Cone: closed ring hulled over the probability circles,
        // wide enough to contain the 110 km circle at +24h.
        let cone = by_kind(&feats, "cone");
        assert_eq!(cone.len(), 1);
        let ring = cone[0].geometry["coordinates"][0].as_array().unwrap();
        assert!(ring.len() >= 4, "ring has {} points", ring.len());
        assert_eq!(ring.first(), ring.last(), "ring must be closed");
        let lons: Vec<f64> = ring.iter().map(|c| c[0].as_f64().unwrap()).collect();
        let max_lon = lons.iter().cloned().fold(f64::MIN, f64::max);
        let min_lon = lons.iter().cloned().fold(f64::MAX, f64::min);
        // Contains the analysis point and reaches past the +24h
        // circle's western edge (122.9E minus ~1 deg radius).
        assert!(max_lon >= 126.6 && min_lon <= 122.0, "{min_lon}..{max_lon}");
    }

    #[test]
    fn jma_unnamed_td_falls_back_to_the_directory_key() {
        // Pre-typhoon TDs carry an empty en name and a placeholder
        // typhoonNumber; the tropicalCyclone key is the identity.
        let forecast: Value = serde_json::from_str(
            r#"[
              { "issue": { "UTC": "2026-06-12T00:00:00Z" },
                "typhoonNumber": "a", "name": { "jp": "", "en": "" } },
              { "validtime": { "UTC": "2026-06-12T00:00:00Z" },
                "category": { "jp": "熱帯低気圧", "en": "TD" },
                "center": [14.2, 138.0], "pressure": "1004" }
            ]"#,
        )
        .unwrap();
        let feats = normalize_jma_storm("TC2607", &forecast, None);
        assert_eq!(feats.len(), 1);
        assert_eq!(feats[0].properties.name, "TC2607");
        assert_eq!(feats[0].properties.classification.as_deref(), Some("TD"));
    }

    #[test]
    fn jma_past_track_scopes_to_the_requested_storm() {
        let past: Value = serde_json::from_str(fixtures::JMA_PAST_TRACKS).unwrap();
        assert_eq!(jma_past_track(&past, "TC2105").len(), 4);
        assert_eq!(jma_past_track(&past, "TC2104").len(), 2);
        assert!(jma_past_track(&past, "TC9999").is_empty());
    }

    #[test]
    fn cone_ring_needs_circles_and_closes() {
        assert!(cone_ring(Some((20.0, 130.0)), &[]).is_none());
        let ring = cone_ring(Some((20.0, 130.0)), &[(21.0, 129.0, 100_000.0)]).unwrap();
        assert_eq!(ring.first(), ring.last());
        assert!(ring.len() >= 4);
    }

    #[test]
    fn convex_hull_is_a_hull() {
        // A square plus an interior point: the interior point drops.
        let hull = convex_hull(vec![
            (0.0, 0.0),
            (2.0, 0.0),
            (2.0, 2.0),
            (0.0, 2.0),
            (1.0, 1.0),
        ]);
        assert_eq!(hull.len(), 4);
        assert!(!hull.contains(&(1.0, 1.0)));
    }

    // ---- JTWC --------------------------------------------------------

    #[test]
    fn jtwc_rss_scan_survives_multibyte_utf8() {
        // The live RSS is free text and can carry multi-byte UTF-8
        // (accented place names, typographic punctuation); the scanner
        // walks byte offsets, so it must never slice the str directly.
        let rss = "à la Réunion · sh0126.tcw … « io0226 » sh99";
        assert_eq!(jtwc_sector_ids(rss), ["sh0126", "io0226"]);
        // Tiny and empty inputs are fine too.
        assert!(jtwc_sector_ids("").is_empty());
        assert!(jtwc_sector_ids("sh01").is_empty());
        // An id flush against the end of the body is still found.
        assert_eq!(jtwc_sector_ids("products/io0326"), ["io0326"]);
    }

    #[test]
    fn jtwc_rss_yields_sh_and_io_but_never_wp() {
        let ids = jtwc_sector_ids(fixtures::JTWC_RSS);
        assert_eq!(ids, ["sh0226", "io0326"]);
        assert_eq!(
            jtwc_storm_name(fixtures::JTWC_RSS, "sh0226").as_deref(),
            Some("Anahita")
        );
        assert_eq!(
            jtwc_storm_name(fixtures::JTWC_RSS, "io0326").as_deref(),
            Some("Three")
        );
        assert!(jtwc_storm_name(fixtures::JTWC_RSS, "sh9999").is_none());
    }

    #[test]
    fn tcw_parses_tenths_coords_and_skips_radii_tokens() {
        let pts = parse_tcw(fixtures::JTWC_TCW);
        assert_eq!(pts.len(), 4);
        assert_eq!(pts[0].tau, 0);
        assert!((pts[0].lat - -12.1).abs() < 1e-9);
        assert!((pts[0].lon - 63.3).abs() < 1e-9);
        assert_eq!(pts[0].intensity_kt, Some(45.0));
        // The R034/R050 quadrant tokens never pollute the intensity.
        assert_eq!(pts[1].intensity_kt, Some(55.0));
        assert_eq!(pts[3].tau, 48);
        // Explicit-decimal coordinates also parse.
        let pts = parse_tcw("T000 18.7N 131.2E 075\n");
        assert!((pts[0].lat - 18.7).abs() < 1e-9 && (pts[0].lon - 131.2).abs() < 1e-9);
    }

    #[test]
    fn jtwc_storm_normalizes_with_basin_local_term() {
        let feats = normalize_jtwc_storm("sh0226", Some("Anahita"), fixtures::JTWC_TCW);
        assert_eq!(feats.len(), 2);
        let p = &feats[0].properties;
        assert_eq!(p.kind, "position");
        assert_eq!(p.name, "Anahita");
        // lon 63.3 -> SW Indian Ocean -> local term, JTWC issuer.
        assert_eq!(p.basin, "southwest_indian");
        assert_eq!((p.term, p.agency), ("tropical cyclone", "JTWC"));
        assert_eq!(p.intensity_kt, Some(45.0));
        assert_eq!(feats[1].properties.kind, "forecast_track");
        assert_eq!(
            feats[1].geometry["coordinates"].as_array().unwrap().len(),
            4
        );
    }

    #[test]
    fn jtwc_basin_split_matches_the_catalog_boxes() {
        assert_eq!(jtwc_basin("io0326", 88.0), "north_indian");
        assert_eq!(jtwc_basin("sh0226", 55.0), "southwest_indian");
        assert_eq!(jtwc_basin("sh0326", 130.0), "australian");
        assert_eq!(jtwc_basin("sh0426", 175.0), "south_pacific");
        assert_eq!(jtwc_basin("sh0526", -160.0), "south_pacific");
    }

    #[test]
    fn empty_tcw_yields_nothing() {
        assert!(normalize_jtwc_storm("sh0226", None, "NO DATA\n").is_empty());
    }

    // ---- Shared helpers + envelope ------------------------------------

    #[test]
    fn compass_rose_quarters_are_right() {
        assert_eq!(compass(0.0), "N");
        assert_eq!(compass(90.0), "E");
        assert_eq!(compass(285.0), "WNW");
        assert_eq!(compass(359.9), "N");
        assert_eq!(
            movement_label(Some(300.0), Some(14.0)).as_deref(),
            Some("WNW at 14 kt")
        );
        assert_eq!(movement_label(None, Some(14.0)), None);
    }

    #[test]
    fn tolerant_value_helpers_accept_numbers_and_numeric_strings() {
        let v: Value = serde_json::json!({"a": "75", "b": 12.5, "c": "x"});
        assert_eq!(vnum(&v, &["a"]), Some(75.0));
        assert_eq!(vnum(&v, &["missing", "b"]), Some(12.5));
        assert_eq!(vnum(&v, &["c"]), None);
        assert_eq!(vstr(&v, &["c", "a"]), Some("x"));
        // [lat, lon] plausibility gate.
        assert_eq!(
            latlon_pair(&serde_json::json!([23.3, 126.6])),
            Some((23.3, 126.6))
        );
        assert_eq!(latlon_pair(&serde_json::json!([126.6, 23.3])), None);
        assert_eq!(latlon_pair(&serde_json::json!([1, 2, 3])), None);
    }

    #[test]
    fn collection_envelope_reports_per_source_health() {
        let v = test_fixture_collection();
        assert_eq!(v["type"], "FeatureCollection");
        let feats = v["features"].as_array().unwrap();
        // 2 NHC positions + 1 track + 1 cone, 4 JMA features
        // (position/track/forecast_track/cone), 2 JTWC features.
        assert_eq!(feats.len(), 10);
        let sources = v["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[0]["agency"], "NOAA NHC + CPHC");
        assert_eq!(sources[0]["storms"], 2);
        assert_eq!(sources[1]["storms"], 1);
        assert_eq!(sources[2]["storms"], 1);
        assert!(sources.iter().all(|s| s["ok"] == true));
        assert_eq!(v["generated_at"], "2026-06-12T00:00:00Z");
        // Every feature carries the full uniform property bag.
        for f in feats {
            let p = &f["properties"];
            for key in [
                "kind",
                "id",
                "name",
                "term",
                "agency",
                "basin",
                "classification",
                "intensity_kt",
                "pressure_mb",
                "movement",
                "updated",
            ] {
                assert!(p.get(key).is_some(), "missing property '{key}'");
            }
        }
    }

    #[test]
    fn cache_entry_expires_after_ttl() {
        let cache: Cache = Mutex::new(None);
        assert!(cache_get(&cache).is_none());
        *cache.lock().unwrap() = Some((Instant::now(), Arc::new(Value::Null)));
        assert!(cache_get(&cache).is_some());
        // checked_sub: a young monotonic clock cannot underflow.
        if let Some(expired) = Instant::now().checked_sub(CACHE_TTL + Duration::from_secs(1)) {
            *cache.lock().unwrap() = Some((expired, Arc::new(Value::Null)));
            assert!(cache_get(&cache).is_none());
        }
    }
}
