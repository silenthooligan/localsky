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
    /// no sources -> Open-Meteo only (a cloud-only deployment, keyed off the
    /// location from the Location step; NO synthesized Tempest listener, so a
    /// no-hardware install does not boot with a passive station socket that
    /// reports offline); no timezone -> infer
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
        // Mark the sole controller default when the operator left none set, so
        // the single-controller happy path no longer 422s at the save gate.
        // Must run before validate_for_apply so the validator sees the marked
        // default (see post_apply's call order).
        crate::config::loader::auto_default_controller(&mut draft.config);
        // Auto-provision a VAPID keypair when the operator enabled Web Push in
        // the wizard but supplied no keys (the toggle wrote a placeholder
        // WebPushConfig the runtime could never use). Best-effort: a failure
        // leaves the placeholder, which the dispatcher then treats as "no keys".
        Self::finalize_web_push(draft);
    }

    /// Generate and persist a VAPID keypair into the draft's web_push config
    /// when the operator turned on Web Push but left the keys blank. The
    /// wizard's "Web Push" toggle emits a placeholder `WebPushConfig` with
    /// empty `vapid_public`/`vapid_private_path`/`vapid_subject`; before this,
    /// nothing ever filled them, so push silently never worked (the dispatcher
    /// reads VAPID from env, found nothing, and dropped every event). Here we
    /// mint a P-256 keypair, write the private key PEM next to the config (on
    /// the same persistent /data volume), and record the public key + path +
    /// subject so the dispatcher's config path picks them up on the next boot.
    /// A web_push block that already carries a public key is left untouched.
    fn finalize_web_push(draft: &mut WizardDraft) {
        let Some(wp) = draft.config.notifications.web_push.as_mut() else {
            return;
        };
        // Already provisioned (env-synthesized or a prior apply): leave it.
        if !wp.vapid_public.trim().is_empty() {
            return;
        }
        let private_path = Self::default_vapid_private_path();
        match crate::push::dispatcher::generate_vapid_keypair(&private_path) {
            Ok(public_b64u) => {
                wp.vapid_public = public_b64u;
                wp.vapid_private_path = private_path.to_string_lossy().into_owned();
                if wp.vapid_subject.trim().is_empty() {
                    // RFC 8292 allows an https: subject; the project URL is a
                    // sane operator-neutral default when none was entered.
                    wp.vapid_subject = "https://github.com/silenthooligan/localsky".to_string();
                }
            }
            Err(e) => {
                tracing::warn!(
                    "wizard: could not auto-generate VAPID keypair ({e}); Web Push left disabled until VAPID_* env or keys are provided"
                );
            }
        }
    }

    /// Default VAPID private-key location: alongside the config file (same
    /// persistent volume, /data in the container), under a keys/ subdir.
    fn default_vapid_private_path() -> PathBuf {
        let config_path =
            std::env::var("CONFIG_PATH").unwrap_or_else(|_| "/data/localsky.toml".to_string());
        Path::new(&config_path)
            .parent()
            .unwrap_or_else(|| Path::new("/data"))
            .join("keys")
            .join("vapid-private.pem")
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
        use crate::config::schema::{OpenMeteoConfig, SourceEntry, SourceKind};
        if draft.config.sources.is_empty() {
            // The user declared NO hardware, so add ONLY the cloud forecast
            // source (Open-Meteo, keyed off the Location step). We deliberately
            // do NOT synthesize a passive `tempest_lan` UDP listener here: a
            // no-hardware install must not boot with a station socket that, with
            // nothing ever transmitting on it, drives a global "tempest_lan
            // offline / degraded" health banner ~60s after first load and makes
            // the dashboard look broken. A user with a real Tempest adds it
            // explicitly on the Sources step; Open-Meteo alone is a first-class
            // cloud-only deployment (has_live_station = false downstream).
            //
            // OpenMeteoConfig is fully serde-defaulted; round-trip through an
            // empty JSON object to get those defaults without a Default impl.
            let open_meteo: OpenMeteoConfig =
                serde_json::from_value(serde_json::json!({})).expect("serde defaults");
            // Priority + freshness window come from the region helper, keyed off
            // the Location step. Open-Meteo always ranks 50 (the keyless cloud
            // backstop, last link in the cloud-only fallback chain); its ~1800s
            // refresh cadence widens max_age to ~2100 so a per-field pin survives
            // a full refresh cycle. Seeding the region-aware priority here means
            // any cloud the user later adds slots in at the correct rank.
            let om_kind = SourceKind::OpenMeteo(open_meteo);
            let (lat, lon) = (
                draft.config.deployment.location.lat,
                draft.config.deployment.location.lon,
            );
            draft.config.sources.push(SourceEntry {
                id: "open_meteo".into(),
                priority: crate::config::region::default_priority_for(&om_kind, lat, lon),
                max_age_s: crate::config::region::default_max_age_for(&om_kind),
                enabled: crate::config::region::default_enabled_for(&om_kind, lat, lon),
                source: om_kind,
            });

            // In addition to the always-on Open-Meteo backstop, light up the
            // region's KEYLESS authority so a no-hardware user boots with honest,
            // complete cloud current-conditions zero-clicks: NWS in the US, Met.no
            // in Europe/the Nordics, nothing extra elsewhere. Both are keyless
            // (the helper auto-fills the required user_agent the same way the
            // Open-Meteo default above is filled) and land at their region rank
            // (70, above the Open-Meteo 50 backstop) with the slow-cadence
            // freshness window. We are still inside the `sources.is_empty()`
            // branch, so a configured install (any source already present) is
            // never touched. A real live LAN station the user adds on the Sources
            // step preempts this branch and outranks these clouds (priority 100,
            // live_current=true; these cloud authorities are live_current=false).
            // NEVER a keyed source (Pirate/OpenWeather/WeatherKit): those stay
            // operator opt-in.
            for entry in crate::config::region::region_keyless_authority_entries(lat, lon) {
                draft.config.sources.push(entry);
            }
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

    #[test]
    fn finalize_marks_sole_controller_default() {
        // Bug #9: a single-controller wizard left default=false; the save gate
        // then 422'd at "Save and finish". finalize must auto-mark it so the
        // happy path goes through.
        use crate::config::schema::{ControllerEntry, ControllerKind};
        let mut d = draft_at(28.5, -81.4);
        d.config.controllers.push(ControllerEntry {
            id: "os".into(),
            default: false,
            enabled: true,
            controller: ControllerKind::DryRun(Default::default()),
        });
        WizardStore::finalize_for_apply(&mut d);
        assert!(
            d.config.controllers[0].default,
            "the sole controller must be auto-marked default"
        );
        // And the finalized draft now passes the loader save gate that used to
        // 422, proving the end-to-end fix.
        assert!(crate::config::loader::validate(&d.config).is_ok());
    }

    #[test]
    fn finalize_does_not_guess_default_with_two_controllers() {
        // With two controllers and no default, the choice is ambiguous: leave
        // it for the operator (validate::validate surfaces it in Review).
        use crate::config::schema::{ControllerEntry, ControllerKind};
        let mut d = draft_at(28.5, -81.4);
        for id in ["a", "b"] {
            d.config.controllers.push(ControllerEntry {
                id: id.into(),
                default: false,
                enabled: true,
                controller: ControllerKind::DryRun(Default::default()),
            });
        }
        WizardStore::finalize_for_apply(&mut d);
        assert_eq!(
            d.config.controllers.iter().filter(|c| c.default).count(),
            0,
            "two controllers, none default: do not guess"
        );
        let report = crate::config::validate::validate(&d.config);
        assert!(report
            .errors
            .iter()
            .any(|i| i.code == "controller_default_missing"));
    }

    #[test]
    fn finalize_web_push_provisions_a_keypair() {
        // The wizard "Web Push" toggle writes a placeholder WebPushConfig with
        // empty keys; finalize must mint a real keypair so the dispatcher's
        // config path can actually sign pushes (before this, push silently
        // never worked because the runtime read VAPID from env only).
        use crate::config::schema::WebPushConfig;
        // Point CONFIG_PATH at a temp dir so the generated PEM lands there.
        let dir = std::env::temp_dir().join(format!(
            "ls-wizard-vapid-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CONFIG_PATH", dir.join("localsky.toml"));

        let mut d = draft_at(28.5, -81.4);
        d.config.notifications.web_push = Some(WebPushConfig {
            vapid_public: String::new(),
            vapid_private_path: String::new(),
            vapid_subject: String::new(),
        });
        WizardStore::finalize_for_apply(&mut d);

        let wp = d.config.notifications.web_push.as_ref().unwrap();
        assert!(
            !wp.vapid_public.trim().is_empty(),
            "public key must be filled"
        );
        assert!(
            !wp.vapid_private_path.trim().is_empty(),
            "private key path must be filled"
        );
        assert!(
            !wp.vapid_subject.trim().is_empty(),
            "a default subject must be filled"
        );
        // The base64url public key must decode (it's the raw uncompressed
        // P-256 point: 65 bytes, 0x04 prefix).
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(wp.vapid_public.trim())
            .expect("vapid_public must be valid base64url");
        assert_eq!(bytes.len(), 65, "uncompressed P-256 point is 65 bytes");
        assert_eq!(bytes[0], 0x04, "uncompressed point starts with 0x04");
        // And the private PEM exists on disk where the config points.
        assert!(std::path::Path::new(&wp.vapid_private_path).exists());

        std::env::remove_var("CONFIG_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalize_sources_us_adds_open_meteo_and_enabled_nws() {
        // A no-hardware US install (empty sources) must boot with the keyless
        // regional authority live, zero clicks: Open-Meteo (always-on backstop,
        // 50) PLUS an enabled NWS at the US authority rank (70) with the 2100s
        // slow-cadence freshness window, both keyless (NWS user_agent auto-filled,
        // which the config validator requires non-empty).
        let mut d = draft_at(28.5, -81.4); // Orlando, US
        WizardStore::finalize_sources(&mut d);

        let by = |id: &str| d.config.sources.iter().find(|s| s.id == id);
        let om = by("open_meteo").expect("open_meteo synthesized");
        assert_eq!(om.priority, 50, "Open-Meteo is the 50 backstop");
        assert!(om.enabled, "Open-Meteo is enabled");

        let nws = by("nws").expect("US install synthesizes NWS");
        assert_eq!(nws.priority, 70, "US NWS lands at the authority rank 70");
        assert!(nws.enabled, "NWS is enabled in the US, zero clicks");
        assert_eq!(
            nws.max_age_s,
            Some(crate::config::region::MAX_AGE_SLOW_CADENCE_S),
            "NWS gets the 2100s slow-cadence freshness window"
        );
        match &nws.source {
            crate::config::schema::SourceKind::Nws(c) => assert!(
                !c.user_agent.trim().is_empty(),
                "the keyless NWS user_agent must be auto-filled"
            ),
            other => panic!("expected an NWS source, got {other:?}"),
        }
        // No keyed source was ever auto-enabled.
        assert!(by("pirate_weather").is_none() && by("openweather").is_none());
    }

    #[test]
    fn finalize_sources_nordic_adds_open_meteo_and_met_norway() {
        // A no-hardware Nordic install gets Met.no live alongside Open-Meteo.
        let mut d = draft_at(59.9, 10.75); // Oslo, Europe/Nordic
        WizardStore::finalize_sources(&mut d);

        let by = |id: &str| d.config.sources.iter().find(|s| s.id == id);
        assert!(by("open_meteo").is_some(), "Open-Meteo always synthesized");
        let metno = by("met_norway").expect("Nordic install synthesizes Met.no");
        assert_eq!(
            metno.priority, 70,
            "Nordic Met.no lands at the authority rank 70"
        );
        assert!(
            metno.enabled,
            "Met.no is enabled in the Nordics, zero clicks"
        );
        assert_eq!(
            metno.max_age_s,
            Some(crate::config::region::MAX_AGE_SLOW_CADENCE_S)
        );
        // NWS is US-only; it is not added for a Nordic install.
        assert!(by("nws").is_none(), "no NWS outside the US");
    }

    #[test]
    fn finalize_sources_global_adds_open_meteo_only() {
        // Outside the US and Europe/Nordic, no keyless regional authority covers
        // the location, so Open-Meteo is the sole synthesized cloud (no keyed
        // provider is ever auto-enabled).
        let mut d = draft_at(-33.87, 151.21); // Sydney, Global
        WizardStore::finalize_sources(&mut d);

        let ids: Vec<&str> = d.config.sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["open_meteo"],
            "a non-US/Nordic empty install synthesizes Open-Meteo only"
        );
    }

    #[test]
    fn finalize_sources_leaves_a_configured_install_untouched() {
        // The gate is strictly `sources.is_empty()`: a user who already declared a
        // source on the Sources step (here a real LAN Tempest) must NOT get an
        // unsolicited Open-Meteo or regional authority bolted on.
        use crate::config::schema::{SourceEntry, SourceKind, TempestUdpConfig};
        let mut d = draft_at(28.5, -81.4); // US location, but already configured
        d.config.sources.push(SourceEntry {
            id: "tempest_lan".into(),
            priority: 100,
            max_age_s: None,
            enabled: true,
            source: SourceKind::TempestUdp(TempestUdpConfig {
                bind_addr: "0.0.0.0:50222".into(),
                hub_serial: None,
            }),
        });
        WizardStore::finalize_sources(&mut d);

        let ids: Vec<&str> = d.config.sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["tempest_lan"],
            "a configured install must be left byte-identical (no synthesized clouds)"
        );
    }

    #[test]
    fn finalize_web_push_leaves_provisioned_keys_alone() {
        // A web_push block that already carries a public key (env-synthesized
        // or a prior apply) must not be regenerated.
        use crate::config::schema::WebPushConfig;
        let mut d = draft_at(28.5, -81.4);
        d.config.notifications.web_push = Some(WebPushConfig {
            vapid_public: "ALREADY_SET".into(),
            vapid_private_path: "/data/keys/existing.pem".into(),
            vapid_subject: "mailto:ops@example.com".into(),
        });
        WizardStore::finalize_for_apply(&mut d);
        let wp = d.config.notifications.web_push.as_ref().unwrap();
        assert_eq!(wp.vapid_public, "ALREADY_SET");
        assert_eq!(wp.vapid_private_path, "/data/keys/existing.pem");
        assert_eq!(wp.vapid_subject, "mailto:ops@example.com");
    }
}
