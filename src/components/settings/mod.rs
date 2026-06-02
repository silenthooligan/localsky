// Settings UI components.
//
// Shipped:
//   home.rs   - section list with 9 navigable sections
//   theme.rs  - preset picker (dark/light/auto/hc); persists to localStorage
//
// Planned (Phase 10 follow-up):
//   location.rs        - lat/lon/elevation/timezone via /api/config
//   sources.rs         - per-source editor with Test
//   controllers.rs     - per-controller editor with Test + Scan + Stop
//   zones.rs           - per-zone editor (calibration modal, Kc preview)
//   llm.rs             - provider picker + test prompt
//   notifications.rs   - Web Push + MQTT + ntfy + Slack
//   units.rs           - imperial / metric / custom per-field
//   advanced.rs        - nerd mode, rollback snapshots

pub mod advanced;
pub mod controllers;
pub mod devices;
pub mod home;
pub mod llm;
pub mod location;
pub mod notifications;
pub mod restrictions;
pub mod schedules;
pub mod skip_rules;
pub mod sources;
pub mod theme;
pub mod units;
pub mod zones;

pub use advanced::SettingsAdvanced;
pub use controllers::SettingsControllers;
pub use devices::SettingsDevices;
pub use home::SettingsHome;
pub use llm::SettingsLlm;
pub use location::SettingsLocation;
pub use notifications::SettingsNotifications;
pub use restrictions::SettingsRestrictions;
pub use schedules::SettingsSchedules;
pub use skip_rules::SettingsSkipRules;
pub use sources::SettingsSources;
pub use theme::SettingsTheme;
pub use units::SettingsUnits;
pub use zones::SettingsZones;
