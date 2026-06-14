// Radar overlay catalog: every map provider and feature layer the Live
// Radar panel can offer, as plain static data with no ssr-only
// dependencies so both the server (which resolves the effective set and
// serializes it onto #radar-map) and the WASM settings UI compile it
// (the gates_catalog precedent). Every external endpoint here was
// verified live on 2026-06-12 (GetCapabilities plus a real GetMap or
// tile fetch returning image/png); do not add providers without the
// same verification.

use serde::Serialize;

/// Frontend machinery a provider needs. `Rainviewer` keeps the animated
/// frame timeline driven by weather-maps.json; `Wms` becomes a plain
/// Leaflet `L.tileLayer.wms` built from `url` + `wms_layer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Rainviewer,
    Wms,
}

/// One radar tile/animation source. Serialized verbatim (camelCase)
/// into the `data-radar-providers` attribute that radar.js builds the
/// overlay menu from, and rendered by the settings UI with coverage
/// and attribution labels.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarProvider {
    /// Stable machine id; what `ui.radar.providers`, `default_layers`,
    /// and the per-browser localStorage prefs speak.
    pub id: &'static str,
    /// Menu/settings display label.
    pub label: &'static str,
    pub kind: ProviderKind,
    /// Machine coverage token consumed by `covers()`:
    /// global | conus | us_all | canada | germany | finland.
    pub coverage: &'static str,
    /// Human coverage label for the settings UI.
    pub coverage_label: &'static str,
    /// Rainviewer: the weather-maps.json catalog URL. Wms: the GetMap
    /// endpoint to hand to L.tileLayer.wms.
    pub url: &'static str,
    /// WMS LAYERS value; absent (and omitted from the JSON) for
    /// non-WMS providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wms_layer: Option<&'static str>,
    /// Attribution line shown in the map controls and settings.
    pub attribution: &'static str,
    /// True for CONUS reflectivity sources that zoom-crossfade against
    /// the RainViewer layer (RainViewer dominant when zoomed out, the
    /// high-res US source dominant when zoomed in). radar.js applies
    /// the crossfade only between rainviewer and a visible provider
    /// with this flag set.
    pub crossfade: bool,
}

/// One non-tile overlay (vector/timeline features). Serialized
/// (camelCase) into `data-radar-features`; behavior is keyed by `id`
/// in radar.js, so these descriptors carry display metadata plus any
/// fetchable endpoints the layer needs. Owned strings (not &'static)
/// because the tropical entry is assembled per-station: its label and
/// endpoint order follow the home basin resolved from the lat/lon.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarFeature {
    pub id: &'static str,
    pub label: String,
    /// Fetchable endpoints, in feature-specific order (see each entry).
    /// Empty when the feature rides an existing fetch (nowcast reuses
    /// the rainviewer weather-maps.json) or a local source (lightning).
    pub endpoints: Vec<String>,
    pub attribution: String,
}

// ---- Tropical cyclone basins ---------------------------------------------
//
// The single "hurricanes" feature (id kept verbatim for persisted layer
// prefs and configs) is basin-aware: the same storms render anywhere on
// the map, but the label, terminology, and endpoint ordering follow the
// station's HOME basin so a Brisbane user sees "Cyclones (BOM)" and a
// Tokyo user sees "Typhoons (JMA / RSMC Tokyo)". Every endpoint below
// was verified live (2026-06-12 recon): key-free, valid quiet-basin
// states confirmed. Where a basin has no verified RSMC machine feed
// (N Indian, S Pacific, SW Indian), the JTWC RSS + per-storm products
// are the documented fallback; we never scrape unverified sources, so
// a basin with nothing verified would simply render empty.

/// The server-side normalizer (api::tropical). It fetches every
/// verified agency feed, normalizes them into one GeoJSON
/// FeatureCollection, and caches 10 minutes; radar.js consumes ONLY
/// this endpoint, so it stays first in the feature's endpoint list.
/// The raw agency endpoints follow for provenance and attribution.
pub const TROPICAL_NORMALIZED_ENDPOINT: &str = "/api/v1/radar/tropical";

/// One tropical-cyclone basin: identity, the local term for a mature
/// storm, the issuing agency, the feature label used when this is the
/// station's home basin, and the verified upstream endpoints (primary
/// feed first, cross-checks after).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TropicalBasin {
    pub id: &'static str,
    /// What a mature tropical cyclone is CALLED here ("hurricane",
    /// "typhoon", "tropical cyclone", "cyclonic storm"); drives the
    /// per-storm tooltip wording and the home-basin feature label.
    pub term: &'static str,
    /// Issuing agency label, as shown in attribution.
    pub agency: &'static str,
    /// Feature label when this is the home basin.
    pub label: &'static str,
    /// Verified upstream endpoints for this basin, primary first.
    pub endpoints: &'static [&'static str],
}

/// NHC/CPHC feed trio shared by the three RSMC Miami/Honolulu basins.
/// CurrentStorms.json carries Central Pacific storms with binNumber
/// CP1-CP5 (verified: archived 2025-08-11 production copy shows
/// ep082025 Henriette as "CP3" after crossing 140W), and the same
/// MapServer hosts live CP track/cone layers, so CPHC coverage needs
/// NO extra integration: accept CP bins and cp* ids on these feeds.
const NHC_ENDPOINTS: &[&str] = &[
    "https://www.nhc.noaa.gov/CurrentStorms.json",
    "https://mapservices.weather.noaa.gov/tropical/rest/services/tropical/NHC_tropical_weather_summary/MapServer/6/query?where=1%3D1&outFields=*&f=geojson",
    "https://mapservices.weather.noaa.gov/tropical/rest/services/tropical/NHC_tropical_weather_summary/MapServer/7/query?where=1%3D1&outFields=*&f=geojson",
];

/// JTWC RSS index: the only verified key-free all-basin safety net.
/// Per-storm warning products ({sh|io}NNYY.tcw etc.) hang off it.
const JTWC_RSS: &str = "https://www.metoc.navy.mil/jtwc/rss/jtwc.rss";

/// Catalog of every basin with a verified source, in fixed order (the
/// first matching basin wins the home label when a station borders
/// two). The terminology and agency assignments follow the WMO RSMC
/// responsibility map; where the RSMC has no verified machine feed the
/// agency below is the verified fallback issuer (JTWC).
pub fn tropical_basins() -> &'static [TropicalBasin] {
    &[
        TropicalBasin {
            id: "north_atlantic",
            term: "hurricane",
            agency: "NOAA NHC",
            label: "Hurricanes (NOAA NHC)",
            endpoints: NHC_ENDPOINTS,
        },
        TropicalBasin {
            id: "east_pacific",
            term: "hurricane",
            agency: "NOAA NHC",
            label: "Hurricanes (NOAA NHC)",
            endpoints: NHC_ENDPOINTS,
        },
        TropicalBasin {
            id: "central_pacific",
            term: "hurricane",
            agency: "NOAA CPHC",
            label: "Hurricanes (NOAA CPHC)",
            endpoints: NHC_ENDPOINTS,
        },
        TropicalBasin {
            // RSMC Tokyo bosai JSON, verified incl. real archived
            // per-storm payloads; JTWC RSS as the secondary cross-check.
            id: "west_pacific",
            term: "typhoon",
            agency: "JMA / RSMC Tokyo",
            label: "Typhoons (JMA / RSMC Tokyo)",
            endpoints: &[
                "https://www.jma.go.jp/bosai/typhoon/data/targetTc.json",
                "https://www.jma.go.jp/bosai/typhoon/data/pastTracks.json",
                JTWC_RSS,
            ],
        },
        TropicalBasin {
            // "Cyclonic storm" is the IMD/RSMC New Delhi term, but New
            // Delhi has no verified machine feed; JTWC io-sector
            // products are the verified fallback issuer.
            id: "north_indian",
            term: "cyclonic storm",
            agency: "JTWC",
            label: "Cyclonic Storms (JTWC)",
            endpoints: &[
                JTWC_RSS,
                "https://www.metoc.navy.mil/jtwc/products/abioweb.txt",
            ],
        },
        TropicalBasin {
            // BOM's anonymous FTP is the sanctioned machine channel
            // (api.weather.bom.gov.au works key-free but its own
            // metadata forbids reuse, so it is deliberately absent).
            // The FTP products are listed for provenance; server-side
            // normalization of Southern Hemisphere storms rides the
            // verified JTWC sh-sector products (reqwest is HTTP-only).
            id: "australian",
            term: "tropical cyclone",
            agency: "BOM",
            label: "Cyclones (BOM)",
            endpoints: &["ftp://ftp.bom.gov.au/anon/gen/fwo/", JTWC_RSS],
        },
        TropicalBasin {
            // RSMC Nadi has no verified feed; JTWC's TWA explicitly
            // covers the South Pacific (verified).
            id: "south_pacific",
            term: "tropical cyclone",
            agency: "JTWC",
            label: "Cyclones (JTWC)",
            endpoints: &[
                JTWC_RSS,
                "https://www.metoc.navy.mil/jtwc/products/abpwweb.txt",
            ],
        },
        TropicalBasin {
            // RSMC La Reunion is WMS raster only (no verified vector
            // feed); JTWC sh-sector fallback.
            id: "southwest_indian",
            term: "tropical cyclone",
            agency: "JTWC",
            label: "Cyclones (JTWC)",
            endpoints: &[
                JTWC_RSS,
                "https://www.metoc.navy.mil/jtwc/products/abioweb.txt",
            ],
        },
    ]
}

pub fn tropical_basin_by_id(id: &str) -> Option<&'static TropicalBasin> {
    tropical_basins().iter().find(|b| b.id == id)
}

/// Home basin(s) for a station. Bounding regions are deliberately
/// generous and OVERLAP at the seams so a station bordering two basins
/// gets both (Cancun sits in the Atlantic box and the East Pacific box;
/// the first catalog match wins the label). Boxes, in catalog order:
///
///   north_atlantic   lat   5..55,  lon -100..-15   (Gulf, Caribbean,
///                    US/Canada east coast, out to the Azores)
///   east_pacific     lat   5..35,  lon -145..-85   (Mexico/C. America
///                    west coast to the 140W CPHC handoff fringe)
///   central_pacific  lat   0..35,  lon -180..-140  (140W to dateline,
///                    Hawaii; CPHC responsibility)
///   west_pacific     lat   0..50,  lon   98..180   (incl. South China
///                    Sea; overlaps NIO at the Malay Peninsula)
///   north_indian     lat   0..30,  lon   45..100   (Arabian Sea + Bay
///                    of Bengal)
///   australian       lat -45..0,   lon   90..160   (90E-160E TCWC
///                    Australia region)
///   south_pacific    lat -40..0,   lon  158..180 and -180..-130
///                    (east of ~160E, RSMC Nadi area; overlaps the
///                    Australian box at the Coral Sea seam)
///   southwest_indian lat -40..0,   lon   30..93    (RSMC La Reunion
///                    area; overlaps the Australian box at 90E)
///
/// A station inside no box (London, inland Eurasia, high latitudes)
/// defaults to the North Atlantic: it matches the pre-basin-aware
/// behavior, and the Atlantic basin is the one English-language
/// "hurricane" framing fits least badly everywhere.
pub fn home_basins(lat: f64, lon: f64) -> Vec<&'static TropicalBasin> {
    let inside = |b: &TropicalBasin| match b.id {
        "north_atlantic" => in_box(lat, lon, 5.0, 55.0, -100.0, -15.0),
        "east_pacific" => in_box(lat, lon, 5.0, 35.0, -145.0, -85.0),
        "central_pacific" => in_box(lat, lon, 0.0, 35.0, -180.0, -140.0),
        "west_pacific" => in_box(lat, lon, 0.0, 50.0, 98.0, 180.0),
        "north_indian" => in_box(lat, lon, 0.0, 30.0, 45.0, 100.0),
        "australian" => in_box(lat, lon, -45.0, 0.0, 90.0, 160.0),
        "south_pacific" => {
            in_box(lat, lon, -40.0, 0.0, 158.0, 180.0)
                || in_box(lat, lon, -40.0, 0.0, -180.0, -130.0)
        }
        "southwest_indian" => in_box(lat, lon, -40.0, 0.0, 30.0, 93.0),
        _ => false,
    };
    let homes: Vec<&'static TropicalBasin> =
        tropical_basins().iter().filter(|b| inside(b)).collect();
    if homes.is_empty() {
        vec![tropical_basin_by_id("north_atlantic").expect("atlantic basin in catalog")]
    } else {
        homes
    }
}

/// Build the basin-aware "hurricanes" feature for a station. The
/// normalized endpoint radar.js actually fetches comes first; the raw
/// agency endpoints follow for provenance, home basin(s) first, every
/// other verified basin after (storms anywhere render when the user
/// pans; the LABEL is what localizes). Attribution lists each
/// contributing agency once, home agency first.
fn tropical_feature(lat: f64, lon: f64) -> RadarFeature {
    let homes = home_basins(lat, lon);
    let mut endpoints: Vec<String> = vec![TROPICAL_NORMALIZED_ENDPOINT.to_string()];
    let mut agencies: Vec<&'static str> = Vec::new();
    let ordered = homes
        .iter()
        .copied()
        .chain(
            tropical_basins()
                .iter()
                .filter(|b| !homes.iter().any(|h| h.id == b.id)),
        )
        .collect::<Vec<_>>();
    for basin in ordered {
        for ep in basin.endpoints {
            if !endpoints.iter().any(|e| e == ep) {
                endpoints.push((*ep).to_string());
            }
        }
        if !agencies.contains(&basin.agency) {
            agencies.push(basin.agency);
        }
    }
    RadarFeature {
        id: "hurricanes",
        label: homes[0].label.to_string(),
        endpoints,
        attribution: agencies.join(" · "),
    }
}

/// Catalog of every radar tile provider, in menu order (global first,
/// then regional). All entries are key-free public services.
pub fn providers() -> &'static [RadarProvider] {
    &[
        RadarProvider {
            // RainViewer-v2 compatible (same weather-maps.json shape:
            // radar.past, radar.nowcast, host; byte-identical tile URL
            // template), but unlike RainViewer's free tier it returns
            // REAL radar nowcast frames. Placed FIRST so it is the
            // regional default radar wherever it has coverage (the
            // frontend animates the first rainviewer-kind provider in
            // recommended(), which preserves catalog order); the global
            // RainViewer entry below stays the fallback everywhere else.
            id: "librewxr",
            label: "Radar + nowcast (LibreWXR)",
            kind: ProviderKind::Rainviewer,
            coverage: "librewxr",
            coverage_label: "Radar + 60 min nowcast (US, Canada, Europe, Japan, Taiwan, SE Asia)",
            url: "https://api.librewxr.net/public/weather-maps.json",
            wms_layer: None,
            attribution: "LibreWXR",
            crossfade: false,
        },
        RadarProvider {
            // Tile path template: {host}{path}/256/{z}/{x}/{y}/{color}/
            // {smooth}_{snow}.png from weather-maps.json radar.past
            // (13 frames, 10-min cadence). Only past frames are served
            // key-free; the nowcast/satellite arrays were empty on
            // every verification fetch (see the nowcast feature entry).
            id: "rainviewer",
            label: "RainViewer (global composite)",
            kind: ProviderKind::Rainviewer,
            coverage: "global",
            coverage_label: "Global",
            url: "https://api.rainviewer.com/public/weather-maps.json",
            wms_layer: None,
            attribution: "RainViewer.com",
            crossfade: false,
        },
        RadarProvider {
            // Time-enabled WMS (5-min steps; omit TIME for latest),
            // verified against Leaflet's 1.1.1/SRS GetMap defaults.
            id: "nexrad_iem",
            label: "IEM NEXRAD Base Reflectivity (CONUS)",
            kind: ProviderKind::Wms,
            coverage: "conus",
            coverage_label: "US (CONUS)",
            url: "https://mesonet.agron.iastate.edu/cgi-bin/wms/nexrad/n0r-t.cgi",
            wms_layer: Some("nexrad-n0r-wmst"),
            attribution: "Iowa Environmental Mesonet / NWS NEXRAD",
            crossfade: true,
        },
        RadarProvider {
            // GeoServer group layer covering the conus/alaska/hawaii/
            // caribbean/guam regional mosaics in one LAYERS value.
            // Verified in both WMS 1.3.0/CRS and 1.1.1/SRS EPSG:3857.
            id: "nowcoast",
            label: "NOAA nowCOAST Radar Mosaic (US incl. AK/HI/PR/Guam)",
            kind: ProviderKind::Wms,
            coverage: "us_all",
            coverage_label: "US (CONUS, Alaska, Hawaii, Caribbean, Guam)",
            url: "https://nowcoast.noaa.gov/geoserver/observations/weather_radar/wms",
            wms_layer: Some("base_reflectivity_mosaic"),
            attribution: "NOAA/NWS nowCOAST (MRMS)",
            crossfade: true,
        },
        RadarProvider {
            // 1km rain-rate composite, 6-min cadence, time-enabled.
            id: "geomet_ca",
            label: "Environment Canada GeoMet Radar (precip rate)",
            kind: ProviderKind::Wms,
            coverage: "canada",
            coverage_label: "Canada",
            url: "https://geo.weather.gc.ca/geomet",
            wms_layer: Some("RADAR_1KM_RRAI"),
            attribution: "Environment and Climate Change Canada (MSC GeoMet)",
            crossfade: false,
        },
        RadarProvider {
            // RADOLAN precip composite, 5-min cadence, CC BY 4.0.
            id: "dwd_de",
            label: "DWD Niederschlagsradar (RADOLAN composite)",
            kind: ProviderKind::Wms,
            coverage: "germany",
            coverage_label: "Germany / Central Europe",
            url: "https://maps.dwd.de/geoserver/dwd/wms",
            wms_layer: Some("dwd:Niederschlagsradar"),
            attribution: "Deutscher Wetterdienst (DWD)",
            crossfade: false,
        },
        RadarProvider {
            // National dBZ composite, 5-min cadence, Finland only
            // (Nordic neighbors are not in this composite).
            id: "fmi_fi",
            label: "FMI Radar Composite (dBZ, Finland)",
            kind: ProviderKind::Wms,
            coverage: "finland",
            coverage_label: "Finland",
            url: "https://openwms.fmi.fi/geoserver/wms",
            wms_layer: Some("Radar:suomi_dbz_eureffin"),
            attribution: "Finnish Meteorological Institute (CC BY 4.0)",
            crossfade: false,
        },
    ]
}

/// Catalog of every feature layer for a station (always offered;
/// `default_layers` and per-browser prefs control which start
/// visible). Takes the station lat/lon because the tropical entry is
/// basin-aware (label + endpoint order localize to the home basin);
/// every other entry is identical everywhere. Ids are fixed and listed
/// in `feature_ids()`.
pub fn features(lat: f64, lon: f64) -> Vec<RadarFeature> {
    vec![
        RadarFeature {
            // Extends the RainViewer timeline into future frames, which
            // the time scrubber/label must clearly mark as forecast.
            // Short-range precipitation forecast sampled from Open-Meteo
            // over the map's bbox (server endpoint below), rendered as an
            // animated heatmap that extends the radar timeline into the
            // future. Replaces the old RainViewer nowcast, whose key-free
            // nowcast array was empty on every fetch (verified 2026-06-12).
            id: "precip_forecast",
            label: "Precipitation forecast".to_string(),
            endpoints: vec!["/api/v1/radar/precip".to_string()],
            attribution: "Open-Meteo".to_string(),
        },
        RadarFeature {
            // Severity-colored NWS alert polygons, refreshed every
            // 2 minutes. ETIQUETTE: api.weather.gov requires a
            // meaningful User-Agent identifying the app and a contact;
            // requests without one risk 403. Accept: application/geo+json.
            id: "warnings_us",
            label: "NWS severe weather alerts (US)".to_string(),
            endpoints: vec![
                "https://api.weather.gov/alerts/active?status=actual&severity=Severe,Extreme"
                    .to_string(),
            ],
            attribution: "NOAA/NWS".to_string(),
        },
        // Basin-aware tropical tracking ("hurricanes" id kept for
        // persistence compat). radar.js fetches endpoints[0], the
        // server-side normalizer; the agency feeds follow for
        // provenance. See the tropical basin section above.
        tropical_feature(lat, lon),
        RadarFeature {
            // Strike layer fed by /api/snapshot's lightning_recent.
            // Two contributing networks, distinguished per-strike by
            // the `source` tag: the on-prem Tempest station (distance
            // only, rendered as rings) and, when a blitzortung source
            // is enabled, the Blitzortung.org community network (true
            // lat/lon, rendered as point markers). radar.js derives
            // the visible legend/attribution from which sources the
            // snapshot actually reports; Blitzortung attribution (CC
            // BY-SA 4.0 + link) is mandatory whenever its strikes
            // show. The id keeps its historical name so persisted
            // layer prefs and configs survive the generalization.
            id: "lightning_tempest",
            label: "Lightning strikes".to_string(),
            endpoints: Vec::new(),
            attribution: "Tempest (local station) / Blitzortung.org contributors (CC BY-SA 4.0)"
                .to_string(),
        },
        RadarFeature {
            // Animated leaflet-velocity particle field. The endpoint
            // is our own server: it makes ONE batched Open-Meteo call
            // per bbox grid (cached ~30 min) and returns grib2json-
            // style U/V records, so the browser never talks to
            // Open-Meteo directly. radar.js appends ?bbox=<map bounds>
            // and skips the feature when the vendored plugin failed to
            // load (no window.L.velocityLayer). Default OFF: it is a
            // forecast-model field, not radar truth.
            id: "wind",
            label: "Wind flow (Open-Meteo)".to_string(),
            endpoints: vec!["/api/v1/radar/windgrid".to_string()],
            attribution: "Open-Meteo".to_string(),
        },
    ]
}

/// The fixed feature-id set, independent of station location (only
/// labels/endpoints localize, never identity). This is what id
/// validation and `canonical_layer_id` key off, so persisted layer
/// prefs resolve without needing a lat/lon.
pub fn feature_ids() -> &'static [&'static str] {
    &[
        "precip_forecast",
        "warnings_us",
        "hurricanes",
        "lightning_tempest",
        "wind",
    ]
}

pub fn provider_by_id(id: &str) -> Option<&'static RadarProvider> {
    providers().iter().find(|p| p.id == id)
}

fn in_box(lat: f64, lon: f64, south: f64, north: f64, west: f64, east: f64) -> bool {
    (south..=north).contains(&lat) && (west..=east).contains(&lon)
}

/// Axis-aligned coverage test for a provider's `coverage` token. Boxes
/// are deliberately generous at the borders (the original NEXRAD CONUS
/// box precedent): a station just outside a composite still sees useful
/// returns from radars near the edge, while stations far away skip
/// layers that would only ever render empty tiles.
pub fn covers(coverage: &str, lat: f64, lon: f64) -> bool {
    match coverage {
        "global" => true,
        // Contiguous US, carried over verbatim from the old
        // nexrad_applicable(): southern Canada and northern Mexico are
        // kept inside on purpose so border users get nearby US radars;
        // Alaska, Hawaii, and Puerto Rico are NOT in IEM's composite.
        "conus" => in_box(lat, lon, 21.0, 50.0, -127.0, -65.0),
        // Union of the regional mosaics nowCOAST's group layer serves:
        // CONUS (same box as above), mainland Alaska (far-west
        // Aleutians past the dateline are out of scope), Hawaii,
        // Caribbean (Puerto Rico + USVI), and Guam/Marianas.
        "us_all" => {
            in_box(lat, lon, 21.0, 50.0, -127.0, -65.0)
                || in_box(lat, lon, 50.0, 72.0, -180.0, -129.0)
                || in_box(lat, lon, 17.0, 24.0, -161.0, -153.0)
                || in_box(lat, lon, 16.0, 20.0, -68.5, -64.0)
                || in_box(lat, lon, 12.0, 21.0, 143.0, 147.0)
        }
        // All provinces and territories; the southern edge dips into
        // the northern-US border strip where Canadian radars still
        // paint useful returns (mirror of the CONUS box including
        // southern Canada).
        "canada" => in_box(lat, lon, 41.0, 84.0, -142.0, -52.0),
        // LibreWXR's real-radar regions, the union that makes it the
        // regional default. US + Canada reuse the EXACT boxes the
        // "us_all" and "canada" arms use (kept identical so US/Canada
        // coverage never drifts between the two providers).
        "librewxr" => {
            // US: CONUS, mainland Alaska, Hawaii, PR/USVI, Guam/Marianas
            // (same boxes as the "us_all" arm).
            in_box(lat, lon, 21.0, 50.0, -127.0, -65.0)
                || in_box(lat, lon, 50.0, 72.0, -180.0, -129.0)
                || in_box(lat, lon, 17.0, 24.0, -161.0, -153.0)
                || in_box(lat, lon, 16.0, 20.0, -68.5, -64.0)
                || in_box(lat, lon, 12.0, 21.0, 143.0, 147.0)
                // Canada (same box as the "canada" arm).
                || in_box(lat, lon, 41.0, 84.0, -142.0, -52.0)
                // El Salvador + Central American neighbors.
                || in_box(lat, lon, 8.0, 18.0, -92.0, -82.0)
                // Europe (EUMETNET OPERA, incl. Italy).
                || in_box(lat, lon, 35.0, 72.0, -12.0, 45.0)
                // Taiwan.
                || in_box(lat, lon, 21.0, 26.0, 119.0, 122.5)
                // Japan.
                || in_box(lat, lon, 24.0, 46.0, 122.0, 146.0)
                // SE Asia (Malaysia/Borneo/Brunei/Singapore/N. Sumatra).
                || in_box(lat, lon, -6.0, 8.0, 95.0, 120.0)
        }
        // Roughly the RADOLAN 900 km composite grid footprint: Germany
        // plus the border fringe (Benelux, Alps, western Poland) where
        // the composite still paints edge returns.
        "germany" => in_box(lat, lon, 45.5, 56.0, 3.5, 19.0),
        // Finland only, per the FMI national composite extent.
        "finland" => in_box(lat, lon, 59.0, 71.0, 19.0, 32.0),
        _ => false,
    }
}

/// Region-smart provider set for a station: global providers always,
/// regional providers when the station sits inside their coverage box.
/// Catalog order is preserved so the menu reads global, then regional.
pub fn recommended(lat: f64, lon: f64) -> Vec<&'static RadarProvider> {
    providers()
        .iter()
        .filter(|p| covers(p.coverage, lat, lon))
        .collect()
}

/// Resolve the configured `ui.radar.providers` list to descriptors.
/// Empty means Auto: the recommended set for the station. Non-empty
/// means exactly this menu, in the configured order, with ANY catalog
/// provider allowed anywhere (comparing an out-of-region source is
/// deliberate freedom, not an error). Unknown ids are skipped (validate
/// warns about them) and duplicates collapse to the first occurrence.
pub fn effective_providers(
    configured: &[String],
    lat: f64,
    lon: f64,
) -> Vec<&'static RadarProvider> {
    if configured.is_empty() {
        return recommended(lat, lon);
    }
    let mut out: Vec<&'static RadarProvider> = Vec::new();
    for id in configured {
        if let Some(p) = provider_by_id(id) {
            if !out.iter().any(|q| q.id == p.id) {
                out.push(p);
            }
        }
    }
    out
}

/// Resolve a configured/stored layer id to its canonical catalog id.
/// Accepts current provider and feature ids verbatim plus the legacy
/// pre-catalog trio. Returns None for unknown ids, including the
/// retired `satellite` IR layer: RainViewer stopped serving the
/// key-free infrared frames (verified empty on every 2026-06-12 fetch),
/// so it has no successor.
pub fn canonical_layer_id(id: &str) -> Option<&'static str> {
    match id {
        "precip" => return Some("rainviewer"),
        "nexrad" => return Some("nexrad_iem"),
        "lightning" => return Some("lightning_tempest"),
        _ => {}
    }
    if let Some(p) = provider_by_id(id) {
        return Some(p.id);
    }
    feature_ids().iter().find(|f| **f == id).copied()
}

/// Stock default-visible layer set: the catalog successors of the old
/// hardcoded precip + NEXRAD + strikes trio. config::schema builds the
/// `ui.radar.default_layers` serde default from this, and the radar
/// panel's non-ssr fallback uses it directly, so the two can never
/// drift apart.
pub fn default_layer_ids() -> &'static [&'static str] {
    &["librewxr", "rainviewer", "nexrad_iem", "lightning_tempest"]
}

/// Serialize a provider set for the `data-radar-providers` attribute.
/// serde emits struct fields in declaration order, so the string is
/// byte-stable for a given input on both targets (the ssr/hydrate
/// render-parity contract).
pub fn providers_json(set: &[&'static RadarProvider]) -> String {
    serde_json::to_string(set).unwrap_or_else(|_| "[]".to_string())
}

/// Serialize the full feature catalog for `data-radar-features`. Takes
/// the station coordinates because the tropical entry localizes to the
/// home basin; same byte-stability contract as `providers_json` (serde
/// emits fields in declaration order on both targets).
pub fn features_json(lat: f64, lon: f64) -> String {
    serde_json::to_string(&features(lat, lon)).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_unique_across_providers_and_features() {
        let mut seen = std::collections::HashSet::new();
        for p in providers() {
            assert!(seen.insert(p.id), "duplicate catalog id '{}'", p.id);
        }
        for f in feature_ids() {
            assert!(seen.insert(*f), "duplicate catalog id '{f}'");
        }
    }

    #[test]
    fn feature_ids_match_the_built_catalog_everywhere() {
        // Identity never localizes: only labels/endpoints do. Probe a
        // station per basin plus the no-basin default.
        for (lat, lon) in [
            (28.5, -81.4),  // Orlando
            (35.7, 139.7),  // Tokyo
            (-27.5, 153.0), // Brisbane
            (21.3, -157.9), // Honolulu
            (51.5, -0.1),   // London
            (19.1, 72.9),   // Mumbai
            (-18.1, 178.4), // Suva
            (-20.2, 57.5),  // Mauritius
        ] {
            let built: Vec<&str> = features(lat, lon).iter().map(|f| f.id).collect();
            assert_eq!(built, feature_ids(), "feature ids drifted at ({lat},{lon})");
        }
    }

    #[test]
    fn wms_providers_carry_a_layer_and_rainviewer_does_not() {
        for p in providers() {
            match p.kind {
                ProviderKind::Wms => assert!(p.wms_layer.is_some(), "{} missing wms_layer", p.id),
                ProviderKind::Rainviewer => assert!(p.wms_layer.is_none()),
            }
        }
    }

    fn ids(set: &[&'static RadarProvider]) -> Vec<&'static str> {
        set.iter().map(|p| p.id).collect()
    }

    #[test]
    fn orlando_recommends_global_plus_both_us_sources() {
        // LibreWXR leads (regional default) ahead of the global
        // RainViewer fallback and the two US WMS sources.
        assert_eq!(
            ids(&recommended(28.5, -81.4)),
            ["librewxr", "rainviewer", "nexrad_iem", "nowcoast"]
        );
    }

    #[test]
    fn lisbon_recommends_librewxr_then_global() {
        // Inside LibreWXR's Europe box: LibreWXR is the default radar,
        // RainViewer stays as the global fallback.
        assert_eq!(ids(&recommended(38.7, -9.1)), ["librewxr", "rainviewer"]);
    }

    #[test]
    fn sydney_outside_librewxr_leads_with_rainviewer() {
        // No LibreWXR region covers Australia, so the global RainViewer
        // is the default radar there (region-aware fallback).
        let got = ids(&recommended(-33.9, 151.2));
        assert!(!got.contains(&"librewxr"), "librewxr should be absent");
        assert_eq!(got.first(), Some(&"rainviewer"));
    }

    #[test]
    fn toronto_includes_geomet_and_keeps_border_nexrad() {
        // Border users get both national composites on purpose.
        let got = ids(&recommended(43.7, -79.4));
        assert!(got.contains(&"geomet_ca"));
        assert!(got.contains(&"nexrad_iem"));
    }

    #[test]
    fn berlin_recommends_global_plus_dwd() {
        assert_eq!(
            ids(&recommended(52.5, 13.4)),
            ["librewxr", "rainviewer", "dwd_de"]
        );
    }

    #[test]
    fn helsinki_recommends_global_plus_fmi() {
        assert_eq!(
            ids(&recommended(60.2, 24.9)),
            ["librewxr", "rainviewer", "fmi_fi"]
        );
    }

    #[test]
    fn off_conus_us_keeps_nowcoast_but_not_iem() {
        // Honolulu and Anchorage have WSR-88D sites in the nowCOAST
        // mosaics but are outside IEM's CONUS-only composite.
        for (lat, lon) in [(21.3, -157.9), (61.2, -149.9)] {
            let got = ids(&recommended(lat, lon));
            assert!(got.contains(&"nowcoast"), "({lat},{lon}) missing nowcoast");
            assert!(!got.contains(&"nexrad_iem"), "({lat},{lon}) has nexrad_iem");
        }
    }

    #[test]
    fn effective_empty_means_auto() {
        assert_eq!(
            ids(&effective_providers(&[], 28.5, -81.4)),
            ids(&recommended(28.5, -81.4))
        );
    }

    #[test]
    fn effective_explicit_list_is_honored_anywhere() {
        // A Lisbon user deliberately comparing a US source gets exactly
        // that menu, in their order; unknowns drop, duplicates collapse.
        let configured: Vec<String> = ["nowcoast", "sharknado", "rainviewer", "nowcoast"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            ids(&effective_providers(&configured, 38.7, -9.1)),
            ["nowcoast", "rainviewer"]
        );
    }

    #[test]
    fn legacy_layer_ids_normalize_and_satellite_is_retired() {
        assert_eq!(canonical_layer_id("precip"), Some("rainviewer"));
        assert_eq!(canonical_layer_id("nexrad"), Some("nexrad_iem"));
        assert_eq!(canonical_layer_id("lightning"), Some("lightning_tempest"));
        assert_eq!(canonical_layer_id("satellite"), None);
        assert_eq!(canonical_layer_id("warnings_us"), Some("warnings_us"));
        assert_eq!(canonical_layer_id("geomet_ca"), Some("geomet_ca"));
        assert_eq!(canonical_layer_id("sharknado"), None);
    }

    #[test]
    fn default_layer_ids_are_canonical() {
        for id in default_layer_ids() {
            assert_eq!(canonical_layer_id(id), Some(*id));
        }
    }

    #[test]
    fn provider_json_shape_is_pinned() {
        // Pins the exact serialization radar.js parses from
        // data-radar-providers: camelCase keys in declaration order,
        // wmsLayer omitted for non-WMS providers.
        let one = providers_json(&[provider_by_id("nexrad_iem").unwrap()]);
        assert_eq!(
            one,
            concat!(
                "[{\"id\":\"nexrad_iem\",\"label\":\"IEM NEXRAD Base Reflectivity (CONUS)\",",
                "\"kind\":\"wms\",\"coverage\":\"conus\",\"coverageLabel\":\"US (CONUS)\",",
                "\"url\":\"https://mesonet.agron.iastate.edu/cgi-bin/wms/nexrad/n0r-t.cgi\",",
                "\"wmsLayer\":\"nexrad-n0r-wmst\",",
                "\"attribution\":\"Iowa Environmental Mesonet / NWS NEXRAD\",",
                "\"crossfade\":true}]"
            )
        );
        let rv = providers_json(&[provider_by_id("rainviewer").unwrap()]);
        assert!(!rv.contains("wmsLayer"));
        assert!(rv.contains("\"kind\":\"rainviewer\""));
    }

    #[test]
    fn feature_json_shape_is_pinned() {
        // Orlando: the Atlantic home basin, so the tropical label is
        // the NHC one. The serialization contract radar.js parses from
        // data-radar-features is camelCase keys in declaration order.
        let json = features_json(28.5, -81.4);
        assert!(json.starts_with(concat!(
            "[{\"id\":\"precip_forecast\",\"label\":\"Precipitation forecast\",",
            "\"endpoints\":[\"/api/v1/radar/precip\"],\"attribution\":\"Open-Meteo\"}"
        )));
        assert!(json.contains(concat!(
            "{\"id\":\"warnings_us\",\"label\":\"NWS severe weather alerts (US)\",",
            "\"endpoints\":[\"https://api.weather.gov/alerts/active",
            "?status=actual&severity=Severe,Extreme\"],",
            "\"attribution\":\"NOAA/NWS\"}"
        )));
        assert!(json.contains("{\"id\":\"hurricanes\",\"label\":\"Hurricanes (NOAA NHC)\","));
        // The wind feature points at our own windgrid endpoint.
        assert!(json.contains(concat!(
            "{\"id\":\"wind\",\"label\":\"Wind flow (Open-Meteo)\",",
            "\"endpoints\":[\"/api/v1/radar/windgrid\"],",
            "\"attribution\":\"Open-Meteo\"}"
        )));
    }

    #[test]
    fn wind_feature_is_cataloged_but_not_default_on() {
        assert!(feature_ids().contains(&"wind"));
        assert!(!default_layer_ids().contains(&"wind"));
    }

    // ---- Tropical basin picker --------------------------------------

    fn tropical_for(lat: f64, lon: f64) -> RadarFeature {
        features(lat, lon)
            .into_iter()
            .find(|f| f.id == "hurricanes")
            .expect("hurricanes feature present")
    }

    fn home_ids(lat: f64, lon: f64) -> Vec<&'static str> {
        home_basins(lat, lon).iter().map(|b| b.id).collect()
    }

    #[test]
    fn orlando_is_hurricane_country() {
        assert_eq!(home_ids(28.5, -81.4), ["north_atlantic"]);
        let f = tropical_for(28.5, -81.4);
        assert_eq!(f.label, "Hurricanes (NOAA NHC)");
        // Normalizer first, then the home-basin NHC feeds.
        assert_eq!(f.endpoints[0], TROPICAL_NORMALIZED_ENDPOINT);
        assert_eq!(
            f.endpoints[1],
            "https://www.nhc.noaa.gov/CurrentStorms.json"
        );
        // Every other verified basin's feeds follow (panning renders
        // storms anywhere), e.g. the JMA enumerator is in the tail.
        assert!(f
            .endpoints
            .iter()
            .any(|e| e.ends_with("/typhoon/data/targetTc.json")));
        // Home agency leads the attribution.
        assert!(f.attribution.starts_with("NOAA NHC"));
    }

    #[test]
    fn tokyo_gets_typhoons_from_jma() {
        assert_eq!(home_ids(35.7, 139.7), ["west_pacific"]);
        let f = tropical_for(35.7, 139.7);
        assert_eq!(f.label, "Typhoons (JMA / RSMC Tokyo)");
        assert_eq!(f.endpoints[0], TROPICAL_NORMALIZED_ENDPOINT);
        assert!(f.endpoints[1].ends_with("/typhoon/data/targetTc.json"));
        assert!(f.attribution.starts_with("JMA / RSMC Tokyo"));
    }

    #[test]
    fn brisbane_gets_cyclones_from_bom() {
        assert_eq!(home_ids(-27.5, 153.0), ["australian"]);
        let f = tropical_for(-27.5, 153.0);
        assert_eq!(f.label, "Cyclones (BOM)");
        // The sanctioned BOM channel (anonymous FTP) leads the raw
        // endpoints; the undocumented app API is deliberately absent.
        assert_eq!(f.endpoints[1], "ftp://ftp.bom.gov.au/anon/gen/fwo/");
        assert!(!f.endpoints.iter().any(|e| e.contains("api.weather.bom")));
        assert!(f.attribution.starts_with("BOM"));
    }

    #[test]
    fn honolulu_is_cphc_hurricane_coverage_on_the_shipped_nhc_feeds() {
        assert_eq!(home_ids(21.3, -157.9), ["central_pacific"]);
        let f = tropical_for(21.3, -157.9);
        assert_eq!(f.label, "Hurricanes (NOAA CPHC)");
        let basin = tropical_basin_by_id("central_pacific").unwrap();
        assert_eq!(basin.term, "hurricane");
        // CPHC coverage rides the SAME verified NHC feeds (CP1-CP5
        // bins in CurrentStorms.json + CP MapServer layers): the
        // central_pacific endpoints are byte-identical to the Atlantic
        // basin's, no new integration.
        assert_eq!(
            basin.endpoints,
            tropical_basin_by_id("north_atlantic").unwrap().endpoints
        );
    }

    #[test]
    fn london_defaults_to_the_atlantic_basin() {
        // Inside no basin box: the documented default applies.
        assert_eq!(home_ids(51.5, -0.1), ["north_atlantic"]);
        assert_eq!(tropical_for(51.5, -0.1).label, "Hurricanes (NOAA NHC)");
    }

    #[test]
    fn cancun_borders_two_basins_and_gets_both() {
        // Atlantic and East Pacific boxes overlap across Mexico and
        // Central America on purpose; catalog order picks the label.
        assert_eq!(home_ids(21.2, -86.8), ["north_atlantic", "east_pacific"]);
        assert_eq!(tropical_for(21.2, -86.8).label, "Hurricanes (NOAA NHC)");
    }

    #[test]
    fn mumbai_and_suva_and_mauritius_fall_back_to_jtwc() {
        // Basins with no verified RSMC machine feed use the verified
        // JTWC fallback (never an unverified scrape) and keep their
        // local term.
        for (lat, lon, basin, term, label) in [
            (
                19.1,
                72.9,
                "north_indian",
                "cyclonic storm",
                "Cyclonic Storms (JTWC)",
            ),
            (
                -18.1,
                178.4,
                "south_pacific",
                "tropical cyclone",
                "Cyclones (JTWC)",
            ),
            (
                -20.2,
                57.5,
                "southwest_indian",
                "tropical cyclone",
                "Cyclones (JTWC)",
            ),
        ] {
            assert_eq!(home_ids(lat, lon), [basin], "({lat},{lon})");
            let f = tropical_for(lat, lon);
            assert_eq!(f.label, label, "({lat},{lon})");
            assert_eq!(tropical_basin_by_id(basin).unwrap().term, term);
            assert!(f.endpoints[1].contains("metoc.navy.mil/jtwc"));
        }
    }

    #[test]
    fn basin_catalog_is_sane() {
        let mut seen = std::collections::HashSet::new();
        for b in tropical_basins() {
            assert!(seen.insert(b.id), "duplicate basin id '{}'", b.id);
            assert!(!b.term.is_empty() && !b.agency.is_empty());
            assert!(!b.endpoints.is_empty(), "{} has no endpoints", b.id);
            for ep in b.endpoints {
                assert!(
                    ep.starts_with("https://") || ep.starts_with("ftp://"),
                    "{} endpoint '{}' is neither https nor ftp",
                    b.id,
                    ep
                );
            }
        }
    }

    #[test]
    fn tropical_endpoints_are_deduped_and_cover_every_basin() {
        let f = tropical_for(28.5, -81.4);
        let mut seen = std::collections::HashSet::new();
        for e in &f.endpoints {
            assert!(seen.insert(e.clone()), "duplicate endpoint '{e}'");
        }
        // Every verified basin's every endpoint is represented.
        for b in tropical_basins() {
            for ep in b.endpoints {
                assert!(
                    f.endpoints.iter().any(|e| e == ep),
                    "{} endpoint '{}' missing from the feature",
                    b.id,
                    ep
                );
            }
        }
    }

    #[test]
    fn crossfade_flags_only_conus_reflectivity_sources() {
        let flagged: Vec<&str> = providers()
            .iter()
            .filter(|p| p.crossfade)
            .map(|p| p.id)
            .collect();
        assert_eq!(flagged, ["nexrad_iem", "nowcoast"]);
    }
}
