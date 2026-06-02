// Davis WeatherLink Live (WLL) LAN source.
//
// The WLL is Davis's modern LAN gateway for Vantage Pro 2 / Vantage Vue
// / EnviroMonitor stations. It exposes a public LAN-only HTTP endpoint
// with no auth: GET http://{host}/v1/current_conditions returns a JSON
// blob with the most recent ISS + barometer + indoor readings.
//
// Response shape (abridged, real-world):
//   {
//     "data": {
//       "did": "001D0A...",
//       "ts": 1715000000,
//       "conditions": [
//         { "data_structure_type": 1, "txid": 1,
//           "temp": 75.0, "hum": 60.0, "dew_point": 60.0,
//           "wind_speed_last": 5.0, "wind_dir_last": 180,
//           "wind_speed_hi_last_10_min": 10.0,
//           "rain_rate_last_in": 0.0, "rainfall_daily_in": 0.0,
//           "uv_index": 5.0, "solar_rad": 800 },
//         { "data_structure_type": 3, "bar_sea_level": 30.0, ... },  // barometer
//         { "data_structure_type": 4, "temp_in": 72.0, "hum_in": 45.0 }  // indoor
//       ]
//     }
//   }
//
// We poll every 10s — well within the WLL's documented 10s sampling
// cadence. Fast enough for irrigation decisions, slow enough that one
// LocalSky tick doesn't crowd out wakeups from other adapters.
//
// data_structure_type values: 1 = ISS, 2 = leaf/soil sensors, 3 =
// barometer, 4 = indoor temp/hum. We read 1 + 3 + 4 (skip 2 until
// LocalSky supports soil/leaf wetness via WLL).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::DavisWllConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const POLL_INTERVAL: Duration = Duration::from_secs(10);

pub struct DavisWll {
    id: String,
    config: DavisWllConfig,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct CurrentConditionsResponse {
    data: ConditionsData,
}

#[derive(Debug, Deserialize)]
struct ConditionsData {
    conditions: Vec<Condition>,
}

#[derive(Debug, Deserialize)]
struct Condition {
    data_structure_type: u32,
    #[serde(default)]
    txid: Option<u32>,
    // ISS (type 1) fields
    #[serde(default)]
    temp: Option<f64>,
    #[serde(default)]
    hum: Option<f64>,
    #[serde(default)]
    dew_point: Option<f64>,
    #[serde(default)]
    wind_speed_last: Option<f64>,
    #[serde(default)]
    wind_dir_last: Option<f64>,
    #[serde(default)]
    wind_speed_hi_last_10_min: Option<f64>,
    #[serde(default)]
    rain_rate_last_in: Option<f64>,
    #[serde(default)]
    rainfall_daily_in: Option<f64>,
    #[serde(default)]
    uv_index: Option<f64>,
    #[serde(default)]
    solar_rad: Option<f64>,
    // Barometer (type 3) fields
    #[serde(default)]
    bar_sea_level: Option<f64>, // inHg already
                                // Indoor (type 4) — currently unused, kept for documentation.
}

impl DavisWll {
    pub fn new(id: impl Into<String>, config: DavisWllConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            config,
            client,
        }
    }

    async fn fetch(&self) -> anyhow::Result<CurrentConditionsResponse> {
        let url = format!("http://{}/v1/current_conditions", self.config.host);
        let resp = self.client.get(&url).send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }
}

fn extract_fields(resp: &CurrentConditionsResponse, txid: u32) -> Vec<(WeatherField, f64)> {
    let mut out = Vec::new();
    for c in &resp.data.conditions {
        match c.data_structure_type {
            1 => {
                // ISS — only emit if txid matches.
                if c.txid != Some(txid) {
                    continue;
                }
                if let Some(v) = c.temp {
                    out.push((WeatherField::AirTempF, v));
                }
                if let Some(v) = c.dew_point {
                    out.push((WeatherField::DewPointF, v));
                }
                if let Some(v) = c.hum {
                    out.push((WeatherField::RhPct, v));
                }
                if let Some(v) = c.wind_speed_last {
                    out.push((WeatherField::WindMph, v));
                }
                if let Some(v) = c.wind_speed_hi_last_10_min {
                    out.push((WeatherField::WindGustMph, v));
                }
                if let Some(v) = c.wind_dir_last {
                    out.push((WeatherField::WindBearingDeg, v));
                }
                if let Some(v) = c.rain_rate_last_in {
                    out.push((WeatherField::RainIntensityInHr, v));
                }
                if let Some(v) = c.rainfall_daily_in {
                    out.push((WeatherField::RainTodayIn, v));
                }
                if let Some(v) = c.uv_index {
                    out.push((WeatherField::UvIndex, v));
                }
                if let Some(v) = c.solar_rad {
                    out.push((WeatherField::SolarWm2, v));
                }
            }
            3 => {
                // Barometer (one per WLL; not per-txid).
                if let Some(v) = c.bar_sea_level {
                    out.push((WeatherField::PressureInHg, v));
                }
            }
            _ => {}
        }
    }
    out
}

#[async_trait]
impl WeatherSource for DavisWll {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        fields.insert(WeatherField::AirTempF);
        fields.insert(WeatherField::DewPointF);
        fields.insert(WeatherField::RhPct);
        fields.insert(WeatherField::WindMph);
        fields.insert(WeatherField::WindGustMph);
        fields.insert(WeatherField::WindBearingDeg);
        fields.insert(WeatherField::UvIndex);
        fields.insert(WeatherField::SolarWm2);
        fields.insert(WeatherField::PressureInHg);
        fields.insert(WeatherField::RainTodayIn);
        fields.insert(WeatherField::RainIntensityInHr);
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
        // Direct LAN station, no cloud round-trip. Priority 80 —
        // equal to Tempest UDP / Ecowitt LAN. Beats every cloud
        // source (forecast or cloud-routed station) but ties with
        // other direct-LAN stations; the merge engine breaks ties
        // by source order.
        match field {
            WeatherField::AirTempF
            | WeatherField::DewPointF
            | WeatherField::RhPct
            | WeatherField::WindMph
            | WeatherField::WindGustMph
            | WeatherField::WindBearingDeg
            | WeatherField::UvIndex
            | WeatherField::SolarWm2
            | WeatherField::PressureInHg
            | WeatherField::RainTodayIn
            | WeatherField::RainIntensityInHr => 80,
            _ => i32::MIN,
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source_id = %self.id, host = self.config.host, "DavisWll source started");
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
                            let fields = extract_fields(&resp, self.config.txid);
                            if !fields.is_empty() {
                                debug!(source_id = %self.id, fields_n = fields.len(), "DavisWll updated");
                                let _ = bus.send(SourceEvent::Observation {
                                    source_id: self.id.clone(),
                                    fields,
                                    at_epoch: chrono::Utc::now().timestamp(),
                                });
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "DavisWll fetch failed");
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
                        info!(source_id = %self.id, "DavisWll shutdown");
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
    use serde_json::json;

    fn wll_test() -> DavisWll {
        DavisWll::new(
            "wll",
            DavisWllConfig {
                host: "192.0.2.10".into(),
                txid: 1,
            },
        )
    }

    #[test]
    fn caps_advertise_full_iss_set() {
        let w = wll_test();
        let caps = w.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::SolarWm2));
        assert!(caps.fields.contains(&WeatherField::RainTodayIn));
        assert!(caps.fields.contains(&WeatherField::PressureInHg));
    }

    #[test]
    fn priority_above_cloud() {
        let w = wll_test();
        // Direct LAN must beat any cloud-routed station (70).
        assert!(w.priority(WeatherField::AirTempF) > 70);
    }

    #[test]
    fn extract_fields_iss_plus_barometer() {
        let body: CurrentConditionsResponse = serde_json::from_value(json!({
            "data": {
                "conditions": [
                    {
                        "data_structure_type": 1,
                        "txid": 1,
                        "temp": 75.0,
                        "hum": 60.0,
                        "dew_point": 60.0,
                        "wind_speed_last": 5.0,
                        "wind_dir_last": 180,
                        "wind_speed_hi_last_10_min": 12.0,
                        "rain_rate_last_in": 0.0,
                        "rainfall_daily_in": 0.0,
                        "uv_index": 6.0,
                        "solar_rad": 800
                    },
                    {
                        "data_structure_type": 3,
                        "bar_sea_level": 30.05
                    },
                    {
                        "data_structure_type": 4,
                        "temp_in": 72.0
                    }
                ]
            }
        }))
        .unwrap();
        let f = extract_fields(&body, 1);
        let temp = f
            .iter()
            .find(|(k, _)| *k == WeatherField::AirTempF)
            .unwrap()
            .1;
        let press = f
            .iter()
            .find(|(k, _)| *k == WeatherField::PressureInHg)
            .unwrap()
            .1;
        let solar = f
            .iter()
            .find(|(k, _)| *k == WeatherField::SolarWm2)
            .unwrap()
            .1;
        assert_eq!(temp, 75.0);
        assert!((press - 30.05).abs() < 0.001);
        assert_eq!(solar, 800.0);
        // Confirm the indoor block (type 4) didn't contribute.
        assert!(f
            .iter()
            .all(|(k, _)| *k != WeatherField::AirTempF || *k == WeatherField::AirTempF));
        // Confirm only one AirTempF (from ISS, not type 4).
        let temp_count = f
            .iter()
            .filter(|(k, _)| *k == WeatherField::AirTempF)
            .count();
        assert_eq!(temp_count, 1);
    }

    #[test]
    fn skips_iss_with_wrong_txid() {
        let body: CurrentConditionsResponse = serde_json::from_value(json!({
            "data": {
                "conditions": [
                    {
                        "data_structure_type": 1,
                        "txid": 2,
                        "temp": 99.0
                    }
                ]
            }
        }))
        .unwrap();
        let f = extract_fields(&body, 1);
        assert!(
            f.is_empty(),
            "ISS with txid 2 must be skipped when configured for txid 1"
        );
    }
}
