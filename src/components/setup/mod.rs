// First-run wizard step components.
//
// Shipped:
//   shell.rs   - SetupShell wraps every step with header + progress
//                strip + footer; URL param :step picks the active step
//   welcome.rs - license accept + telemetry opt-in (defaults off)
//   location.rs - lat / lon / elevation / timezone form
//   review.rs  - summary + Save and finish POST /api/wizard/apply
//
// Placeholder (renders friendly "step coming soon" with working nav):
//   sources, controllers, zones, llm, notifications
//
// Follow-up commits flesh those out. Apply works any time
// /api/wizard/draft has license_accepted=true + lat/lon set + at least
// one source + one controller (env_compat synthesizes baseline sources
// and a placeholder controller from env vars when those steps haven't
// been edited yet).

pub mod controllers;
pub mod llm;
pub mod location;
pub mod notifications;
pub mod review;
pub mod shell;
pub mod sources;
pub mod welcome;
pub mod zones;

pub use controllers::ControllersStep;
pub use llm::LlmStep;
pub use location::LocationStep;
pub use notifications::NotificationsStep;
pub use review::ReviewStep;
pub use shell::SetupShell;
pub use sources::SourcesStep;
pub use welcome::WelcomeStep;
pub use zones::ZonesStep;
