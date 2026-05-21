// IrrigationController adapters + registry.
//
// Adapters at v0.1 ship:
//   dry_run.rs              - no-op records-intent adapter; demo + tests
//   opensprinkler_direct.rs - OS HTTP API (firmware 2.1.9+)
//   ha_service_call.rs      - HA REST service-call wrapper (legacy)
// Deferred (community/planned):
//   esphome_native.rs       - ESPHome native API
//   rachio.rs               - Rachio Gen 2/3 cloud
//
// The ControllerRegistry holds the configured set behind an arc-swap
// so hot-reload via PUT /api/config replaces the active controller
// atomically without dropping in-flight runs.

pub mod dry_run;
pub mod ha_service_call;
pub mod opensprinkler_direct;
pub mod registry;

pub use dry_run::DryRunController;
pub use ha_service_call::HaServiceCall;
pub use opensprinkler_direct::OpenSprinklerDirect;
pub use registry::ControllerRegistry;
