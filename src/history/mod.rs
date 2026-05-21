// Run-history persistence layer for the irrigation page. Captures
// completed zone runs to a local SQLite file so the dashboard can
// show a Gantt strip + utilization heatmap that survives across
// HA recorder purges (default 10 days). Retention here is unbounded;
// the table is small (~4 rows/day × 365 = ~1500 rows/year).
//
// Writes happen from a tokio::task::spawn_blocking against rusqlite's
// sync API. Reads happen from the same place when serving
// /api/irrigation/history. The DB handle is wrapped in
// `tokio::sync::Mutex` to serialize access since rusqlite::Connection
// is `!Send` across thread boundaries unless held briefly.

#[cfg(feature = "ssr")]
pub mod db;
#[cfg(feature = "ssr")]
pub mod ingest;

pub mod types;

#[cfg(feature = "ssr")]
pub use db::HistoryDb;
#[cfg(feature = "ssr")]
pub use ingest::IngestState;
