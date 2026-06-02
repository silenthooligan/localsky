// OpenWeatherMap weather source — api.openweathermap.org "One Call API 3.0".
//
// Requires a paid API key (free tier covers 1000 calls/day = poll
// every ~90 seconds). Global coverage. Standard pick for users without
// a LAN station or a free regional service.
//
// Endpoint:
//   GET /data/3.0/onecall?lat={lat}&lon={lon}&appid={key}&units=imperial
//
// One Call returns current + minutely (1h) + hourly (48h) + daily (8d)
// in a single response. We emit live observation fields from `current`
// and reachability on success/failure.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, OpenWeatherConfig};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.openweathermap.org/data/3.0";
const POLL_INTERVAL: Duration = Duration::from_secs(10 * 60); // 10 min (free-tier safe)

pub struct OpenWeather {
    id: String,
    config: OpenWeatherConfig,
    location: Location,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct OneCallResponse {
    current: Option<CurrentBlock>,
}

#[derive(Debug, Deserialize)]
struct CurrentBlock {
    temp: Option<f64>,
    feels_like: Option<f64>,
    pressure: Option<f64>, // hPa
    humidity: Option<f64>,
    dew_point: Option<f64>,
    uvi: Option<f64>,
    wind_speed: Option<f64>, // mph (imperial)
    wind_gust: Option<f64>,
    wind_deg: Option<f64>,
}

impl OpenWeather {
    pub fn new(id: impl Into<String>, config: OpenWeatherConfig, location: Location) -> Self {
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

    async fn fetch(&self) -> anyhow::Result<OneCallResponse> {
        let url = format!(
            "{API_BASE}/onecall?lat={lat}&lon={lon}&appid={key}&units=imperial",
            lat = self.location.lat,
            lon = self.location.lon,
            key = self.config.api_key,
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
impl WeatherSource for OpenWeather {
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
        fields.insert(WeatherField::ForecastDaily);
        fields.insert(WeatherField::ForecastHourly);
        SourceCaps {
            live_current: false,
            hourly_forecast_hours: 48,
            daily_forecast_days: 8,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            // Forecast: solid commercial source.
            WeatherField::ForecastDaily | WeatherField::ForecastHourly => 50,
            // Live values: model-derived, low vs any LAN station.
            WeatherField::AirTempF
            | WeatherField::DewPointF
            | WeatherField::RhPct
            | WeatherField::PressureInHg
            | WeatherField::WindMph
            | WeatherField::WindGustMph
            | WeatherField::WindBearingDeg
            | WeatherField::UvIndex => 25,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "OpenWeather source started");
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
                            if let Some(c) = resp.current {
                                let mut fields = Vec::new();
                                if let Some(v) = c.temp { fields.push((WeatherField::AirTempF, v)); }
                                if let Some(v) = c.dew_point { fields.push((WeatherField::DewPointF, v)); }
                                if let Some(v) = c.humidity { fields.push((WeatherField::RhPct, v)); }
                                // OWM returns pressure in hPa even on imperial units.
                                if let Some(v) = c.pressure { fields.push((WeatherField::PressureInHg, v * 0.02953)); }
                                if let Some(v) = c.wind_speed { fields.push((WeatherField::WindMph, v)); }
                                if let Some(v) = c.wind_gust { fields.push((WeatherField::WindGustMph, v)); }
                                if let Some(v) = c.wind_deg { fields.push((WeatherField::WindBearingDeg, v)); }
                                if let Some(v) = c.uvi { fields.push((WeatherField::UvIndex, v)); }
                                if !fields.is_empty() {
                                    debug!(source_id = %self.id, fields_n = fields.len(), "OpenWeather updated");
                                    let _ = bus.send(SourceEvent::Observation {
                                        source_id: self.id.clone(),
                                        fields,
                                        at_epoch: chrono::Utc::now().timestamp(),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "OpenWeather fetch failed");
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
                        info!(source_id = %self.id, "OpenWeather shutdown");
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

    fn ow_test() -> OpenWeather {
        OpenWeather::new(
            "ow",
            OpenWeatherConfig {
                api_key: "test".into(),
            },
            Location {
                lat: 30.0738,
                lon: -81.4716,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_include_forecast_and_uv() {
        let o = ow_test();
        let caps = o.capabilities();
        assert!(caps.fields.contains(&WeatherField::ForecastDaily));
        assert!(caps.fields.contains(&WeatherField::UvIndex));
        assert_eq!(caps.hourly_forecast_hours, 48);
    }
}
