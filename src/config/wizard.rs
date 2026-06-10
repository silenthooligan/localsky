// First-run wizard state machine. Persisted as a JSON draft alongside
// /data/localsky.toml so the user can quit the wizard mid-flow, restart
// the container, and pick up where they left off.
//
// The draft is a partial Config plus a step pointer. On `apply`, the
// runtime validates + writes the draft to /data/localsky.toml via the
// FileConfigStore, then deletes the draft file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::schema::Config;

/// Wizard steps in canonical order. Components route by matching on this
/// enum; deep links like /setup/zones jump to the named step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WizardStep {
    Welcome,
    Location,
    Sources,
    Controllers,
    Zones,
    Llm,
    Notifications,
    Review,
}

impl WizardStep {
    pub fn next(self) -> Option<Self> {
        use WizardStep::*;
        match self {
            Welcome => Some(Location),
            Location => Some(Sources),
            Sources => Some(Controllers),
            Controllers => Some(Zones),
            Zones => Some(Llm),
            Llm => Some(Notifications),
            Notifications => Some(Review),
            Review => None,
        }
    }

    pub fn previous(self) -> Option<Self> {
        use WizardStep::*;
        match self {
            Welcome => None,
            Location => Some(Welcome),
            Sources => Some(Location),
            Controllers => Some(Sources),
            Zones => Some(Controllers),
            Llm => Some(Zones),
            Notifications => Some(Llm),
            Review => Some(Notifications),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardDraft {
    pub current_step: WizardStep,
    /// The Config-in-progress. Defaults are filled per-step.
    pub config: Config,
    /// True after Welcome accepts the license. Apply refuses without this.
    pub license_accepted: bool,
    /// True after the operator opted in (or explicitly out). Defaults to
    /// `None` so we can require an explicit decision.
    pub telemetry_choice: Option<bool>,
    /// Last update epoch. Used by the UI to show "resumed Xm ago".
    pub last_updated_epoch: i64,
}

impl Default for WizardDraft {
    fn default() -> Self {
        Self {
            current_step: WizardStep::Welcome,
            config: Config::default(),
            license_accepted: false,
            telemetry_choice: None,
            last_updated_epoch: chrono::Utc::now().timestamp(),
        }
    }
}

#[derive(Debug, Error)]
pub enum WizardError {
    #[error("io error: {0}")]
    Io(String),
    #[error("draft serialize: {0}")]
    Serialize(String),
    #[error("draft parse: {0}")]
    Parse(String),
    #[error("draft not present")]
    NotPresent,
    #[error("license must be accepted before apply")]
    LicenseNotAccepted,
    #[error("validation: {0}")]
    Validation(String),
}

pub struct WizardStore {
    /// Path to the draft JSON file. Conventionally <config_path>.draft.
    path: PathBuf,
}

impl WizardStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn load(&self) -> Result<WizardDraft, WizardError> {
        let raw = std::fs::read_to_string(&self.path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => WizardError::NotPresent,
            _ => WizardError::Io(e.to_string()),
        })?;
        serde_json::from_str(&raw).map_err(|e| WizardError::Parse(e.to_string()))
    }

    pub fn save(&self, draft: &WizardDraft) -> Result<(), WizardError> {
        let mut d = draft.clone();
        d.last_updated_epoch = chrono::Utc::now().timestamp();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| WizardError::Io(e.to_string()))?;
        }
        let json =
            serde_json::to_string_pretty(&d).map_err(|e| WizardError::Serialize(e.to_string()))?;
        let tmp = self.path.with_extension("draft.tmp");
        std::fs::write(&tmp, json).map_err(|e| WizardError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| WizardError::Io(e.to_string()))?;
        Ok(())
    }

    pub fn clear(&self) -> Result<(), WizardError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(WizardError::Io(e.to_string())),
        }
    }

    /// Pre-apply checks. Doesn't call into the FileConfigStore; the API
    /// handler does that after this returns Ok.
    pub fn validate_for_apply(&self, draft: &WizardDraft) -> Result<(), WizardError> {
        if !draft.license_accepted {
            return Err(WizardError::LicenseNotAccepted);
        }
        // Location is the one hard requirement: sunrise scheduling and
        // forecasts are meaningless without it. Sources get defaulted at
        // apply (see finalize_for_apply, matching the wizard UI's
        // "skipping is fine" promise); controllers are optional so a
        // weather-station-only deployment is a first-class setup.
        if draft.config.deployment.location.lat == 0.0
            && draft.config.deployment.location.lon == 0.0
        {
            return Err(WizardError::Validation(
                "location is required (lat/lon both 0 is not a valid setup)".into(),
            ));
        }
        // Structural validation (ids, references, ranges). Warnings pass;
        // errors block with every detail joined so the Review step can
        // show exactly what to fix.
        let report = crate::config::validate::validate(&draft.config);
        if !report.ok() {
            let details: Vec<String> = report.errors.iter().map(|i| i.detail.clone()).collect();
            return Err(WizardError::Validation(details.join("; ")));
        }
        Ok(())
    }

    /// Fill the defaults the wizard UI promises when steps are skipped:
    /// no sources -> Tempest UDP (zero-config LAN listener) + Open-Meteo
    /// (uses the location from the Location step); no timezone -> infer
    /// from lat/lon so sunrise scheduling lands at the right wall-clock
    /// hour even when the container TZ is UTC. Called by apply after
    /// validation passes, before the config is written.
    pub fn finalize_for_apply(draft: &mut WizardDraft) {
        if draft.config.deployment.timezone.is_none() {
            let loc = &draft.config.deployment.location;
            draft.config.deployment.timezone = crate::timeutil::tz_name_for(loc.lat, loc.lon);
        }
        Self::finalize_sources(draft);
    }

    fn finalize_sources(draft: &mut WizardDraft) {
        use crate::config::schema::{OpenMeteoConfig, SourceEntry, SourceKind, TempestUdpConfig};
        if draft.config.sources.is_empty() {
            // Both configs are fully serde-defaulted; round-trip through
            // an empty JSON object to get those defaults without the
            // structs needing a Default impl.
            let tempest: TempestUdpConfig =
                serde_json::from_value(serde_json::json!({})).expect("serde defaults");
            let open_meteo: OpenMeteoConfig =
                serde_json::from_value(serde_json::json!({})).expect("serde defaults");
            draft.config.sources.push(SourceEntry {
                id: "tempest_lan".into(),
                priority: 100,
                enabled: true,
                source: SourceKind::TempestUdp(tempest),
            });
            draft.config.sources.push(SourceEntry {
                id: "open_meteo".into(),
                priority: 50,
                enabled: true,
                source: SourceKind::OpenMeteo(open_meteo),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_navigation_roundtrips() {
        let mut s = WizardStep::Welcome;
        let mut hops = 0;
        while let Some(n) = s.next() {
            s = n;
            hops += 1;
            assert!(hops < 20, "infinite loop in next()");
        }
        assert_eq!(s, WizardStep::Review);
        while let Some(p) = s.previous() {
            s = p;
            hops -= 1;
        }
        assert_eq!(s, WizardStep::Welcome);
        assert_eq!(hops, 0);
    }

    #[test]
    fn draft_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ls-wizard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = WizardStore::new(dir.join("localsky.toml.draft"));
        let mut d = WizardDraft::default();
        d.license_accepted = true;
        d.current_step = WizardStep::Zones;
        store.save(&d).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.current_step, WizardStep::Zones);
        assert!(loaded.license_accepted);
    }

    #[test]
    fn validate_for_apply_rejects_unaccepted_license() {
        let draft = WizardDraft::default();
        let store = WizardStore::new("/tmp/nope");
        let err = store.validate_for_apply(&draft).unwrap_err();
        assert!(matches!(err, WizardError::LicenseNotAccepted));
    }
}
