// First-run wizard step components. Every step is real (no
// placeholders):
//
//   shell.rs        - SetupShell: header + progress strip + footer
//   welcome.rs      - license accept + telemetry note
//   location.rs     - lat / lon / elevation / timezone (+ geocode)
//   sources.rs      - weather source editor (shared with Sensors hub)
//   controllers.rs  - controller editor + live test + zone scan/import
//   zones.rs        - species gallery + per-zone tuning
//   llm.rs          - optional advisor provider + live test
//   notifications.rs- push/MQTT/ntfy/Slack channels
//   account.rs      - owner account (built-in auth; skippable)
//   review.rs       - per-section summary + Save and finish
//
// Apply requires license_accepted + a real lat/lon; skipped sections
// get the documented defaults via WizardStore::finalize_for_apply.

pub mod account;
pub mod controllers;
pub mod discover;
pub mod llm;
pub mod location;
pub mod notifications;
pub mod review;
pub mod shell;
pub mod sources;
pub mod welcome;
pub mod zones;

pub use account::AccountStep;
pub use controllers::ControllersStep;
pub use llm::LlmStep;
pub use location::LocationStep;
pub use notifications::NotificationsStep;
pub use review::ReviewStep;
pub use shell::SetupShell;
pub use sources::SourcesStep;
pub use welcome::WelcomeStep;
pub use zones::ZonesStep;
