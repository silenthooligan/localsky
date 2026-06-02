// Demo-mode synthetic data feeder. When LOCALSKY_DEMO=1, this module
// spawns a background task that periodically writes plausible weather +
// irrigation + forecast values into the live stores so the dashboard
// renders fully populated UI without any external dependency.
//
// Uses ..Default::default() on every snapshot so we only set the fields
// we want to show; the rest stay zero/empty and the UI degrades
// gracefully through its existing empty-state handling.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::interval;
use tracing::info;

use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::forecast::ForecastStore;
use crate::ha::snapshot::{
    DayVerdict, IrrigationSnapshot, SkipCheck, SoilForecast, WaterBudget, ZoneMath, ZoneState,
};
use crate::ha::IrrigationStore;
use crate::tempest::state::{Snapshot as TempestSnapshot, TempestStore};

/// Spawn the demo-data feeder. Tick cadence 3s; synthesized "day"
/// loops every ~8.6 min so screenshots can capture variety quickly.
pub fn spawn(
    tempest: Arc<TempestStore>,
    irrigation: Arc<IrrigationStore>,
    forecast: Arc<ForecastStore>,
) {
    info!("demo_data: spawning synthetic data feeder (LOCALSKY_DEMO=1)");
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(3));
        forecast.store(synth_forecast());
        let started = std::time::Instant::now();
        loop {
            tick.tick().await;
            let elapsed_s = started.elapsed().as_secs() as f64;
            let t_sim = (elapsed_s * 167.0) % 86400.0;
            tempest.store(synth_tempest(t_sim));
            irrigation.store(synth_irrigation(t_sim));
        }
    });
}

fn synth_tempest(t_sim: f64) -> TempestSnapshot {
    let day_phase = (t_sim / 86400.0) * std::f64::consts::TAU;
    let solar_norm = (day_phase - std::f64::consts::FRAC_PI_2).sin();
    let solar = if solar_norm > 0.0 {
        solar_norm * 950.0
    } else {
        0.0
    };
    let temp_c = 27.0 + 5.0 * (day_phase - 0.4 * std::f64::consts::TAU).sin();
    let temp_f = temp_c * 9.0 / 5.0 + 32.0;
    let rh = (75.0 - 15.0 * (day_phase - 0.4 * std::f64::consts::TAU).sin()).clamp(35.0, 95.0);
    let wind_mph = 4.5 + 2.5 * (day_phase * 2.5).sin().abs();
    let gust_mph = wind_mph + 3.0;
    let dew_f = temp_f - (100.0 - rh) / 5.0;
    let now = chrono::Utc::now().timestamp();
    TempestSnapshot {
        last_packet_epoch: now,
        air_temp_f: temp_f,
        feels_like_f: if temp_f > 80.0 && rh > 40.0 {
            temp_f + (rh - 40.0) * 0.1
        } else {
            temp_f
        },
        dew_point_f: dew_f,
        wet_bulb_f: temp_f - (temp_f - dew_f) * 0.4,
        rh_pct: rh,
        pressure_inhg: 30.05 + (day_phase * 0.5).sin() * 0.05,
        pressure_trend_inhg: (0..72)
            .map(|i| {
                (
                    now - (72 - i) as i64 * 300,
                    30.05 + ((i as f64 / 12.0).sin() * 0.04),
                )
            })
            .collect(),
        wind_lull_mph: (wind_mph - 1.5).max(0.0),
        wind_avg_mph: wind_mph,
        wind_gust_mph: gust_mph,
        wind_dir_deg: (180.0 + 60.0 * (day_phase * 0.5).sin()).rem_euclid(360.0),
        rapid_wind_mph: wind_mph + (day_phase * 7.0).sin() * 0.6,
        rapid_wind_dir: (180.0 + 60.0 * (day_phase * 0.5).sin()).rem_euclid(360.0),
        illuminance_lx: solar * 130.0,
        uv_index: (solar / 100.0).clamp(0.0, 11.0),
        solar_w_m2: solar,
        rain_in_last_min: 0.0,
        rain_in_today: 0.0,
        rain_intensity_in_hr: 0.0,
        precip_type: 0,
        lightning_count_last_min: 0,
        lightning_strikes_last_hour: 0,
        lightning_recent: Vec::new(),
        lightning_avg_dist_mi: 0.0,
        last_strike_distance_mi: None,
        last_strike_epoch: None,
        battery_v: 2.68,
        battery_pct: TempestSnapshot::battery_pct_from_v(2.68),
        station_serial: "ST-DEMO0001".into(),
        hub_serial: "HB-DEMO0001".into(),
    }
}

fn synth_irrigation(t_sim: f64) -> IrrigationSnapshot {
    let now = chrono::Utc::now().timestamp();
    let day_phase = (t_sim / 86400.0) * std::f64::consts::TAU;

    let zones = vec![
        synth_zone(
            "back_yard",
            "Back Yard",
            -12.5 + 4.0 * day_phase.sin(),
            1200,
            now - 18 * 3600,
        ),
        synth_zone(
            "front_yard",
            "Front Yard",
            -8.2 + 3.0 * (day_phase * 1.3).sin(),
            900,
            now - 24 * 3600,
        ),
        synth_zone(
            "side_yard",
            "Side Yard",
            -5.1 + 2.0 * (day_phase * 0.7).sin(),
            600,
            now - 36 * 3600,
        ),
        synth_zone(
            "back_yard_shrubs",
            "Back Yard Shrubs",
            -2.8 + 1.5 * (day_phase * 0.5).sin(),
            1800,
            now - 48 * 3600,
        ),
    ];

    let phase = (t_sim / 86400.0 * 5.0) % 5.0;
    let (verdict, reason) = if phase < 1.0 {
        ("run", String::new())
    } else if phase < 2.0 {
        ("skip", "Rain expected within 4h (0.18\" forecast)".into())
    } else if phase < 3.0 {
        (
            "run_extended",
            "Heat advisory - running planned + 15% (peak 97 F)".into(),
        )
    } else if phase < 4.0 {
        ("skip", "Currently raining (0.05 in/hr)".into())
    } else {
        ("skip", "Tomorrow rain (0.40\" x 85% confidence)".into())
    };

    let mut snap = IrrigationSnapshot::default();
    snap.last_refresh_epoch = now;
    snap.ha_reachable = true;
    snap.master_enable = true;
    snap.iu_enabled = true;
    snap.water_level_pct = 100.0;
    snap.next_run_epoch = now + 6 * 3600;
    snap.next_run_total_minutes = 75.0;
    snap.zones = zones;
    snap.skip_check = SkipCheck {
        temp_now_f: 82.0,
        wind_now_mph: 5.5,
        rain_today_in: 0.0,
        rain_intensity_now_in_hr: if verdict == "skip" && reason.starts_with("Currently") {
            0.05
        } else {
            0.0
        },
        humidity_now_pct: 62.0,
        forecast_in: 0.18,
        rain_tomorrow_prob_pct: 65,
        rain_3day_weighted_in: 0.42,
        rain_7day_weighted_in: 0.95,
        rain_next_4h_in: 0.18,
        wind_max_today_mph: 8.0,
        temp_min_24h_f: 71.0,
        temp_max_3day_f: 97.0,
        days_since_significant_rain: 2,
        heat_index_now_f: 88.0,
        heat_index_max_3day_f: 109.0,
        max_wind_mph: 10.0,
        min_temp_f: 38.0,
        rain_skip_in: 0.25,
        soil_back_yard_pct: Some(42.0),
        soil_front_yard_pct: Some(48.0),
        soil_side_yard_pct: Some(50.0),
        soil_back_yard_shrubs_pct: Some(55.0),
        soil_temp_yard_min_f: Some(74.0),
        soil_temp_yard_max_f: Some(82.0),
        frost_skip_soil_f: 35.0,
        saturation_back_yard_pct: 70.0,
        saturation_front_yard_pct: 70.0,
        saturation_side_yard_pct: 70.0,
        saturation_back_yard_shrubs_pct: 85.0,
        is_paused: false,
        is_dry_run: false,
        will_skip: verdict == "skip",
        verdict: verdict.to_string(),
        reason,
    };
    snap.seven_day_verdicts = synth_seven_day_verdicts(now);
    snap.soil_forecasts = synth_soil_forecasts();
    snap.water_budgets = synth_water_budgets(now);
    snap
}

fn synth_zone(slug: &str, name: &str, bucket_mm: f64, planned_s: u32, last_run: i64) -> ZoneState {
    let mut z = ZoneState::default();
    z.name = name.into();
    z.slug = slug.into();
    z.bucket_mm = bucket_mm;
    z.planned_run_seconds = planned_s;
    z.last_run_epoch = last_run;
    z.math = Some(ZoneMath {
        bucket_mm,
        kc: if slug.contains("shrub") { 0.50 } else { 1.00 },
        throughput_mm_hr: 14.2,
        heat_mult: 1.15,
        capture_eff: 0.70,
        raw_seconds: planned_s + 200,
        max_duration_seconds: 3600,
        scheduled_seconds: planned_s,
        cap_binding: false,
    });
    z
}

fn synth_seven_day_verdicts(now: i64) -> Vec<DayVerdict> {
    let verdicts = [
        ("run", "", 7u32),
        ("skip", "Rain expected (0.40\" x 85%)", 80),
        ("run_extended", "Heat advisory - peak 97 F", 2),
        ("run", "", 1),
        ("skip", "Heavy rain in next 3 days (0.62\" weighted)", 80),
        ("run", "", 2),
        ("run", "", 1),
    ];
    verdicts
        .iter()
        .enumerate()
        .map(|(i, (v, r, w_code))| {
            let mut d = DayVerdict::default();
            d.day_offset = i as u32;
            d.time_epoch = now + (i as i64) * 86400;
            d.weather_code = *w_code;
            d.temp_max_f = 88.0 + (i as f64) * 0.5;
            d.temp_min_f = 71.0 + (i as f64) * 0.2;
            d.precip_in = if v.starts_with("skip") { 0.4 } else { 0.0 };
            d.precip_probability_max = if v.starts_with("skip") { 85 } else { 15 };
            d.verdict = v.to_string();
            d.reason = r.to_string();
            d
        })
        .collect()
}

fn synth_soil_forecasts() -> Vec<SoilForecast> {
    let zones = [
        ("back_yard", "Back Yard", 42.0, 30.0, 70.0),
        ("front_yard", "Front Yard", 48.0, 30.0, 70.0),
        ("side_yard", "Side Yard", 50.0, 30.0, 70.0),
        ("back_yard_shrubs", "Back Yard Shrubs", 55.0, 25.0, 85.0),
    ];
    zones
        .iter()
        .map(|(slug, name, start, tmin, tmax)| {
            let predicted: Vec<f64> = (0..7)
                .map(|d| (start - d as f64 * 4.5).clamp(0.0, 100.0))
                .collect();
            let min_pred = predicted.iter().copied().fold(100.0_f64, f64::min);
            let max_pred = predicted.iter().copied().fold(0.0_f64, f64::max);
            let days_below = predicted.iter().filter(|p| **p <= *tmin).count() as u32;
            let mut s = SoilForecast::default();
            s.zone_slug = slug.to_string();
            s.zone_name = name.to_string();
            s.current_pct = Some(*start);
            s.target_min_pct = *tmin;
            s.target_max_pct = *tmax;
            s.predicted_pct = predicted;
            s.min_predicted_pct = min_pred;
            s.max_predicted_pct = max_pred;
            s.days_below_target = days_below;
            s.days_above_max = 0;
            s.status = if days_below >= 2 {
                "dry".into()
            } else {
                "ok".into()
            };
            s
        })
        .collect()
}

fn synth_water_budgets(now: i64) -> Vec<WaterBudget> {
    let zones = [
        ("back_yard", "Back Yard", true, 1.00, 2u32),
        ("front_yard", "Front Yard", true, 1.00, 2),
        ("side_yard", "Side Yard", false, 1.00, 2),
        ("back_yard_shrubs", "Back Yard Shrubs", false, 0.50, 1),
    ];
    zones
        .iter()
        .map(|(slug, name, mode, budget_in, sessions)| {
            let mut w = WaterBudget::default();
            w.zone_slug = slug.to_string();
            w.zone_name = name.to_string();
            w.mode_active = *mode;
            w.weekly_budget_in = *budget_in;
            w.sessions_per_week = *sessions;
            w.expected_rain_mm = 8.5;
            w.needed_mm = 25.4 * budget_in - 8.5;
            w.mm_per_session = (25.4 * budget_in - 8.5) / (*sessions as f64);
            w.seconds_per_session = 1200;
            w.session_capped = false;
            w.last_run_epoch = now - 18 * 3600;
            w.today_seconds = if *mode { 1200 } else { 0 };
            w.today_reason = if *mode {
                "scheduled session 1 of 2 this week".into()
            } else {
                "budget mode off".into()
            };
            w
        })
        .collect()
}

fn synth_forecast() -> ForecastSnapshot {
    let now = chrono::Utc::now().timestamp();
    let daily: Vec<DailyEntry> = (0..7)
        .map(|d| {
            let mut e = DailyEntry::default();
            e.time_epoch = now + d * 86400;
            e.weather_code = if d == 1 || d == 4 { 80 } else { 2 };
            e.temp_max_f = 88.0 + (d as f64) * 0.3;
            e.temp_min_f = 71.0 + (d as f64) * 0.2;
            e.precip_sum_in = if d == 1 {
                0.4
            } else if d == 4 {
                0.6
            } else {
                0.0
            };
            e.precip_probability_max = if d == 1 {
                85
            } else if d == 4 {
                70
            } else {
                15
            };
            e.wind_max_mph = 8.0;
            e.uv_index_max = 9.0;
            e.sunrise_epoch = now + d * 86400 + 6 * 3600;
            e.sunset_epoch = now + d * 86400 + 19 * 3600;
            e
        })
        .collect();
    let hourly: Vec<HourlyEntry> = (0..48)
        .map(|h| {
            let mut e = HourlyEntry::default();
            e.time_epoch = now + h * 3600;
            e.temp_f = 75.0 + 8.0 * ((h as f64) / 24.0 * std::f64::consts::TAU).sin();
            e.precip_in = if h > 2 && h < 6 { 0.04 } else { 0.0 };
            e.precip_probability = if h > 2 && h < 6 { 75 } else { 10 };
            e.wind_mph = 5.0;
            e.humidity_pct = 60;
            e.weather_code = 2;
            e
        })
        .collect();

    let mut f = ForecastSnapshot::default();
    f.last_refresh_epoch = now;
    f.source_reachable = true;
    f.timezone = "America/New_York".into();
    f.daily = daily;
    f.hourly = hourly;
    f.past_daily = (0..7)
        .map(|d| {
            let mut e = DailyEntry::default();
            e.time_epoch = now - (7 - d) * 86400;
            e.precip_sum_in = if d == 4 { 0.25 } else { 0.0 };
            e.temp_max_f = 86.0;
            e.temp_min_f = 70.0;
            e
        })
        .collect();
    f
}
