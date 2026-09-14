// What a run and a verdict LOOK like, and what the observer makes of a
// snapshot.
//
// The tables these describe belong to `crate::persistence`; this is the
// shape /api/v1/irrigation/history answers with, the rollups the
// dashboard draws from it, and the run-edge observer that turns "the
// zone stopped reporting running" into a row.

#[cfg(feature = "ssr")]
pub mod ingest;

pub mod rollup;
pub mod types;

#[cfg(feature = "ssr")]
pub use ingest::IngestState;
