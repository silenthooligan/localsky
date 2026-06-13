// Shared store of the latest Tempest readings + a watch channel that the
// SSE endpoint subscribes to so browsers see updates the moment a packet
// lands. arc-swap gives us a copy-on-write Arc<Snapshot> so handlers can
// read the current state without taking a lock.

use crate::tempest::packets::StrikeEvent;
use serde::{Deserialize, Serialize};

#[cfg(feature = "ssr")]
use {
    crate::tempest::packets::{ObsSt, RapidWindOb},
    arc_swap::ArcSwap,
    std::collections::VecDeque,
    std::sync::{Arc, Mutex},
    tokio::sync::watch,
};

/// One immutable snapshot of every value the dashboard renders. Rebuilt on
/// each Tempest packet and atomically swapped into the store. Cheap to
/// clone (it's `Arc`-wrapped before any client touches it).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub last_packet_epoch: i64,
    pub air_temp_f: f64,
    pub feels_like_f: f64,
    pub dew_point_f: f64,
    pub wet_bulb_f: f64,
    pub rh_pct: f64,
    pub pressure_inhg: f64,
    pub pressure_trend_inhg: Vec<(i64, f64)>,
    pub wind_lull_mph: f64,
    pub wind_avg_mph: f64,
    pub wind_gust_mph: f64,
    pub wind_dir_deg: f64,
    pub rapid_wind_mph: f64,
    pub rapid_wind_dir: f64,
    pub illuminance_lx: f64,
    pub uv_index: f64,
    pub solar_w_m2: f64,
    pub rain_in_last_min: f64,
    pub rain_in_today: f64,
    pub rain_intensity_in_hr: f64,
    pub precip_type: u8, // 0=none 1=rain 2=hail
    pub lightning_count_last_min: u32,
    pub lightning_strikes_last_hour: u32,
    pub lightning_recent: Vec<StrikeEvent>,
    pub lightning_avg_dist_mi: f64,
    pub last_strike_distance_mi: Option<f64>,
    pub last_strike_epoch: Option<i64>,
    pub battery_v: f64,
    pub battery_pct: f64,
    pub station_serial: String,
    pub hub_serial: String,
}

impl Snapshot {
    /// State-of-charge curve for the Tempest's lithium-titanate (LTO)
    /// battery. Piecewise-linear table copied verbatim from
    /// pyweatherflowudp's calc.py so this app's percentage matches what
    /// HA's WeatherFlow integration shows (and the WeatherFlow help docs
    /// at help.tempest.earth/.../Solar-Power-Rechargeable-Battery).
    /// Charges to 2.80 V; 2.70 is treated as 100% so a slightly degraded
    /// pack still reads "full".
    pub fn battery_pct_from_v(v: f64) -> f64 {
        const CURVE: &[(f64, f64)] = &[
            (2.00, 0.0),
            (2.10, 5.0),
            (2.15, 10.0),
            (2.16, 20.0),
            (2.19, 30.0),
            (2.20, 40.0),
            (2.23, 50.0),
            (2.28, 60.0),
            (2.32, 70.0),
            (2.40, 80.0),
            (2.50, 90.0),
            (2.52, 95.0),
            (2.70, 100.0),
        ];
        if v <= CURVE[0].0 {
            return CURVE[0].1;
        }
        if v >= CURVE[CURVE.len() - 1].0 {
            return CURVE[CURVE.len() - 1].1;
        }
        for w in CURVE.windows(2) {
            let (l, r) = (w[0], w[1]);
            if v >= l.0 && v <= r.0 {
                let slope = (r.1 - l.1) / (r.0 - l.0);
                return l.1 + slope * (v - l.0);
            }
        }
        0.0
    }
}

/// Cap on the recent-strike ring buffer (see apply_strikes for why).
#[cfg(feature = "ssr")]
const MAX_RECENT_STRIKES: usize = 500;

#[cfg(feature = "ssr")]
pub struct TempestStore {
    current: ArcSwap<Snapshot>,
    tx: watch::Sender<Arc<Snapshot>>,
    rx: watch::Receiver<Arc<Snapshot>>,
    rolling: Mutex<RollingBuffers>,
}

#[cfg(feature = "ssr")]
#[derive(Default)]
struct RollingBuffers {
    pressure: VecDeque<(i64, f64)>, // last 6h of pressure samples
    strikes: VecDeque<StrikeEvent>, // last hour of strikes
    rain_today: f64,                // sum of rain_mm_last_min, day-bucket
    rain_today_day: i32,            // current LOCAL calendar-day bucket (num_days_from_ce)
}

/// Local calendar-day ordinal for a UNIX epoch: num_days_from_ce of the
/// local date, so the rain-today bucket rolls over at LOCAL midnight.
/// The previous `(epoch / 86400)` bucket was a UTC day despite its
/// comment, which zeroed the accumulator mid-evening for any tz west of
/// UTC and split overnight storms across two "days". Falls back to the
/// integer UTC day when the timestamp doesn't resolve to a local time.
#[cfg(feature = "ssr")]
fn local_day_ordinal(epoch: i64) -> i32 {
    use chrono::{Datelike, TimeZone};
    match chrono::Local.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt.date_naive().num_days_from_ce(),
        None => (epoch / 86400) as i32,
    }
}

#[cfg(feature = "ssr")]
impl Default for TempestStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "ssr")]
impl TempestStore {
    pub fn new() -> Self {
        let initial = Arc::new(Snapshot::default());
        let (tx, rx) = watch::channel(initial.clone());
        Self {
            current: ArcSwap::from(initial),
            tx,
            rx,
            rolling: Mutex::new(RollingBuffers::default()),
        }
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.rx.clone()
    }

    /// Replace the snapshot wholesale. Used by demo-mode synthesis to
    /// drop synthetic data into the live store without going through
    /// the per-packet apply_* path. Real packet processing should
    /// continue to use apply_obs / apply_rapid_wind / apply_strike so
    /// rolling buffers stay accurate.
    pub fn store(&self, snap: Snapshot) {
        let arc = Arc::new(snap);
        self.current.store(arc.clone());
        let _ = self.tx.send(arc);
    }

    /// Seed the rain-today accumulator from persisted history after a
    /// restart, so a mid-storm reboot doesn't zero the daily total (the
    /// per-minute UDP packets carry deltas, not the accumulation). No-op
    /// unless `day_epoch` falls on the current local day. Only ever
    /// raises the accumulator: packets that landed since boot stay
    /// counted when they already exceed the seed. Returns true when the
    /// seed was applied.
    pub fn seed_rain_today(&self, rain_in: f64, day_epoch: i64) -> bool {
        let bucket = local_day_ordinal(day_epoch);
        let today = local_day_ordinal(chrono::Utc::now().timestamp());
        if bucket != today || rain_in <= 0.0 {
            return false;
        }
        let rain_mm = rain_in * 25.4;
        let mut roll = self.rolling.lock().unwrap();
        if roll.rain_today_day == bucket {
            if roll.rain_today >= rain_mm {
                return false;
            }
            roll.rain_today = rain_mm;
        } else {
            // A stale (or default-zero) bucket never carries into today.
            roll.rain_today_day = bucket;
            roll.rain_today = rain_mm;
        }
        true
    }

    pub fn apply_obs(&self, station_serial: &str, hub_serial: &str, obs: &ObsSt) {
        let mut roll = self.rolling.lock().unwrap();

        // Trim pressure buffer to last 6 hours.
        let now = obs.time_epoch;
        let six_hours_ago = now - 6 * 3600;
        while roll
            .pressure
            .front()
            .is_some_and(|(t, _)| *t < six_hours_ago)
        {
            roll.pressure.pop_front();
        }
        let pressure_inhg = obs.pressure_mb * 0.02953;
        roll.pressure.push_back((now, pressure_inhg));

        // Today rain accumulation, bucketed by the LOCAL calendar day of
        // the observation so the total resets at local midnight.
        let day_bucket = local_day_ordinal(now);
        if roll.rain_today_day != day_bucket {
            roll.rain_today_day = day_bucket;
            roll.rain_today = 0.0;
        }
        roll.rain_today += obs.rain_mm_last_min;
        let rain_today_in = roll.rain_today / 25.4;

        // Trim strike buffer to last hour.
        let one_hour_ago = now - 3600;
        while roll
            .strikes
            .front()
            .is_some_and(|s| s.time_epoch < one_hour_ago)
        {
            roll.strikes.pop_front();
        }
        let last = roll.strikes.back().cloned();
        let pressure_trend: Vec<_> = roll.pressure.iter().cloned().collect();
        drop(roll);

        let air_temp_f = obs.air_temp_c * 9.0 / 5.0 + 32.0;
        let dew_c = dew_point_c(obs.air_temp_c, obs.rh_pct);
        let dew_f = dew_c * 9.0 / 5.0 + 32.0;
        let wet_c = wet_bulb_c(obs.air_temp_c, obs.rh_pct);
        let wet_f = wet_c * 9.0 / 5.0 + 32.0;
        let wind_avg_mph = obs.wind_avg_mps * 2.23694;
        let feels_f = feels_like_f(air_temp_f, obs.rh_pct, wind_avg_mph);

        let prev = self.current.load_full();
        let new = Arc::new(Snapshot {
            last_packet_epoch: now,
            air_temp_f,
            feels_like_f: feels_f,
            dew_point_f: dew_f,
            wet_bulb_f: wet_f,
            rh_pct: obs.rh_pct,
            pressure_inhg,
            pressure_trend_inhg: pressure_trend,
            wind_lull_mph: obs.wind_lull_mps * 2.23694,
            wind_avg_mph,
            wind_gust_mph: obs.wind_gust_mps * 2.23694,
            wind_dir_deg: obs.wind_dir_deg,
            // rapid_wind keeps its own values; carry forward whatever the last
            // 3s tick set so a fresh obs_st doesn't blank them out.
            rapid_wind_mph: prev.rapid_wind_mph,
            rapid_wind_dir: prev.rapid_wind_dir,
            illuminance_lx: obs.illuminance_lx,
            uv_index: obs.uv_index,
            solar_w_m2: obs.solar_w_m2,
            rain_in_last_min: obs.rain_mm_last_min / 25.4,
            rain_in_today: rain_today_in,
            // 60 * (mm/min) → mm/hr → in/hr.
            rain_intensity_in_hr: obs.rain_mm_last_min * 60.0 / 25.4,
            precip_type: obs.precip_type,
            lightning_count_last_min: obs.lightning_strike_count_last_min,
            lightning_strikes_last_hour: prev.lightning_strikes_last_hour,
            lightning_recent: prev.lightning_recent.clone(),
            lightning_avg_dist_mi: obs.lightning_avg_dist_km * 0.621371,
            last_strike_distance_mi: last.as_ref().map(|s| s.distance_km * 0.621371),
            last_strike_epoch: last.as_ref().map(|s| s.time_epoch),
            battery_v: obs.battery_v,
            battery_pct: Snapshot::battery_pct_from_v(obs.battery_v),
            station_serial: station_serial.to_string(),
            hub_serial: hub_serial.to_string(),
        });
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }

    pub fn apply_rapid_wind(&self, ob: &RapidWindOb) {
        let prev = self.current.load_full();
        let new = Arc::new(Snapshot {
            rapid_wind_mph: ob.speed_mps * 2.23694,
            rapid_wind_dir: ob.direction_deg,
            ..(*prev).clone()
        });
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }

    pub fn apply_strike(&self, evt: &StrikeEvent) {
        self.apply_strikes(std::slice::from_ref(evt));
    }

    /// Batch strike insert: one lock, one snapshot swap, one SSE event
    /// for the whole slice. The Tempest UDP path delegates here one
    /// strike at a time; the Blitzortung feed batches because the
    /// community network can deliver several strikes per second during
    /// an outbreak and each swap broadcasts a full snapshot.
    pub fn apply_strikes(&self, evts: &[StrikeEvent]) {
        let Some(newest) = evts.iter().max_by_key(|e| e.time_epoch).cloned() else {
            return;
        };
        let strikes: Vec<StrikeEvent> = {
            let mut roll = self.rolling.lock().unwrap();
            for evt in evts {
                roll.strikes.push_back(evt.clone());
            }
            // Trim to last hour.
            let one_hour_ago = newest.time_epoch - 3600;
            while roll
                .strikes
                .front()
                .is_some_and(|s| s.time_epoch < one_hour_ago)
            {
                roll.strikes.pop_front();
            }
            // Hard cap. The local Tempest alone never gets near it
            // (strikes are rare per-minute events), but the Blitzortung
            // community feed can keep hundreds in the hour window, and
            // the buffer is serialized into every snapshot/SSE payload,
            // so it must stay bounded regardless of storm size.
            while roll.strikes.len() > MAX_RECENT_STRIKES {
                roll.strikes.pop_front();
            }
            roll.strikes.iter().cloned().collect()
        };
        let prev = self.current.load_full();
        let count = strikes.len() as u32;
        let new = Arc::new(Snapshot {
            lightning_strikes_last_hour: count,
            lightning_recent: strikes,
            last_strike_distance_mi: Some(newest.distance_km * 0.621371),
            last_strike_epoch: Some(newest.time_epoch),
            ..(*prev).clone()
        });
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }

    pub fn apply_battery(&self, voltage: f64) {
        let prev = self.current.load_full();
        let new = Arc::new(Snapshot {
            battery_v: voltage,
            battery_pct: Snapshot::battery_pct_from_v(voltage),
            ..(*prev).clone()
        });
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }
}

/// Magnus-Tetens dew point (°C) from temperature (°C) and RH (%).
#[cfg(feature = "ssr")]
fn dew_point_c(t_c: f64, rh: f64) -> f64 {
    let a = 17.625;
    let b = 243.04;
    let alpha = (rh.max(1.0) / 100.0).ln() + a * t_c / (b + t_c);
    b * alpha / (a - alpha)
}

/// Stull (2011) wet-bulb approximation, valid for normal RH/temp ranges.
#[cfg(feature = "ssr")]
fn wet_bulb_c(t_c: f64, rh: f64) -> f64 {
    let rh = rh.max(1.0);
    t_c * (0.151_977 * (rh + 8.313_659).sqrt()).atan() + (t_c + rh).atan() - (rh - 1.676_331).atan()
        + 0.003_918_38 * rh.powf(1.5) * (0.023_101 * rh).atan()
        - 4.686_035
}

#[cfg(all(test, feature = "ssr"))]
mod rain_day_tests {
    use super::*;
    use chrono::TimeZone;

    /// UNIX epoch for a LOCAL wall-clock instant. Test times stay clear
    /// of the 02:00-03:00 DST-transition window so `.single()` is safe.
    fn local_epoch(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap()
            .timestamp()
    }

    fn obs(epoch: i64, rain_mm: f64) -> ObsSt {
        ObsSt {
            time_epoch: epoch,
            rain_mm_last_min: rain_mm,
            ..Default::default()
        }
    }

    #[test]
    fn local_day_ordinal_buckets_by_local_date() {
        let d1a = local_epoch(2026, 3, 3, 0, 30);
        let d1b = local_epoch(2026, 3, 3, 23, 30);
        let d2 = local_epoch(2026, 3, 4, 0, 30);
        assert_eq!(local_day_ordinal(d1a), local_day_ordinal(d1b));
        assert_eq!(local_day_ordinal(d2), local_day_ordinal(d1b) + 1);
    }

    #[test]
    fn rain_today_accumulates_within_a_local_day_and_resets_at_local_midnight() {
        let store = TempestStore::new();
        // 23:30 local: 5.08 mm = 0.20".
        store.apply_obs("ST", "HB", &obs(local_epoch(2026, 1, 15, 23, 30), 5.08));
        assert!((store.snapshot().rain_in_today - 0.20).abs() < 1e-9);
        // 23:50 same local day: accumulates to 0.30".
        store.apply_obs("ST", "HB", &obs(local_epoch(2026, 1, 15, 23, 50), 2.54));
        assert!((store.snapshot().rain_in_today - 0.30).abs() < 1e-9);
        // 00:30 the NEXT local day: bucket rolls, total restarts at 0.10".
        // With the old UTC bucketing this either kept accumulating or had
        // already reset hours before local midnight, tz-dependent.
        store.apply_obs("ST", "HB", &obs(local_epoch(2026, 1, 16, 0, 30), 2.54));
        assert!((store.snapshot().rain_in_today - 0.10).abs() < 1e-9);
    }

    #[test]
    fn seed_rain_today_applies_only_for_the_current_local_day() {
        let store = TempestStore::new();
        let now = chrono::Utc::now().timestamp();
        // A reading from two days ago must not seed today's bucket.
        assert!(!store.seed_rain_today(0.5, now - 2 * 86_400));
        // Today's persisted total seeds the accumulator.
        assert!(store.seed_rain_today(0.5, now));
        // A live packet accumulates ON TOP of the seed (0.5" + 0.10").
        store.apply_obs("ST", "HB", &obs(now, 2.54));
        assert!((store.snapshot().rain_in_today - 0.60).abs() < 1e-6);
        // A smaller re-seed never lowers the accumulator.
        assert!(!store.seed_rain_today(0.10, now));
        // Zero / negative seeds are rejected outright.
        assert!(!store.seed_rain_today(0.0, now));
    }
}

#[cfg(all(test, feature = "ssr"))]
mod strike_buffer_tests {
    use super::*;

    fn strike(epoch: i64, dist_km: f64) -> StrikeEvent {
        StrikeEvent {
            time_epoch: epoch,
            distance_km: dist_km,
            ..Default::default()
        }
    }

    #[test]
    fn ring_prunes_strikes_older_than_one_hour() {
        let store = TempestStore::new();
        let t0 = 1_700_000_000;
        store.apply_strike(&strike(t0, 5.0));
        store.apply_strike(&strike(t0 + 60, 8.0));
        assert_eq!(store.snapshot().lightning_strikes_last_hour, 2);
        // A strike >1h later evicts both earlier ones.
        store.apply_strike(&strike(t0 + 3700, 12.0));
        let snap = store.snapshot();
        assert_eq!(snap.lightning_strikes_last_hour, 1);
        assert_eq!(snap.lightning_recent.len(), 1);
        assert_eq!(snap.last_strike_epoch, Some(t0 + 3700));
    }

    #[test]
    fn ring_caps_at_500_strikes() {
        // A Blitzortung-scale burst (600 strikes inside the hour) must
        // not grow the snapshot beyond the cap; the oldest fall off.
        let store = TempestStore::new();
        let t0 = 1_700_000_000;
        let batch: Vec<StrikeEvent> = (0..600).map(|i| strike(t0 + i, 10.0)).collect();
        store.apply_strikes(&batch);
        let snap = store.snapshot();
        assert_eq!(snap.lightning_recent.len(), 500);
        assert_eq!(snap.lightning_strikes_last_hour, 500);
        // Oldest 100 evicted, newest kept.
        assert_eq!(snap.lightning_recent[0].time_epoch, t0 + 100);
        assert_eq!(snap.last_strike_epoch, Some(t0 + 599));
    }

    #[test]
    fn batch_apply_swaps_snapshot_once_and_mixes_sources() {
        let store = TempestStore::new();
        let mut rx = store.subscribe();
        rx.mark_unchanged();
        let community = StrikeEvent {
            time_epoch: 1_700_000_100,
            distance_km: 42.0,
            source: crate::tempest::packets::STRIKE_SOURCE_BLITZORTUNG.to_string(),
            lat: Some(28.5),
            lon: Some(-81.4),
            ..Default::default()
        };
        store.apply_strikes(&[strike(1_700_000_000, 7.0), community]);
        // Exactly one watch notification for the batch.
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        assert!(!rx.has_changed().unwrap());
        let snap = store.snapshot();
        assert_eq!(snap.lightning_recent.len(), 2);
        assert_eq!(snap.lightning_recent[0].source, "tempest");
        assert_eq!(snap.lightning_recent[1].source, "blitzortung");
        assert_eq!(snap.lightning_recent[1].lat, Some(28.5));
        // last_strike_* follows the newest of the batch.
        assert_eq!(snap.last_strike_epoch, Some(1_700_000_100));
    }

    #[test]
    fn empty_batch_is_a_noop() {
        let store = TempestStore::new();
        let mut rx = store.subscribe();
        rx.mark_unchanged();
        store.apply_strikes(&[]);
        assert!(!rx.has_changed().unwrap());
        assert_eq!(store.snapshot().last_strike_epoch, None);
    }
}

/// NWS heat-index formula above 80 °F / 40% RH; NWS wind-chill below 50 °F
/// with wind ≥ 3 mph; otherwise just the air temperature.
#[cfg(feature = "ssr")]
fn feels_like_f(t_f: f64, rh: f64, wind_mph: f64) -> f64 {
    if t_f >= 80.0 && rh >= 40.0 {
        -42.379 + 2.049_015_23 * t_f + 10.143_331_27 * rh
            - 0.224_755_41 * t_f * rh
            - 0.006_837_83 * t_f * t_f
            - 0.054_817_17 * rh * rh
            + 0.001_228_74 * t_f * t_f * rh
            + 0.000_852_82 * t_f * rh * rh
            - 0.000_001_99 * t_f * t_f * rh * rh
    } else if t_f <= 50.0 && wind_mph >= 3.0 {
        35.74 + 0.6215 * t_f - 35.75 * wind_mph.powf(0.16) + 0.4275 * t_f * wind_mph.powf(0.16)
    } else {
        t_f
    }
}
