// Config layer. The single source of truth for /data/localsky.toml.
//
// Sub-modules:
//   schema.rs   - serde structs + schemars JSON Schema (Phase 2, this commit)
//   loader.rs   - TOML read + env interpolation (next commit)
//   migrate.rs  - versioned config migrations
//   store.rs    - ConfigStore impl writing /data/localsky.toml atomically
//   hot_reload.rs - notify file watch + SIGHUP -> broadcast<ConfigEvent>
//   wizard.rs  - first-run draft state machine
//   env_compat.rs - synthesize Config from legacy v0.1 env vars

// What is SHARED and what is server-only, and why.
//
// Everything here is pure data or pure computation EXCEPT the three
// modules that touch the disk. The shared half compiles for the browser
// too, so the settings UI names the same types, reads the same catalogs
// and runs the same validator the engine does, instead of carrying its
// own copy of each and pinning the copies together with a test. A
// validator that disagrees with dispatch is worse than no validator, and
// the same is true of a form placeholder that disagrees with the engine.
pub mod env_compat;
pub mod kind_labels;
pub mod region;
pub mod schema;

// Server-gated for a reason, not by habit. `validate` parses timezones and
// CIDR blocks, which would drag the whole tz database and an IP-parsing
// crate into the browser bundle for a check the server already performs
// on save. `field_overrides` names weather fields from `ports`, which is
// the adapter layer and genuinely server-side.
#[cfg(feature = "ssr")]
pub mod field_overrides;
#[cfg(feature = "ssr")]
pub mod validate;

// The disk. TOML reads and atomic writes, and the first-run draft state
// machine that persists one. Server only.
#[cfg(feature = "ssr")]
pub mod loader;
#[cfg(feature = "ssr")]
pub mod store;
#[cfg(feature = "ssr")]
pub mod wizard;

pub use schema::*;
#[cfg(feature = "ssr")]
pub use store::FileConfigStore;
