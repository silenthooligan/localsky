// Open-Meteo-backed forecast pipeline. Pulls a 7-day daily +
// 48-hour hourly forecast at 30-min intervals from the no-auth
// Open-Meteo REST API and serves it via /api/forecast/* with the
// same arc-swap + SSE pattern Tempest uses. Decoupled from the HA
// `irrigation::ha::Forecast` struct because they have different
// shapes (irrigation cares about ET₀ + skip-check inputs; the
// weather page cares about hour-by-hour conditions + WMO codes for
// iconography).

#[cfg(feature = "ssr")]
pub mod archive;
pub mod model_catalog;
pub(crate) mod precip;
pub mod snapshot;
#[cfg(feature = "ssr")]
pub mod tracks;
pub mod window;

#[cfg(feature = "ssr")]
pub mod open_meteo;
#[cfg(feature = "ssr")]
pub mod store;

#[cfg(feature = "ssr")]
pub use open_meteo::spawn_forecast_refresher;
#[cfg(feature = "ssr")]
pub use store::ForecastStore;

#[cfg(test)]
mod et0_evidence_tests;
