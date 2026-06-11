// PirateWeather source, api.pirateweather.net, the open Dark-Sky-API
// replacement. Same response shape as the original Dark Sky API, free
// tier 10k/day. Useful for users who built tooling against Dark Sky
// before Apple shut it down.
//
// Endpoint:
//   GET /forecast/{key}/{lat},{lon}?units=us
//
// The `currently` block has live values; `daily` + `hourly` blocks have
// the forecast arrays.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, PirateWeatherConfig};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.pirateweather.net/forecast";
const POLL_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub struct PirateWeather {
    id: String,
    config: PirateWeatherConfig,
    location: Location,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    currently: Option<CurrentBlock>,
}

#[derive(Debug, Deserialize)]
struct CurrentBlock {
    temperature: Option<f64>,
    #[serde(rename = "apparentTemperature")]
    #[allow(dead_code)] // kept to mirror the API shape
    apparent_temperature: Option<f64>,
    #[serde(rename = "dewPoint")]
    dew_point: Option<f64>,
    humidity: Option<f64>, // 0..1 in Dark-Sky-compatible APIs
    pressure: Option<f64>, // hPa
    #[serde(rename = "windSpeed")]
    wind_speed: Option<f64>,
    #[serde(rename = "windGust")]
    wind_gust: Option<f64>,
    #[serde(rename = "windBearing")]
    wind_bearing: Option<f64>,
    #[serde(rename = "uvIndex")]
    uv_index: Option<f64>,
    #[serde(rename = "precipIntensity")]
    precip_intensity: Option<f64>, // in/h with units=us
}

impl PirateWeather {
    pub fn new(id: impl Into<String>, config: PirateWeatherConfig, location: Location) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            config,
            location,
            client,
        }
    }

    async fn fetch(&self) -> anyhow::Result<ForecastResponse> {
        let url = format!(
            "{API_BASE}/{key}/{lat},{lon}?units=us",
            key = self.config.api_key,
            lat = self.location.lat,
            lon = self.location.lon,
        );
        Ok(self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

#[async_trait]
impl WeatherSource for PirateWeather {
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
        fields.insert(WeatherField::RainIntensityInHr);
        fields.insert(WeatherField::ForecastDaily);
        fields.insert(WeatherField::ForecastHourly);
        SourceCaps {
            live_current: false,
            hourly_forecast_hours: 48,
            daily_forecast_days: 7,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            WeatherField::ForecastDaily | WeatherField::ForecastHourly => 50,
            WeatherField::AirTempF
            | WeatherField::DewPointF
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

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "PirateWeather source started");
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match self.fetch().await {
                        Ok(resp) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            if let Some(c) = resp.currently {
                                let mut fields = Vec::new();
                                if let Some(v) = c.temperature { fields.push((WeatherField::AirTempF, v)); }
                                if let Some(v) = c.dew_point { fields.push((WeatherField::DewPointF, v)); }
                                // Dark-Sky-compatible: humidity is 0..1, convert to %.
                                if let Some(v) = c.humidity { fields.push((WeatherField::RhPct, v * 100.0)); }
                                if let Some(v) = c.pressure { fields.push((WeatherField::PressureInHg, v * 0.02953)); }
                                if let Some(v) = c.wind_speed { fields.push((WeatherField::WindMph, v)); }
                                if let Some(v) = c.wind_gust { fields.push((WeatherField::WindGustMph, v)); }
                                if let Some(v) = c.wind_bearing { fields.push((WeatherField::WindBearingDeg, v)); }
                                if let Some(v) = c.uv_index { fields.push((WeatherField::UvIndex, v)); }
                                if let Some(v) = c.precip_intensity { fields.push((WeatherField::RainIntensityInHr, v)); }
                                if !fields.is_empty() {
                                    debug!(source_id = %self.id, fields_n = fields.len(), "PirateWeather updated");
                                    let _ = bus.send(SourceEvent::Observation {
                                        source_id: self.id.clone(),
                                        fields,
                                        at_epoch: chrono::Utc::now().timestamp(),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "PirateWeather fetch failed");
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
                        info!(source_id = %self.id, "PirateWeather shutdown");
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

    fn pw_test() -> PirateWeather {
        PirateWeather::new(
            "pw",
            PirateWeatherConfig {
                api_key: "test".into(),
            },
            Location {
                lat: 30.0,
                lon: -81.0,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_include_rain_intensity() {
        let p = pw_test();
        assert!(p
            .capabilities()
            .fields
            .contains(&WeatherField::RainIntensityInHr));
    }
}
