// AmbientWeather cloud source, api.ambientweather.net REST API.
//
// AmbientWeather sells consumer weather stations (WS-2902, WS-5000,
// PWS) that auto-upload to ambientweather.net. Their REST API exposes
// the live observations plus historical data per device.
//
// Auth uses TWO keys: app_key (per-application) + api_key (per-user).
// Each MAC address identifies one of the user's devices.
//
// Endpoint:
//   GET /v1/devices/{mac}?applicationKey={app}&apiKey={api}&limit=1
//
// The response is an array of recent observations; the first entry is
// the most recent. We poll every 60s, well within the 1 req/sec rate
// limit. Fields include tempf, humidity, baromrelin, windspeedmph,
// windgustmph, winddir, uv, solarradiation, hourlyrainin, dailyrainin.
//
// The poll loop itself (tick, missed-tick policy, fetch metric,
// reachability edges, shutdown) is `sources::poll::run_polling`; this
// file only owns the request and the field mapping.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tracing::warn;

use crate::config::schema::AmbientWeatherConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, WeatherField, WeatherSource,
};
use crate::sources::poll::{run_polling, Poll};

const API_BASE: &str = "https://api.ambientweather.net/v1";
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct AmbientWeather {
    id: String,
    config: AmbientWeatherConfig,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct Observation {
    tempf: Option<f64>,
    // Ambient returns the calculated dew point as camelCase `dewPoint`; without
    // the rename it deserialized to None and DewPointF was never emitted despite
    // capabilities() advertising it (matches pirate_weather's `dewPoint` handling).
    #[serde(rename = "dewPoint")]
    dewpoint: Option<f64>,
    humidity: Option<f64>,
    baromrelin: Option<f64>, // inHg already
    windspeedmph: Option<f64>,
    windgustmph: Option<f64>,
    winddir: Option<f64>,
    uv: Option<f64>,
    solarradiation: Option<f64>, // W/m²
    hourlyrainin: Option<f64>,
    dailyrainin: Option<f64>,
}

impl Observation {
    /// The engine fields this observation carries, in emit order. A key the
    /// station omits (no rain gauge, no solar sensor) is simply absent, never
    /// a fabricated 0.0.
    fn fields(self) -> Vec<(WeatherField, f64)> {
        [
            (WeatherField::AirTempF, self.tempf),
            (WeatherField::DewPointF, self.dewpoint),
            (WeatherField::RhPct, self.humidity),
            (WeatherField::PressureInHg, self.baromrelin),
            (WeatherField::WindMph, self.windspeedmph),
            (WeatherField::WindGustMph, self.windgustmph),
            (WeatherField::WindBearingDeg, self.winddir),
            (WeatherField::UvIndex, self.uv),
            (WeatherField::SolarWm2, self.solarradiation),
            (WeatherField::RainTodayIn, self.dailyrainin),
            (WeatherField::RainIntensityInHr, self.hourlyrainin),
        ]
        .into_iter()
        .filter_map(|(field, v)| v.map(|v| (field, v)))
        .collect()
    }
}

impl AmbientWeather {
    pub fn new(id: impl Into<String>, config: AmbientWeatherConfig) -> Self {
        Self {
            id: id.into(),
            config,
            client: crate::net::client(HTTP_TIMEOUT),
        }
    }

    async fn fetch_latest(&self) -> anyhow::Result<Option<Observation>> {
        // app_key/api_key are alphanumeric from AmbientWeather's dashboard
        // and the MAC's `:` chars are valid path-segment characters per
        // RFC 3986, so no percent-encoding is required here.
        let url = format!(
            "{API_BASE}/devices/{mac}?applicationKey={app}&apiKey={api}&limit=1",
            mac = self.config.mac_address,
            app = self.config.app_key,
            api = self.config.api_key,
        );
        let resp = self.client.get(&url).send().await?.error_for_status()?;
        let body: Vec<Observation> = resp.json().await?;
        Ok(body.into_iter().next())
    }

    /// One poll: the latest observation as engine fields. A 200 with an
    /// empty device array means the cloud answered but this MAC has nothing
    /// to read (brand-new station, or a wrong mac_address), which the poll
    /// reports as unreachable rather than as an online-but-silent station.
    async fn poll_once(self: Arc<Self>) -> anyhow::Result<Poll> {
        match self.fetch_latest().await? {
            Some(o) => Ok(Poll::observation(
                &self.id,
                o.fields(),
                chrono::Utc::now().timestamp(),
            )),
            None => {
                warn!(
                    source_id = %self.id,
                    "AmbientWeather returned 0 observations; check mac_address"
                );
                Ok(Poll::none().unreachable())
            }
        }
    }
}

#[async_trait]
impl WeatherSource for AmbientWeather {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::DewPointF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::PressureInHg);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindGustMph);
        fields.insert(WeatherField::WindBearingDeg);
        fields.insert(WeatherField::UvIndex);
        fields.insert(WeatherField::SolarWm2);
        fields.insert(WeatherField::RainTodayIn);
        fields.insert(WeatherField::RainIntensityInHr);
        SourceCaps {
            // AmbientWeather IS a live station (just cloud-routed),
            // unlike forecast sources.
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
            // Cloud-routed LAN station: between forecast (low) and a
            // direct-LAN station (highest). Aim ~70.
            WeatherField::AirTempF
            | WeatherField::DewPointF
            | WeatherField::RhPct
            | WeatherField::PressureInHg
            | WeatherField::WindMph
            | WeatherField::WindGustMph
            | WeatherField::WindBearingDeg
            | WeatherField::UvIndex
            | WeatherField::SolarWm2
            | WeatherField::RainTodayIn
            | WeatherField::RainIntensityInHr => 70,
            _ => i32::MIN,
        }
    }

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "AmbientWeather",
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
    use super::*;

    fn aw_test() -> AmbientWeather {
        AmbientWeather::new(
            "aw",
            AmbientWeatherConfig {
                app_key: "a".into(),
                api_key: "b".into(),
                mac_address: "AA:BB:CC:DD:EE:FF".into(),
            },
        )
    }

    #[test]
    fn caps_live_current() {
        let a = aw_test();
        let caps = a.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::SolarWm2));
    }

    #[test]
    fn priority_above_forecast() {
        let a = aw_test();
        // Cloud-routed station priority should beat the typical 25-50 of
        // a forecast source.
        assert!(a.priority(WeatherField::AirTempF) > 50);
    }

    /// A realistic GET /v1/devices/{mac}?limit=1 body, field names exactly
    /// as ambientweather.net returns them (the API docs' device-data
    /// example): camelCase CALCULATED fields (feelsLike, dewPoint),
    /// epoch-ms dateutc, the full rain family, and indoor/battery fields
    /// the adapter does not read. Everything unknown must parse-tolerate.
    const REALISTIC_DEVICE_PAYLOAD: &str = r#"[
      {
        "dateutc": 1751212345000,
        "tempf": 84.2,
        "humidity": 57,
        "windspeedmph": 4.7,
        "windgustmph": 8.1,
        "maxdailygust": 14.5,
        "winddir": 193,
        "baromrelin": 29.921,
        "baromabsin": 29.362,
        "tempinf": 74.1,
        "humidityin": 48,
        "hourlyrainin": 0.118,
        "eventrainin": 0.24,
        "dailyrainin": 0.36,
        "weeklyrainin": 1.02,
        "monthlyrainin": 2.75,
        "totalrainin": 48.61,
        "solarradiation": 612.4,
        "uv": 6,
        "battout": 1,
        "feelsLike": 88.9,
        "dewPoint": 67.3,
        "lastRain": "2026-06-29T10:04:00.000Z",
        "date": "2026-06-29T16:32:25.000Z"
      }
    ]"#;

    /// The payload-parse path fetch_latest feeds run(): a realistic
    /// /v1/devices document deserializes into the Observation the field
    /// mapping reads, with each engine-bound field carried by the right
    /// JSON key in the right unit.
    #[test]
    fn realistic_payload_extracts_engine_fields_with_expected_units() {
        let body: Vec<Observation> =
            serde_json::from_str(REALISTIC_DEVICE_PAYLOAD).expect("realistic payload parses");
        let o = body.into_iter().next().expect("one observation");

        // Fahrenheit / percent / mph / degrees / index / W/m2, passed
        // through 1:1 to AirTempF / RhPct / WindMph / WindGustMph /
        // WindBearingDeg / UvIndex / SolarWm2.
        assert_eq!(o.tempf, Some(84.2));
        assert_eq!(o.humidity, Some(57.0));
        assert_eq!(o.windspeedmph, Some(4.7));
        assert_eq!(o.windgustmph, Some(8.1));
        assert_eq!(o.winddir, Some(193.0));
        assert_eq!(o.uv, Some(6.0));
        assert_eq!(o.solarradiation, Some(612.4));

        // Pressure: baromRELin (sea-level RELATIVE, already inHg) is the
        // key mapped to PressureInHg, NOT baromabsin (absolute). The two
        // differ in the payload, so a key mixup would show here.
        assert_eq!(o.baromrelin, Some(29.921));

        // RAIN units: dailyrainin is the day's ACCUMULATION (inches) ->
        // RainTodayIn; hourlyrainin is the current RATE (in/hr) ->
        // RainIntensityInHr. The values differ, so swapping the keys (or
        // grabbing eventrainin/weeklyrainin) would mis-feed the
        // observed-rain gate and fail here.
        assert_eq!(o.dailyrainin, Some(0.36));
        assert_eq!(o.hourlyrainin, Some(0.118));
    }

    /// The field mapping the poll publishes: every advertised field comes
    /// out of the realistic payload under the right WeatherField, with the
    /// two rain keys landing on accumulation vs rate and not swapped.
    #[test]
    fn realistic_payload_maps_to_all_advertised_fields() {
        let body: Vec<Observation> =
            serde_json::from_str(REALISTIC_DEVICE_PAYLOAD).expect("realistic payload parses");
        let o = body.into_iter().next().expect("one observation");
        let fields = o.fields();
        assert_eq!(fields.len(), 11, "every advertised field is carried");
        assert!(fields.contains(&(WeatherField::RainTodayIn, 0.36)));
        assert!(fields.contains(&(WeatherField::RainIntensityInHr, 0.118)));
        assert!(fields.contains(&(WeatherField::PressureInHg, 29.921)));
        assert!(fields.contains(&(WeatherField::DewPointF, 67.3)));
    }

    /// ambientweather.net serves the calculated dew point as camelCase
    /// "dewPoint" (see the API docs' device-data example); the `#[serde(rename)]`
    /// maps it so DewPointF is actually emitted (capabilities() advertises it).
    #[test]
    fn realistic_payload_parses_camelcase_dew_point() {
        let body: Vec<Observation> =
            serde_json::from_str(REALISTIC_DEVICE_PAYLOAD).expect("realistic payload parses");
        let o = body.into_iter().next().expect("one observation");
        assert_eq!(
            o.dewpoint,
            Some(67.3),
            "camelCase dewPoint maps to `dewpoint`"
        );
    }

    /// A station without a rain gauge / solar sensor omits those keys
    /// entirely: they must parse as None (fields not emitted), never a
    /// fabricated 0.0 that would feed a false-dry into the observed-rain
    /// gate. An empty device array (brand-new station) yields no
    /// observation at all.
    #[test]
    fn sparse_and_empty_payloads_yield_none_not_zero() {
        let body: Vec<Observation> = serde_json::from_str(r#"[{ "tempf": 51.3, "humidity": 82 }]"#)
            .expect("sparse payload parses");
        let o = body.into_iter().next().expect("one observation");
        assert_eq!(o.tempf, Some(51.3));
        assert_eq!(o.humidity, Some(82.0));
        assert_eq!(o.dailyrainin, None, "absent rain key must stay None");
        assert_eq!(o.hourlyrainin, None);
        assert_eq!(o.baromrelin, None);
        assert_eq!(o.windspeedmph, None);
        assert_eq!(o.solarradiation, None);
        // The mapping drops the absent keys instead of inventing zeros.
        let fields = o.fields();
        assert_eq!(fields.len(), 2);
        assert!(!fields.iter().any(|(f, _)| *f == WeatherField::RainTodayIn));

        // Empty response array: fetch_latest's `.into_iter().next()` view.
        let body: Vec<Observation> = serde_json::from_str("[]").expect("empty array parses");
        assert!(body.into_iter().next().is_none());
    }
}
