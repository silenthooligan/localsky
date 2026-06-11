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

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::AmbientWeatherConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.ambientweather.net/v1";
const POLL_INTERVAL: Duration = Duration::from_secs(60);

pub struct AmbientWeather {
    id: String,
    config: AmbientWeatherConfig,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct Observation {
    tempf: Option<f64>,
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

impl AmbientWeather {
    pub fn new(id: impl Into<String>, config: AmbientWeatherConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            config,
            client,
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

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "AmbientWeather source started");
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match self.fetch_latest().await {
                        Ok(Some(o)) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            let mut fields = Vec::new();
                            if let Some(v) = o.tempf { fields.push((WeatherField::AirTempF, v)); }
                            if let Some(v) = o.dewpoint { fields.push((WeatherField::DewPointF, v)); }
                            if let Some(v) = o.humidity { fields.push((WeatherField::RhPct, v)); }
                            if let Some(v) = o.baromrelin { fields.push((WeatherField::PressureInHg, v)); }
                            if let Some(v) = o.windspeedmph { fields.push((WeatherField::WindMph, v)); }
                            if let Some(v) = o.windgustmph { fields.push((WeatherField::WindGustMph, v)); }
                            if let Some(v) = o.winddir { fields.push((WeatherField::WindBearingDeg, v)); }
                            if let Some(v) = o.uv { fields.push((WeatherField::UvIndex, v)); }
                            if let Some(v) = o.solarradiation { fields.push((WeatherField::SolarWm2, v)); }
                            if let Some(v) = o.dailyrainin { fields.push((WeatherField::RainTodayIn, v)); }
                            if let Some(v) = o.hourlyrainin { fields.push((WeatherField::RainIntensityInHr, v)); }
                            if !fields.is_empty() {
                                debug!(source_id = %self.id, fields_n = fields.len(), "AmbientWeather updated");
                                let _ = bus.send(SourceEvent::Observation {
                                    source_id: self.id.clone(),
                                    fields,
                                    at_epoch: chrono::Utc::now().timestamp(),
                                });
                            }
                        }
                        Ok(None) => {
                            // Successful API call, no observations yet, station
                            // is online but new (or device MAC is wrong).
                            warn!(source_id = %self.id, "AmbientWeather returned 0 observations; check mac_address");
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "AmbientWeather fetch failed");
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
                        info!(source_id = %self.id, "AmbientWeather shutdown");
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
}
