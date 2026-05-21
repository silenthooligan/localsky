// Forecast components for the Weather page. Two views consuming
// ForecastSnapshot: a 48-hour rolling hourly strip and a 7-day daily
// summary card row. Both type-erase via .into_any() to keep rustc's
// monomorphization budget happy on this big nested view tree.

pub mod daily;
pub mod glyph;
pub mod hourly;

pub use daily::DailyForecast;
pub use hourly::HourlyForecast;
