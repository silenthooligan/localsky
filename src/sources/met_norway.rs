// MetNorway (met.no) weather source — api.met.no/weatherapi/locationforecast.
//
// Free, no API key. Global coverage. Requires a descriptive User-Agent
// per met.no terms of service. The compact endpoint returns ~9 days of
// hourly forecast in a single response — generous for free data.
//
// Endpoint:
//   GET /weatherapi/locationforecast/2.0/compact?lat={lat}&lon={lon}
//
// The compact response gives per-timestep `air_temperature`,
// `air_pressure_at_sea_level`, `relative_humidity`, `wind_speed`,
// `precipitation_amount` (in mm), and a `symbol_code` for the
// next_1_hours / next_6_hours / next_12_hours summary. No native
// probability-of-precipitation field; it has to be derived from the
// symbol code.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, MetNorwayConfig};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.met.no/weatherapi/locationforecast/2.0";
const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 min

pub struct MetNorway {
    id: String,
    config: MetNorwayConfig,
    location: Location,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    properties: ForecastProperties,
}

#[derive(Debug, Deserialize)]
struct ForecastProperties {
    timeseries: Vec<TimeStep>,
}

#[derive(Debug, Deserialize)]
struct TimeStep {
    data: TimeStepData,
}

#[derive(Debug, Deserialize)]
struct TimeStepData {
    instant: InstantBlock,
    #[serde(rename = "next_1_hours")]
    next_1_hours: Option<NextBlock>,
}

#[derive(Debug, Deserialize)]
struct InstantBlock {
    details: InstantDetails,
}

#[derive(Debug, Deserialize)]
struct InstantDetails {
    air_temperature: Option<f64>,
    air_pressure_at_sea_level: Option<f64>,
    relative_humidity: Option<f64>,
    wind_speed: Option<f64>,
    wind_from_direction: Option<f64>,
    cloud_area_fraction: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct NextBlock {
    details: NextDetails,
}

#[derive(Debug, Deserialize)]
struct NextDetails {
    precipitation_amount: Option<f64>,
}

impl MetNorway {
    pub fn new(id: impl Into<String>, config: MetNorwayConfig, location: Location) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(config.user_agent.clone())
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            config,
            location,
            client,
        }
    }

    async fn fetch_forecast(&self) -> anyhow::Result<ForecastResponse> {
        let url = format!(
            "{API_BASE}/compact?lat={lat}&lon={lon}",
            lat = self.location.lat,
            lon = self.location.lon
        );
        let resp: ForecastResponse = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }
}

#[async_trait]
impl WeatherSource for MetNorway {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        // MetNorway's compact response is current+forecast in one
        // response. The "instant" block at the first timestep is the
        // closest thing to a current observation.
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindBearingDeg);
        fields.insert(WeatherField::PressureInHg);
        fields.insert(WeatherField::ForecastHourly);
        SourceCaps {
            live_current: false,
            hourly_forecast_hours: 216, // compact = ~9 days
            daily_forecast_days: 9,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            WeatherField::ForecastHourly | WeatherField::ForecastDaily => 55,
            // Numeric live values are present in the first timestep but
            // it's a model forecast, not an actual instrument; very low
            // priority vs any LAN sensor.
            WeatherField::AirTempF
            | WeatherField::RhPct
            | WeatherField::WindMph
            | WeatherField::WindBearingDeg
            | WeatherField::PressureInHg => 20,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "MetNorway source started");
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match self.fetch_forecast().await {
                        Ok(forecast) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            // First timestep = "now" (instant).
                            if let Some(step) = forecast.properties.timeseries.first() {
                                let d = &step.data.instant.details;
                                let mut fields = Vec::new();
                                if let Some(t_c) = d.air_temperature {
                                    fields.push((WeatherField::AirTempF, t_c * 9.0 / 5.0 + 32.0));
                                }
                                if let Some(rh) = d.relative_humidity {
                                    fields.push((WeatherField::RhPct, rh));
                                }
                                if let Some(p_hpa) = d.air_pressure_at_sea_level {
                                    // hPa -> inHg
                                    fields.push((WeatherField::PressureInHg, p_hpa * 0.02953));
                                }
                                if let Some(ws_ms) = d.wind_speed {
                                    // m/s -> mph
                                    fields.push((WeatherField::WindMph, ws_ms * 2.23694));
                                }
                                if let Some(wd) = d.wind_from_direction {
                                    fields.push((WeatherField::WindBearingDeg, wd));
                                }
                                if !fields.is_empty() {
                                    debug!(
                                        source_id = %self.id,
                                        fields_n = fields.len(),
                                        "MetNorway forecast updated"
                                    );
                                    let _ = bus.send(SourceEvent::Observation {
                                        source_id: self.id.clone(),
                                        fields,
                                        at_epoch: chrono::Utc::now().timestamp(),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "MetNorway forecast fetch failed");
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
                        info!(source_id = %self.id, "MetNorway source shutdown");
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

    fn met_test() -> MetNorway {
        MetNorway::new(
            "met",
            MetNorwayConfig {
                user_agent: "LocalSky test (test@example.com)".into(),
            },
            Location {
                lat: 59.9139,
                lon: 10.7522,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_advertise_forecast() {
        let m = met_test();
        let caps = m.capabilities();
        assert!(caps.fields.contains(&WeatherField::ForecastHourly));
        assert!(caps.fields.contains(&WeatherField::PressureInHg));
        assert_eq!(caps.daily_forecast_days, 9);
    }

    #[test]
    fn forecast_higher_priority_than_live() {
        let m = met_test();
        assert!(m.priority(WeatherField::ForecastHourly) > m.priority(WeatherField::AirTempF));
    }
}
