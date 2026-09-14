// The live weather store: every current-conditions reading the
// dashboard, the engine and the HA entities share, arbitrated per field
// across every source on the bus.
//
// It was `tempest::state::TempestStore`, named for the one station that
// wrote it directly. Every source now reaches it the same way, through
// the snapshot bridge, so the name says what it holds rather than who
// first filled it. `tempest::state` re-exports the old names.

pub mod arbitration;
pub mod derived;
pub mod live_store;

// Every item in `arbitration` is ssr-only, so the glob re-export is
// empty in a hydrate build and warns there. Gate it with its contents.
#[cfg(feature = "ssr")]
pub use arbitration::*;
pub use derived::*;
pub use live_store::*;
