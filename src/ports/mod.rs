// Port traits. Defines the abstract boundary between LocalSky's pure logic
// (engine/) and every adapter that touches the outside world (sources/,
// controllers/, ha/, llm/providers/, notifications/sinks/, config/store).
//
// Phase 1 stub: traits land here so subsequent phases can implement
// against them without further structural churn. No concrete impls yet.

pub mod config_store;
pub mod irrigation_controller;
pub mod llm_provider;
pub mod notification_sink;
pub mod weather_source;

pub use config_store::*;
pub use irrigation_controller::*;
pub use llm_provider::*;
pub use notification_sink::*;
pub use weather_source::*;
