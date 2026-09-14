// The refresher: the 10 s tick that turns what the sources, the stores
// and the controllers know into one irrigation snapshot.
//
//   policy.rs    what the config says about how this yard waters
//   evidence.rs  what a pass needs from the database, fetched up front
//   shell.rs     the tick itself: read, prefetch, assemble, overlay,
//                finalize, store
//   observers.rs what a stored snapshot sets in motion: push edges,
//                ledger rows, metrics, the run-edge ingest
//
// The pure assembly lives under `crate::assembly`; the shell is the only
// place a tick reads the clock, and it reads it once.

pub(crate) mod evidence;
pub(crate) mod observers;
pub(crate) mod policy;
pub(crate) mod shell;

pub mod store;

pub use evidence::*;
pub use policy::*;
pub use shell::*;
pub use store::IrrigationStore;

#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod observers_tests;
#[cfg(test)]
mod shell_tests;
