// Live precipitation radar, Leaflet map + multi-layer overlay set
// (RainViewer precip animation, RainViewer IR satellite, IEM NEXRAD
// for high-res US reflectivity, plus a local Tempest strike-ring layer
// drawn from /api/snapshot). The actual Leaflet bootstrap and the
// layer toggle wiring live in /public/radar.js, this component just
// renders the target div + control row and pipes lat/lon/zoom in via
// data-* attrs read at SSR from the WEATHER_APP_LAT / LON / ZOOM env
// vars (sourced from HA's /api/config when the env was seeded).
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

#[cfg(feature = "ssr")]
fn coords() -> (f64, f64, u32) {
    use crate::config::FileConfigStore;
    use std::sync::Arc;

    // Prefer the live deployment.location from the config so changes
    // made in Settings -> Location actually flow through to the radar
    // center. Earlier this read env vars only; env_compat seeds the
    // config from WEATHER_APP_LAT/LON at first boot, but later edits via
    // the settings page never updated the env, so the radar kept
    // recentering on the env defaults (40.0, -75.0, NYC area) regardless
    // of what the user configured.
    let from_config: Option<(f64, f64)> = use_context::<Arc<FileConfigStore>>()
        .and_then(|store| store.load_blocking())
        .map(|cfg| (cfg.deployment.location.lat, cfg.deployment.location.lon))
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
    (lat, lon, zoom)
}

#[cfg(not(feature = "ssr"))]
fn coords() -> (f64, f64, u32) {
    (40.0, -75.0, 8)
}

/// True when the station sits inside IEM's n0r NEXRAD composite, which
/// covers the contiguous US only (Alaska, Hawaii, and Puerto Rico have
/// WSR-88D sites but are not in the composite). A generous CONUS
/// bounding box rather than a coastline polygon: border users (southern
/// Canada, northern Mexico) still get usable reflectivity from nearby
/// US radars, while everyone else skips a layer that would only ever
/// render empty tiles. radar.js reads the verdict from data-nexrad.
fn nexrad_applicable(lat: f64, lon: f64) -> bool {
    (21.0..=50.0).contains(&lat) && (-127.0..=-65.0).contains(&lon)
}

#[cfg(feature = "ssr")]
fn default_layers() -> String {
    use crate::config::FileConfigStore;
    use std::sync::Arc;

    // Same store-from-context read as coords(). The configured list
    // (ui.radar.default_layers) seeds the layer set for browsers with
    // no stored preference; radar.js persists per-browser toggles to
    // localStorage, which then win over this list.
    use_context::<Arc<FileConfigStore>>()
        .and_then(|store| store.load_blocking())
        .map(|cfg| cfg.ui.radar.default_layers.join(","))
        .unwrap_or_else(|| "precip,nexrad,lightning".to_string())
}

#[cfg(not(feature = "ssr"))]
fn default_layers() -> String {
    "precip,nexrad,lightning".to_string()
}

#[component]
pub fn RadarPanel() -> impl IntoView {
    let (lat, lon, zoom) = coords();
    // The non-ssr coords() fallback (40.0, -75.0) sits inside the CONUS
    // box, so the hydrate-side attribute mirrors the ssr default ("1").
    let nexrad = if nexrad_applicable(lat, lon) {
        "1"
    } else {
        "0"
    };
    view! {
        <section class="panel radar-panel">
            <h2 class="panel-title">"Live Radar"</h2>
            <div id="radar-map"
                class="radar-map"
                role="img"
                aria-label="Precipitation radar centered on the station"
                data-lat=lat.to_string()
                data-lon=lon.to_string()
                data-zoom=zoom.to_string()
                data-nexrad=nexrad
                data-default-layers=default_layers()>
            </div>
            // Mobile layer-chip row. radar.js populates this with one chip
            // per overlay when the viewport is <=760px and skips the in-map
            // L.control.layers entirely. On desktop the row is hidden via
            // SCSS and the in-map control wins. Putting the toggles outside
            // the map means they don't cover content at phone widths.
            <div id="radar-layer-chips" class="radar-layer-chips" aria-label="Radar layers"></div>
            <div class="radar-controls">
                <button id="radar-play" class="radar-btn">"⏸ pause"</button>
                <span id="radar-time" class="radar-time"></span>
                <a class="radar-attr" href="https://rainviewer.com" target="_blank" rel="noopener">
                    "RainViewer"
                </a>
                <span class="radar-attr">" · "</span>
                <a class="radar-attr" href="https://mesonet.agron.iastate.edu/" target="_blank" rel="noopener">
                    "IEM NEXRAD"
                </a>
            </div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::nexrad_applicable;

    #[test]
    fn conus_stations_get_nexrad() {
        assert!(nexrad_applicable(28.5, -81.4)); // Orlando
        assert!(nexrad_applicable(47.6, -122.3)); // Seattle
    }

    #[test]
    fn southern_canada_kept_inside_the_box() {
        // Toronto sits inside the bounding box on purpose: border users
        // still get usable returns from nearby US radars.
        assert!(nexrad_applicable(43.7, -79.4));
    }

    #[test]
    fn outside_the_composite_skips_nexrad() {
        assert!(!nexrad_applicable(38.7, -9.1)); // Lisbon
        assert!(!nexrad_applicable(21.3, -157.9)); // Honolulu
        assert!(!nexrad_applicable(61.2, -149.9)); // Anchorage
    }
}
