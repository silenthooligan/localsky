// Bespoke panels for the Tempest live page. Each renders directly off the
// shared `Snapshot` signal — no API fetches at the component level. The
// irrigation panels under `irrigation/` follow the same pattern with
// IrrigationSnapshot.

pub mod about;
pub mod footer;
pub mod forecast;
pub mod hero;
pub mod install_prompt;
pub mod irrigation;
pub mod lightning;
pub mod mobile_nav;
pub mod nav;
pub mod pressure;
pub mod radar;
pub mod rain;
pub mod settings;
pub mod setup;
pub mod solar;
pub mod ui;
pub mod wind;
