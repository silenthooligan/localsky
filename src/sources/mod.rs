// WeatherSource adapters + merge engine + registry.
//
// Adapters shipped at v0.2:
//   demo_replay.rs  - synthetic data for demo mode (Phase 12)
//
// Adapters deferred to a later phase (legacy v0.1 paths still serve them
// via src/tempest/* and src/forecast/* until Runtime composition swaps):
//   tempest_udp, tempest_ws, open_meteo, ecowitt_local, nws, openweather,
//   pirate_weather, met_norway, ambient_weather, ha_passthrough
//
// Foundation modules shipped now so Phase 7+ can compose against them:
//   merge.rs    - MergedSnapshot + FieldValue + MergePolicy + merge_field
//   registry.rs - SourceRegistry behind arc-swap

pub mod demo_replay;
pub mod ecowitt_local;
pub mod http_webhook;
pub mod merge;
pub mod mqtt_subscribe;
pub mod registry;

pub use demo_replay::DemoReplay;
pub use ecowitt_local::EcowittLocal;
pub use http_webhook::HttpWebhook;
pub use merge::{default_policy, merge_field, FieldValue, MergePolicy, MergedSnapshot};
pub use mqtt_subscribe::MqttSubscribe;
pub use registry::SourceRegistry;
