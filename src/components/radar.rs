// Live precipitation radar — Leaflet map + multi-layer overlay set
// (RainViewer precip animation, RainViewer IR satellite, IEM NEXRAD
// for high-res US reflectivity, plus a local Tempest strike-ring layer
// drawn from /api/snapshot). The actual Leaflet bootstrap and the
// layer toggle wiring live in /public/radar.js — this component just
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
    // recentering on the env defaults (40.0, -75.0 — NYC area) regardless
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
        </section>
    }
}
