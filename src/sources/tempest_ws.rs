// Tempest cloud source, swd.weatherflow.com (WeatherFlow Tempest REST).
//
// This adapter covers the use case where LocalSky can't reach the
// Tempest hub on the same LAN (the Tempest hub is local; many users
// run LocalSky on a VPS or away from the hub). It polls WeatherFlow's
// per-station REST endpoint and emits observations on the same bus
// as the LAN UDP path.
//
// The schema kind is named `tempest_ws` because the protocol future is
// the WebSocket stream at wss://ws.weatherflow.com/swd/data. REST polling
// is what every real-world integration uses today (HA, Weatherbit, the
// official WeatherFlow website) because the WS streams the same data
// at 1Hz, which is way oversampled for irrigation use; REST at 1-min
// cadence is identical for our purposes and skips the persistent-WS
// reconnection complexity.
//
// Endpoint:
//   GET https://swd.weatherflow.com/swd/rest/observations/station/{station_id}?token={access_token}
//
// Response is in METRIC units (°C, m/s, hPa, mm). We convert at the
// boundary so the rest of LocalSky stays in its canonical units
// (°F, mph, inHg, in).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::TempestWsConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const API_BASE: &str = "https://swd.weatherflow.com/swd/rest";
const POLL_INTERVAL: Duration = Duration::from_secs(60);

pub struct TempestWs {
    id: String,
    config: TempestWsConfig,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct ObservationsResponse {
    #[serde(default)]
    obs: Vec<Observation>,
}

#[derive(Debug, Deserialize)]
struct Observation {
    #[serde(default)]
    air_temperature: Option<f64>, // °C
    #[serde(default)]
    relative_humidity: Option<f64>,
    #[serde(default)]
    dew_point: Option<f64>, // °C
    #[serde(default)]
    sea_level_pressure: Option<f64>, // hPa
    #[serde(default)]
    barometric_pressure: Option<f64>, // hPa (fallback)
    #[serde(default)]
    wind_avg: Option<f64>, // m/s
    #[serde(default)]
    wind_gust: Option<f64>, // m/s
    #[serde(default)]
    wind_direction: Option<f64>,
    #[serde(default)]
    uv: Option<f64>,
    #[serde(default)]
    solar_radiation: Option<f64>, // W/m²
    #[serde(default)]
    brightness: Option<f64>, // lux
    #[serde(default)]
    precip_accum_local_day: Option<f64>, // mm
    #[serde(default)]
    precip_accum_last_1hr: Option<f64>, // mm
    #[serde(default)]
    lightning_strike_count: Option<f64>,
    #[serde(default)]
    lightning_strike_last_distance: Option<f64>, // km
}

impl TempestWs {
    pub fn new(id: impl Into<String>, config: TempestWsConfig) -> Self {
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

    async fn fetch(&self) -> anyhow::Result<ObservationsResponse> {
        let url = format!(
            "{API_BASE}/observations/station/{station}?token={token}",
            station = self.config.station_id,
            token = self.config.access_token,
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

fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}
fn mps_to_mph(v: f64) -> f64 {
    v * 2.236936
}
fn hpa_to_inhg(p: f64) -> f64 {
    p * 0.02953
}
fn mm_to_in(mm: f64) -> f64 {
    mm * 0.03937
}
fn km_to_mi(km: f64) -> f64 {
    km * 0.621371
}

fn extract_fields(o: &Observation) -> Vec<(WeatherField, f64)> {
    let mut out = Vec::new();
    if let Some(v) = o.air_temperature {
        out.push((WeatherField::AirTempF, c_to_f(v)));
    }
    if let Some(v) = o.dew_point {
        out.push((WeatherField::DewPointF, c_to_f(v)));
    }
    if let Some(v) = o.relative_humidity {
        out.push((WeatherField::RhPct, v));
    }
    if let Some(v) = o.sea_level_pressure.or(o.barometric_pressure) {
        out.push((WeatherField::PressureInHg, hpa_to_inhg(v)));
    }
    if let Some(v) = o.wind_avg {
        out.push((WeatherField::WindMph, mps_to_mph(v)));
    }
    if let Some(v) = o.wind_gust {
        out.push((WeatherField::WindGustMph, mps_to_mph(v)));
    }
    if let Some(v) = o.wind_direction {
        out.push((WeatherField::WindBearingDeg, v));
    }
    if let Some(v) = o.uv {
        out.push((WeatherField::UvIndex, v));
    }
    if let Some(v) = o.solar_radiation {
        out.push((WeatherField::SolarWm2, v));
    }
    if let Some(v) = o.brightness {
        out.push((WeatherField::Illuminance, v));
    }
    if let Some(v) = o.precip_accum_local_day {
        out.push((WeatherField::RainTodayIn, mm_to_in(v)));
    }
    if let Some(v) = o.precip_accum_last_1hr {
        out.push((WeatherField::RainIntensityInHr, mm_to_in(v)));
    }
    if let Some(v) = o.lightning_strike_count {
        out.push((WeatherField::LightningCount, v));
    }
    if let Some(v) = o.lightning_strike_last_distance {
        out.push((WeatherField::LightningDistanceMi, km_to_mi(v)));
    }
    out
}

#[async_trait]
impl WeatherSource for TempestWs {
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
        fields.insert(WeatherField::Illuminance);
        fields.insert(WeatherField::RainTodayIn);
        fields.insert(WeatherField::RainIntensityInHr);
        fields.insert(WeatherField::LightningCount);
        fields.insert(WeatherField::LightningDistanceMi);
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
        // Cloud-routed Tempest: between forecast and direct UDP. Same
        // priority bucket as AmbientWeather (70).
        match field {
            WeatherField::AirTempF
            | WeatherField::DewPointF
            | WeatherField::RhPct
            | WeatherField::PressureInHg
            | WeatherField::WindMph
            | WeatherField::WindGustMph
            | WeatherField::WindBearingDeg
            | WeatherField::UvIndex
            | WeatherField::SolarWm2
            | WeatherField::Illuminance
            | WeatherField::RainTodayIn
            | WeatherField::RainIntensityInHr
            | WeatherField::LightningCount
            | WeatherField::LightningDistanceMi => 70,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, station = self.config.station_id, "TempestWs source started");
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
                            if let Some(latest) = resp.obs.first() {
                                let fields = extract_fields(latest);
                                if !fields.is_empty() {
                                    debug!(source_id = %self.id, fields_n = fields.len(), "TempestWs updated");
                                    let _ = bus.send(SourceEvent::Observation {
                                        source_id: self.id.clone(),
                                        fields,
                                        at_epoch: chrono::Utc::now().timestamp(),
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "TempestWs fetch failed");
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
                        info!(source_id = %self.id, "TempestWs shutdown");
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

    fn tw_test() -> TempestWs {
        TempestWs::new(
            "tw",
            TempestWsConfig {
                access_token: "tok".into(),
                station_id: 12345,
            },
        )
    }

    #[test]
    fn caps_include_lightning_and_rain() {
        let t = tw_test();
        let caps = t.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::LightningCount));
        assert!(caps.fields.contains(&WeatherField::RainTodayIn));
        assert!(caps.fields.contains(&WeatherField::Illuminance));
    }

    #[test]
    fn metric_conversions_match_expectations() {
        let o = Observation {
            air_temperature: Some(25.0), // 25C = 77F
            relative_humidity: Some(50.0),
            dew_point: Some(10.0),             // 10C = 50F
            sea_level_pressure: Some(1013.25), // 1013.25 hPa = 29.92 inHg
            barometric_pressure: None,
            wind_avg: Some(10.0),  // 10 m/s = 22.37 mph
            wind_gust: Some(15.0), // 15 m/s = 33.55 mph
            wind_direction: Some(270.0),
            uv: Some(5.0),
            solar_radiation: Some(800.0),
            brightness: Some(90000.0),
            precip_accum_local_day: Some(25.4), // 25.4mm = 1.0in
            precip_accum_last_1hr: Some(2.54),  // 2.54mm = 0.1in
            lightning_strike_count: Some(3.0),
            lightning_strike_last_distance: Some(8.0), // 8km = 4.97mi
        };
        let f = extract_fields(&o);
        let temp = f
            .iter()
            .find(|(k, _)| *k == WeatherField::AirTempF)
            .unwrap()
            .1;
        let wind = f
            .iter()
            .find(|(k, _)| *k == WeatherField::WindMph)
            .unwrap()
            .1;
        let press = f
            .iter()
            .find(|(k, _)| *k == WeatherField::PressureInHg)
            .unwrap()
            .1;
        let day = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainTodayIn)
            .unwrap()
            .1;
        let dist = f
            .iter()
            .find(|(k, _)| *k == WeatherField::LightningDistanceMi)
            .unwrap()
            .1;
        assert!((temp - 77.0).abs() < 0.001);
        assert!((wind - 22.37).abs() < 0.05);
        assert!((press - 29.92).abs() < 0.01);
        assert!((day - 1.0).abs() < 0.05);
        assert!((dist - 4.97).abs() < 0.05);
    }

    #[test]
    fn falls_back_to_barometric_when_sea_level_missing() {
        let o = Observation {
            air_temperature: None,
            relative_humidity: None,
            dew_point: None,
            sea_level_pressure: None,
            barometric_pressure: Some(1000.0),
            wind_avg: None,
            wind_gust: None,
            wind_direction: None,
            uv: None,
            solar_radiation: None,
            brightness: None,
            precip_accum_local_day: None,
            precip_accum_last_1hr: None,
            lightning_strike_count: None,
            lightning_strike_last_distance: None,
        };
        let f = extract_fields(&o);
        let press = f
            .iter()
            .find(|(k, _)| *k == WeatherField::PressureInHg)
            .unwrap()
            .1;
        assert!((press - 29.53).abs() < 0.05);
    }
}
