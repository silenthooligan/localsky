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
// The adapter polls each configured device every 60s on
// `sources::poll::run_polling` (tick, fetch metric, reachability edges,
// shutdown). The bearer token lives in a `sources::auth::TokenCache`; a
// 401 from the v2 API invalidates it and `with_reauth` logs in again and
// retries the call once. The user maps LocalSky WeatherFields onto
// specific (device_id, state_path) pairs, same pattern as ha_passthrough
// but talking to YoLink instead of HA.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use tracing::{debug, warn};

use crate::failure::Failure;
use crate::net::source_failure::from_anyhow;

use crate::config::schema::{YolinkConfig, YolinkFieldMap};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::auth::{with_reauth, TokenCache};
use crate::sources::mqtt_subscribe::parse_weather_field;
use crate::sources::poll::{run_polling, Poll};

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Yolink {
    id: String,
    config: YolinkConfig,
    /// Pre-parsed mapping list. Entries with unparseable field strings
    /// are dropped at construction with a warn.
    mapping: Vec<ResolvedMapping>,
    /// The OAuth2 bearer token: fetched on first use, dropped on a 401.
    token: TokenCache,
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

/// The v2 API refused the bearer token (HTTP 401). `with_reauth` treats
/// exactly this error as "log in again and retry once"; every other
/// failure (timeout, 5xx, bad JSON) is an outage and is not retried.
#[derive(Debug)]
struct TokenRejected(crate::failure::Failure);

impl std::fmt::Display for TokenRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for TokenRejected {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
impl TokenRejected {
    fn new(status: u16) -> Self {
        Self(crate::failure::Failure::http(
            status,
            None,
            "yolink authenticated request",
        ))
    }
}

fn token_rejected(e: &anyhow::Error) -> bool {
    e.downcast_ref::<TokenRejected>().is_some()
}

impl Yolink {
    pub fn new(id: impl Into<String>, config: YolinkConfig) -> Self {
        let id = id.into();
        let mapping = build_mapping(&id, &config.device_field_map);
        // No stored client. Each request builds an SSRF-hardened client
        // pinned to the (operator-overridable) base_url's resolved host via
        // safe_fetch, so a base_url pointed at a private/loopback address is
        // refused instead of probed.
        Self {
            id,
            config,
            mapping,
            token: TokenCache::new(),
        }
    }

    /// One client_credentials grant. This is the `TokenCache` auth_fn: it
    /// only returns the token; the cache is what keeps it.
    async fn fetch_token(&self) -> anyhow::Result<String> {
        let url = format!(
            "{}/open/yolink/token",
            self.config.base_url.trim_end_matches('/')
        );
        // YoLink's /open/yolink/token uses standard OAuth2 form encoding.
        let body = format!(
            "grant_type=client_credentials&client_id={cid}&client_secret={cs}",
            cid = crate::text::form_value(&self.config.client_id),
            cs = crate::text::form_value(&self.config.client_secret),
        );
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(&url, HTTP_TIMEOUT).await?;
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
        Ok(resp.access_token)
    }

    /// One authenticated POST to the v2 API. A 401 becomes `TokenRejected`
    /// so `with_reauth` can re-authenticate and retry; any other non-2xx
    /// is reported by status only (no upstream body).
    async fn post_authed(&self, url: &str, body: &Value, token: String) -> anyhow::Result<Value> {
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(url, HTTP_TIMEOUT).await?;
        let resp = client
            .post(safe_url)
            .bearer_auth(&token)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(TokenRejected::new(401).into());
        }
        if !status.is_success() {
            return Err(Failure::http(
                status.as_u16(),
                Some(crate::net::source_failure::response_format(&resp)),
                "YoLink API request",
            )
            .into());
        }
        Ok(crate::net::safe_fetch::read_json_capped(resp).await?)
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
        let url = url.as_str();
        let body = &body;
        with_reauth(
            &self.token,
            move || self.fetch_token(),
            token_rejected,
            move |token| self.post_authed(url, body, token),
        )
        .await
    }

    /// One poll: every mapped device's `{Type}.getState`, folded into one
    /// Observation (global fields) plus a KeyedReading per soil zone. A
    /// device that fails or lacks the state path is skipped at debug. When
    /// no device answers at all the poll fails, which the loop logs at warn
    /// and reports as the offline edge; an empty mapping is idle and simply
    /// never reports online.
    async fn poll_once(self: Arc<Self>) -> anyhow::Result<Poll> {
        if self.mapping.is_empty() {
            return Ok(Poll::none().unreachable());
        }
        let mut fields: Vec<(WeatherField, f64)> = Vec::new();
        let mut soil: Vec<(String, f64)> = Vec::new();
        let mut any_ok = false;
        let mut failures = Vec::new();
        for (index, m) in self.mapping.iter().enumerate() {
            let method = format!("{}.getState", m.device_type);
            let resp = match self.api_call(&method, &m.device_id).await {
                Ok(resp) => resp,
                Err(e) => {
                    let failure = from_anyhow(&e, "YoLink getState")
                        .with_item(index)
                        .with_resource(&m.device_id);
                    debug!(source_id = %self.id, error = %failure, "yolink getState failed");
                    failures.push(failure);
                    continue;
                }
            };
            any_ok = true;
            let Some(raw) = extract_state_number(&resp, &m.state_path) else {
                debug!(
                    source_id = %self.id,
                    device_id = m.device_id,
                    path = %m.state_path.join("."),
                    "yolink state path missing or non-numeric"
                );
                continue;
            };
            let v = raw * m.scale + m.offset;
            if let Some(zone) = &m.zone_slug {
                // Per-zone soil channel -> KeyedReading.
                soil.push((crate::sources::bus_recorder::zone_soil_key(zone), v));
            } else if let Some(f) = m.field {
                fields.push((f, v));
            }
        }
        let now = chrono::Utc::now().timestamp();
        let mut poll = Poll::none();
        if !failures.is_empty() {
            let failure = Failure::batch("YoLink device batch", failures, any_ok);
            if !any_ok {
                return Err(failure.into());
            }
            poll = poll.with_failure(failure);
        }
        for (key, value) in soil {
            poll = poll.with(SourceEvent::KeyedReading {
                source_id: self.id.clone(),
                key,
                value,
                at_epoch: now,
            });
        }
        if !fields.is_empty() {
            poll = poll.with(SourceEvent::Observation {
                source_id: self.id.clone(),
                fields,
                at_epoch: now,
            });
        }
        Ok(poll)
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

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        debug!(source_id = %self.id, mapping_n = self.mapping.len(), "YoLink mapping resolved");
        if self.mapping.is_empty() {
            warn!(source_id = %self.id, "YoLink has empty device_field_map; idle");
        }
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "YoLink",
            POLL_INTERVAL,
            bus,
            shutdown,
            Self::poll_once,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reauth_marker_preserves_http_evidence_through_context() {
        let error =
            anyhow::Error::new(super::TokenRejected::new(401)).context("credential-must-not-leak");
        assert!(super::token_rejected(&error));
        let failure = crate::net::source_failure::from_anyhow(&error, "poll");
        assert_eq!(failure.code, crate::failure::FailureCode::HttpUnauthorized);
        assert_eq!(failure.http_status, Some(401));
        assert!(!failure.to_string().contains("credential-must-not-leak"));
    }

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

    /// Only the 401 marker asks `with_reauth` for a fresh login; an
    /// outage-shaped error must not trigger a re-authentication.
    #[test]
    fn only_a_401_counts_as_a_rejected_token() {
        assert!(token_rejected(&anyhow::Error::from(TokenRejected::new(
            401
        ))));
        assert!(!token_rejected(&anyhow::anyhow!(
            "yolink api 503 Service Unavailable"
        )));
        assert!(TokenRejected::new(401).to_string().contains("LS_HTTP_401"));
    }
}
