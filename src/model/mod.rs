// The shapes LocalSky reasons about, and answers with.
//
// The irrigation snapshot and everything hanging off it: what a zone is
// doing, what the skip ladder decided and why, the week's water budget,
// the soil forecast. Both targets compile it, because the browser
// renders the same structs the server builds.
//
// It used to live under `ha/`, which made the pure engine import a
// Home Assistant module to name its own inputs and outputs. Home
// Assistant is one integration among several; these types are the
// vocabulary the engine, the API and the UI all speak, so they are
// nobody's integration.

pub mod control;
pub mod flow;
pub mod snapshot;

pub use control::*;
pub use flow::FlowReadout;
pub use snapshot::*;
