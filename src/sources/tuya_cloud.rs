// Tuya cloud source, openapi.tuya{us|eu|cn|in}.com.
//
// Tuya is the white-label OEM behind RainPoint, Smart Life-branded
// irrigation timers, and a vast catalog of cheap WiFi soil moisture
// + leak + temperature/humidity + flow sensors. Many consumer brands
// you've never heard of are Tuya devices in disguise. Powering this
// adapter means LocalSky covers them all.
//
// Auth: HMAC-SHA256 signed REST. Each request signs:
//   stringToSign = HTTPMethod + "\n" + sha256_hex(body) + "\n" + headers + "\n" + url
//   sign         = HMAC_SHA256(access_secret,
//                              access_id + access_token(opt) + t + nonce + stringToSign)
// where t = epoch_ms, nonce = empty for v2 SDK.
//
// Endpoints used:
//   POST /v1.0/token?grant_type=1                -> {access_token, expire_time}
//   GET  /v1.0/iot-03/devices/{id}/status        -> [{ code, value }, ...]
//
// Tuya status payload: each device exposes a list of "DP codes" with
// values. e.g. a RainPoint timer might report:
//   [{code: "temp_current", value: 250},  // value is *deci* °C (25.0°C)
//    {code: "humi_current", value: 60},
//    {code: "water_total",  value: 1234},
//    {code: "battery_percentage", value: 95}]
// Some codes return integers scaled by 10 or 100, user provides the
// scale + offset per mapping to normalize.
//
// 60s poll on `sources::poll::run_polling`. Token lives in
// `sources::auth::TokenCache`, dropped early on expiry and replaced once
// on a 401 through `with_reauth`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hex::ToHex;
use hmac::{
    digest::{KeyInit, Mac},
    Hmac,
};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::failure::{Failure, FailureCode as Code};
use crate::net::source_failure::from_anyhow;

use crate::config::schema::{TuyaCloudConfig, TuyaFieldMap};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::auth::{with_reauth, TokenCache};
use crate::sources::mqtt_subscribe::parse_weather_field;
use crate::sources::poll::{run_polling, Poll};
use crate::sources::yolink::parse_camel as parse_camel_field;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Drop the access_token this long before Tuya says it expires, so a poll
/// never starts on a token that dies mid-request.
const TOKEN_REFRESH_MARGIN_MS: i64 = 60_000;

pub struct TuyaCloud {
    id: String,
    config: TuyaCloudConfig,
    mapping: Vec<ResolvedMapping>,
    tokens: TokenCache,
    /// Unix-ms when the cached access_token expires. `TokenCache` keeps no
    /// clock of its own, so the expiry rides alongside it and
    /// `drop_expiring_token` invalidates the cache when it comes due.
    token_expires_at_ms: AtomicI64,
}

/// Tuya answered 401 to a signed request: the access_token is bad, not the
/// upstream. `with_reauth` reads this as "log in again and retry once".
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
            "tuya_cloud authenticated request",
        ))
    }
}

fn token_rejected(e: &anyhow::Error) -> bool {
    e.downcast_ref::<TokenRejected>().is_some()
}

#[derive(Debug, Clone)]
struct ResolvedMapping {
    /// Global weather field (None when this is a per-zone soil channel).
    field: Option<WeatherField>,
    /// Per-zone soil channel slug (None for a normal weather mapping).
    zone_slug: Option<String>,
    device_id: String,
    status_code: String,
    scale: f64,
    offset: f64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    result: Option<TokenResult>,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
struct TokenResult {
    access_token: String,
    /// Seconds until token expiry.
    expire_time: i64,
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    #[serde(default)]
    result: Vec<StatusItem>,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    code: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct StatusItem {
    code: String,
    value: Value,
}

impl TuyaCloud {
    pub fn new(id: impl Into<String>, config: TuyaCloudConfig) -> Self {
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
            tokens: TokenCache::new(),
            token_expires_at_ms: AtomicI64::new(0),
        }
    }

    /// Build the HMAC-SHA256 sign string per Tuya OpenAPI v2 spec.
    /// `token` is "" when fetching a new access_token, or the current
    /// access_token for authenticated requests.
    fn sign(&self, method: &str, path_with_query: &str, body: &str, t: i64, token: &str) -> String {
        // sha256_hex(body), empty body = e3b0c44...
        let mut body_hasher = Sha256::new();
        body_hasher.update(body.as_bytes());
        let body_hash: String = body_hasher.finalize().encode_hex();

        // stringToSign = METHOD\n<sha256_hex(body)>\n<headers>\n<url>
        // headers row is empty for our use; Tuya allows that.
        let string_to_sign = format!("{method}\n{body_hash}\n\n{path_with_query}");

        // signStr = client_id + access_token + t + "" + stringToSign
        // "" = nonce, omitted in v2 SDK.
        let sign_str = format!("{}{}{}{}", self.config.client_id, token, t, string_to_sign);

        let mut mac = Hmac::<Sha256>::new_from_slice(self.config.client_secret.as_bytes())
            .expect("hmac key length is always valid for sha256");
        mac.update(sign_str.as_bytes());
        let sig: String = mac.finalize().into_bytes().encode_hex();
        sig.to_ascii_uppercase()
    }

    /// Log in: fetch a fresh access_token. The token itself is what the
    /// `TokenCache` stores (this is its `auth` fn); the expiry Tuya quotes
    /// is recorded here as a side effect.
    async fn refresh_token(&self) -> anyhow::Result<String> {
        let path = "/v1.0/token?grant_type=1";
        let url = format!("{}{path}", self.config.base_url.trim_end_matches('/'));
        let t = chrono::Utc::now().timestamp_millis();
        let sig = self.sign("GET", path, "", t, "");
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(&url, Duration::from_secs(15)).await?;
        let resp: TokenResponse = client
            .get(safe_url)
            .header("client_id", &self.config.client_id)
            .header("sign", &sig)
            .header("t", t.to_string())
            .header("sign_method", "HMAC-SHA256")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if !resp.success {
            return Err(Failure::new(Code::ProviderRejected, "Tuya acquire token")
                .with_provider_code(resp.code.as_ref().and_then(Value::as_i64))
                .into());
        }
        let r = resp.result.ok_or_else(|| {
            Failure::new(Code::MissingField, "Tuya acquire token").with_field("result")
        })?;
        self.token_expires_at_ms.store(
            chrono::Utc::now().timestamp_millis() + r.expire_time * 1000,
            Ordering::Relaxed,
        );
        Ok(r.access_token)
    }

    /// Forget a token that is within the refresh margin of expiry so the
    /// next request logs in again instead of failing mid-flight.
    async fn drop_expiring_token(&self) {
        let now = chrono::Utc::now().timestamp_millis();
        if self.token_expires_at_ms.load(Ordering::Relaxed) <= now + TOKEN_REFRESH_MARGIN_MS {
            self.tokens.invalidate().await;
        }
    }

    /// A device's DP status with the cached token; on a 401 the token is
    /// replaced and the request made once more.
    async fn fetch_device_status(&self, device_id: &str) -> anyhow::Result<Vec<StatusItem>> {
        self.drop_expiring_token().await;
        with_reauth(
            &self.tokens,
            || self.refresh_token(),
            token_rejected,
            |token| async move { self.device_status_with(device_id, &token).await },
        )
        .await
    }

    /// One signed GET of a device's DP status with the given access_token.
    /// A 401 comes back as `TokenRejected` so `with_reauth` can re-auth.
    async fn device_status_with(
        &self,
        device_id: &str,
        token: &str,
    ) -> anyhow::Result<Vec<StatusItem>> {
        let path = format!("/v1.0/iot-03/devices/{device_id}/status");
        let url = format!("{}{path}", self.config.base_url.trim_end_matches('/'));
        let t = chrono::Utc::now().timestamp_millis();
        let sig = self.sign("GET", &path, "", t, token);
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(&url, Duration::from_secs(15)).await?;
        let resp = client
            .get(safe_url)
            .header("client_id", &self.config.client_id)
            .header("access_token", token)
            .header("sign", &sig)
            .header("t", t.to_string())
            .header("sign_method", "HMAC-SHA256")
            .send()
            .await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(TokenRejected::new(401).into());
        }
        let http = resp.error_for_status()?;
        let body: StatusResponse = crate::net::safe_fetch::read_json_capped(http).await?;
        if !body.success {
            return Err(Failure::new(Code::ProviderRejected, "Tuya device status")
                .with_provider_code(body.code.as_ref().and_then(Value::as_i64))
                .into());
        }
        Ok(body.result)
    }

    /// One poll: every mapped device's DP status, batched by device_id so a
    /// device with many mapped codes costs one request. A device that fails
    /// is logged and skipped; the poll is reachable when at least one device
    /// answered and an error (the loop's warn + offline edge) when none did.
    /// No mapped devices at all is an idle source: nothing to fetch, and
    /// nothing to call reachable.
    async fn poll_once(self: Arc<Self>) -> anyhow::Result<Poll> {
        let mut by_device: HashMap<&str, Vec<&ResolvedMapping>> = HashMap::new();
        for m in &self.mapping {
            by_device.entry(m.device_id.as_str()).or_default().push(m);
        }
        if by_device.is_empty() {
            return Ok(Poll::none().unreachable());
        }
        let mut poll = Poll::none();
        let mut fields = Vec::new();
        let mut any_ok = false;
        let mut failures = Vec::new();
        for (device_id, mappings) in &by_device {
            let items = match self.fetch_device_status(device_id).await {
                Ok(items) => items,
                Err(e) => {
                    let failure = from_anyhow(&e, "Tuya device status").with_resource(device_id);
                    debug!(source_id = %self.id, error = %failure, "tuya device status failed");
                    failures.push(failure);
                    continue;
                }
            };
            any_ok = true;
            for m in mappings {
                let Some(item) = items.iter().find(|i| i.code == m.status_code) else {
                    debug!(source_id = %self.id, device_id, code = m.status_code, "tuya status_code not present on device");
                    continue;
                };
                let Some(raw) = value_as_number(&item.value) else {
                    debug!(source_id = %self.id, device_id, code = m.status_code, "tuya value not numeric");
                    continue;
                };
                let value = raw * m.scale + m.offset;
                // Per-zone soil channel -> KeyedReading.
                if let Some(zone) = &m.zone_slug {
                    poll = poll.with(SourceEvent::KeyedReading {
                        source_id: self.id.clone(),
                        key: crate::sources::bus_recorder::zone_soil_key(zone),
                        value,
                        at_epoch: chrono::Utc::now().timestamp(),
                    });
                    continue;
                }
                if let Some(f) = m.field {
                    fields.push((f, value));
                }
            }
        }
        if !failures.is_empty() {
            let failure = Failure::batch("Tuya device batch", failures, any_ok);
            if !any_ok {
                return Err(failure.into());
            }
            poll = poll.with_failure(failure);
        }
        if !fields.is_empty() {
            poll = poll.with(SourceEvent::Observation {
                source_id: self.id.clone(),
                fields,
                at_epoch: chrono::Utc::now().timestamp(),
            });
        }
        Ok(poll)
    }
}

fn build_mapping(source_id: &str, field_map: &[TuyaFieldMap]) -> Vec<ResolvedMapping> {
    let mut out = Vec::new();
    for entry in field_map {
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
                status_code: entry.status_code.clone(),
                scale: entry.scale,
                offset: entry.offset,
            });
            continue;
        }
        let Some(field) =
            parse_weather_field(&entry.field).or_else(|| parse_camel_field(&entry.field))
        else {
            warn!(source_id, field = %entry.field, "tuya field_map: unknown WeatherField; ignoring");
            continue;
        };
        out.push(ResolvedMapping {
            field: Some(field),
            zone_slug: None,
            device_id: entry.device_id.clone(),
            status_code: entry.status_code.clone(),
            scale: entry.scale,
            offset: entry.offset,
        });
    }
    out
}

fn value_as_number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_i64().map(|i| i as f64))
}

#[async_trait]
impl WeatherSource for TuyaCloud {
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
            // Live weather only counts mappings that carry a WeatherField
            // (soil-only mappings don't make this a current-conditions source).
            live_current: self.mapping.iter().any(|m| m.field.is_some()),
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        // Cloud-routed sensor: same tier as YoLink + AmbientWeather (70).
        if self.mapping.iter().any(|m| m.field == Some(field)) {
            70
        } else {
            i32::MIN
        }
    }

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        debug!(source_id = %self.id, mapping_n = self.mapping.len(), "TuyaCloud mapping resolved");
        if self.mapping.is_empty() {
            warn!(source_id = %self.id, "TuyaCloud has empty device_field_map; idle");
        }
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "TuyaCloud",
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

    fn cfg() -> TuyaCloudConfig {
        TuyaCloudConfig {
            client_id: "demo_client_id".into(),
            client_secret: "demo_client_secret".into(),
            base_url: "https://openapi.tuyaus.com".into(),
            device_field_map: vec![TuyaFieldMap {
                field: "AirTempF".into(),
                device_id: "bf...".into(),
                status_code: "temp_current".into(),
                scale: 0.18, // deci-°C -> °F: (v/10)*1.8 = v * 0.18 (offset adds 32)
                offset: 32.0,
                zone_slug: None,
            }],
        }
    }

    #[test]
    fn sign_produces_uppercase_hex() {
        let t = TuyaCloud::new("tc", cfg());
        let sig = t.sign("GET", "/v1.0/token?grant_type=1", "", 1_700_000_000_000, "");
        // 64-char uppercase hex (HMAC-SHA256 -> 32 bytes -> 64 hex)
        assert_eq!(sig.len(), 64);
        assert!(sig
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn sign_deterministic_for_same_input() {
        let t = TuyaCloud::new("tc", cfg());
        let s1 = t.sign("GET", "/x", "body", 12345, "tok");
        let s2 = t.sign("GET", "/x", "body", 12345, "tok");
        assert_eq!(s1, s2);
    }

    #[test]
    fn sign_changes_with_time() {
        let t = TuyaCloud::new("tc", cfg());
        let s1 = t.sign("GET", "/x", "", 1, "");
        let s2 = t.sign("GET", "/x", "", 2, "");
        assert_ne!(s1, s2);
    }

    #[test]
    fn value_as_number_handles_int_and_float() {
        assert_eq!(value_as_number(&serde_json::json!(42)), Some(42.0));
        assert_eq!(value_as_number(&serde_json::json!(2.5)), Some(2.5));
        assert_eq!(value_as_number(&serde_json::json!("nope")), None);
    }

    #[test]
    fn caps_reflect_mapping() {
        let t = TuyaCloud::new("tc", cfg());
        let caps = t.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::AirTempF));
    }

    #[test]
    fn build_mapping_drops_unknown_fields() {
        let bad = vec![TuyaFieldMap {
            field: "definitely_not_a_field".into(),
            device_id: "x".into(),
            status_code: "x".into(),
            scale: 1.0,
            offset: 0.0,
            zone_slug: None,
        }];
        assert!(build_mapping("test", &bad).is_empty());
    }

    /// Only the 401 marker means "re-auth"; an outage or a Tuya-side
    /// `success: false` is left alone so `with_reauth` does not log in
    /// again on every transport error.
    #[test]
    fn only_the_401_marker_counts_as_a_rejected_token() {
        assert!(token_rejected(&anyhow::Error::from(TokenRejected::new(
            401
        ))));
        assert!(token_rejected(
            &anyhow::Error::from(TokenRejected::new(401)).context("status fetch")
        ));
        assert!(!token_rejected(&anyhow::anyhow!("request timed out")));
    }

    /// The cache has no clock; the adapter drops a token inside the refresh
    /// margin so the next request logs in again, and keeps one with time
    /// left.
    #[tokio::test]
    async fn a_token_near_expiry_is_dropped_and_a_fresh_one_kept() {
        let t = TuyaCloud::new("tc", cfg());
        let now = chrono::Utc::now().timestamp_millis();

        t.tokens.set("soon-dead".into()).await;
        t.token_expires_at_ms
            .store(now + TOKEN_REFRESH_MARGIN_MS / 2, Ordering::Relaxed);
        t.drop_expiring_token().await;
        assert_eq!(t.tokens.peek().await, None);

        t.tokens.set("fresh".into()).await;
        t.token_expires_at_ms
            .store(now + 2 * TOKEN_REFRESH_MARGIN_MS, Ordering::Relaxed);
        t.drop_expiring_token().await;
        assert_eq!(t.tokens.peek().await.as_deref(), Some("fresh"));
    }
}
