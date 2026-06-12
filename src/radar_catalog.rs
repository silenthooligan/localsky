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
/// fetchable endpoints the layer needs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarFeature {
    pub id: &'static str,
    pub label: &'static str,
    /// Fetchable endpoints, in feature-specific order (see each entry).
    /// Empty when the feature rides an existing fetch (nowcast reuses
    /// the rainviewer weather-maps.json) or a local source (lightning).
    pub endpoints: &'static [&'static str],
    pub attribution: &'static str,
}

/// Catalog of every radar tile provider, in menu order (global first,
/// then regional). All entries are key-free public services.
pub fn providers() -> &'static [RadarProvider] {
    &[
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

/// Catalog of every feature layer (always offered; `default_layers`
/// and per-browser prefs control which start visible).
pub fn features() -> &'static [RadarFeature] {
    &[
        RadarFeature {
            // Extends the RainViewer timeline into future frames, which
            // the time scrubber/label must clearly mark as forecast.
            // No endpoint of its own: frames come from the radar.nowcast
            // array of the weather-maps.json the rainviewer provider
            // already fetches. CAVEAT (verified 2026-06-12): that array
            // was empty on every key-free fetch, so the layer has to
            // degrade gracefully to "no forecast frames".
            id: "nowcast",
            label: "Forecast frames (RainViewer nowcast)",
            endpoints: &[],
            attribution: "RainViewer.com",
        },
        RadarFeature {
            // Severity-colored NWS alert polygons, refreshed every
            // 2 minutes. ETIQUETTE: api.weather.gov requires a
            // meaningful User-Agent identifying the app and a contact;
            // requests without one risk 403. Accept: application/geo+json.
            id: "warnings_us",
            label: "NWS severe weather alerts (US)",
            endpoints: &["https://api.weather.gov/alerts/active?status=actual&severity=Severe,Extreme"],
            attribution: "NOAA/NWS",
        },
        RadarFeature {
            // Endpoints in order: active-storm summary JSON, then the
            // aggregate forecast track (layer 6) and cone (layer 7)
            // GeoJSON queries from the NHC summary MapServer. All three
            // return valid (possibly empty) payloads when the basin is
            // quiet, so the layer needs a graceful empty state.
            id: "hurricanes",
            label: "Hurricanes (NOAA NHC track + cone)",
            endpoints: &[
                "https://www.nhc.noaa.gov/CurrentStorms.json",
                "https://mapservices.weather.noaa.gov/tropical/rest/services/tropical/NHC_tropical_weather_summary/MapServer/6/query?where=1%3D1&outFields=*&f=geojson",
                "https://mapservices.weather.noaa.gov/tropical/rest/services/tropical/NHC_tropical_weather_summary/MapServer/7/query?where=1%3D1&outFields=*&f=geojson",
            ],
            attribution: "NOAA/NHC",
        },
        RadarFeature {
            // Existing local strike-ring layer fed by the on-prem
            // Tempest station via /api/snapshot; carried forward as-is.
            id: "lightning_tempest",
            label: "Lightning (Tempest station)",
            endpoints: &[],
            attribution: "Tempest (local station)",
        },
    ]
}

pub fn provider_by_id(id: &str) -> Option<&'static RadarProvider> {
    providers().iter().find(|p| p.id == id)
}

pub fn feature_by_id(id: &str) -> Option<&'static RadarFeature> {
    features().iter().find(|f| f.id == id)
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
    feature_by_id(id).map(|f| f.id)
}

/// Stock default-visible layer set: the catalog successors of the old
/// hardcoded precip + NEXRAD + strikes trio. config::schema builds the
/// `ui.radar.default_layers` serde default from this, and the radar
/// panel's non-ssr fallback uses it directly, so the two can never
/// drift apart.
pub fn default_layer_ids() -> &'static [&'static str] {
    &["rainviewer", "nexrad_iem", "lightning_tempest"]
}

/// Serialize a provider set for the `data-radar-providers` attribute.
/// serde emits struct fields in declaration order, so the string is
/// byte-stable for a given input on both targets (the ssr/hydrate
/// render-parity contract).
pub fn providers_json(set: &[&'static RadarProvider]) -> String {
    serde_json::to_string(set).unwrap_or_else(|_| "[]".to_string())
}

/// Serialize the full feature catalog for `data-radar-features`.
pub fn features_json() -> String {
    serde_json::to_string(features()).unwrap_or_else(|_| "[]".to_string())
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
        for f in features() {
            assert!(seen.insert(f.id), "duplicate catalog id '{}'", f.id);
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
        assert_eq!(
            ids(&recommended(28.5, -81.4)),
            ["rainviewer", "nexrad_iem", "nowcoast"]
        );
    }

    #[test]
    fn lisbon_recommends_global_only() {
        assert_eq!(ids(&recommended(38.7, -9.1)), ["rainviewer"]);
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
        assert_eq!(ids(&recommended(52.5, 13.4)), ["rainviewer", "dwd_de"]);
    }

    #[test]
    fn helsinki_recommends_global_plus_fmi() {
        assert_eq!(ids(&recommended(60.2, 24.9)), ["rainviewer", "fmi_fi"]);
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
        let json = features_json();
        assert!(json.starts_with("[{\"id\":\"nowcast\","));
        assert!(json.contains(concat!(
            "{\"id\":\"warnings_us\",\"label\":\"NWS severe weather alerts (US)\",",
            "\"endpoints\":[\"https://api.weather.gov/alerts/active",
            "?status=actual&severity=Severe,Extreme\"],",
            "\"attribution\":\"NOAA/NWS\"}"
        )));
        // Hurricanes carries summary JSON + track + cone, in that order.
        let hur = feature_by_id("hurricanes").unwrap();
        assert_eq!(hur.endpoints.len(), 3);
        assert!(hur.endpoints[1].contains("/MapServer/6/query"));
        assert!(hur.endpoints[2].contains("/MapServer/7/query"));
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
