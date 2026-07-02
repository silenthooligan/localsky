// IrrigationController adapters + registry.
//
// Adapters shipped:
//   bhyve.rs                - Orbit B-hyve cloud (api.orbitbhyve.com)
//   dry_run.rs              - no-op records-intent adapter; demo + tests
//   ha_service_call.rs      - HA REST service-call wrapper (legacy)
//   hydrawise.rs            - Hunter Hydrawise cloud (app.hydrawise.com)
//   mqtt_command.rs         - generic MQTT command-sink (ESPHome MQTT,
//                             Tasmota, Sonoff/Shelly MQTT, DIY relays)
//   opensprinkler_direct.rs - OS HTTP API (firmware 2.1.9+)
//   rachio.rs               - Rachio Gen 2/3/Smart Hose Timer cloud
//   rainbird.rs             - Rain Bird LNK2 cloud (rdz-rest.rainbird.com)
// Deferred:
//   esphome_native.rs       - ESPHome native API (binary protocol)
//
// The ControllerRegistry holds the configured set behind an arc-swap
// so hot-reload via PUT /api/config replaces the active controller
// atomically without dropping in-flight runs.

pub mod bhyve;
pub mod dry_run;
pub mod ha_service_call;
pub mod http_generic;
pub mod hydrawise;
pub mod mqtt_command;
pub mod opensprinkler_direct;
pub mod rachio;
pub mod rainbird;
pub mod reaper;
pub mod registry;

// P1-3: trait-contract conformance harness (offline-testable adapters).
#[cfg(test)]
mod conformance;

pub use bhyve::Bhyve;
pub use dry_run::DryRunController;
pub use ha_service_call::HaServiceCall;
pub use http_generic::HttpGeneric;
pub use hydrawise::Hydrawise;
pub use mqtt_command::MqttCommand;
pub use opensprinkler_direct::OpenSprinklerDirect;
pub use rachio::Rachio;
pub use rainbird::Rainbird;
pub use registry::ControllerRegistry;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// P0-8: per-zone async lock that serializes Run dispatch on the same zone
/// across EVERY dispatch path (manual API, manual scheduler, smart-morning
/// cycle). Two near-simultaneous `run_zone` calls on one zone otherwise race
/// the hardware shutoff timer (last-writer-wins on HTTP, two timers closing at
/// the shorter duration on MQTT) and double-write the run row. Lazily created
/// per slug. ONLY Run takes it, so a Stop / StopAll is never blocked behind a
/// running zone. Held only across the `run_zone` dispatch, never the run
/// duration (the controller owns the shutoff), so it cannot block a later
/// manual run for the length of a cycle. The std mutex guarding the map is
/// dropped before the caller awaits the tokio mutex, so it is never held across
/// an await.
pub fn zone_run_lock(zone: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    guard
        .entry(zone.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}
