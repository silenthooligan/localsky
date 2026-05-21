// Home Assistant integration backbone for the irrigation page.
//
// Mirrors the shape of the tempest module: a typed snapshot, an
// arc-swap store with a watch channel for SSE, a long-running task
// that polls HA REST every refresh_interval and rebuilds the snapshot
// from the entities we care about. Polling instead of WebSocket
// because irrigation state is low-frequency (zones change minute-
// scale, schedule changes once per night) and a single GET /api/states
// is cheaper to operate than maintaining a WS subscription.

#[cfg(feature = "ssr")]
pub mod skip_logic;
pub mod snapshot;

#[cfg(feature = "ssr")]
pub mod mqtt_publish;
#[cfg(feature = "ssr")]
pub mod refresher;
#[cfg(feature = "ssr")]
pub mod rest;
#[cfg(feature = "ssr")]
pub mod store;

#[cfg(feature = "ssr")]
pub use mqtt_publish::{slugify, HaMqttPublisher, MqttPublishError};
#[cfg(feature = "ssr")]
pub use refresher::spawn_refresher;
#[cfg(feature = "ssr")]
pub use store::IrrigationStore;
