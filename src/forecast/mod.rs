// Open-Meteo-backed forecast pipeline. Pulls a 7-day daily +
// 48-hour hourly forecast at 30-min intervals from the no-auth
// Open-Meteo REST API and serves it via /api/forecast/* with the
// same arc-swap + SSE pattern Tempest uses. Decoupled from the HA
// `irrigation::ha::Forecast` struct because they have different
// shapes (irrigation cares about ET₀ + skip-check inputs; the
// weather page cares about hour-by-hour conditions + WMO codes for
// iconography).

pub mod snapshot;

#[cfg(feature = "ssr")]
pub mod refresher;
#[cfg(feature = "ssr")]
pub mod store;

#[cfg(feature = "ssr")]
pub use refresher::spawn_forecast_refresher;
#[cfg(feature = "ssr")]
pub use store::ForecastStore;
