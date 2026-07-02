// YoLink (YoSmart) cloud source, api.yosmart.com.
//
// YoLink sells LoRa-based 915MHz consumer sensors (THSensor outdoor
// temp/RH, LeakSensor, WaterMeterController, etc) that report via a
// B-LAN / M-LAN hub up to the YoSmart cloud. Auth is OAuth2
// client_credentials (UAID + Secret Key from the developer portal).
//
// Endpoints:
//   POST /open/yolink/token              client_credentials grant
//   POST /open/yolink/v2/api             { method, params, msgid, time, targetDevice }
//
// Common methods used here:
//   Home.getDeviceList  , list devices once at startup (logged, not used for queries today)
//   {Type}.getState     , pull current device state per mapping
//
// The adapter polls each configured device every 60s. Token is cached
// and refreshed automatically on 401. The user maps LocalSky
// WeatherFields onto specific (device_id, state_path) pairs, same
// pattern as ha_passthrough but talking to YoLink instead of HA.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{YolinkConfig, YolinkFieldMap};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::mqtt_subscribe::parse_weather_field;

const POLL_INTERVAL: Duration = Duration::from_secs(60);

pub struct Yolink {
    id: String,
    config: YolinkConfig,
    /// Pre-parsed mapping list. Entries with unparseable field strings
    /// are dropped at construction with a warn.
    mapping: Vec<ResolvedMapping>,
    access_token: Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
struct ResolvedMapping {
    /// Global weather field (None when this is a per-zone soil channel).
    field: Option<WeatherField>,
    /// Per-zone soil channel slug (None for a normal weather mapping).
    zone_slug: Option<String>,
    device_id: String,
    device_type: String,
    state_path: Vec<String>,
    scale: f64,
    offset: f64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

impl Yolink {
    pub fn new(id: impl Into<String>, config: YolinkConfig) -> Self {
        let id = id.into();
        let mapping = build_mapping(&id, &config.device_field_map);
        // P4-7: no stored client. Each request builds an SSRF-hardened client
        // pinned to the (operator-overridable) base_url's resolved host via
        // safe_fetch, so a base_url pointed at a private/loopback address is
        // refused instead of probed.
        Self {
            id,
            config,
            mapping,
            access_token: Mutex::new(None),
        }
    }

    async fn refresh_token(&self) -> anyhow::Result<String> {
        let url = format!(
            "{}/open/yolink/token",
            self.config.base_url.trim_end_matches('/')
        );
        // YoLink's /open/yolink/token uses standard OAuth2 form encoding.
        let body = format!(
            "grant_type=client_credentials&client_id={cid}&client_secret={cs}",
            cid = form_encode(&self.config.client_id),
            cs = form_encode(&self.config.client_secret),
        );
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(&url, Duration::from_secs(15)).await?;
        let resp: TokenResponse = client
            .post(safe_url)
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
        *self.access_token.lock().await = Some(resp.access_token.clone());
        Ok(resp.access_token)
    }

    async fn current_token(&self) -> anyhow::Result<String> {
        if let Some(t) = self.access_token.lock().await.clone() {
            return Ok(t);
        }
        self.refresh_token().await
    }

    async fn api_call(&self, method: &str, target_device: &str) -> anyhow::Result<Value> {
        let url = format!(
            "{}/open/yolink/v2/api",
            self.config.base_url.trim_end_matches('/')
        );
        let body = json!({
            "method": method,
            "targetDevice": target_device,
            "time": chrono::Utc::now().timestamp_millis(),
            "msgid": format!("{}", chrono::Utc::now().timestamp_millis()),
        });
        let mut token = self.current_token().await?;
        for attempt in 0..2 {
            let (client, safe_url) =
                crate::net::safe_fetch::build_safe_client(&url, Duration::from_secs(15)).await?;
            let resp = client
                .post(safe_url)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                *self.access_token.lock().await = None;
                token = self.refresh_token().await?;
                continue;
            }
            if !status.is_success() {
                return Err(anyhow::anyhow!("yolink api {status}"));
            }
            return Ok(resp.json().await?);
        }
        Err(anyhow::anyhow!("yolink retry exhausted"))
    }
}

fn build_mapping(source_id: &str, field_map: &[YolinkFieldMap]) -> Vec<ResolvedMapping> {
    let mut out = Vec::new();
    for entry in field_map {
        let state_path: Vec<String> = entry
            .state_path
            .split('.')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        let zone = entry
            .zone_slug
            .as_deref()
            .map(str::trim)
            .filter(|z| !z.is_empty());
        // Per-zone soil channel: emitted as a KeyedReading, no WeatherField.
        if let Some(zone) = zone {
            out.push(ResolvedMapping {
                field: None,
                zone_slug: Some(zone.to_string()),
                device_id: entry.device_id.clone(),
                device_type: entry.device_type.clone(),
                state_path,
                scale: entry.scale,
                offset: entry.offset,
            });
            continue;
        }
        let Some(field) = parse_weather_field(&entry.field).or_else(|| parse_camel(&entry.field))
        else {
            warn!(source_id, field = %entry.field, "yolink field map: unknown WeatherField; ignoring");
            continue;
        };
        out.push(ResolvedMapping {
            field: Some(field),
            zone_slug: None,
            device_id: entry.device_id.clone(),
            device_type: entry.device_type.clone(),
            state_path,
            scale: entry.scale,
            offset: entry.offset,
        });
    }
    out
}

/// Accept CamelCase variants (e.g. "AirTempF") in addition to the
/// snake_case mqtt_subscribe parser handles. Avoids forcing wizard
/// JSON to be snake_case-only. Pub so tuya_cloud + future cloud
/// adapters can reuse it.
pub fn parse_camel(name: &str) -> Option<WeatherField> {
    use WeatherField::*;
    Some(match name {
        "AirTempF" => AirTempF,
        "DewPointF" => DewPointF,
        "RhPct" => RhPct,
        "WindMph" => WindMph,
        "WindGustMph" => WindGustMph,
        "WindBearingDeg" => WindBearingDeg,
        "SolarWm2" => SolarWm2,
        "UvIndex" => UvIndex,
        "Illuminance" => Illuminance,
        "PressureInHg" => PressureInHg,
        "RainTodayIn" => RainTodayIn,
        "RainIntensityInHr" => RainIntensityInHr,
        "FlowGpm" => FlowGpm,
        "FlowTotalGalToday" => FlowTotalGalToday,
        _ => return None,
    })
}

/// Walk `path` keys into the JSON, rooted at `data.state`. Returns the
/// terminal value if every key exists and the leaf is numeric.
fn extract_state_number(api_response: &Value, path: &[String]) -> Option<f64> {
    let mut cur = api_response.get("data").and_then(|d| d.get("state"))?;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_f64()
}

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
impl WeatherSource for Yolink {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        for m in &self.mapping {
            if let Some(f) = m.field {
                fields.insert(f);
            }
        }
        SourceCaps {
            live_current: self.mapping.iter().any(|m| m.field.is_some()),
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        // Cloud-routed LoRa sensor: same tier as AmbientWeather (70).
        if self.mapping.iter().any(|m| m.field == Some(field)) {
            70
        } else {
            i32::MIN
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, mapping_n = self.mapping.len(), "YoLink source started");
        if self.mapping.is_empty() {
            warn!(source_id = %self.id, "YoLink has empty device_field_map; idle");
        }
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let mut fields = Vec::new();
                    let mut any_ok = false;
                    for m in &self.mapping {
                        let method = format!("{}.getState", m.device_type);
                        match self.api_call(&method, &m.device_id).await {
                            Ok(resp) => {
                                any_ok = true;
                                if let Some(raw) = extract_state_number(&resp, &m.state_path) {
                                    let v = raw * m.scale + m.offset;
                                    if let Some(zone) = &m.zone_slug {
                                        // Per-zone soil channel -> KeyedReading.
                                        let _ = bus.send(SourceEvent::KeyedReading {
                                            source_id: self.id.clone(),
                                            key: crate::sources::bus_recorder::zone_soil_key(zone),
                                            value: v,
                                            at_epoch: chrono::Utc::now().timestamp(),
                                        });
                                    } else if let Some(f) = m.field {
                                        fields.push((f, v));
                                    }
                                } else {
                                    debug!(
                                        source_id = %self.id,
                                        device_id = m.device_id,
                                        path = %m.state_path.join("."),
                                        "yolink state path missing or non-numeric"
                                    );
                                }
                            }
                            Err(e) => {
                                debug!(source_id = %self.id, device_id = m.device_id, error = %e, "yolink getState failed");
                            }
                        }
                    }
                    let reach = any_ok;
                    if last_reachable != Some(reach) {
                        let _ = bus.send(SourceEvent::Reachability {
                            source_id: self.id.clone(),
                            reachable: reach,
                        });
                        last_reachable = Some(reach);
                    }
                    if !fields.is_empty() {
                        let _ = bus.send(SourceEvent::Observation {
                            source_id: self.id.clone(),
                            fields,
                            at_epoch: chrono::Utc::now().timestamp(),
                        });
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source_id = %self.id, "YoLink shutdown");
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

    fn cfg() -> YolinkConfig {
        YolinkConfig {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            device_field_map: vec![
                YolinkFieldMap {
                    field: "AirTempF".into(),
                    device_id: "device-1".into(),
                    device_type: "THSensor".into(),
                    state_path: "temperature".into(),
                    scale: 1.0,
                    offset: 0.0,
                    zone_slug: None,
                },
                YolinkFieldMap {
                    field: "FlowGpm".into(),
                    device_id: "device-2".into(),
                    device_type: "WaterMeterController".into(),
                    state_path: "waterFlow".into(),
                    scale: 1.0,
                    offset: 0.0,
                    zone_slug: None,
                },
            ],
            base_url: "https://api.yosmart.com".into(),
        }
    }

    #[test]
    fn build_mapping_drops_unknown_fields() {
        let bad = vec![YolinkFieldMap {
            field: "garbage".into(),
            device_id: "x".into(),
            device_type: "x".into(),
            state_path: "x".into(),
            scale: 1.0,
            offset: 0.0,
            zone_slug: None,
        }];
        assert!(build_mapping("test", &bad).is_empty());
    }

    #[test]
    fn build_mapping_accepts_camel_and_snake() {
        let m = vec![
            YolinkFieldMap {
                field: "AirTempF".into(),
                device_id: "d".into(),
                device_type: "T".into(),
                state_path: "p".into(),
                scale: 1.0,
                offset: 0.0,
                zone_slug: None,
            },
            YolinkFieldMap {
                field: "rh_pct".into(),
                device_id: "d".into(),
                device_type: "T".into(),
                state_path: "p".into(),
                scale: 1.0,
                offset: 0.0,
                zone_slug: None,
            },
        ];
        let r = build_mapping("test", &m);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].field, Some(WeatherField::AirTempF));
        assert_eq!(r[1].field, Some(WeatherField::RhPct));
    }

    #[test]
    fn extract_state_number_walks_data_state() {
        let v = json!({
            "data": {
                "state": {
                    "temperature": 72.5,
                    "nested": { "humidity": 45.0 }
                }
            }
        });
        assert_eq!(
            extract_state_number(&v, &["temperature".to_string()]),
            Some(72.5)
        );
        assert_eq!(
            extract_state_number(&v, &["nested".to_string(), "humidity".to_string()]),
            Some(45.0)
        );
        assert_eq!(extract_state_number(&v, &["missing".to_string()]), None);
    }

    #[test]
    fn caps_reflect_mapping() {
        let y = Yolink::new("yl", cfg());
        let caps = y.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::AirTempF));
        assert!(caps.fields.contains(&WeatherField::FlowGpm));
    }
}
