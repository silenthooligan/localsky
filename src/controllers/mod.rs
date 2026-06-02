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
pub mod hydrawise;
pub mod mqtt_command;
pub mod opensprinkler_direct;
pub mod rachio;
pub mod rainbird;
pub mod registry;

pub use bhyve::Bhyve;
pub use dry_run::DryRunController;
pub use ha_service_call::HaServiceCall;
pub use hydrawise::Hydrawise;
pub use mqtt_command::MqttCommand;
pub use opensprinkler_direct::OpenSprinklerDirect;
pub use rachio::Rachio;
pub use rainbird::Rainbird;
pub use registry::ControllerRegistry;
