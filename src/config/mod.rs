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

pub mod env_compat;
pub mod loader;
pub mod schema;
pub mod store;
pub mod validate;
pub mod wizard;

pub use schema::*;
pub use store::FileConfigStore;
