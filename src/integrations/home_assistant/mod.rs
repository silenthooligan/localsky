// Home Assistant, as an integration rather than as the architecture.
//
// LocalSky grew up inside Home Assistant: it read the irrigation state
// from HA entities and wrote its decisions back to HA helpers, so the
// module named `ha` held the snapshot every other layer used, the store
// the dashboard read, and the engine's own vocabulary. None of that was
// about Home Assistant.
//
// What is actually about Home Assistant lives here: the REST client that
// reads an HA instance, the WebSocket path, and the MQTT discovery
// publisher that puts LocalSky's own entities into someone else's HA
// without LocalSky reading anything back. The shared shapes moved to
// `crate::model`, the irrigation store to `crate::refresher`, and the
// refresher itself supports HA or native sources.

pub mod mqtt_publish;
pub mod rest;
pub mod ws;

pub use mqtt_publish::{slugify, HaMqttPublisher, MqttPublishError};
