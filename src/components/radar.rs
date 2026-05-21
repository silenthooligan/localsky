// Live precipitation radar — Leaflet map + multi-layer overlay set
// (RainViewer precip animation, RainViewer IR satellite, IEM NEXRAD
// for high-res US reflectivity, plus a local Tempest strike-ring layer
// drawn from /api/snapshot). The actual Leaflet bootstrap and the
// layer toggle wiring live in /public/radar.js — this component just
// renders the target div + control row and pipes lat/lon/zoom in via
// data-* attrs read at SSR from the WEATHER_APP_LAT / LON / ZOOM env
// vars (sourced from HA's /api/config when the env was seeded).

use leptos::prelude::*;

#[cfg(feature = "ssr")]
fn coords() -> (f64, f64, u32) {
    let lat = std::env::var("WEATHER_APP_LAT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40.0);
    let lon = std::env::var("WEATHER_APP_LON")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-75.0);
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

#[component]
pub fn RadarPanel() -> impl IntoView {
    let (lat, lon, zoom) = coords();
    view! {
        <section class="panel radar-panel">
            <h2 class="panel-title">"Live Radar"</h2>
            <div id="radar-map"
                class="radar-map"
                role="img"
                aria-label="Precipitation radar centered on the station"
                data-lat=lat.to_string()
                data-lon=lon.to_string()
                data-zoom=zoom.to_string()>
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
            <script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"
                integrity="sha256-20nQCchB9co0qIjJZRGuk2/Z9VM+kNiyxNV1lvTlZBo="
                crossorigin=""></script>
            <script src="/radar.js" defer></script>
        </section>
    }
}
