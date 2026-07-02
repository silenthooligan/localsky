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

pub mod account;
pub mod advanced;
pub mod cloud_weather;
pub mod controllers;
pub mod data_sources;
pub mod devices;
pub mod help;
pub mod home;
pub mod home_assistant;
pub mod llm;
pub mod location;
pub mod notifications;
pub mod radar;
pub mod restrictions;
pub mod schedules;
pub mod sensors;
pub mod skip_rules;
pub mod theme;
pub mod units;
pub mod zones;

/// Open-state of a settings add/edit form, derived from the URL so the phone
/// back gesture closes the form (one step back) instead of leaving settings.
/// `?add=1` opens the add form; `?edit=<id>` opens the editor for that entry.
#[derive(Clone, PartialEq, Debug)]
pub enum FormState {
    Closed,
    Add,
    Edit(String),
}

/// Decode a [`FormState`] from a raw search string ("?section=zones&edit=x").
/// `edit` wins over `add`; an empty/missing value is Closed (no phantom state).
pub fn parse_form_state(search: &str) -> FormState {
    let param = |key: &str| -> Option<String> {
        search
            .trim_start_matches('?')
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_string))
            .filter(|v| !v.is_empty())
    };
    if let Some(id) = param("edit") {
        FormState::Edit(id)
    } else if param("add").is_some() {
        FormState::Add
    } else {
        FormState::Closed
    }
}

/// Build the navigation target for a new form state, preserving the current
/// path and the `?section=` param (so it works both standalone at
/// `/settings/<x>` and inside the shell at `/settings?section=<x>`), and
/// dropping any stale `add`/`edit`. Closed drops the form params entirely.
pub fn form_state_url(pathname: &str, search: &str, next: &FormState) -> String {
    // Keep only `section` from the existing query; we own add/edit.
    let mut parts: Vec<String> = search
        .trim_start_matches('?')
        .split('&')
        .filter(|kv| kv.starts_with("section="))
        .map(str::to_string)
        .collect();
    match next {
        FormState::Closed => {}
        FormState::Add => parts.push("add=1".to_string()),
        FormState::Edit(id) => parts.push(format!("edit={id}")),
    }
    if parts.is_empty() {
        pathname.to_string()
    } else {
        format!("{pathname}?{}", parts.join("&"))
    }
}

pub use account::SettingsAccount;
pub use advanced::SettingsAdvanced;
pub use cloud_weather::{CloudWeatherServices, CloudWeatherWizardSection};
pub use controllers::SettingsControllers;
pub use data_sources::{RestartBanner, SettingsDataSources};
pub use devices::SettingsDevices;
pub use home::SettingsHome;
pub use home_assistant::SettingsHomeAssistant;
pub use llm::SettingsLlm;
pub use location::SettingsLocation;
pub use notifications::SettingsNotifications;
pub use radar::SettingsRadar;
pub use restrictions::SettingsRestrictions;
pub use schedules::SettingsSchedules;
pub use sensors::SettingsSensors;
pub use skip_rules::SettingsSkipRules;
pub use theme::SettingsTheme;
pub use units::SettingsUnits;
pub use zones::SettingsZones;
