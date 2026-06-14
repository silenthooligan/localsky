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
    /// hour even when the container TZ is UTC; units -> pre-select from
    /// the timezone (US keeps imperial, everywhere else gets metric).
    /// Called by apply after validation passes, before the config is
    /// written.
    pub fn finalize_for_apply(draft: &mut WizardDraft) {
        if draft.config.deployment.timezone.is_none() {
            let loc = &draft.config.deployment.location;
            draft.config.deployment.timezone = crate::timeutil::tz_name_for(loc.lat, loc.lon);
        }
        Self::finalize_units(draft);
        Self::finalize_sources(draft);
    }

    /// The wizard never asks about units, so a draft always carries the
    /// serde default (imperial). Pre-select from the deployment timezone
    /// instead: US timezones keep imperial, the rest of the world gets
    /// metric. A draft already carrying an explicit non-default choice
    /// (metric) is left alone, and when no timezone could be derived the
    /// serde default stands.
    fn finalize_units(draft: &mut WizardDraft) {
        use crate::config::schema::Units;
        if draft.config.deployment.units != Units::Imperial {
            return;
        }
        if let Some(tz) = draft.config.deployment.timezone.as_deref() {
            if !is_us_timezone(tz) {
                draft.config.deployment.units = Units::Metric;
            }
        }
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
                max_age_s: None,
                enabled: true,
                source: SourceKind::TempestUdp(tempest),
            });
            draft.config.sources.push(SourceEntry {
                id: "open_meteo".into(),
                priority: 50,
                max_age_s: None,
                enabled: true,
                source: SourceKind::OpenMeteo(open_meteo),
            });
        }
    }
}

/// Pragmatic US-timezone check for the units pre-selection: the IANA
/// names covering the 50 states, plus the legacy `US/` aliases. Anything
/// else (including the rest of the Americas) is treated as metric
/// territory. Only the US, Liberia, and Myanmar are non-metric, so a
/// false negative here just means a US user flips one toggle in
/// Settings > Units.
fn is_us_timezone(tz: &str) -> bool {
    if tz.starts_with("US/") {
        return true;
    }
    if tz.starts_with("America/Indiana/")
        || tz.starts_with("America/Kentucky/")
        || tz.starts_with("America/North_Dakota/")
    {
        return true;
    }
    matches!(
        tz,
        "America/New_York"
            | "America/Chicago"
            | "America/Denver"
            | "America/Phoenix"
            | "America/Los_Angeles"
            | "America/Anchorage"
            | "Pacific/Honolulu"
            | "America/Detroit"
            | "America/Boise"
            | "America/Adak"
            | "America/Juneau"
            | "America/Sitka"
            | "America/Metlakatla"
            | "America/Yakutat"
            | "America/Nome"
            | "America/Menominee"
            | "America/Indianapolis"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Units;

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
    fn notifications_channels_survive_draft_roundtrip() {
        // The Notifications wizard step writes each enabled channel as a
        // fully-formed object under config.notifications.*. The draft PUT
        // deserializes into a typed WizardDraft, so a partial object would
        // 422 and silently drop every notification field (the bug this
        // guards). Build the exact shapes the step emits, then prove they
        // deserialize and round-trip back as present (non-null) channels,
        // which is what the Review step checks.
        let mut draft = WizardDraft::default();
        draft.license_accepted = true;
        let mut v = serde_json::to_value(&draft).unwrap();
        v["config"]["notifications"] = serde_json::json!({
            "web_push": {
                "vapid_public": "",
                "vapid_private_path": "",
                "vapid_subject": "",
            },
            "mqtt": {
                "host": "10.0.0.5",
                "port": 1883,
                "username": serde_json::Value::Null,
                "password": serde_json::Value::Null,
                "discovery_prefix": "homeassistant",
                "publish_enabled": true,
                "subscribe_enabled": false,
            },
            "ntfy": {
                "base_url": "https://ntfy.sh",
                "topic": "my-private-topic",
                "auth_token": serde_json::Value::Null,
            },
            "slack": { "webhook_url": "https://hooks.slack.com/services/T/B/X" },
        });

        // This is the deserialize the PUT handler performs.
        let parsed: WizardDraft =
            serde_json::from_value(v).expect("notification draft must deserialize, not 422");
        let n = &parsed.config.notifications;
        assert!(n.web_push.is_some(), "web_push present");
        assert_eq!(n.mqtt.as_ref().unwrap().host, "10.0.0.5");
        let ntfy = n.ntfy.as_ref().unwrap();
        assert_eq!(ntfy.base_url, "https://ntfy.sh");
        assert_eq!(ntfy.topic, "my-private-topic");
        assert_eq!(
            n.slack.as_ref().unwrap().webhook_url,
            "https://hooks.slack.com/services/T/B/X"
        );

        // And the Review step's "is this channel enabled?" check sees them.
        let back = serde_json::to_value(&parsed.config.notifications).unwrap();
        for key in ["web_push", "mqtt", "ntfy", "slack"] {
            assert!(
                back.get(key).map(|x| !x.is_null()).unwrap_or(false),
                "Review sees {key} as enabled"
            );
        }
    }

    #[test]
    fn validate_for_apply_rejects_unaccepted_license() {
        let draft = WizardDraft::default();
        let store = WizardStore::new("/tmp/nope");
        let err = store.validate_for_apply(&draft).unwrap_err();
        assert!(matches!(err, WizardError::LicenseNotAccepted));
    }

    fn draft_at(lat: f64, lon: f64) -> WizardDraft {
        let mut d = WizardDraft::default();
        d.config.deployment.location.lat = lat;
        d.config.deployment.location.lon = lon;
        d
    }

    #[test]
    fn finalize_units_metric_outside_us() {
        // Berlin: timezone inferred from lat/lon, units flip to metric.
        let mut d = draft_at(52.52, 13.40);
        WizardStore::finalize_for_apply(&mut d);
        assert_eq!(
            d.config.deployment.timezone.as_deref(),
            Some("Europe/Berlin")
        );
        assert_eq!(d.config.deployment.units, Units::Metric);

        // Sydney: metric too.
        let mut d = draft_at(-33.87, 151.21);
        WizardStore::finalize_for_apply(&mut d);
        assert_eq!(d.config.deployment.units, Units::Metric);
    }

    #[test]
    fn finalize_units_imperial_inside_us() {
        // Orlando: US timezone keeps the imperial default.
        let mut d = draft_at(28.5, -81.4);
        WizardStore::finalize_for_apply(&mut d);
        assert_eq!(
            d.config.deployment.timezone.as_deref(),
            Some("America/New_York")
        );
        assert_eq!(d.config.deployment.units, Units::Imperial);

        // Legacy US/ alias set explicitly also keeps imperial.
        let mut d = draft_at(39.74, -104.99);
        d.config.deployment.timezone = Some("US/Mountain".into());
        WizardStore::finalize_for_apply(&mut d);
        assert_eq!(d.config.deployment.units, Units::Imperial);
    }

    #[test]
    fn finalize_units_never_overrides_explicit_metric() {
        // An explicit metric choice in a US timezone is left alone.
        let mut d = draft_at(41.88, -87.63);
        d.config.deployment.units = Units::Metric;
        WizardStore::finalize_for_apply(&mut d);
        assert_eq!(
            d.config.deployment.timezone.as_deref(),
            Some("America/Chicago")
        );
        assert_eq!(d.config.deployment.units, Units::Metric);
    }

    #[test]
    fn finalize_units_no_timezone_keeps_default() {
        // lat/lon 0,0 derives no timezone; the serde default stands.
        let mut d = WizardDraft::default();
        WizardStore::finalize_for_apply(&mut d);
        assert_eq!(d.config.deployment.timezone, None);
        assert_eq!(d.config.deployment.units, Units::Imperial);
    }

    #[test]
    fn us_timezone_heuristic() {
        for tz in [
            "America/New_York",
            "America/Phoenix",
            "Pacific/Honolulu",
            "America/Indiana/Knox",
            "US/Eastern",
        ] {
            assert!(is_us_timezone(tz), "{tz} should read as US");
        }
        for tz in [
            "Europe/Berlin",
            "Australia/Sydney",
            "Europe/Madrid",
            "Pacific/Auckland",
            "America/Toronto",
            "America/Mexico_City",
            "America/Sao_Paulo",
        ] {
            assert!(!is_us_timezone(tz), "{tz} should read as non-US");
        }
    }
}
