// Live precipitation radar, Leaflet map + multi-layer overlay set
// built dynamically by /public/radar.js from the provider/feature
// catalog (src/radar_catalog.rs). This component renders the target
// div + control row and pipes everything radar.js needs in via data-*
// attrs read at SSR: station lat/lon/zoom, the EFFECTIVE provider
// descriptors (config ui.radar.providers resolved against the
// region-smart recommended set), the full feature catalog, and the
// configured default-visible layer ids. The old data-nexrad region
// flag is superseded by the per-provider coverage resolution and is
// gone.
//
// Lifecycle note: leaflet.js and radar.js are loaded ONCE at app boot
// via app.rs::shell, not per-route. Earlier versions injected the
// <script> tags from inside this component's view; on every route
// swap Leptos reinserted them, the browser re-executed the IIFE in
// radar.js, and MutationObservers stacked up so the second visit to
// /weather sometimes showed a dead map until a full reload. The IIFE's
// MutationObserver picks up #radar-map appearing/disappearing on
// route changes and calls init()/teardown() accordingly, so no script
// reload is needed here.

use leptos::prelude::*;

use crate::radar_catalog;

/// Everything the panel needs from config: station coords + zoom, the
/// configured provider menu (empty = Auto), and the default-visible
/// layer ids.
struct PanelInputs {
    lat: f64,
    lon: f64,
    zoom: u32,
    providers: Vec<String>,
    default_layers: Vec<String>,
}

#[cfg(feature = "ssr")]
fn panel_inputs() -> PanelInputs {
    use crate::config::FileConfigStore;
    use std::sync::Arc;

    // Prefer the live deployment.location from the config so changes
    // made in Settings -> Location actually flow through to the radar
    // center. Earlier this read env vars only; env_compat seeds the
    // config from WEATHER_APP_LAT/LON at first boot, but later edits via
    // the settings page never updated the env, so the radar kept
    // recentering on the env defaults (40.0, -75.0, NYC area) regardless
    // of what the user configured. One load also supplies the radar UI
    // block (provider menu + default layer set).
    let cfg = use_context::<Arc<FileConfigStore>>().and_then(|store| store.load_blocking());

    let from_config: Option<(f64, f64)> = cfg
        .as_ref()
        .map(|c| (c.deployment.location.lat, c.deployment.location.lon))
        .filter(|(lat, lon)| !(*lat == 0.0 && *lon == 0.0));

    let (lat, lon) = from_config.unwrap_or_else(|| {
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

    let zoom = std::env::var("WEATHER_APP_ZOOM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);

    // ui.radar.providers: empty = Auto (region-recommended menu).
    // ui.radar.default_layers seeds the visible set for browsers with
    // no stored preference; radar.js persists per-browser toggles to
    // localStorage, which then win over this list.
    let (providers, default_layers) = cfg
        .map(|c| (c.ui.radar.providers, c.ui.radar.default_layers))
        .unwrap_or_else(|| (Vec::new(), stock_default_layers()));

    PanelInputs {
        lat,
        lon,
        zoom,
        providers,
        default_layers,
    }
}

#[cfg(not(feature = "ssr"))]
fn panel_inputs() -> PanelInputs {
    // Hydrate-side fallback mirrors the stock config at the env-default
    // coordinates so a non-SSR mount renders byte-identical attributes
    // to the SSR default: Auto provider menu (empty list) + the catalog
    // default layer trio.
    PanelInputs {
        lat: 40.0,
        lon: -75.0,
        zoom: 8,
        providers: Vec::new(),
        default_layers: stock_default_layers(),
    }
}

/// The catalog's stock default-visible trio as owned strings; shared
/// by the ssr no-config fallback and the hydrate fallback so they
/// cannot drift from the config schema default (which is built from
/// the same catalog list).
fn stock_default_layers() -> Vec<String> {
    radar_catalog::default_layer_ids()
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Comma-joined canonical layer ids for data-default-layers: legacy
/// ids (precip/nexrad/lightning) normalize to their catalog
/// successors, unknown or retired ids drop, duplicates collapse. An
/// intentionally empty configured list yields "" (start with
/// everything off); radar.js only falls back on attribute ABSENCE.
fn default_layers_attr(ids: &[String]) -> String {
    let mut out: Vec<&'static str> = Vec::new();
    for id in ids {
        if let Some(canon) = radar_catalog::canonical_layer_id(id) {
            if !out.contains(&canon) {
                out.push(canon);
            }
        }
    }
    out.join(",")
}

#[component]
pub fn RadarPanel() -> impl IntoView {
    let inputs = panel_inputs();
    // Resolve the configured menu (or the region recommendation) to
    // descriptors once, server-side; radar.js builds the overlay menu
    // from the serialized JSON and never needs the catalog itself.
    let effective = radar_catalog::effective_providers(&inputs.providers, inputs.lat, inputs.lon);
    let providers_attr = radar_catalog::providers_json(&effective);
    // Feature catalog localizes to the station: the tropical entry's
    // label/endpoint order follow the home basin for these coords.
    let features_attr = radar_catalog::features_json(inputs.lat, inputs.lon);
    let layers_attr = default_layers_attr(&inputs.default_layers);
    // Attribution for the offered providers, rendered server-side so
    // it is correct before (and without) radar.js running.
    let attribution = effective
        .iter()
        .map(|p| p.attribution)
        .collect::<Vec<_>>()
        .join(" · ");
    view! {
        <section class="panel radar-panel">
            <h2 class="panel-title">"Live Radar"</h2>
            // Positioned shell for the JS-built Layers chip + drawer
            // (public/radar.js): they anchor here, OUTSIDE #radar-map,
            // because the map div's role="img" makes its descendants
            // presentational to assistive tech and the drawer is a real
            // dialog. The drawer overlays the map; the map itself never
            // resizes when it slides in.
            <div class="radar-map-shell">
                <div id="radar-map"
                    class="radar-map"
                    role="img"
                    aria-label="Precipitation radar centered on the station"
                    data-lat=inputs.lat.to_string()
                    data-lon=inputs.lon.to_string()
                    data-zoom=inputs.zoom.to_string()
                    data-radar-providers=providers_attr
                    data-radar-features=features_attr
                    data-default-layers=layers_attr>
                </div>
            </div>
            <div class="radar-controls">
                <button id="radar-play" class="radar-btn">"⏸ pause"</button>
                <span id="radar-time" class="radar-time"></span>
                <span id="radar-attributions" class="radar-attr">{attribution}</span>
            </div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fallback() -> PanelInputs {
        // Mirror of the non-ssr panel_inputs(), reproduced here so the
        // test also guards the ssr no-config defaults against drift.
        PanelInputs {
            lat: 40.0,
            lon: -75.0,
            zoom: 8,
            providers: Vec::new(),
            default_layers: stock_default_layers(),
        }
    }

    #[test]
    fn fallback_attrs_pin_the_default_serialization() {
        // Byte-exact pin of what SSR and hydrate both emit for the
        // stock config at the fallback coordinates (CONUS, so Auto
        // resolves to rainviewer + both US sources). radar.js parses
        // exactly this shape from data-radar-providers.
        let inputs = fallback();
        let effective =
            radar_catalog::effective_providers(&inputs.providers, inputs.lat, inputs.lon);
        assert_eq!(
            radar_catalog::providers_json(&effective),
            concat!(
                "[{\"id\":\"rainviewer\",\"label\":\"RainViewer (global composite)\",",
                "\"kind\":\"rainviewer\",\"coverage\":\"global\",\"coverageLabel\":\"Global\",",
                "\"url\":\"https://api.rainviewer.com/public/weather-maps.json\",",
                "\"attribution\":\"RainViewer.com\",\"crossfade\":false},",
                "{\"id\":\"nexrad_iem\",\"label\":\"IEM NEXRAD Base Reflectivity (CONUS)\",",
                "\"kind\":\"wms\",\"coverage\":\"conus\",\"coverageLabel\":\"US (CONUS)\",",
                "\"url\":\"https://mesonet.agron.iastate.edu/cgi-bin/wms/nexrad/n0r-t.cgi\",",
                "\"wmsLayer\":\"nexrad-n0r-wmst\",",
                "\"attribution\":\"Iowa Environmental Mesonet / NWS NEXRAD\",\"crossfade\":true},",
                "{\"id\":\"nowcoast\",",
                "\"label\":\"NOAA nowCOAST Radar Mosaic (US incl. AK/HI/PR/Guam)\",",
                "\"kind\":\"wms\",\"coverage\":\"us_all\",",
                "\"coverageLabel\":\"US (CONUS, Alaska, Hawaii, Caribbean, Guam)\",",
                "\"url\":\"https://nowcoast.noaa.gov/geoserver/observations/weather_radar/wms\",",
                "\"wmsLayer\":\"base_reflectivity_mosaic\",",
                "\"attribution\":\"NOAA/NWS nowCOAST (MRMS)\",\"crossfade\":true}]"
            )
        );
        assert_eq!(
            default_layers_attr(&inputs.default_layers),
            "rainviewer,nexrad_iem,lightning_tempest"
        );
    }

    #[test]
    fn legacy_default_layer_ids_normalize_in_the_attr() {
        // A config written before the catalog (precip/nexrad/lightning,
        // possibly with the retired satellite layer) still seeds the
        // right overlays.
        let legacy: Vec<String> = ["precip", "nexrad", "satellite", "lightning"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            default_layers_attr(&legacy),
            "rainviewer,nexrad_iem,lightning_tempest"
        );
        // Deliberately empty list means start with everything off, not
        // "fall back to defaults": the attribute renders empty.
        assert_eq!(default_layers_attr(&[]), "");
    }

    #[test]
    fn explicit_provider_menu_overrides_region() {
        // A non-empty ui.radar.providers list is the menu, verbatim,
        // even where Auto would have offered more (or fewer) sources.
        let configured: Vec<String> = vec!["geomet_ca".to_string()];
        let effective = radar_catalog::effective_providers(&configured, 40.0, -75.0);
        let json = radar_catalog::providers_json(&effective);
        assert!(json.contains("\"id\":\"geomet_ca\""));
        assert!(!json.contains("\"id\":\"nexrad_iem\""));
    }
}
