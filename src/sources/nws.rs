// National Weather Service (US) weather source — api.weather.gov.
//
// Free, no API key. US-only coverage. Requires a descriptive
// User-Agent header per the api.weather.gov terms of service.
//
// Two-stage lookup the first time we see a (lat, lon):
//   GET /points/{lat},{lon}                           -> gridId + gridX + gridY
//   GET /gridpoints/{gridId}/{gridX},{gridY}/forecast -> 7-day text forecast
//
// We cache the (gridId, gridX, gridY) since it's stable for a given
// location, then poll the forecast endpoint every 30 min. The free
// daily-forecast call returns probabilityOfPrecipitation per period,
// temperature high/low (Fahrenheit), wind speed (mph), and a long
// text shortForecast.
//
// Emits (WeatherField::Pop, 0..100) for today's first daytime period,
// (WeatherField::AirTempF, today_high), and Reachability flips on
// network errors.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::sync::Mutex;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::{Location, NwsConfig};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://api.weather.gov";
const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 min

pub struct Nws {
    id: String,
    #[allow(dead_code)]
    // user_agent is consumed at construction; kept for parity with other sources
    config: NwsConfig,
    location: Location,
    client: Client,
    /// Cached (gridId, gridX, gridY) from /points/{lat},{lon}.
    grid_cache: Arc<Mutex<Option<GridPoint>>>,
}

#[derive(Debug, Clone)]
struct GridPoint {
    grid_id: String,
    grid_x: u32,
    grid_y: u32,
}

#[derive(Debug, Deserialize)]
struct PointsResponse {
    properties: PointsProperties,
}

#[derive(Debug, Deserialize)]
struct PointsProperties {
    #[serde(rename = "gridId")]
    grid_id: String,
    #[serde(rename = "gridX")]
    grid_x: u32,
    #[serde(rename = "gridY")]
    grid_y: u32,
}

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    properties: ForecastProperties,
}

#[derive(Debug, Deserialize)]
struct ForecastProperties {
    periods: Vec<ForecastPeriod>,
}

#[derive(Debug, Deserialize)]
struct ForecastPeriod {
    #[serde(rename = "isDaytime")]
    is_daytime: bool,
    temperature: Option<f64>,
    #[serde(rename = "probabilityOfPrecipitation")]
    pop: Option<PopObj>,
}

#[derive(Debug, Deserialize)]
struct PopObj {
    value: Option<f64>,
}

impl Nws {
    pub fn new(id: impl Into<String>, config: NwsConfig, location: Location) -> Self {
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
            grid_cache: Arc::new(Mutex::new(None)),
        }
    }

    async fn resolve_grid(&self) -> anyhow::Result<GridPoint> {
        if let Some(cached) = self.grid_cache.lock().await.clone() {
            return Ok(cached);
        }
        let url = format!(
            "{API_BASE}/points/{lat},{lon}",
            lat = self.location.lat,
            lon = self.location.lon
        );
        let resp: PointsResponse = self
            .client
            .get(&url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let grid = GridPoint {
            grid_id: resp.properties.grid_id,
            grid_x: resp.properties.grid_x,
            grid_y: resp.properties.grid_y,
        };
        *self.grid_cache.lock().await = Some(grid.clone());
        Ok(grid)
    }

    async fn fetch_forecast(&self, grid: &GridPoint) -> anyhow::Result<ForecastResponse> {
        let url = format!(
            "{API_BASE}/gridpoints/{}/{},{}/forecast",
            grid.grid_id, grid.grid_x, grid.grid_y
        );
        let resp: ForecastResponse = self
            .client
            .get(&url)
            .header("Accept", "application/geo+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }
}

#[async_trait]
impl WeatherSource for Nws {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::Pop);
        fields.insert(WeatherField::ForecastDaily);
        SourceCaps {
            // NWS forecasts are scheduled-period (12h granularity);
            // not "live current" in the LAN-station sense.
            live_current: false,
            hourly_forecast_hours: 156, // NWS hourly goes ~6.5 days out
            daily_forecast_days: 7,
            radar_tiles: false,
            // NWS doesn't compute reference ET; the engine derives it.
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        match field {
            // Forecast fields: solid US-government truth, higher than
            // Open-Meteo but lower than a LAN station's live readings.
            WeatherField::Pop | WeatherField::ForecastDaily | WeatherField::ForecastHourly => 60,
            // Daily-period AirTempF is forecast, not live; lower priority
            // than any LAN station which is live_current=true.
            WeatherField::AirTempF => 30,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, "NWS source started");
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let grid = match self.resolve_grid().await {
                        Ok(g) => g,
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "NWS grid lookup failed");
                            if last_reachable != Some(false) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: false,
                                });
                                last_reachable = Some(false);
                            }
                            continue;
                        }
                    };
                    match self.fetch_forecast(&grid).await {
                        Ok(forecast) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            // Emit observations from the first daytime period.
                            let day = forecast
                                .properties
                                .periods
                                .iter()
                                .find(|p| p.is_daytime);
                            if let Some(p) = day {
                                let mut fields = Vec::new();
                                if let Some(t) = p.temperature {
                                    fields.push((WeatherField::AirTempF, t));
                                }
                                if let Some(pop) = p.pop.as_ref().and_then(|p| p.value) {
                                    fields.push((WeatherField::Pop, pop));
                                }
                                if !fields.is_empty() {
                                    debug!(
                                        source_id = %self.id,
                                        fields = ?fields,
                                        "NWS forecast updated"
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
                            warn!(source_id = %self.id, error = %e, "NWS forecast fetch failed");
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
                        info!(source_id = %self.id, "NWS source shutdown");
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

    fn nws_test() -> Nws {
        Nws::new(
            "nws",
            NwsConfig {
                user_agent: "LocalSky test (test@example.com)".into(),
            },
            Location {
                lat: 30.0738,
                lon: -81.4716,
                elevation_m: None,
            },
        )
    }

    #[test]
    fn caps_advertise_pop_and_forecast() {
        let n = nws_test();
        let caps = n.capabilities();
        assert!(caps.fields.contains(&WeatherField::Pop));
        assert!(caps.fields.contains(&WeatherField::ForecastDaily));
        assert!(!caps.live_current);
    }

    #[test]
    fn pop_priority_above_air_temp() {
        let n = nws_test();
        assert!(n.priority(WeatherField::Pop) > n.priority(WeatherField::AirTempF));
    }
}
