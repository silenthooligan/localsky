// Bespoke panels for the Tempest live page. Each renders directly off the
// shared `Snapshot` signal — no API fetches at the component level. The
// irrigation panels under `irrigation/` follow the same pattern with
// IrrigationSnapshot.

pub mod about;
pub mod connection;
pub mod controllers_form;
pub mod feature_stub;
pub mod feedback;
pub mod footer;
pub mod forecast;
pub mod health_banner;
pub mod hero;
pub mod historyview;
pub mod install_prompt;
pub mod irrigation;
pub mod lightning;
pub mod login;
pub mod mobile_nav;
pub mod nav;
pub mod page_header;
pub mod pressure;
pub mod radar;
pub mod rain;
pub mod rules;
pub mod sensors;
pub mod settings;
pub mod settings_ui;
pub mod setup;
pub mod sidebar;
pub mod simulator;
pub mod solar;
pub mod sources_form;
pub mod time_bucket;
pub mod ui;
pub mod units_fmt;
pub mod verdict;
pub mod weather_telemetry;
pub mod welcome_card;
pub mod wind;
pub mod zones;
