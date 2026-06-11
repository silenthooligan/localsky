// Netatmo Weather Station cloud source, api.netatmo.com.
//
// Netatmo sells consumer weather stations + outdoor modules + rain
// gauges + anemometers that auto-upload to api.netatmo.com. The cloud
// is the only access path; there's no LAN protocol.
//
// Auth is OAuth2 with refresh_token grant:
//   - User authorizes ONCE in a browser and pastes the refresh_token
//     into the wizard (we don't host an OAuth callback in localsky).
//   - The adapter exchanges refresh_token -> access_token at startup
//     and on every 401, rotating the refresh_token when Netatmo
//     issues a new one in the response.
//   - Rotated refresh tokens are persisted to a sidecar state file
//     (netatmo_tokens.json next to the config file, keyed by source
//     id) because Netatmo invalidates the old token on rotation: an
//     in-memory-only rotation would brick the source on restart. The
//     sidecar records which config token it was rotated from, so
//     pasting a fresh token into the config (re-authorization) takes
//     precedence over a stale sidecar entry.
//
// Endpoint:
//   POST /oauth2/token              refresh_token -> access_token + new refresh_token
//   GET  /api/getstationsdata?device_id={mac}   station + modules tree
//
// Modules we read:
//   - Indoor module (the main station):  Temperature, Humidity, Pressure (mbar)
//   - Outdoor (NAModule1):                Temperature, Humidity
//   - Rain gauge (NAModule3):             Rain (1h sum in mm)
//   - Anemometer (NAModule2):             WindStrength (km/h), WindAngle, GustStrength
//
// We poll every 10 min, Netatmo's docs cap refresh at every 10 min
// per device anyway, so faster polling just wastes API quota.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::NetatmoConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use serde::Serialize;

const API_BASE: &str = "https://api.netatmo.com";
const POLL_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub struct Netatmo {
    id: String,
    config: NetatmoConfig,
    client: Client,
    /// (access_token, refresh_token). Both rotate over the source's
    /// lifetime; refresh_token starts from config (or the persisted
    /// rotation state) and is replaced on every successful
    /// /oauth2/token round-trip.
    tokens: Mutex<NetatmoTokens>,
    /// Sidecar file rotated refresh tokens are persisted to. None
    /// disables persistence (tests).
    state_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Default)]
struct NetatmoTokens {
    access_token: Option<String>,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

impl Netatmo {
    pub fn new(id: impl Into<String>, config: NetatmoConfig) -> Self {
        Self::with_state_path(id, config, Some(default_state_path()))
    }

    /// Construct with an explicit sidecar path (None = no persistence).
    /// `new()` uses the default path next to the config file.
    pub fn with_state_path(
        id: impl Into<String>,
        config: NetatmoConfig,
        state_path: Option<std::path::PathBuf>,
    ) -> Self {
        let id = id.into();
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client construction");
        // Resume a previously rotated refresh token when the sidecar
        // entry descends from the SAME config token; a changed config
        // token means the operator re-authorized, so config wins.
        let mut refresh_token = config.refresh_token.clone();
        if let Some(path) = state_path.as_ref() {
            if let Some(current) = load_persisted_token(path, &id, &config.refresh_token) {
                info!(source_id = %id, "resuming rotated Netatmo refresh token from state file");
                refresh_token = current;
            }
        }
        let initial = NetatmoTokens {
            access_token: None,
            refresh_token,
        };
        Self {
            id,
            config,
            client,
            tokens: Mutex::new(initial),
            state_path,
        }
    }

    async fn refresh_access(&self) -> anyhow::Result<String> {
        let url = format!("{API_BASE}/oauth2/token");
        let refresh_token = {
            let t = self.tokens.lock().await;
            t.refresh_token.clone()
        };
        // OAuth2 spec mandates application/x-www-form-urlencoded; we
        // build it by hand so we don't need reqwest's serde-urlencoded
        // feature.
        let body = format!(
            "grant_type=refresh_token&refresh_token={rt}&client_id={cid}&client_secret={cs}",
            rt = form_encode(&refresh_token),
            cid = form_encode(&self.config.client_id),
            cs = form_encode(&self.config.client_secret),
        );
        let resp: TokenResponse = self
            .client
            .post(&url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let rotated = {
            let mut t = self.tokens.lock().await;
            t.access_token = Some(resp.access_token.clone());
            let changed = t.refresh_token != resp.refresh_token;
            t.refresh_token = resp.refresh_token.clone();
            changed
        };
        // Netatmo invalidates the old refresh token on rotation, so the
        // new one must survive a restart. Best-effort: a failed write
        // only warns (the source keeps running on the in-memory token).
        if rotated {
            if let Some(path) = self.state_path.as_ref() {
                if let Err(e) = persist_rotated_token(
                    path,
                    &self.id,
                    &self.config.refresh_token,
                    &resp.refresh_token,
                ) {
                    warn!(
                        source_id = %self.id,
                        error = %e,
                        "could not persist rotated Netatmo refresh token; re-authorization may be needed after a restart"
                    );
                } else {
                    debug!(source_id = %self.id, "persisted rotated Netatmo refresh token");
                }
            }
        }
        Ok(resp.access_token)
    }

    async fn current_access(&self) -> anyhow::Result<String> {
        if let Some(at) = self.tokens.lock().await.access_token.clone() {
            return Ok(at);
        }
        self.refresh_access().await
    }

    async fn fetch_station(&self) -> anyhow::Result<Value> {
        let url = format!(
            "{API_BASE}/api/getstationsdata?device_id={dev}",
            dev = self.config.device_id,
        );
        let mut access = self.current_access().await?;
        let mut resp = self.client.get(&url).bearer_auth(&access).send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            access = self.refresh_access().await?;
            resp = self.client.get(&url).bearer_auth(&access).send().await?;
        }
        let v: Value = resp.error_for_status()?.json().await?;
        Ok(v)
    }
}

/// Walk a Netatmo station body and emit (WeatherField, value) tuples
/// in LocalSky's canonical units (Fahrenheit, mph, inHg).
fn extract_fields(station: &Value) -> Vec<(WeatherField, f64)> {
    let mut out = Vec::new();
    let Some(device) = station
        .get("body")
        .and_then(|b| b.get("devices"))
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
    else {
        return out;
    };

    // Indoor / main module: Temperature (°C), Humidity (%), Pressure (mbar)
    if let Some(d) = device.get("dashboard_data") {
        if let Some(t) = d.get("Temperature").and_then(|v| v.as_f64()) {
            out.push((WeatherField::AirTempF, c_to_f(t)));
        }
        if let Some(h) = d.get("Humidity").and_then(|v| v.as_f64()) {
            out.push((WeatherField::RhPct, h));
        }
        if let Some(p_mbar) = d.get("AbsolutePressure").and_then(|v| v.as_f64()) {
            // 1 mbar = 0.02953 inHg
            out.push((WeatherField::PressureInHg, p_mbar * 0.02953));
        }
    }

    // Modules: outdoor (NAModule1), rain (NAModule3), wind (NAModule2)
    if let Some(modules) = device.get("modules").and_then(|m| m.as_array()) {
        for m in modules {
            let kind = m.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let Some(d) = m.get("dashboard_data") else {
                continue;
            };
            match kind {
                "NAModule1" => {
                    // Outdoor preferred over indoor for AirTempF; override.
                    if let Some(t) = d.get("Temperature").and_then(|v| v.as_f64()) {
                        // Replace previous AirTempF with outdoor reading.
                        out.retain(|(f, _)| *f != WeatherField::AirTempF);
                        out.push((WeatherField::AirTempF, c_to_f(t)));
                    }
                    if let Some(h) = d.get("Humidity").and_then(|v| v.as_f64()) {
                        out.retain(|(f, _)| *f != WeatherField::RhPct);
                        out.push((WeatherField::RhPct, h));
                    }
                }
                "NAModule3" => {
                    // Rain: sum_rain_1 (mm/1h) -> RainIntensityInHr;
                    //       sum_rain_24 (mm/24h) -> RainTodayIn.
                    if let Some(r) = d.get("sum_rain_1").and_then(|v| v.as_f64()) {
                        out.push((WeatherField::RainIntensityInHr, r * 0.03937));
                    }
                    if let Some(r) = d.get("sum_rain_24").and_then(|v| v.as_f64()) {
                        out.push((WeatherField::RainTodayIn, r * 0.03937));
                    }
                }
                "NAModule2" => {
                    if let Some(w) = d.get("WindStrength").and_then(|v| v.as_f64()) {
                        // km/h -> mph
                        out.push((WeatherField::WindMph, w * 0.6213712));
                    }
                    if let Some(g) = d.get("GustStrength").and_then(|v| v.as_f64()) {
                        out.push((WeatherField::WindGustMph, g * 0.6213712));
                    }
                    if let Some(a) = d.get("WindAngle").and_then(|v| v.as_f64()) {
                        out.push((WeatherField::WindBearingDeg, a));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

// ----- Rotated refresh-token persistence -----
//
// Format: JSON map of source id -> { config_refresh_token,
// current_refresh_token } at <config dir>/netatmo_tokens.json.
// config_refresh_token records which configured token the rotation
// chain descends from; when the operator pastes a new token into the
// config (re-authorization), the sidecar entry no longer matches and
// is ignored (then overwritten on the next rotation).

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTokenEntry {
    config_refresh_token: String,
    current_refresh_token: String,
}

/// Default sidecar location: next to the config file so it lives on
/// the same persistent volume (/data in the container).
fn default_state_path() -> std::path::PathBuf {
    let config_path =
        std::env::var("CONFIG_PATH").unwrap_or_else(|_| "/data/localsky.toml".to_string());
    std::path::Path::new(&config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/data"))
        .join("netatmo_tokens.json")
}

fn read_state_file(
    path: &std::path::Path,
) -> std::collections::HashMap<String, PersistedTokenEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The persisted current token for `id`, but only when the entry was
/// rotated from the SAME config token (otherwise the operator
/// re-authorized and the sidecar is stale).
fn load_persisted_token(path: &std::path::Path, id: &str, config_token: &str) -> Option<String> {
    let entries = read_state_file(path);
    let entry = entries.get(id)?;
    if entry.config_refresh_token == config_token {
        Some(entry.current_refresh_token.clone())
    } else {
        None
    }
}

/// Read-modify-write the sidecar with the newly rotated token for `id`.
/// Atomic via tmp + rename so a crash mid-write can't truncate other
/// sources' entries.
fn persist_rotated_token(
    path: &std::path::Path,
    id: &str,
    config_token: &str,
    current_token: &str,
) -> anyhow::Result<()> {
    let mut entries = read_state_file(path);
    entries.insert(
        id.to_string(),
        PersistedTokenEntry {
            config_refresh_token: config_token.to_string(),
            current_refresh_token: current_token.to_string(),
        },
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&entries)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Minimal application/x-www-form-urlencoded encoder for the four
/// fields we send to /oauth2/token. Encodes everything outside of the
/// unreserved set (RFC 3986 ALPHA / DIGIT / -._~) as %HH.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[async_trait]
impl WeatherSource for Netatmo {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::PressureInHg);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindGustMph);
        fields.insert(WeatherField::WindBearingDeg);
        fields.insert(WeatherField::RainTodayIn);
        fields.insert(WeatherField::RainIntensityInHr);
        SourceCaps {
            // Netatmo IS a live station, just cloud-routed.
            live_current: true,
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            WeatherField::AirTempF
            | WeatherField::RhPct
            | WeatherField::PressureInHg
            | WeatherField::WindMph
            | WeatherField::WindGustMph
            | WeatherField::WindBearingDeg
            | WeatherField::RainTodayIn
            | WeatherField::RainIntensityInHr => 65,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "Netatmo source started");
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match self.fetch_station().await {
                        Ok(body) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            let fields = extract_fields(&body);
                            if !fields.is_empty() {
                                debug!(source_id = %self.id, fields_n = fields.len(), "Netatmo updated");
                                let _ = bus.send(SourceEvent::Observation {
                                    source_id: self.id.clone(),
                                    fields,
                                    at_epoch: chrono::Utc::now().timestamp(),
                                });
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "Netatmo fetch failed");
                            if last_reachable != Some(false) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: false,
                                });
                                last_reachable = Some(false);
                            }
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source_id = %self.id, "Netatmo shutdown");
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn nt_test() -> Netatmo {
        Netatmo::with_state_path(
            "nt",
            NetatmoConfig {
                client_id: "c".into(),
                client_secret: "s".into(),
                refresh_token: "rt".into(),
                device_id: "70:ee:50:00:11:22".into(),
            },
            None,
        )
    }

    fn temp_state_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!(
                "ls-netatmo-{tag}-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
            ))
            .join("netatmo_tokens.json")
    }

    #[test]
    fn persisted_rotation_round_trips() {
        let path = temp_state_path("roundtrip");
        persist_rotated_token(&path, "nt", "config-token", "rotated-1").unwrap();
        assert_eq!(
            load_persisted_token(&path, "nt", "config-token").as_deref(),
            Some("rotated-1")
        );
        // A second rotation replaces the entry.
        persist_rotated_token(&path, "nt", "config-token", "rotated-2").unwrap();
        assert_eq!(
            load_persisted_token(&path, "nt", "config-token").as_deref(),
            Some("rotated-2")
        );
    }

    #[test]
    fn new_config_token_invalidates_sidecar_entry() {
        let path = temp_state_path("reauth");
        persist_rotated_token(&path, "nt", "old-config-token", "rotated-1").unwrap();
        // Operator re-authorized and pasted a new token into config:
        // the stale sidecar entry must NOT be resumed.
        assert_eq!(load_persisted_token(&path, "nt", "new-config-token"), None);
    }

    #[test]
    fn sidecar_keeps_entries_per_source_id() {
        let path = temp_state_path("multi");
        persist_rotated_token(&path, "nt_a", "cfg-a", "rot-a").unwrap();
        persist_rotated_token(&path, "nt_b", "cfg-b", "rot-b").unwrap();
        assert_eq!(
            load_persisted_token(&path, "nt_a", "cfg-a").as_deref(),
            Some("rot-a")
        );
        assert_eq!(
            load_persisted_token(&path, "nt_b", "cfg-b").as_deref(),
            Some("rot-b")
        );
    }

    #[test]
    fn constructor_resumes_persisted_token() {
        let path = temp_state_path("resume");
        persist_rotated_token(&path, "nt", "rt", "rotated-current").unwrap();
        let n = Netatmo::with_state_path(
            "nt",
            NetatmoConfig {
                client_id: "c".into(),
                client_secret: "s".into(),
                refresh_token: "rt".into(),
                device_id: "70:ee:50:00:11:22".into(),
            },
            Some(path),
        );
        let tokens = n.tokens.blocking_lock();
        assert_eq!(tokens.refresh_token, "rotated-current");
    }

    #[test]
    fn caps_advertise_live_station() {
        let n = nt_test();
        let caps = n.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::WindMph));
        assert!(caps.fields.contains(&WeatherField::RainTodayIn));
    }

    #[test]
    fn extract_outdoor_overrides_indoor_temp() {
        // Netatmo reports indoor temp on the main device and outdoor
        // temp on the NAModule1 module. We must surface outdoor.
        let body = json!({
            "body": {
                "devices": [{
                    "dashboard_data": { "Temperature": 21.0, "Humidity": 40.0, "AbsolutePressure": 1013.0 },
                    "modules": [
                        {
                            "type": "NAModule1",
                            "dashboard_data": { "Temperature": 5.0, "Humidity": 80.0 }
                        }
                    ]
                }]
            }
        });
        let f = extract_fields(&body);
        let tempf: Vec<_> = f
            .iter()
            .filter(|(k, _)| *k == WeatherField::AirTempF)
            .collect();
        assert_eq!(tempf.len(), 1, "exactly one AirTempF should remain");
        // 5C = 41F
        assert!((tempf[0].1 - 41.0).abs() < 0.001);
    }

    #[test]
    fn extract_rain_module_emits_today_and_intensity() {
        let body = json!({
            "body": {
                "devices": [{
                    "dashboard_data": {},
                    "modules": [
                        {
                            "type": "NAModule3",
                            "dashboard_data": { "sum_rain_1": 2.54, "sum_rain_24": 25.4 }
                        }
                    ]
                }]
            }
        });
        let f = extract_fields(&body);
        // 2.54mm = 0.1in; 25.4mm = 1.0in
        let int_in_hr = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainIntensityInHr)
            .unwrap()
            .1;
        let today = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainTodayIn)
            .unwrap()
            .1;
        assert!((int_in_hr - 0.1).abs() < 0.005);
        assert!((today - 1.0).abs() < 0.05);
    }
}
