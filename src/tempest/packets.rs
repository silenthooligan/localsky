// Tempest UDP packet wire format, every payload arrives as JSON with a
// `type` discriminator. Only the kinds we actually render are modeled;
// the rest are silently ignored by the listener.
//
// Reference: https://weatherflow.github.io/Tempest/api/udp.html

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TempestPacket {
    /// Once-per-minute full observation. The `obs` array is a single
    /// 18-element snapshot, see `ObsSt::from_array` for the field map.
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

impl TempestPacket {
    /// The hub that broadcast this packet.
    ///
    /// Every variant carries one, which is what makes filtering on the
    /// operator's configured `hub_serial` possible at all. `None` only
    /// for a variant we do not model.
    pub fn hub_sn(&self) -> Option<&str> {
        match self {
            Self::ObsSt { hub_sn, .. }
            | Self::RapidWind { hub_sn, .. }
            | Self::EvtStrike { hub_sn, .. }
            | Self::EvtPrecip { hub_sn, .. }
            | Self::DeviceStatus { hub_sn, .. } => Some(hub_sn.as_str()),
            _ => None,
        }
    }
}

/// Decoded obs_st row. Indices match the WeatherFlow UDP API:
/// 0:time, 1:wind_lull_mps, 2:wind_avg_mps, 3:wind_gust_mps,
/// 4:wind_dir_deg, 5:wind_sample_interval_s, 6:pressure_mb,
/// 7:air_temp_c, 8:rh_pct, 9:illuminance_lx, 10:uv_index,
/// 11:solar_w_m2, 12:rain_mm_last_min, 13:precip_type,
/// 14:lightning_avg_dist_km, 15:lightning_strike_count,
/// 16:battery_v, 17:report_interval_min.
///
/// Every reading is optional, because absence has to be representable
/// separately from zero. A station with a failed temperature/humidity
/// module keeps broadcasting once a minute with `null` in those slots,
/// and a short row is legal on the wire too. On nearly every one of
/// these channels zero is a genuine reading (calm wind, no rain, a quiet
/// lightning minute, due north), so a decoder that defaults to zero
/// produces a value the rest of LocalSky cannot tell apart from a
/// station actually reporting it. Air temperature is where that costs
/// money: 0 C decoded from a null is 32 F, which the freeze gate hard-
/// skips every zone on, in July, in Florida. A null humidity is the same
/// shape one step down: ET0 is computed from the FORECAST day, not from
/// this reading, so a 0 % RH does not reach it, but it is still a
/// measurement the station never took, standing in the heat index, the
/// HA sensors and the history. `None` here becomes an OMITTED bus field in
/// `sources::tempest_udp`, which falls through to the forecast fill and
/// is honestly flagged degraded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObsSt {
    pub time_epoch: i64,
    pub wind_lull_mps: Option<f64>,
    pub wind_avg_mps: Option<f64>,
    pub wind_gust_mps: Option<f64>,
    pub wind_dir_deg: Option<f64>,
    pub pressure_mb: Option<f64>,
    pub air_temp_c: Option<f64>,
    pub rh_pct: Option<f64>,
    pub illuminance_lx: Option<f64>,
    pub uv_index: Option<f64>,
    pub solar_w_m2: Option<f64>,
    pub rain_mm_last_min: Option<f64>,
    pub precip_type: Option<u8>,
    pub lightning_avg_dist_km: Option<f64>,
    pub lightning_strike_count_last_min: Option<u32>,
    pub battery_v: Option<f64>,
    pub report_interval_min: Option<u32>,
}

/// How far AHEAD of this host's clock a row may stamp itself and still be
/// believed. Tight, because ahead is the direction that reads LIVE: a
/// packet at the edge of this window buys at most this plus
/// `TEMPEST_LIVE_MAX_AGE_S` (600 s) of undeserved freshness before it
/// goes stale on its own.
const EPOCH_MAX_AHEAD_S: i64 = 15 * 60;
/// How far BEHIND. Generous, because behind only ever reads STALE, which
/// falls through to the forecast fill and is flagged degraded, and a hub
/// whose clock lags should stay usable rather than vanish.
const EPOCH_MAX_BEHIND_S: i64 = 24 * 60 * 60;

/// The observation time out of slot 0, or `None` when the row carries no
/// usable one.
///
/// The wire format writes an integer epoch; the float branch exists only
/// so a firmware that emits `1700000000.0` is not thrown away.
///
/// "Usable" has to mean PLAUSIBLE and not merely numeric, because the
/// observation decoders reject on this and nothing else. The epoch is
/// what the live store stamps per-field freshness from, and every
/// staleness test downstream is `now - epoch < max_age`
/// (`assembly::readings::resolve_current_conditions`, and `station_fresh`
/// / `rain_live` in `assembly`), so a time in the future is not merely
/// wrong, it is PERMANENTLY fresh: one malformed broadcast on this
/// unauthenticated port would pin its numbers as the live station
/// reading for good, hold the rain rate at its value, and leave no epoch
/// stale enough for a cloud fill to reclaim the field. `as i64`
/// SATURATES, so an unbounded float lands on exactly that time: `1e300 as
/// i64` is `i64::MAX`. A time in the deep past is the milder mirror, but
/// it still buckets the minute's rain under a day that is not today,
/// which zeroes `rain_in_today` -- and a zero rain total is the reading
/// that waters.
///
/// A row whose time is outside the window is dropped whole. A row missing
/// a SENSOR is still an observation, just short a reading.
fn epoch_at(slot: Option<&serde_json::Value>) -> Option<i64> {
    usable_epoch(slot, crate::timefmt::now_epoch())
}

/// `epoch_at` against an explicit clock, so the window is testable
/// without waiting for one.
fn usable_epoch(slot: Option<&serde_json::Value>, now: i64) -> Option<i64> {
    let v = slot?;
    // The cast saturates (1e300 -> i64::MAX, -1e300 -> i64::MIN) and maps
    // NaN to 0, so every unusable float lands far outside the window
    // below rather than arriving as a plausible-looking time.
    let secs = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    let window = now.saturating_sub(EPOCH_MAX_BEHIND_S)..=now.saturating_add(EPOCH_MAX_AHEAD_S);
    (secs > 0 && window.contains(&secs)).then_some(secs)
}

impl ObsSt {
    pub fn from_array(arr: &[serde_json::Value]) -> Option<Self> {
        // A present, numeric slot or nothing at all: a missing index, a
        // JSON `null` and a non-number all mean "no reading" here, and
        // none of them may become a zero. Float -> integer casts saturate,
        // so a garbage count clamps rather than wrapping.
        let f = |i: usize| arr.get(i).and_then(|v| v.as_f64());
        Some(Self {
            time_epoch: epoch_at(arr.first())?,
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
            precip_type: f(13).map(|v| v as u8),
            lightning_avg_dist_km: f(14),
            lightning_strike_count_last_min: f(15).map(|v| v as u32),
            battery_v: f(16),
            report_interval_min: f(17).map(|v| v as u32),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RapidWindOb {
    pub time_epoch: i64,
    /// Optional for the same reason the obs_st readings are: a calm three
    /// seconds is a real 0 m/s, so a null decoded as zero would be
    /// indistinguishable from the station reporting calm.
    pub speed_mps: Option<f64>,
    pub direction_deg: Option<f64>,
}

impl RapidWindOb {
    pub fn from_array(arr: &[serde_json::Value]) -> Option<Self> {
        Some(Self {
            time_epoch: epoch_at(arr.first())?,
            speed_mps: arr.get(1).and_then(|v| v.as_f64()),
            direction_deg: arr.get(2).and_then(|v| v.as_f64()),
        })
    }
}

/// Detection-network tags for StrikeEvent::source. Tempest is the local
/// station (distance-only); Blitzortung is the community network
/// (located strikes with lat/lon), fed by sources::blitzortung.
pub const STRIKE_SOURCE_TEMPEST: &str = "tempest";
pub const STRIKE_SOURCE_BLITZORTUNG: &str = "blitzortung";

fn default_strike_source() -> String {
    STRIKE_SOURCE_TEMPEST.to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrikeEvent {
    pub time_epoch: i64,
    pub distance_km: f64,
    pub energy: u64,
    /// Which detection network produced this strike ("tempest" or
    /// "blitzortung"). Always serialized so the radar layer can
    /// attribute per-strike (Blitzortung's terms require visible
    /// attribution); payloads recorded before this field existed
    /// deserialize to "tempest".
    #[serde(default = "default_strike_source")]
    pub source: String,
    /// True strike position, present only for networks that locate
    /// strikes (Blitzortung). Tempest reports distance but not bearing,
    /// so its strikes stay None and render as distance rings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    /// Stable per-strike identity for dedup: the source network's raw
    /// nanosecond timestamp (Blitzortung). The community feed re-solves
    /// and RE-PUBLISHES a strike under the same nanosecond time as late
    /// station reports arrive, often at a slightly moved position; keying
    /// on the full nanosecond value lets apply_strikes collapse those
    /// refinements to a single strike (last-write-wins position) instead
    /// of double-counting and double-plotting them. time_epoch (seconds)
    /// is far too coarse a key: hundreds of distinct strikes routinely
    /// share one second. 0 means no identity (Tempest distance rings,
    /// and payloads recorded before this field existed).
    #[serde(default, skip_serializing_if = "id_is_absent")]
    pub id: i64,
}

fn id_is_absent(id: &i64) -> bool {
    *id == 0
}

impl Default for StrikeEvent {
    fn default() -> Self {
        Self {
            time_epoch: 0,
            distance_km: 0.0,
            energy: 0,
            source: default_strike_source(),
            lat: None,
            lon: None,
            id: 0,
        }
    }
}

impl StrikeEvent {
    /// Unlike the observation decoders above, a missing distance stays a
    /// zero here. That is not a safety argument, and an earlier version
    /// of this comment claimed it was: NOTHING in the watering decision
    /// reads a strike. There is no lightning input anywhere in
    /// `src/engine`, `src/assembly` or `src/refresher`, no lightning gate
    /// in the catalog, and no strike value in the scripting scope.
    /// `distance_km` is display and history only: the radar rings, the
    /// lightning panel, the sensors page, the HA manifest.
    ///
    /// So the zero is a DISPLAY defect, not a conservative default. An
    /// unknown distance plots at the center of the radar and reads out as
    /// "0 mi", which says overhead. It is left alone because the honest
    /// fix is an Option on the wire type, and that ripples through
    /// `sources::blitzortung` (which builds this struct from a computed
    /// haversine), `live_store::apply_strikes` and the radar layer to
    /// change nothing any decision reads. A strike we know happened is
    /// still worth keeping, so the event is not dropped for it.
    pub fn from_array(arr: &[serde_json::Value]) -> Option<Self> {
        Some(Self {
            time_epoch: arr.first()?.as_i64()?,
            distance_km: arr.get(1)?.as_f64().unwrap_or(0.0),
            energy: arr.get(2)?.as_u64().unwrap_or(0),
            ..Self::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(json: &str) -> Vec<serde_json::Value> {
        serde_json::from_str(json).expect("a valid JSON array")
    }

    /// A row's time is judged against THIS HOST's clock (see `epoch_at`),
    /// so the fixtures stamp themselves rather than pinning a literal
    /// epoch that would age out of the window and start failing on its
    /// own. `rest` is the wire row from slot 1 on, comma and all.
    fn row_at(epoch: i64, rest: &str) -> Vec<serde_json::Value> {
        row(&format!("[{epoch}{rest}]"))
    }

    /// A row the WeatherFlow docs would recognize, minus the temperature
    /// and humidity: the shape a Tempest with a failed temp/RH module
    /// keeps broadcasting once a minute. The nulls must decode as absent.
    /// Decoded as zero they are 32 F and 0 % RH, which the live store
    /// stamps fresh and the freeze gate skips the whole yard on.
    #[test]
    fn a_null_slot_decodes_as_no_reading_not_as_zero() {
        let t = crate::timefmt::now_epoch();
        let obs = ObsSt::from_array(&row_at(
            t,
            ",0.18,0.22,0.27,144,6,1017.57,null,null,328,0.03,3,0.0,0,0,0,2.41,1",
        ))
        .expect("a row with a time is an observation");
        assert_eq!(obs.air_temp_c, None, "a null temperature is not 0 C");
        assert_eq!(obs.rh_pct, None, "a null humidity is not 0 % RH");
        // Everything the packet DID carry still decodes, unchanged.
        assert_eq!(obs.time_epoch, t);
        assert_eq!(obs.pressure_mb, Some(1017.57));
        assert_eq!(obs.wind_avg_mps, Some(0.22));
        // Slot 13 is a reported "no precipitation", which is a reading.
        assert_eq!(obs.precip_type, Some(0));
        assert_eq!(obs.battery_v, Some(2.41));
    }

    /// The other half of the contract, and the reason the fix is not just
    /// "treat zero as missing": a station reporting zero is reporting.
    /// Freezing mornings exist, and calm minutes are the common case.
    #[test]
    fn a_reported_zero_stays_a_reading() {
        let t = crate::timefmt::now_epoch();
        let obs = ObsSt::from_array(&row_at(t, ",0,0,0,0,6,1013,0,0,0,0,0,0,0,0,0,0,1"))
            .expect("a row with a time is an observation");
        assert_eq!(obs.air_temp_c, Some(0.0));
        assert_eq!(obs.rh_pct, Some(0.0));
        assert_eq!(obs.wind_avg_mps, Some(0.0));
        assert_eq!(obs.lightning_strike_count_last_min, Some(0));
    }

    /// A short row is legal on the wire; the slots it never reached are
    /// absent rather than zero, the same as an explicit null.
    #[test]
    fn a_truncated_row_is_absent_past_its_end() {
        let t = crate::timefmt::now_epoch();
        let obs = ObsSt::from_array(&row_at(t, ",0.18,0.22")).expect("decodes");
        assert_eq!(obs.wind_avg_mps, Some(0.22));
        assert_eq!(obs.air_temp_c, None);
        assert_eq!(obs.battery_v, None);
        assert_eq!(obs.lightning_strike_count_last_min, None);
    }

    /// The epoch is what the store stamps freshness from, so a row that
    /// has no usable one is not an observation and is dropped whole.
    #[test]
    fn a_row_without_a_usable_time_is_rejected() {
        let t = crate::timefmt::now_epoch();
        assert!(ObsSt::from_array(&row("[]")).is_none());
        assert!(ObsSt::from_array(&row("[null,0.18,0.22]")).is_none());
        assert!(RapidWindOb::from_array(&row("[null,2.0,180]")).is_none());
        // A float-encoded epoch is still a time, not garbage.
        let ob = RapidWindOb::from_array(&row(&format!("[{t}.0,2.0,180]"))).expect("decodes");
        assert_eq!(ob.time_epoch, t);
    }

    /// A time is not merely a number of the right TYPE, and this is the
    /// ONLY thing the decoders reject on. Every staleness test downstream
    /// is `now - epoch < max_age`, so a time in the future never goes
    /// stale: it would pin this packet's numbers as the live station
    /// reading for good, and no forecast fill could reclaim the field.
    /// The float cast saturates, so `1e300` IS that time, and so is an
    /// integer at the end of the range.
    #[test]
    fn a_time_that_could_never_go_stale_is_not_a_time() {
        let t = crate::timefmt::now_epoch();
        assert!(ObsSt::from_array(&row("[1e300,0.18,0.22]")).is_none());
        assert!(ObsSt::from_array(&row("[9223372036854775807,0.18,0.22]")).is_none());
        assert!(ObsSt::from_array(&row_at(t + EPOCH_MAX_AHEAD_S + 60, ",0.18,0.22")).is_none());
        assert!(RapidWindOb::from_array(&row("[1e300,2.0,180]")).is_none());
        // A hub whose clock is a few seconds off is still a hub reporting.
        assert!(RapidWindOb::from_array(&row_at(t + 5, ",2.0,180")).is_some());
    }

    /// Zero is the other half of the same rule: it is what the old
    /// decoder produced for every unparseable slot, and stamped into the
    /// snapshot it DE-freshens a station that was live. A deep-past time
    /// is milder but not harmless: the minute's rain buckets under a day
    /// that is not today, which zeroes the day's total, and a zero rain
    /// total is the reading that waters.
    #[test]
    fn a_zero_or_ancient_time_is_not_a_time() {
        let t = crate::timefmt::now_epoch();
        assert!(ObsSt::from_array(&row("[0,0.18,0.22]")).is_none());
        assert!(ObsSt::from_array(&row("[-1,0.18,0.22]")).is_none());
        assert!(ObsSt::from_array(&row_at(t - EPOCH_MAX_BEHIND_S - 60, ",0.18,0.22")).is_none());
        assert!(RapidWindOb::from_array(&row("[0,2.0,180]")).is_none());
    }

    /// The window itself, against a clock the test owns. It is lopsided
    /// on purpose: behind is generous because behind only ever reads
    /// stale (the forecast fills, the decision is flagged degraded),
    /// while ahead is tight because ahead is the direction that reads
    /// live.
    #[test]
    fn the_usable_window_is_measured_from_this_hosts_clock() {
        let now = 1_700_000_000;
        let at = |secs: i64| serde_json::Value::from(secs);
        assert_eq!(usable_epoch(Some(&at(now)), now), Some(now));
        assert_eq!(usable_epoch(Some(&at(now - 90)), now), Some(now - 90));
        assert_eq!(
            usable_epoch(Some(&at(now + EPOCH_MAX_AHEAD_S)), now),
            Some(now + EPOCH_MAX_AHEAD_S),
            "the edge of the window is still a time"
        );
        assert_eq!(
            usable_epoch(Some(&at(now + EPOCH_MAX_AHEAD_S + 1)), now),
            None
        );
        assert_eq!(
            usable_epoch(Some(&at(now - EPOCH_MAX_BEHIND_S)), now),
            Some(now - EPOCH_MAX_BEHIND_S)
        );
        assert_eq!(
            usable_epoch(Some(&at(now - EPOCH_MAX_BEHIND_S - 1)), now),
            None
        );
        assert_eq!(usable_epoch(None, now), None);
    }

    /// Rapid wind carries the same hazard at three-second cadence: a calm
    /// sample is a real 0 m/s, so a null one cannot borrow its value.
    #[test]
    fn a_rapid_wind_sample_reports_absence_rather_than_calm() {
        let t = crate::timefmt::now_epoch();
        let missing = RapidWindOb::from_array(&row_at(t, ",null,null")).expect("decodes");
        assert_eq!(missing.speed_mps, None, "a null sample is not a calm one");
        assert_eq!(missing.direction_deg, None);
        let calm = RapidWindOb::from_array(&row_at(t, ",0,0")).expect("decodes");
        assert_eq!(calm.speed_mps, Some(0.0), "a reported calm is a reading");
        assert_eq!(calm.direction_deg, Some(0.0));
    }
}
