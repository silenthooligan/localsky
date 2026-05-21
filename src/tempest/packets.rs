// Tempest UDP packet wire format — every payload arrives as JSON with a
// `type` discriminator. Only the kinds we actually render are modeled;
// the rest are silently ignored by the listener.
//
// Reference: https://weatherflow.github.io/Tempest/api/udp.html

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TempestPacket {
    /// Once-per-minute full observation. The `obs` array is a single
    /// 18-element snapshot — see `ObsSt::from_array` for the field map.
    #[serde(rename = "obs_st")]
    ObsSt {
        serial_number: String,
        hub_sn: String,
        firmware_revision: u32,
        obs: Vec<Vec<serde_json::Value>>,
    },
    /// Every ~3 seconds: instantaneous wind sample.
    /// `ob` is `[time_epoch, wind_speed_mps, wind_direction_deg]`.
    #[serde(rename = "rapid_wind")]
    RapidWind {
        serial_number: String,
        hub_sn: String,
        ob: Vec<serde_json::Value>,
    },
    /// Lightning strike event: `[time_epoch, distance_km, energy]`.
    #[serde(rename = "evt_strike")]
    EvtStrike {
        serial_number: String,
        hub_sn: String,
        evt: Vec<serde_json::Value>,
    },
    /// Precipitation start event: `[time_epoch]`.
    #[serde(rename = "evt_precip")]
    EvtPrecip {
        serial_number: String,
        hub_sn: String,
        evt: Vec<serde_json::Value>,
    },
    #[serde(rename = "device_status")]
    DeviceStatus {
        serial_number: String,
        hub_sn: String,
        timestamp: i64,
        uptime: u64,
        voltage: f64,
        firmware_revision: u32,
        rssi: i32,
        hub_rssi: i32,
        sensor_status: u32,
        debug: u8,
    },
    #[serde(rename = "hub_status")]
    HubStatus {
        serial_number: String,
        firmware_revision: String,
        uptime: u64,
        rssi: i32,
        timestamp: i64,
    },
    #[serde(other)]
    Other,
}

/// Decoded obs_st row. Indices match the WeatherFlow UDP API:
/// 0:time, 1:wind_lull_mps, 2:wind_avg_mps, 3:wind_gust_mps,
/// 4:wind_dir_deg, 5:wind_sample_interval_s, 6:pressure_mb,
/// 7:air_temp_c, 8:rh_pct, 9:illuminance_lx, 10:uv_index,
/// 11:solar_w_m2, 12:rain_mm_last_min, 13:precip_type,
/// 14:lightning_avg_dist_km, 15:lightning_strike_count,
/// 16:battery_v, 17:report_interval_min.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObsSt {
    pub time_epoch: i64,
    pub wind_lull_mps: f64,
    pub wind_avg_mps: f64,
    pub wind_gust_mps: f64,
    pub wind_dir_deg: f64,
    pub pressure_mb: f64,
    pub air_temp_c: f64,
    pub rh_pct: f64,
    pub illuminance_lx: f64,
    pub uv_index: f64,
    pub solar_w_m2: f64,
    pub rain_mm_last_min: f64,
    pub precip_type: u8,
    pub lightning_avg_dist_km: f64,
    pub lightning_strike_count_last_min: u32,
    pub battery_v: f64,
    pub report_interval_min: u32,
}

impl ObsSt {
    pub fn from_array(arr: &[serde_json::Value]) -> Option<Self> {
        let f = |i: usize| arr.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let i = |idx: usize| arr.get(idx).and_then(|v| v.as_i64()).unwrap_or(0);
        Some(Self {
            time_epoch: i(0),
            wind_lull_mps: f(1),
            wind_avg_mps: f(2),
            wind_gust_mps: f(3),
            wind_dir_deg: f(4),
            pressure_mb: f(6),
            air_temp_c: f(7),
            rh_pct: f(8),
            illuminance_lx: f(9),
            uv_index: f(10),
            solar_w_m2: f(11),
            rain_mm_last_min: f(12),
            precip_type: f(13) as u8,
            lightning_avg_dist_km: f(14),
            lightning_strike_count_last_min: f(15) as u32,
            battery_v: f(16),
            report_interval_min: f(17) as u32,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RapidWindOb {
    pub time_epoch: i64,
    pub speed_mps: f64,
    pub direction_deg: f64,
}

impl RapidWindOb {
    pub fn from_array(arr: &[serde_json::Value]) -> Option<Self> {
        Some(Self {
            time_epoch: arr.get(0)?.as_i64()?,
            speed_mps: arr.get(1)?.as_f64().unwrap_or(0.0),
            direction_deg: arr.get(2)?.as_f64().unwrap_or(0.0),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StrikeEvent {
    pub time_epoch: i64,
    pub distance_km: f64,
    pub energy: u64,
}

impl StrikeEvent {
    pub fn from_array(arr: &[serde_json::Value]) -> Option<Self> {
        Some(Self {
            time_epoch: arr.get(0)?.as_i64()?,
            distance_km: arr.get(1)?.as_f64().unwrap_or(0.0),
            energy: arr.get(2)?.as_u64().unwrap_or(0),
        })
    }
}
