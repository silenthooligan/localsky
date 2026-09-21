// LaCrosse View cloud source, lacrosseview.com.
//
// LaCrosse Technology stations with the View Gateway (or "View" badge
// on newer all-in-one stations like the LTV-WSDTH04) upload to
// lacrosseview.com. The mobile + web apps read from a Firebase-style
// REST endpoint that the community has reverse-engineered.
//
// Auth: POST /identitytoolkit/v3/relyingparty/verifyPassword (Google
// Identity Toolkit) with email + password returns an idToken (60min
// TTL, refreshed via /securetoken refresh_token).
//
// Endpoints used:
//   POST https://www.googleapis.com/identitytoolkit/v3/relyingparty/verifyPassword?key=API_KEY
//        {email, password, returnSecureToken: true}
//   GET  https://lax-gateway.appspot.com/_ah/api/lacrosseClient/v1.1/active-user/locations
//        (Authorization: Bearer <idToken>)
//   GET  https://lax-gateway.appspot.com/_ah/api/lacrosseClient/v1.1/active-user/location/{loc}/sensors/{device_id}/feed?...
//
// The API_KEY is published in the LaCrosse Android APK. Multiple
// open-source HACS/HA integrations have surfaced it; we hard-code it
// here as well. Same trade-off as the Bhyve mobile-app endpoints.
//
// Cadence: 5 minutes, LaCrosse stations themselves only update the
// cloud every 5-10 min, so faster polling produces dupes.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::sync::Mutex;

use crate::failure::{Failure, FailureCode as Code};

use crate::config::schema::LacrosseConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, WeatherField, WeatherSource,
};
use crate::sources::auth::{with_reauth, TokenCache};
use crate::sources::poll::{run_polling, Poll};
use crate::units::{c_to_f, kph_to_mph, mm_to_in};

const IDTOOLKIT_URL: &str =
    "https://www.googleapis.com/identitytoolkit/v3/relyingparty/verifyPassword";
const API_BASE: &str = "https://lax-gateway.appspot.com/_ah/api/lacrosseClient/v1.1";
/// Public LaCrosse Firebase API key (also surfaced by the LaCrosse
/// Android APK and used by every open-source LaCrosse integration).
const FIREBASE_KEY: &str = "AIzaSyD-Uo0hkRI_4tGreLJRSL_SXmYsRJSVeYQ";
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub struct Lacrosse {
    id: String,
    config: LacrosseConfig,
    client: Client,
    /// The Firebase idToken (60min TTL). A 401 from the gateway
    /// invalidates it; the next call logs in again.
    id_token: TokenCache,
    /// Cached (location_id, device_id) after first locations lookup so
    /// we don't re-walk the tree on every poll.
    resolved: Mutex<Option<(String, String)>>,
}

#[derive(Debug, Deserialize)]
struct AuthResponse {
    #[serde(rename = "idToken")]
    id_token: String,
}

/// A 401 from the gateway means the idToken expired, not that the
/// upstream is down; only that earns a re-login and a retry.
fn token_rejected(e: &anyhow::Error) -> bool {
    let status = e.downcast_ref::<reqwest::Error>().and_then(|r| r.status());
    status == Some(StatusCode::UNAUTHORIZED)
}

impl Lacrosse {
    pub fn new(id: impl Into<String>, config: LacrosseConfig) -> Self {
        Self {
            id: id.into(),
            config,
            client: crate::net::client(Duration::from_secs(15)),
            id_token: TokenCache::new(),
            resolved: Mutex::new(None),
        }
    }

    /// Exchange email + password for a fresh idToken. Caching is the
    /// `TokenCache`'s job; this only talks to the identity toolkit.
    async fn login(&self) -> anyhow::Result<String> {
        let url = format!("{IDTOOLKIT_URL}?key={FIREBASE_KEY}");
        let body = json!({
            "email": &self.config.email,
            "password": &self.config.password,
            "returnSecureToken": true,
        });
        let resp: AuthResponse = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.id_token)
    }

    /// One bearer-authenticated GET against the gateway. A 401 surfaces
    /// as a `reqwest::Error` carrying the status so `token_rejected`
    /// can tell it from an outage.
    async fn get_with_token(&self, url: &str, token: String) -> anyhow::Result<Value> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// GET with the cached idToken; on a 401, log in again and retry once.
    async fn get_json(&self, url: &str) -> anyhow::Result<Value> {
        with_reauth(
            &self.id_token,
            || self.login(),
            token_rejected,
            |token| self.get_with_token(url, token),
        )
        .await
    }

    async fn locations(&self) -> anyhow::Result<Value> {
        self.get_json(&format!("{API_BASE}/active-user/locations"))
            .await
    }

    async fn resolve_target(&self) -> anyhow::Result<(String, String)> {
        if let Some(cached) = self.resolved.lock().await.clone() {
            return Ok(cached);
        }
        let body = self.locations().await?;
        // Walk locations[].devices[]; pick first device whose id matches
        // self.config.device_id, or the first device overall if unset.
        let locs = body
            .get("items")
            .and_then(|a| a.as_array())
            .ok_or_else(|| {
                Failure::new(Code::MissingField, "LaCrosse locations").with_field("items[]")
            })?;
        for loc in locs {
            let loc_id = loc
                .get("id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| {
                    Failure::new(Code::MissingField, "LaCrosse locations").with_field("items[].id")
                })?
                .to_string();
            if let Some(devs) = loc.get("devices").and_then(|a| a.as_array()) {
                for dev in devs {
                    let dev_id = dev
                        .get("id")
                        .or_else(|| dev.get("sensor"))
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string());
                    let Some(dev_id) = dev_id else { continue };
                    let matches = self
                        .config
                        .device_id
                        .as_ref()
                        .map(|want| want == &dev_id)
                        .unwrap_or(true);
                    if matches {
                        let resolved = (loc_id, dev_id);
                        *self.resolved.lock().await = Some(resolved.clone());
                        return Ok(resolved);
                    }
                }
            }
        }
        Err(Failure::new(Code::DeviceMissing, "LaCrosse resolve device").into())
    }

    async fn fetch_feed(&self) -> anyhow::Result<Value> {
        let (loc, dev) = self.resolve_target().await?;
        let url = format!(
            "{API_BASE}/active-user/location/{loc}/sensors/{dev}/feed?fields=Temperature,Humidity,WindSpeed,Rain&from=0",
        );
        self.get_json(&url).await
    }
}

/// Pick the most recent value out of a LaCrosse "feed" response slot.
/// The feed structure is:
///   { "Temperature": { "values": [{ "u": <epoch>, "s": <value> }, ...] }, ... }
/// We grab the last entry's `s` as a float.
fn latest_value(feed: &Value, key: &str) -> Option<f64> {
    feed.get(key)?
        .get("values")?
        .as_array()?
        .last()?
        .get("s")?
        .as_f64()
}

fn extract_fields(feed: &Value) -> Vec<(WeatherField, f64)> {
    let mut out = Vec::new();
    // LaCrosse stations report Temperature in °C and rainfall in mm.
    if let Some(t) = latest_value(feed, "Temperature") {
        out.push((WeatherField::AirTempF, c_to_f(t)));
    }
    if let Some(h) = latest_value(feed, "Humidity") {
        out.push((WeatherField::RhPct, h));
    }
    if let Some(w) = latest_value(feed, "WindSpeed") {
        // WindSpeed is km/h.
        out.push((WeatherField::WindMph, kph_to_mph(w)));
    }
    if let Some(r) = latest_value(feed, "Rain") {
        out.push((WeatherField::RainTodayIn, mm_to_in(r)));
    }
    out
}

#[async_trait]
impl WeatherSource for Lacrosse {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::RainTodayIn);
        SourceCaps {
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
            | WeatherField::WindMph
            | WeatherField::RainTodayIn => 65,
            _ => i32::MIN,
        }
    }

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        // The loop (tick, missed-tick policy, fetch metric, reachability
        // edges, shutdown) is `run_polling`'s; one poll is one feed read.
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "LaCrosse",
            POLL_INTERVAL,
            bus,
            shutdown,
            |s| async move {
                let feed = s.fetch_feed().await?;
                let fields = extract_fields(&feed);
                let now = chrono::Utc::now().timestamp();
                Ok(Poll::observation(&s.id, fields, now))
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lc_test() -> Lacrosse {
        Lacrosse::new(
            "lc",
            LacrosseConfig {
                email: "user@example.com".into(),
                password: "pw".into(),
                device_id: None,
            },
        )
    }

    #[test]
    fn caps_advertise_live() {
        let l = lc_test();
        let caps = l.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::AirTempF));
        assert!(caps.fields.contains(&WeatherField::RainTodayIn));
    }

    #[test]
    fn extract_fields_converts_units() {
        let feed = json!({
            "Temperature": { "values": [{ "u": 1, "s": 0.0 }, { "u": 2, "s": 25.0 }] },
            "Humidity":    { "values": [{ "u": 2, "s": 50.0 }] },
            "WindSpeed":   { "values": [{ "u": 2, "s": 10.0 }] }, // km/h
            "Rain":        { "values": [{ "u": 2, "s": 25.4 }] }, // mm
        });
        let f = extract_fields(&feed);
        let t = f
            .iter()
            .find(|(k, _)| *k == WeatherField::AirTempF)
            .unwrap()
            .1;
        let w = f
            .iter()
            .find(|(k, _)| *k == WeatherField::WindMph)
            .unwrap()
            .1;
        let r = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainTodayIn)
            .unwrap()
            .1;
        // 25°C = 77°F
        assert!((t - 77.0).abs() < 0.01);
        // 10 km/h ≈ 6.21 mph
        assert!((w - 6.21).abs() < 0.05);
        // 25.4 mm = 1.0 in
        assert!((r - 1.0).abs() < 0.05);
    }

    #[test]
    fn latest_value_picks_last_entry() {
        let feed = json!({
            "Temperature": { "values": [{ "u": 1, "s": 20.0 }, { "u": 2, "s": 21.5 }] }
        });
        assert_eq!(latest_value(&feed, "Temperature"), Some(21.5));
        assert_eq!(latest_value(&feed, "Missing"), None);
    }

    #[test]
    fn an_outage_is_not_a_rejected_token() {
        // Only a 401 carried by a reqwest error earns a re-login; a
        // parse or transport failure must not burn a login per poll.
        assert!(!token_rejected(&anyhow::anyhow!("timeout")));
        assert!(!token_rejected(&anyhow::anyhow!(
            "lacrosse locations response missing items[]"
        )));
    }
}
