// Compatibility re-exports: the store moved to `crate::weather` when it
// stopped being Tempest-specific. Readers that still say `tempest::state`
// see the same types.

pub use crate::weather::*;

#[cfg(feature = "ssr")]
pub type TempestStore = crate::weather::live_store::LiveWeatherStore;
