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
//           "rain_rate_last": 0, "rainfall_daily": 0, "rain_size": 1,
//           "uv_index": 5.0, "solar_rad": 800 },
//         { "data_structure_type": 3, "bar_sea_level": 30.0, ... },  // barometer
//         { "data_structure_type": 4, "temp_in": 72.0, "hum_in": 45.0 }  // indoor
//       ]
//     }
//   }
//
// We poll every 10s, well within the WLL's documented 10s sampling
// cadence. Fast enough for irrigation decisions, slow enough that one
// LocalSky tick doesn't crowd out wakeups from other adapters. The loop
// itself is the shared one (sources::poll::run_polling): it owns the tick,
// the fetch metric, the shutdown, and the reachability edges; this file
// only builds the client and turns one response into one Poll.
//
// data_structure_type values: 1 = ISS, 2 = leaf/soil sensors, 3 =
// barometer, 4 = indoor temp/hum. We read 1 (ISS, txid-filtered), 2 (leaf
// wetness -> global LeafWetness), and 3 (barometer). Type 4 (indoor) is
// ignored. Leaf wetness is a bounded 0-15 index and scales to a percent
// cleanly. Davis `moist_soil_N` is soil water TENSION in centibars, which
// is a different physical quantity from the volumetric water content the
// per-zone soil channel carries, so it is NOT published at all: see the
// tension note above `extract`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use tracing::{debug, info};

use crate::config::schema::DavisWllConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::poll::{run_polling, Poll};

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
    // Rain is reported in TIP COUNTS, not inches: `rain_rate_last` is counts
    // per hour and `rainfall_daily` is counts since local midnight. The tip
    // size is carried in `rain_size` (1 = 0.01 in, 2 = 0.2 mm, 3 = 0.1 mm,
    // 4 = 0.001 in), so counts must be multiplied by the per-tip inches before
    // emitting (see `davis_rain_size_in` + the mapping in `extract`). The old
    // `rain_rate_last_in` / `rainfall_daily_in` keys do NOT exist in the WLL
    // local API, so they always deserialized to None and Davis rain never fed
    // the engine.
    #[serde(default)]
    rain_rate_last: Option<f64>,
    #[serde(default)]
    rainfall_daily: Option<f64>,
    #[serde(default)]
    rain_size: Option<u8>,
    #[serde(default)]
    uv_index: Option<f64>,
    #[serde(default)]
    solar_rad: Option<f64>,
    // Barometer (type 3) fields
    #[serde(default)]
    bar_sea_level: Option<f64>, // inHg already
    // Soil/leaf (type 2) fields. Field names match the WeatherLink Live local
    // API (weatherlink.github.io/weatherlink-live-local-api, data_structure_type
    // 2). `moist_soil_N` is CENTIBARS of tension (low = wet), which is not a
    // moisture percent and is deliberately not published -- see the tension
    // note above `extract`. It is still deserialized so the poll can say which
    // channel it is withholding. Leaf wetness is a bounded 0-15 index
    // (davisinstruments 6420) and does scale to a percent. Soil/leaf stations
    // have their OWN txid (separate from the ISS), so type-2 records are NOT
    // filtered by the configured ISS txid.
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

// WHY DAVIS SOIL IS NOT PUBLISHED.
//
// `moist_soil_N` from a WLL soil station (the Davis 6440 / Watermark granular
// matrix head) is soil water TENSION in centibars: the suction a root has to
// pull against. The per-zone soil channel on the bus is a MOISTURE percent,
// which the saturation and dry-floor gates read as volumetric water content.
// Those are not the same quantity, and one is not a linear rescale of the
// other: tension maps to water content through the soil water retention
// curve, which is steeply non-linear and specific to the soil's texture and
// structure. No constant here can turn one into the other.
//
// This adapter used to ship `100 * (1 - cb / 200)`, clamped to 0-100. That map
// runs the right DIRECTION (wet = high), which is what made it survive review,
// but its scale is fabricated, and the shipped default band reads the result
// exactly backwards. `saturation_pct_soil` defaults to 70 and skips at or
// above it, so every tension at or below 60 cb published >= 70% and skipped
// the zone: 30 cb -> 85%, 50 cb -> 75%, 60 cb -> 70%. For most soils 30-60 cb
// is the standard BEGIN IRRIGATING band, so the whole "this zone is dry, water
// it" range was reported as saturated and the zone was held dry, with a
// confident number on the card explaining why.
//
// A missing reading is reported as missing. With no soil channel the gates see
// `None`, every ZoneSoilPct comparison is Unknown and can never fire a rule
// (`engine::conditions`), and the zone falls back to the modeled soil bucket,
// which is the documented behavior for a zone with no probe. That is a
// well-founded fallback; a number pointing the wrong way is not.
//
// Publishing tension honestly needs a channel that is typed as tension, plus
// gates that know high = dry. That work lands outside this file; until then
// this adapter stays silent rather than guessing.

/// Davis leaf wetness is a 0-15 index; scale to 0-100%.
fn leaf_index_to_pct(idx: f64) -> f64 {
    (idx * 100.0 / 15.0).clamp(0.0, 100.0)
}

/// Inches of rain per tip for a Davis `rain_size` code (WeatherLink Live local
/// API): 1 = 0.01 in, 2 = 0.2 mm, 3 = 0.1 mm, 4 = 0.001 in. `rain_rate_last`
/// (counts/hr) and `rainfall_daily` (counts since midnight) are multiplied by
/// this to reach in/hr and inches. Unknown codes fall back to 0.01 in (US).
fn davis_rain_size_in(rain_size: u8) -> f64 {
    match rain_size {
        1 => 0.01,
        2 => crate::units::mm_to_in(0.2),
        3 => crate::units::mm_to_in(0.1),
        4 => 0.001,
        _ => 0.01,
    }
}

/// What one poll yields: global weather fields (Observation) plus per-zone soil
/// channels (KeyedReading). Kept separate because soil is zone-qualified and
/// rides the bus as a KeyedReading, not a global WeatherField.
///
/// `soil` is currently always EMPTY: a WLL's only soil channel is tension in
/// centibars, which this adapter does not publish (see the tension note above
/// `extract`). The field and its plumbing stay so a correctly-typed soil
/// reading has somewhere to land without reshaping the poll.
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
        Ok(crate::net::safe_fetch::read_json_capped(resp).await?)
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
                // Scale tip COUNTS to inches by the reported tip size. Default
                // to 0.01 in (the US Davis default) only when rain_size is
                // absent, which a rain-capable ISS never omits.
                let tip_in = davis_rain_size_in(c.rain_size.unwrap_or(1));
                if let Some(v) = c.rain_rate_last {
                    out.fields
                        .push((WeatherField::RainIntensityInHr, v * tip_in));
                }
                if let Some(v) = c.rainfall_daily {
                    out.fields.push((WeatherField::RainTodayIn, v * tip_in));
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
                // station is a separate transmitter. Leaf wetness is global
                // (0-15 index -> %). Soil tension is NOT published in any
                // form -- see the tension note above this function. We still
                // walk the mapped channels so the log names the zone whose
                // reading is being withheld, instead of the channel just going
                // quiet with no explanation.
                for (ch, cb) in [
                    (1u32, c.moist_soil_1),
                    (2, c.moist_soil_2),
                    (3, c.moist_soil_3),
                    (4, c.moist_soil_4),
                ] {
                    if let (Some(cb), Some(zone)) = (cb, soil_zone_map.get(&ch)) {
                        debug!(
                            channel = ch,
                            zone = %zone,
                            tension_cb = cb,
                            "Davis soil channel withheld: tension in centibars is \
                             not a moisture percent and does not convert to one; \
                             the zone uses its modeled soil bucket instead"
                        );
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
        // A WLL may carry a soil/leaf station; advertise leaf wetness only.
        // Soil would be a per-zone KeyedReading rather than a global field, so
        // it would not be listed here either way -- and this adapter publishes
        // no soil at all (see the tension note above `extract`).
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

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        // The LAN host is the first thing an operator needs when this source
        // goes quiet; the shared loop logs the start, this line names the
        // target it will poll.
        info!(source_id = %self.id, host = %self.config.host, "DavisWll polling LAN host");
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "DavisWll",
            POLL_INTERVAL,
            bus,
            shutdown,
            |s: Arc<Self>| async move {
                let resp = s.fetch().await?;
                let ex = extract(&resp, s.config.txid, &s.config.soil_zone_map);
                let at_epoch = chrono::Utc::now().timestamp();
                if !ex.fields.is_empty() {
                    debug!(
                        source_id = %s.id,
                        fields_n = ex.fields.len(),
                        soil_n = ex.soil.len(),
                        "DavisWll conditions parsed"
                    );
                }
                // Global fields ride as one Observation (none when empty);
                // each mapped soil channel is its own zone-keyed reading,
                // published after it in the same poll.
                let mut poll = Poll::observation(&s.id, ex.fields, at_epoch);
                for (key, value) in ex.soil {
                    poll = poll.with(SourceEvent::KeyedReading {
                        source_id: s.id.clone(),
                        key,
                        value,
                        at_epoch,
                    });
                }
                anyhow::Ok(poll)
            },
        )
        .await
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
                        "rain_rate_last": 5.0,
                        "rainfall_daily": 63.0,
                        "rain_size": 2,
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
        // Rain is TIP COUNTS scaled by rain_size (2 = 0.2 mm/tip). Daily 63
        // counts -> 63 * 0.2/25.4 = 0.496 in; rate 5 counts/hr -> 0.0394 in/hr.
        // (Proves the real API keys deserialize and the count->inch conversion
        // runs; the old `_in` keys left rain permanently None.)
        let daily = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainTodayIn)
            .expect("RainTodayIn emitted")
            .1;
        let rate = f
            .iter()
            .find(|(k, _)| *k == WeatherField::RainIntensityInHr)
            .expect("RainIntensityInHr emitted")
            .1;
        assert!((daily - 0.496).abs() < 1e-3, "daily inches, got {daily}");
        assert!((rate - 0.0394).abs() < 1e-3, "in/hr rate, got {rate}");
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
    fn type2_soil_tension_is_not_published_as_a_moisture_percent() {
        // Soil/leaf station on its OWN txid (3), so it must NOT be filtered by
        // the configured ISS txid (1). Both channels are bound to zones, so
        // under the old `100 * (1 - cb / 200)` map this emitted two readings
        // (0 cb -> 100%, 100 cb -> 50%). Centibars of tension are not a
        // moisture percent, so the correct output is NO soil reading at all,
        // and the zones fall back to the modeled soil bucket.
        let body: CurrentConditionsResponse = serde_json::from_value(json!({
            "data": { "conditions": [ {
                "data_structure_type": 2, "txid": 3,
                "moist_soil_1": 0.0,
                "moist_soil_2": 100.0,
                "moist_soil_3": 20.0,
                "wet_leaf_1": 15.0      // full index -> 100%
            } ] }
        }))
        .unwrap();
        let mut map = std::collections::BTreeMap::new();
        map.insert(1u32, "back_yard".to_string());
        map.insert(2u32, "front_yard".to_string());
        let ex = extract(&body, 1, &map);
        assert!(
            ex.soil.is_empty(),
            "tension in centibars must publish no soil percent, got {:?}",
            ex.soil
        );
        // The rest of the type-2 record is unaffected: leaf wetness is a
        // bounded 0-15 index and still scales, 15/15 -> 100%.
        let leaf = ex
            .fields
            .iter()
            .find(|(k, _)| *k == WeatherField::LeafWetness)
            .unwrap();
        assert!((leaf.1 - 100.0).abs() < 0.001);
    }

    #[test]
    fn dry_soil_tension_never_reads_as_saturated() {
        // The regression this file existed to create. `saturation_pct_soil`
        // ships at DEFAULT_SATURATION_PCT (70.0) and skips the zone at or
        // above it. The old map published `100 * (1 - cb / 200)`, so every
        // tension at or below 60 cb cleared that bar: 30 cb -> 85%,
        // 50 cb -> 75%, 60 cb -> 70.0%. For most soils 30-60 cb is the band
        // where irrigation should START, so a dry zone was reported saturated
        // and held dry. Each of these would have emitted a >= 70 reading
        // before; now each emits nothing.
        let sat = crate::config::schema::DEFAULT_SATURATION_PCT;
        for cb in [30.0_f64, 50.0, 60.0] {
            let body: CurrentConditionsResponse = serde_json::from_value(json!({
                "data": { "conditions": [ {
                    "data_structure_type": 2, "txid": 3, "moist_soil_1": cb
                } ] }
            }))
            .unwrap();
            let mut map = std::collections::BTreeMap::new();
            map.insert(1u32, "back_yard".to_string());
            let ex = extract(&body, 1, &map);
            // The old value, recomputed here so the assertion names what it
            // is guarding against rather than a bare magic number.
            let fabricated = 100.0 * (1.0 - cb / 200.0);
            assert!(
                fabricated >= sat,
                "{cb} cb used to publish {fabricated}%, at or above the \
                 {sat}% saturation skip; if that stops holding, this test drifted"
            );
            assert!(
                ex.soil.is_empty(),
                "{cb} cb (dry, water it) must publish no soil reading, not \
                 {fabricated}%, which the default band reads as saturated"
            );
        }
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
        // Unbound channels emitted nothing even under the old conversion, and
        // now no channel emits soil at all -- bound or not. This still pins
        // the unbound path so a future correctly-typed soil reading cannot
        // start leaking out of a channel with no zone behind it.
        assert!(
            ex.soil.is_empty(),
            "an unmapped soil channel must never be emitted"
        );
    }
}
