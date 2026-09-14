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

/// The smallest run a controller of this kind can be told, in seconds.
///
/// The adapters own this truth in their `supports()`; this is the same
/// answer for a config entry that has not been built into an adapter
/// yet, which is what the watering policy has in hand when it prices a
/// morning. B-hyve and Rain Bird take whole minutes; everything else
/// takes seconds.
pub fn duration_quantum_for(kind: &crate::config::schema::ControllerKind) -> u32 {
    use crate::config::schema::ControllerKind as K;
    match kind {
        K::Bhyve(_) => bhyve::DURATION_QUANTUM_S,
        K::Rainbird(_) => rainbird::DURATION_QUANTUM_S,
        _ => 1,
    }
}

/// The quantum of the controller the registry would pick as default:
/// the entry flagged `default`, else the lowest enabled id, mirroring
/// `ControllerRegistry::set`. 1 when nothing is configured.
pub fn default_duration_quantum_s(cfg: &crate::config::schema::Config) -> u32 {
    let enabled = cfg.controllers.iter().filter(|c| c.enabled);
    let picked = enabled
        .clone()
        .rfind(|c| c.default)
        .or_else(|| enabled.min_by(|a, b| a.id.cmp(&b.id)));
    picked.map_or(1, |c| duration_quantum_for(&c.controller))
}

pub mod bhyve;
pub mod ceiling;
pub mod dispatch;
pub mod dry_run;
pub mod flow;
pub mod guard;
pub mod ha_service_call;
pub mod http_generic;
pub mod hydrawise;
pub mod mqtt_command;
pub mod opensprinkler_direct;
pub mod rachio;
pub mod rainbird;
pub mod reaper;
pub mod registry;
pub mod zone_map;

// Trait-contract conformance harness (offline-testable adapters).
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

pub mod restart;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-zone async locks that serialize Run dispatch on the same zone
/// across EVERY dispatch path (manual API, manual scheduler, smart-morning
/// cycle). Two near-simultaneous `run_zone` calls on one zone otherwise race
/// the hardware shutoff timer (last-writer-wins on HTTP, two timers closing at
/// the shorter duration on MQTT) and double-write the run row. One table per
/// controller registry, so the three dispatch paths that share a registry
/// share the locks and two tests with their own registries do not. Lazily
/// created per slug. A command-order barrier also serializes every Stop against
/// in-flight Run commands and their bookkeeping. Neither lock lasts for a
/// valve's watering duration. Held from the ceiling judgment through the
/// run row insert (`controllers::dispatch`), never the run duration (the
/// controller owns the shutoff), so it cannot block a later manual run for
/// the length of a cycle. The std mutex guarding the map is dropped before
/// the caller awaits the tokio mutex, so it is never held across an await.
#[derive(Clone, Default)]
pub struct ZoneLocks {
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    command_order: Arc<tokio::sync::RwLock<()>>,
    restart_hold: restart::RestartHold,
}

impl ZoneLocks {
    /// Runs share the barrier; a zone/device/all-controller stop takes it
    /// exclusively. Acquire before the per-zone lock so already queued runs
    /// finish before a waiting Stop, instead of reopening the valve afterward.
    pub fn command_order(&self) -> Arc<tokio::sync::RwLock<()>> {
        self.command_order.clone()
    }

    pub fn restart_hold(&self) -> restart::RestartHold {
        self.restart_hold.clone()
    }

    pub fn lock_for(&self, zone: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut guard = self.locks.lock().unwrap();
        guard
            .entry(zone.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}
