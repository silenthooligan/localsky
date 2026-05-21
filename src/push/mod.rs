// Web Push subsystem. Wires three pieces:
//
//   1. Subscriptions persisted in the existing SQLite (push_subscriptions
//      table). One row per browser+device combination.
//   2. PushDispatcher: an in-process channel + background task. The HA
//      refresher emits PushEvent::ZoneStarted/ZoneStopped/DailyVerdict on
//      edge transitions; the dispatcher fans those out to all current
//      subscriptions, prunes endpoints that 404/410.
//   3. HTTP API at /api/push: subscribe, unsubscribe, vapid-key (returns
//      the public key the frontend needs for PushManager.subscribe()).
//
// VAPID config is read from env at startup:
//   - VAPID_PUBLIC_KEY        base64url-encoded P-256 public key
//   - VAPID_PRIVATE_KEY_PATH  PEM file path (mounted into the container)
//   - VAPID_SUBJECT           "mailto:..." or https URL identifying the app
//
// If any are missing, push is disabled cleanly: subscribe still 200s
// (so the frontend can store the row), but the dispatcher logs and
// drops events instead of hitting WebPush. That way generating VAPID
// keys is a follow-up step the user can take whenever they want
// without breaking the rest of the deploy.

pub mod api;
pub mod dispatcher;
pub mod store;

pub use api::{router, PushState};
pub use dispatcher::{spawn_dispatcher, PushDispatcher, PushEvent};
