// Apple WeatherKit REST source.
//
// GET https://weatherkit.apple.com/api/v1/weather/{lang}/{lat}/{lon}
//     ?dataSets=currentWeather,forecastDaily,forecastHourly&timezone={tz}
// Authorization: Bearer <ES256 JWT signed with the developer's .p8 key>.
//
// Emits a live Observation from currentWeather and a full ForecastSnapshot from
// forecastDaily.days[] + forecastHourly.hours[]. WeatherKit reports metric units
// (temp C, wind km/h, precip mm, pressure mb, humidity/POP/cloud 0..1), so every
// field is converted on the way out. Field names follow Apple's documented
// WeatherKit REST schema (developer.apple.com/documentation/weatherkitrestapi);
// currentWeather + hourly map directly, daily wind comes from the daytime part.
//
// Auth: a short-lived ES256 JWT is minted per fetch from the team/key/service
// ids + the PKCS#8 .p8 private key (pure-Rust p256 signing, no network).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use std::collections::HashSet;
use tracing::debug;

use crate::config::schema::{Location, WeatherKitConfig};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::poll::{run_polling, Poll};
use crate::units::{c_to_f, kph_to_mph, mm_to_in};

const API_BASE: &str = "https://weatherkit.apple.com/api/v1/weather";
const POLL_INTERVAL: Duration = Duration::from_secs(10 * 60); // 10 min
const WK_TIMEOUT: Duration = Duration::from_secs(15);
/// JWT lifetime. Apple allows up to ~6 months; a fresh short-lived token per
/// poll keeps the blast radius small and avoids clock-skew renewal logic.
const JWT_TTL_S: i64 = 3600;

pub struct WeatherKit {
    id: String,
    config: WeatherKitConfig,
    location: Location,
    timezone: String,
}

// ---- unit helpers ----
fn mb_to_inhg(mb: f64) -> f64 {
    mb * 0.029_53
}
fn frac_to_pct(f: f64) -> u32 {
    ((f * 100.0).round() as i64).clamp(0, 100) as u32
}
fn iso_to_epoch(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

/// base64url without padding (JWS).
fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Mint an ES256 JWT for WeatherKit from the config + the current time. Pure
/// computation (the signing key is parsed from the configured PKCS#8 PEM); no
/// network. Returned token is `Bearer`-ready.
fn build_jwt(config: &WeatherKitConfig, now: i64) -> anyhow::Result<String> {
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    let header = serde_json::json!({
        "alg": "ES256",
        "kid": config.key_id,
        "id": format!("{}.{}", config.team_id, config.service_id),
        "typ": "JWT",
    });
    let payload = serde_json::json!({
        "iss": config.team_id,
        "iat": now,
        "exp": now + JWT_TTL_S,
        "sub": config.service_id,
    });
    let signing_input = format!(
        "{}.{}",
        b64url(&serde_json::to_vec(&header)?),
        b64url(&serde_json::to_vec(&payload)?),
    );
    let key = SigningKey::from_pkcs8_pem(config.private_key_pem.trim())
        .map_err(|e| anyhow::anyhow!("invalid WeatherKit private key (PKCS#8 PEM): {e}"))?;
    let sig: Signature = key.sign(signing_input.as_bytes());
    let sig_bytes = sig.to_bytes();
    Ok(format!("{signing_input}.{}", b64url(sig_bytes.as_ref())))
}

// ---- WeatherKit response shapes (subset) ----
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WkResponse {
    current_weather: Option<CurrentWeather>,
    #[serde(default)]
    forecast_daily: Option<ForecastDaily>,
    #[serde(default)]
    forecast_hourly: Option<ForecastHourly>,
    #[serde(default)]
    forecast_next_hour: Option<NextHourForecast>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentWeather {
    temperature: Option<f64>,          // C
    temperature_apparent: Option<f64>, // C
    humidity: Option<f64>,             // 0..1
    pressure: Option<f64>,             // mb
    wind_speed: Option<f64>,           // km/h
    wind_gust: Option<f64>,            // km/h
    wind_direction: Option<f64>,       // deg
    uv_index: Option<f64>,
    cloud_cover: Option<f64>,             // 0..1
    precipitation_intensity: Option<f64>, // mm/h (current rain rate)
}

/// Apple's minute-by-minute nowcast (the "next hour" precipitation forecast).
/// Only available in supported regions; absent elsewhere. We read minutes[0] as
/// the current rain rate when currentWeather.precipitationIntensity is absent.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextHourForecast {
    #[serde(default)]
    minutes: Vec<ForecastMinute>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForecastMinute {
    precipitation_intensity: Option<f64>, // mm/h
}

#[derive(Debug, Deserialize)]
struct ForecastDaily {
    #[serde(default)]
    days: Vec<DayWeather>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DayWeather {
    forecast_start: Option<String>,
    temperature_max: Option<f64>,      // C
    temperature_min: Option<f64>,      // C
    precipitation_chance: Option<f64>, // 0..1
    precipitation_amount: Option<f64>, // mm
    max_uv_index: Option<f64>,
    sunrise: Option<String>,
    sunset: Option<String>,
    #[serde(default)]
    condition_code: Option<String>,
    // Wind lives in the daytime part of the day.
    #[serde(default)]
    daytime_forecast: Option<DayPart>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DayPart {
    wind_speed: Option<f64>,          // km/h
    wind_gust_speed_max: Option<f64>, // km/h
}

#[derive(Debug, Deserialize)]
struct ForecastHourly {
    #[serde(default)]
    hours: Vec<HourWeather>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HourWeather {
    forecast_start: Option<String>,
    temperature: Option<f64>,          // C
    temperature_apparent: Option<f64>, // C
    humidity: Option<f64>,             // 0..1
    wind_speed: Option<f64>,           // km/h
    wind_direction: Option<f64>,       // deg
    precipitation_chance: Option<f64>, // 0..1
    precipitation_amount: Option<f64>, // mm
    cloud_cover: Option<f64>,          // 0..1
    #[serde(default)]
    condition_code: Option<String>,
}

/// Map an Apple WeatherKit conditionCode string to a WMO code for the shared
/// glyph registry. Loose mapping; unknown codes fall back to 0 (clear).
fn apple_to_wmo(code: &str) -> u32 {
    match code {
        "Clear" | "MostlyClear" | "Hot" | "Frigid" => 0,
        "PartlyCloudy" => 2,
        "MostlyCloudy" | "Cloudy" => 3,
        "Fog" => 45,
        "Haze" | "Smoke" | "Dust" => 45,
        "Drizzle" => 51,
        "FreezingDrizzle" => 56,
        "Rain" | "MixedRainfall" => 63,
        "HeavyRain" => 65,
        "FreezingRain" => 66,
        "Showers" | "ScatteredShowers" | "IsolatedThunderstorms" => 80,
        "Sleet" | "MixedRainAndSleet" | "MixedSnowAndSleet" => 67,
        "Flurries" | "Snow" | "SnowShowers" | "ScatteredSnowShowers" | "MixedRainAndSnow" => 73,
        "HeavySnow" | "Blizzard" | "BlowingSnow" => 75,
        "Hail" => 96,
        "Thunderstorm"
        | "ScatteredThunderstorms"
        | "SevereThunderstorm"
        | "Hurricane"
        | "TropicalStorm" => 95,
        _ => 0,
    }
}

fn wmo_opt(code: &Option<String>) -> u32 {
    code.as_deref().map(apple_to_wmo).unwrap_or(0)
}

/// Build a ForecastSnapshot from a WeatherKit response.
///
/// Missing rain amounts remain unknown. WeatherKit's daily amount is required
/// by its schema; neither an incomplete response nor an optional hourly field
/// constitutes an explicit dry forecast.
fn build_snapshot(resp: &WkResponse, timezone: &str, now_epoch: i64) -> ForecastSnapshot {
    let daily: Vec<DailyEntry> = resp
        .forecast_daily
        .as_ref()
        .map(|fd| {
            fd.days
                .iter()
                .map(|d| {
                    let part = d.daytime_forecast.as_ref();
                    DailyEntry {
                        // WeatherKit stamps forecastStart.
                        day_marker: crate::engine::clock::DayMarker::inside_local_day(
                            d.forecast_start.as_deref().map(iso_to_epoch).unwrap_or(0),
                        ),
                        weather_code: wmo_opt(&d.condition_code),
                        temp_max_f: d.temperature_max.map(c_to_f).filter(|t| t.is_finite()),
                        temp_min_f: d.temperature_min.map(c_to_f).filter(|t| t.is_finite()),
                        // The daily DayPart we deserialize carries no RH; filled
                        // from hourly by backfill_daily_humidity below.
                        humidity_pct: None,
                        precip_sum_in: d
                            .precipitation_amount
                            .filter(|v| crate::forecast::precip::valid_amount(*v))
                            .map(mm_to_in),
                        // Absent chance stays None, not a fabricated 0%.
                        precip_probability_max: d.precipitation_chance.map(frac_to_pct),
                        wind_max_mph: part
                            .and_then(|p| p.wind_speed)
                            .map(kph_to_mph)
                            .filter(|w| w.is_finite() && *w >= 0.0),
                        wind_gust_max_mph: kph_to_mph(
                            part.and_then(|p| p.wind_gust_speed_max).unwrap_or(0.0),
                        ),
                        uv_index_max: d.max_uv_index.unwrap_or(0.0),
                        sunrise_epoch: d.sunrise.as_deref().map(iso_to_epoch).unwrap_or(0),
                        sunset_epoch: d.sunset.as_deref().map(iso_to_epoch).unwrap_or(0),
                        ..Default::default()
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let hourly: Vec<HourlyEntry> = resp
        .forecast_hourly
        .as_ref()
        .map(|fh| {
            fh.hours
                .iter()
                .map(|h| HourlyEntry {
                    time_epoch: h.forecast_start.as_deref().map(iso_to_epoch).unwrap_or(0),
                    weather_code: wmo_opt(&h.condition_code),
                    temp_f: h.temperature.map(c_to_f).filter(|t| t.is_finite()),
                    apparent_temp_f: c_to_f(h.temperature_apparent.unwrap_or(0.0)),
                    precip_in: h
                        .precipitation_amount
                        .filter(|v| crate::forecast::precip::valid_amount(*v))
                        .map(mm_to_in),
                    precip_probability: h.precipitation_chance.map(frac_to_pct),
                    wind_mph: h
                        .wind_speed
                        .map(kph_to_mph)
                        .filter(|w| w.is_finite() && *w >= 0.0),
                    wind_dir_deg: (h.wind_direction.unwrap_or(0.0).round() as i64).rem_euclid(360)
                        as u32,
                    humidity_pct: h
                        .humidity
                        .filter(|rh| rh.is_finite() && (0.0..=1.0).contains(rh))
                        .map(frac_to_pct),
                    cloud_cover_pct: frac_to_pct(h.cloud_cover.unwrap_or(0.0)),
                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default();

    let mut snap = ForecastSnapshot {
        last_refresh_epoch: now_epoch,
        source_reachable: true,
        source_label: "WeatherKit".to_string(),
        timezone: timezone.to_string(),
        daily,
        past_daily: vec![],
        hourly,
        ..Default::default()
    };
    // Pair each day's high temp with THAT day's afternoon humidity (hourly).
    snap.backfill_daily_humidity(crate::timeutil::deployment_calendar());
    snap
}

/// Current-conditions Observation fields from currentWeather, with the
/// minute-by-minute nextHourForecast nowcast used as a rain-rate fallback.
fn current_fields(resp: &WkResponse) -> Vec<(WeatherField, f64)> {
    let mut f = Vec::new();
    let Some(c) = resp.current_weather.as_ref() else {
        return f;
    };
    if let Some(v) = c.temperature {
        f.push((WeatherField::AirTempF, c_to_f(v)));
    }
    if let Some(v) = c.humidity {
        f.push((WeatherField::RhPct, (v * 100.0).clamp(0.0, 100.0)));
    }
    if let Some(v) = c.pressure {
        f.push((WeatherField::PressureInHg, mb_to_inhg(v)));
    }
    if let Some(v) = c.wind_speed {
        f.push((WeatherField::WindMph, kph_to_mph(v)));
    }
    if let Some(v) = c.wind_gust {
        f.push((WeatherField::WindGustMph, kph_to_mph(v)));
    }
    if let Some(v) = c.wind_direction {
        f.push((WeatherField::WindBearingDeg, v));
    }
    if let Some(v) = c.uv_index {
        f.push((WeatherField::UvIndex, v));
    }
    // Current rain rate. Apple reports precipitationIntensity in mm/h; the
    // canonical RainIntensityInHr is in/hr, so mm_to_in is the right per-hour
    // conversion (the time base is identical on both sides). When the field is
    // absent, fall back to the first minute of Apple's nextHourForecast nowcast
    // (also mm/h), which is the "now" precipitation rate at minute 0.
    let rain_mm_hr = c.precipitation_intensity.or_else(|| {
        resp.forecast_next_hour
            .as_ref()
            .and_then(|n| n.minutes.first())
            .and_then(|m| m.precipitation_intensity)
    });
    if let Some(v) = rain_mm_hr {
        f.push((WeatherField::RainIntensityInHr, mm_to_in(v)));
    }
    // Cloud cover (0..1 -> percent). Only emitted if the shared WeatherField
    // catalog gains a CloudCoverPct current-conditions field; today the enum
    // has no such variant, so currentWeather.cloudCover stays parsed-but-unused
    // here (it still rides through forecastHourly.cloudCover -> cloud_cover_pct).
    let _ = c.cloud_cover;
    let _ = c.temperature_apparent; // apparent temp is recomputed downstream
    f
}

impl WeatherKit {
    pub fn new(
        id: impl Into<String>,
        config: WeatherKitConfig,
        location: Location,
        timezone: Option<String>,
    ) -> Self {
        // Apple requires the timezone query param for forecastDaily day
        // boundaries. Prefer the configured tz; when absent (e.g. a hand-written
        // localsky.toml that skipped the wizard), derive it from the location so
        // the daily forecast still groups correctly instead of dropping the param.
        let timezone = timezone
            .filter(|t| !t.trim().is_empty())
            .or_else(|| crate::timeutil::tz_name_for(location.lat, location.lon))
            .unwrap_or_default();
        Self {
            id: id.into(),
            config,
            location,
            timezone,
        }
    }

    async fn fetch(&self) -> anyhow::Result<WkResponse> {
        let now = chrono::Utc::now().timestamp();
        let jwt = build_jwt(&self.config, now)?;
        let lang = if self.config.language.trim().is_empty() {
            "en"
        } else {
            self.config.language.trim()
        };
        let base = format!(
            "{API_BASE}/{lang}/{lat}/{lon}",
            lat = self.location.lat,
            lon = self.location.lon,
        );
        let mut url = reqwest::Url::parse(&base)?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair(
                "dataSets",
                "currentWeather,forecastDaily,forecastHourly,nextHourForecast",
            );
            if !self.timezone.trim().is_empty() {
                q.append_pair("timezone", self.timezone.trim());
            }
        }
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(url.as_str(), WK_TIMEOUT).await?;
        let resp = client
            .get(safe_url)
            .bearer_auth(jwt)
            .send()
            .await?
            .error_for_status()?;
        Ok(crate::net::safe_fetch::read_json_capped(resp).await?)
    }
}

#[async_trait]
impl WeatherSource for WeatherKit {
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
        fields.insert(WeatherField::UvIndex);
        fields.insert(WeatherField::RainIntensityInHr);
        fields.insert(WeatherField::ForecastDaily);
        fields.insert(WeatherField::ForecastHourly);
        SourceCaps {
            live_current: false,
            hourly_forecast_hours: 240,
            daily_forecast_days: 10,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            WeatherField::ForecastDaily | WeatherField::ForecastHourly => 50,
            WeatherField::AirTempF
            | WeatherField::RhPct
            | WeatherField::PressureInHg
            | WeatherField::WindMph
            | WeatherField::WindGustMph
            | WeatherField::WindBearingDeg
            | WeatherField::UvIndex
            | WeatherField::RainIntensityInHr => 25,
            _ => i32::MIN,
        }
    }

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        // The loop (tick, missed-tick policy, fetch metric, reachability
        // edges in both directions, shutdown) is `run_polling`'s; this
        // closure is one poll: mint a JWT + fetch, map currentWeather, build
        // the forecast snapshot from the SAME response.
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "WeatherKit",
            POLL_INTERVAL,
            bus,
            shutdown,
            |s: Arc<Self>| async move {
                let resp = s.fetch().await?;
                let now = chrono::Utc::now().timestamp();
                let mut poll = Poll::observation(&s.id, current_fields(&resp), now);
                let has_fc = resp
                    .forecast_daily
                    .as_ref()
                    .map(|d| !d.days.is_empty())
                    .unwrap_or(false)
                    || resp
                        .forecast_hourly
                        .as_ref()
                        .map(|h| !h.hours.is_empty())
                        .unwrap_or(false);
                if has_fc {
                    let snapshot = build_snapshot(&resp, &s.timezone, now);
                    debug!(
                        source_id = %s.id,
                        daily_n = snapshot.daily.len(),
                        hourly_n = snapshot.hourly.len(),
                        "WeatherKit forecast"
                    );
                    poll = poll.with(SourceEvent::Forecast {
                        source_id: s.id.clone(),
                        snapshot,
                        at_epoch: now,
                    });
                }
                Ok(poll)
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // A throwaway P-256 PKCS#8 PEM for signing tests (NOT a real Apple key).
    // Generated with: openssl ecparam -genkey -name prime256v1 -noout
    //   | openssl pkcs8 -topk8 -nocrypt
    const TEST_P8: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgevZzL1gdAFr88hb2\nOF/2NxApJCzGCEDdfSp6VQO30hyhRANCAAQRWz+jn65BtOMvdyHKcvjBeBSDZH2r\n1RTwjmYSi9R/zpBnuQ4EiMnCqfMPWiZqB4QdbAd0E7oH50VpuZ1P087G\n-----END PRIVATE KEY-----\n";

    fn cfg() -> WeatherKitConfig {
        WeatherKitConfig {
            key_id: "ABC123KEYID".into(),
            team_id: "DEF456TEAM".into(),
            service_id: "com.example.localsky".into(),
            private_key_pem: TEST_P8.into(),
            language: "en".into(),
        }
    }

    #[test]
    fn jwt_has_three_parts_and_decodable_claims() {
        let token = build_jwt(&cfg(), 1_715_000_000).expect("sign");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT must be header.payload.signature");
        let dec = |p: &str| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(p)
                .unwrap()
        };
        let header: serde_json::Value = serde_json::from_slice(&dec(parts[0])).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&dec(parts[1])).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "ABC123KEYID");
        assert_eq!(header["id"], "DEF456TEAM.com.example.localsky");
        assert_eq!(payload["iss"], "DEF456TEAM");
        assert_eq!(payload["sub"], "com.example.localsky");
        assert_eq!(payload["iat"], 1_715_000_000i64);
        assert_eq!(payload["exp"], 1_715_000_000i64 + JWT_TTL_S);
        // Signature is the raw 64-byte r||s, base64url-encoded (86 chars).
        assert_eq!(
            parts[2].len(),
            86,
            "ES256 sig is 64 bytes -> 86 b64url chars"
        );
    }

    #[test]
    fn missing_or_invalid_temperatures_do_not_become_32_fahrenheit() {
        let mut resp: WkResponse = serde_json::from_value(json!({
            "forecastDaily": { "days": [{ "forecastStart": "2026-06-24T00:00:00Z" }] },
            "forecastHourly": { "hours": [{ "forecastStart": "2026-06-24T01:00:00Z" }] }
        }))
        .unwrap();
        for invalid in [None, Some(f64::NAN), Some(f64::INFINITY), Some(f64::MAX)] {
            let day = &mut resp.forecast_daily.as_mut().unwrap().days[0];
            day.temperature_max = invalid;
            day.temperature_min = invalid;
            resp.forecast_hourly.as_mut().unwrap().hours[0].temperature = invalid;
            let snapshot = build_snapshot(&resp, "UTC", 1000);
            assert_eq!(snapshot.daily[0].temp_max_f, None);
            assert_eq!(snapshot.daily[0].temp_min_f, None);
            assert_eq!(snapshot.hourly[0].temp_f, None);
        }
        resp.forecast_daily.as_mut().unwrap().days[0].temperature_max = Some(0.0);
        resp.forecast_hourly.as_mut().unwrap().hours[0].temperature = Some(0.0);
        let snapshot = build_snapshot(&resp, "UTC", 1000);
        assert_eq!(snapshot.daily[0].temp_max_f, Some(32.0));
        assert_eq!(snapshot.hourly[0].temp_f, Some(32.0));
    }

    #[test]
    fn absent_invalid_and_zero_wind_and_humidity_stay_distinct() {
        let mut resp: WkResponse = serde_json::from_value(json!({
            "forecastDaily": { "days": [{ "daytimeForecast": {} }] },
            "forecastHourly": { "hours": [{}] }
        }))
        .unwrap();
        for invalid in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            resp.forecast_daily.as_mut().unwrap().days[0]
                .daytime_forecast
                .as_mut()
                .unwrap()
                .wind_speed = invalid;
            let hour = &mut resp.forecast_hourly.as_mut().unwrap().hours[0];
            hour.wind_speed = invalid;
            hour.humidity = invalid;
            let snapshot = build_snapshot(&resp, "UTC", 1000);
            assert_eq!(snapshot.daily[0].wind_max_mph, None);
            assert_eq!(snapshot.hourly[0].wind_mph, None);
            assert_eq!(snapshot.hourly[0].humidity_pct, None);
        }
        resp.forecast_hourly.as_mut().unwrap().hours[0].humidity = Some(1.1);
        assert_eq!(
            build_snapshot(&resp, "UTC", 1000).hourly[0].humidity_pct,
            None
        );
        resp.forecast_daily.as_mut().unwrap().days[0]
            .daytime_forecast
            .as_mut()
            .unwrap()
            .wind_speed = Some(0.0);
        let hour = &mut resp.forecast_hourly.as_mut().unwrap().hours[0];
        hour.wind_speed = Some(0.0);
        hour.humidity = Some(0.0);
        let snapshot = build_snapshot(&resp, "UTC", 1000);
        assert_eq!(snapshot.daily[0].wind_max_mph, Some(0.0));
        assert_eq!(snapshot.hourly[0].wind_mph, Some(0.0));
        assert_eq!(snapshot.hourly[0].humidity_pct, Some(0));
    }

    #[test]
    fn build_snapshot_converts_units() {
        let resp: WkResponse = serde_json::from_value(json!({
            "currentWeather": { "temperature": 20.0, "humidity": 0.5, "windSpeed": 16.0934 },
            "forecastDaily": { "days": [ {
                "forecastStart": "2026-06-24T00:00:00Z",
                "temperatureMax": 30.0, "temperatureMin": 10.0,
                "precipitationChance": 0.4, "precipitationAmount": 25.4,
                "maxUvIndex": 8, "conditionCode": "Rain",
                "daytimeForecast": { "windSpeed": 16.0934, "windGustSpeedMax": 32.1868 }
            } ] },
            "forecastHourly": { "hours": [ {
                "forecastStart": "2026-06-24T01:00:00Z",
                "temperature": 25.0, "humidity": 0.6, "windSpeed": 16.0934,
                "windDirection": 180, "precipitationChance": 0.2,
                "precipitationAmount": 2.54, "cloudCover": 0.75, "conditionCode": "Cloudy"
            } ] }
        }))
        .unwrap();
        let s = build_snapshot(&resp, "America/New_York", 1000);
        assert_eq!(s.source_label, "WeatherKit");
        let d = &s.daily[0];
        assert!((d.temp_max_f.unwrap() - 86.0).abs() < 0.01, "30C -> 86F");
        assert!((d.temp_min_f.unwrap() - 50.0).abs() < 0.01, "10C -> 50F");
        assert!(
            (d.precip_sum_in.unwrap() - 1.0).abs() < 0.01,
            "25.4mm -> 1in"
        );
        assert_eq!(d.precip_probability_max, Some(40));
        assert!(
            (d.wind_max_mph.unwrap() - 10.0).abs() < 0.05,
            "16.0934 km/h -> 10 mph"
        );
        assert!((d.wind_gust_max_mph - 20.0).abs() < 0.1);
        assert_eq!(d.weather_code, 63);
        let h = &s.hourly[0];
        assert!((h.temp_f.unwrap() - 77.0).abs() < 0.01, "25C -> 77F");
        assert_eq!(h.humidity_pct, Some(60));
        assert_eq!(h.cloud_cover_pct, 75);
        assert_eq!(h.precip_probability, Some(20));
        assert!((h.precip_in.unwrap() - 0.1).abs() < 0.01, "2.54mm -> 0.1in");
    }

    #[test]
    fn absent_precip_remains_unknown() {
        // A day/hour with precip fields omitted maps to 0 precip and an
        // ABSENT probability (None, same rule as the other forecast
        // sources): a missing chance is a provider gap, not a confident
        // "0% chance". Pins the behavior so a refactor can't regress it to
        // a fabricated zero.
        let resp: WkResponse = serde_json::from_value(json!({
            "forecastDaily": { "days": [ {
                "forecastStart": "2026-06-24T00:00:00Z",
                "temperatureMax": 25.0, "temperatureMin": 15.0
            } ] },
            "forecastHourly": { "hours": [ {
                "forecastStart": "2026-06-24T01:00:00Z", "temperature": 20.0
            } ] }
        }))
        .unwrap();
        let s = build_snapshot(&resp, "UTC", 1000);
        assert_eq!(s.daily[0].precip_sum_in, None);
        assert_eq!(s.daily[0].precip_probability_max, None);
        assert_eq!(s.hourly[0].precip_in, None);
        assert_eq!(s.hourly[0].precip_probability, None);
    }

    #[test]
    fn current_fields_convert_and_skip_absent() {
        let resp: WkResponse = serde_json::from_value(json!({
            "currentWeather": { "temperature": 0.0, "humidity": 0.9, "pressure": 1015.0 }
        }))
        .unwrap();
        let f = current_fields(&resp);
        let temp = f
            .iter()
            .find(|(k, _)| *k == WeatherField::AirTempF)
            .unwrap()
            .1;
        assert!((temp - 32.0).abs() < 0.01, "0C -> 32F");
        let rh = f.iter().find(|(k, _)| *k == WeatherField::RhPct).unwrap().1;
        assert!((rh - 90.0).abs() < 0.01);
        // No wind in the payload -> no wind field.
        assert!(!f.iter().any(|(k, _)| *k == WeatherField::WindMph));
        // No precip fields anywhere -> no rain intensity.
        assert!(!f.iter().any(|(k, _)| *k == WeatherField::RainIntensityInHr));
    }

    #[test]
    fn current_rain_intensity_maps_mm_per_hr_to_in_per_hr() {
        // currentWeather.precipitationIntensity is mm/h; 25.4 mm/h -> 1.0 in/hr.
        let resp: WkResponse = serde_json::from_value(json!({
            "currentWeather": { "temperature": 18.0, "precipitationIntensity": 25.4 }
        }))
        .unwrap();
        let f = current_fields(&resp);
        let rate = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainIntensityInHr)
            .expect("rain intensity emitted")
            .1;
        assert!((rate - 1.0).abs() < 1e-6, "25.4 mm/h -> 1.0 in/hr");
    }

    #[test]
    fn next_hour_nowcast_backs_current_rain_rate() {
        // When currentWeather has no precipitationIntensity, minutes[0] of the
        // nextHourForecast nowcast supplies the current rain rate (mm/h -> in/hr).
        let resp: WkResponse = serde_json::from_value(json!({
            "currentWeather": { "temperature": 18.0 },
            "forecastNextHour": { "minutes": [
                { "precipitationIntensity": 12.7 },
                { "precipitationIntensity": 0.0 }
            ] }
        }))
        .unwrap();
        let f = current_fields(&resp);
        let rate = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainIntensityInHr)
            .expect("rain intensity from nowcast")
            .1;
        assert!((rate - 0.5).abs() < 1e-6, "12.7 mm/h -> 0.5 in/hr");
    }

    #[test]
    fn current_precip_intensity_wins_over_nowcast() {
        // currentWeather.precipitationIntensity is the primary signal; the
        // nextHour nowcast is only a fallback for when it is absent.
        let resp: WkResponse = serde_json::from_value(json!({
            "currentWeather": { "temperature": 18.0, "precipitationIntensity": 25.4 },
            "forecastNextHour": { "minutes": [ { "precipitationIntensity": 0.0 } ] }
        }))
        .unwrap();
        let f = current_fields(&resp);
        let rate = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainIntensityInHr)
            .unwrap()
            .1;
        assert!(
            (rate - 1.0).abs() < 1e-6,
            "current wins: 25.4 mm/h -> 1.0 in/hr"
        );
    }
    #[test]
    fn rain_amounts_require_finite_nonnegative_evidence() {
        let mut resp: WkResponse = serde_json::from_value(json!({
            "forecastDaily": { "days": [{}] }, "forecastHourly": { "hours": [{}] }
        }))
        .unwrap();
        for value in [
            None,
            Some(-1.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(0.0),
        ] {
            resp.forecast_daily.as_mut().unwrap().days[0].precipitation_amount = value;
            resp.forecast_hourly.as_mut().unwrap().hours[0].precipitation_amount = value;
            let snapshot = build_snapshot(&resp, "UTC", 1000);
            let expected = if value == Some(0.0) { Some(0.0) } else { None };
            assert_eq!(snapshot.daily[0].precip_sum_in, expected);
            assert_eq!(snapshot.hourly[0].precip_in, expected);
        }
    }
}
