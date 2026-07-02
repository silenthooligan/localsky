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
// We poll every 10s, well within the WLL's documented 10s sampling
// cadence. Fast enough for irrigation decisions, slow enough that one
// LocalSky tick doesn't crowd out wakeups from other adapters.
//
// data_structure_type values: 1 = ISS, 2 = leaf/soil sensors, 3 =
// barometer, 4 = indoor temp/hum. We read 1 (ISS, txid-filtered), 2
// (soil moisture -> per-zone KeyedReading via soil_zone_map; leaf wetness
// -> global LeafWetness), and 3 (barometer). Type 4 (indoor) is ignored.
// Davis soil is centibars of tension and leaf wetness is a 0-15 index, so
// both are converted to a monotonic percent on the way out (see `extract`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::DavisWllConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Per-request budget for the WLL LAN poll. Matches the previous persistent
/// client's timeout; each fetch now builds an SSRF-hardened client.
const WLL_TIMEOUT: Duration = Duration::from_secs(8);

pub struct DavisWll {
    id: String,
    config: DavisWllConfig,
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
    // Soil/leaf (type 2) fields. Field names match the WeatherLink Live local
    // API (weatherlink.github.io/weatherlink-live-local-api, data_structure_type
    // 2). Davis reports soil moisture in CENTIBARS of tension (low = wet) and
    // leaf wetness on a 0-15 index (davisinstruments 6420), so both are
    // converted on the way out (see `extract`). Soil/leaf stations have their
    // OWN txid (separate from the ISS), so type-2 records are NOT filtered by
    // the configured ISS txid.
    #[serde(default)]
    moist_soil_1: Option<f64>,
    #[serde(default)]
    moist_soil_2: Option<f64>,
    #[serde(default)]
    moist_soil_3: Option<f64>,
    #[serde(default)]
    moist_soil_4: Option<f64>,
    #[serde(default)]
    wet_leaf_1: Option<f64>,
    #[serde(default)]
    wet_leaf_2: Option<f64>,
    // Indoor (type 4), currently unused, kept for documentation.
}

/// Davis soil moisture is reported in centibars of tension (0 = saturated,
/// rising as the soil dries). Convert to a monotonic "percent available" so it
/// reads like every other soil source (wet = high) for the zone soil gate. This
/// is a coarse linear map over the 0-200 cb working range, NOT a soil-texture
/// calibration; the operator sets their zone threshold against it.
fn soil_cb_to_pct(cb: f64) -> f64 {
    (100.0 * (1.0 - cb / 200.0)).clamp(0.0, 100.0)
}

/// Davis leaf wetness is a 0-15 index; scale to 0-100%.
fn leaf_index_to_pct(idx: f64) -> f64 {
    (idx * 100.0 / 15.0).clamp(0.0, 100.0)
}

/// What one poll yields: global weather fields (Observation) plus per-zone soil
/// channels (KeyedReading). Kept separate because soil is zone-qualified and
/// rides the bus as a KeyedReading, not a global WeatherField.
#[derive(Debug, Default, PartialEq)]
struct Extracted {
    fields: Vec<(WeatherField, f64)>,
    soil: Vec<(String, f64)>,
}

impl DavisWll {
    pub fn new(id: impl Into<String>, config: DavisWllConfig) -> Self {
        Self {
            id: id.into(),
            config,
        }
    }

    async fn fetch(&self) -> anyhow::Result<CurrentConditionsResponse> {
        let url = format!("http://{}/v1/current_conditions", self.config.host);
        // SSRF-hardened client built per poll. The WLL host is config-supplied
        // and this poller is always-on, so route outbound through
        // net::safe_fetch (defense in depth): forbidden-target filter,
        // resolved-IP pin (anti DNS-rebinding), no redirects. RFC1918/ULA stays
        // allowed (the WLL lives on the LAN), so legitimate polling is
        // unaffected.
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(&url, WLL_TIMEOUT).await?;
        let resp = client.get(safe_url).send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }
}

fn extract(
    resp: &CurrentConditionsResponse,
    txid: u32,
    soil_zone_map: &std::collections::BTreeMap<u32, String>,
) -> Extracted {
    let mut out = Extracted::default();
    for c in &resp.data.conditions {
        match c.data_structure_type {
            1 => {
                // ISS, only emit if txid matches.
                if c.txid != Some(txid) {
                    continue;
                }
                if let Some(v) = c.temp {
                    out.fields.push((WeatherField::AirTempF, v));
                }
                if let Some(v) = c.dew_point {
                    out.fields.push((WeatherField::DewPointF, v));
                }
                if let Some(v) = c.hum {
                    out.fields.push((WeatherField::RhPct, v));
                }
                if let Some(v) = c.wind_speed_last {
                    out.fields.push((WeatherField::WindMph, v));
                }
                if let Some(v) = c.wind_speed_hi_last_10_min {
                    out.fields.push((WeatherField::WindGustMph, v));
                }
                if let Some(v) = c.wind_dir_last {
                    out.fields.push((WeatherField::WindBearingDeg, v));
                }
                if let Some(v) = c.rain_rate_last_in {
                    out.fields.push((WeatherField::RainIntensityInHr, v));
                }
                if let Some(v) = c.rainfall_daily_in {
                    out.fields.push((WeatherField::RainTodayIn, v));
                }
                if let Some(v) = c.uv_index {
                    out.fields.push((WeatherField::UvIndex, v));
                }
                if let Some(v) = c.solar_rad {
                    out.fields.push((WeatherField::SolarWm2, v));
                }
            }
            2 => {
                // Soil/leaf station. NOT filtered by the ISS txid: the soil/leaf
                // station is a separate transmitter. Soil moisture maps per
                // channel to a zone (centibars -> %); leaf wetness is global
                // (0-15 index -> %). Unmapped soil channels are dropped (zone
                // binding is required for soil).
                for (ch, cb) in [
                    (1u32, c.moist_soil_1),
                    (2, c.moist_soil_2),
                    (3, c.moist_soil_3),
                    (4, c.moist_soil_4),
                ] {
                    if let (Some(cb), Some(zone)) = (cb, soil_zone_map.get(&ch)) {
                        out.soil.push((
                            crate::sources::bus_recorder::zone_soil_key(zone),
                            soil_cb_to_pct(cb),
                        ));
                    }
                }
                // Up to two leaf sensors; take the WETTER of the two present
                // (the conservative reading for disease-pressure monitoring).
                let leaf = match (c.wet_leaf_1, c.wet_leaf_2) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                if let Some(idx) = leaf {
                    out.fields
                        .push((WeatherField::LeafWetness, leaf_index_to_pct(idx)));
                }
            }
            3 => {
                // Barometer (one per WLL; not per-txid).
                if let Some(v) = c.bar_sea_level {
                    out.fields.push((WeatherField::PressureInHg, v));
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
        // A WLL may carry a soil/leaf station; advertise leaf wetness (soil is a
        // per-zone KeyedReading, not a global field, so it isn't listed here).
        fields.insert(WeatherField::LeafWetness);
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
        // Adapter-level priority for the legacy merge layer only; the LIVE
        // current-conditions arbitration uses the config SourceEntry.priority
        // (default_priority_for_kind: a direct LAN station defaults to 100). This
        // 80 just ranks Davis as a direct-LAN station above any cloud source in
        // that dead layer; field-tie order there is by source order.
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
            | WeatherField::RainIntensityInHr
            | WeatherField::LeafWetness => 80,
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
                            let ex = extract(&resp, self.config.txid, &self.config.soil_zone_map);
                            let at_epoch = chrono::Utc::now().timestamp();
                            if !ex.fields.is_empty() {
                                debug!(source_id = %self.id, fields_n = ex.fields.len(), soil_n = ex.soil.len(), "DavisWll updated");
                                let _ = bus.send(SourceEvent::Observation {
                                    source_id: self.id.clone(),
                                    fields: ex.fields,
                                    at_epoch,
                                });
                            }
                            for (key, value) in ex.soil {
                                let _ = bus.send(SourceEvent::KeyedReading {
                                    source_id: self.id.clone(),
                                    key,
                                    value,
                                    at_epoch,
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
                soil_zone_map: Default::default(),
            },
        )
    }

    fn no_soil() -> std::collections::BTreeMap<u32, String> {
        Default::default()
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
        let f = extract(&body, 1, &no_soil()).fields;
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
        let f = extract(&body, 1, &no_soil()).fields;
        assert!(
            f.is_empty(),
            "ISS with txid 2 must be skipped when configured for txid 1"
        );
    }

    #[test]
    fn type2_soil_maps_to_zone_channels_and_converts_centibars() {
        // Soil/leaf station on its OWN txid (3), so it must NOT be filtered by
        // the configured ISS txid (1). Channels 1+2 are mapped to zones; 3 is
        // not, so it must be dropped.
        let body: CurrentConditionsResponse = serde_json::from_value(json!({
            "data": { "conditions": [ {
                "data_structure_type": 2, "txid": 3,
                "moist_soil_1": 0.0,    // saturated -> 100%
                "moist_soil_2": 100.0,  // 100 cb -> 50%
                "moist_soil_3": 20.0,   // unmapped channel -> dropped
                "wet_leaf_1": 15.0      // full index -> 100%
            } ] }
        }))
        .unwrap();
        let mut map = std::collections::BTreeMap::new();
        map.insert(1u32, "back_yard".to_string());
        map.insert(2u32, "front_yard".to_string());
        let ex = extract(&body, 1, &map);
        // Two soil channels (1, 2); channel 3 dropped (unmapped).
        assert_eq!(ex.soil.len(), 2);
        let back = ex
            .soil
            .iter()
            .find(|(k, _)| k.ends_with("back_yard"))
            .unwrap();
        let front = ex
            .soil
            .iter()
            .find(|(k, _)| k.ends_with("front_yard"))
            .unwrap();
        assert!((back.1 - 100.0).abs() < 0.001, "0 cb -> 100% (saturated)");
        assert!((front.1 - 50.0).abs() < 0.001, "100 cb -> 50%");
        // Leaf wetness 15/15 -> 100%, a global LeafWetness field.
        let leaf = ex
            .fields
            .iter()
            .find(|(k, _)| *k == WeatherField::LeafWetness)
            .unwrap();
        assert!((leaf.1 - 100.0).abs() < 0.001);
    }

    #[test]
    fn type2_dual_leaf_takes_the_wetter() {
        // Both leaf sensors present -> the wetter (max) index wins.
        let body: CurrentConditionsResponse = serde_json::from_value(json!({
            "data": { "conditions": [ {
                "data_structure_type": 2, "wet_leaf_1": 3.0, "wet_leaf_2": 12.0
            } ] }
        }))
        .unwrap();
        let leaf = extract(&body, 1, &no_soil())
            .fields
            .into_iter()
            .find(|(k, _)| *k == WeatherField::LeafWetness)
            .unwrap()
            .1;
        // max(3, 12) = 12 -> 12/15 * 100 = 80%.
        assert!((leaf - 80.0).abs() < 0.001, "wetter sensor wins: {leaf}");
    }

    #[test]
    fn type2_with_no_zone_map_emits_no_soil() {
        let body: CurrentConditionsResponse = serde_json::from_value(json!({
            "data": { "conditions": [ {
                "data_structure_type": 2, "moist_soil_1": 50.0
            } ] }
        }))
        .unwrap();
        let ex = extract(&body, 1, &no_soil());
        assert!(
            ex.soil.is_empty(),
            "soil needs a zone binding to be emitted"
        );
    }
}
