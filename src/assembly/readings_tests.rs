#![allow(unused_imports)]
use crate::assembly::*;
use crate::controllers::registry::ControllerRegistry;
use crate::engine::scripting::CompiledScripts;
use crate::engine::sizing::*;
use crate::engine::skip_rules::{self as skip_logic, et_heat_multiplier, Inputs};
use crate::engine::skip_rules::{LiveReadings, ZoneSoil};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::forecast::ForecastStore;
use crate::history::IngestState;
use crate::integrations::home_assistant::rest::HaClient;
use crate::model::{DayVerdict, IrrigationSnapshot, RuleEval, SoilForecast, WaterBudget};
use crate::refresher::evidence::*;
use crate::refresher::policy::*;
use crate::refresher::shell::*;
use crate::refresher::store::IrrigationStore;
use crate::refresher::*;
use crate::tempest::state::TempestStore;
use arc_swap::ArcSwap;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[cfg(test)]
mod unprobed_soil_zone_tests {
    use super::*;

    fn zones(slugs: &[&str]) -> Vec<crate::zones::ZoneIdent> {
        crate::zones::from_pairs(slugs.iter().map(|s| (*s, *s)))
    }

    /// Every active zone gets an entry, because the entry is what earns
    /// the zone a per-zone verdict; none of them reads a probe.
    #[test]
    fn every_active_zone_gets_an_unprobed_entry() {
        let out = unprobed_soil_zones(&zones(&["orchard", "herb_bed", "back_yard"]));
        let slugs: Vec<&str> = out.iter().map(|z| z.slug.as_str()).collect();
        assert_eq!(slugs, vec!["orchard", "herb_bed", "back_yard"]);
        assert!(out.iter().all(|z| z.pct.is_none()));
        assert_eq!(out[1].saturation_pct, 85.0, "bed default");
        assert_eq!(out[2].saturation_pct, 70.0, "turf default, no helper read");
    }
}

#[cfg(test)]
mod et0_resolution_tests {
    use super::*;
    use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot};

    /// Two-day forecast whose entries carry usable temps plus (optionally) the
    /// provider's own daily ET0 in inches.
    fn fc_with(et0_in: f64) -> ForecastSnapshot {
        let day = |max: f64, min: f64| DailyEntry {
            temp_max_f: Some(max),
            temp_min_f: Some(min),
            et0_in,
            ..Default::default()
        };
        ForecastSnapshot {
            daily: vec![day(90.0, 65.0), day(88.0, 64.0)],
            ..Default::default()
        }
    }

    #[test]
    fn et0_today_honors_the_mapped_bus_value_then_full_day_forecast_ranks() {
        // 1. A source-reported/mapped bus value owns the figure outright: the
        //    field's contract is FULL-DAY mm (the OM fill emits the converted
        //    daily total; an explicit HA/MQTT mapping is honored as mapped).
        let got = resolve_et0_today_mm(4.2, &fc_with(0.18), 40.0, 180, 0.0).expect("bus rung");
        assert!((got - 4.2).abs() < 1e-9, "bus value wins: {got}");
        // 2. No bus value -> the provider's full-day daily[0] ET0
        //    (0.18 in -> 4.572 mm). This is the rank whose UNITS issue #4
        //    broke: the OM fill emitted raw inches onto the mm bus.
        let got = resolve_et0_today_mm(0.0, &fc_with(0.18), 40.0, 180, 0.0).expect("provider rung");
        assert!((got - 4.572).abs() < 1e-9, "provider daily next: {got}");
        // 3. No provider ET0 -> native Hargreaves (a real value).
        let native = resolve_et0_today_mm(0.0, &fc_with(0.0), 40.0, 180, 0.0).expect("native rung");
        assert!(native > 0.0, "native = {native}");
        // 4. Nothing anywhere -> None. The published eto_today_mm stays null
        //    (HA sensor unknown, dashboard dash); only the advisory soil
        //    projection opts into ENGINE_ET0_FALLBACK_MM, explicitly.
        let empty = ForecastSnapshot::default();
        assert_eq!(resolve_et0_today_mm(0.0, &empty, 40.0, 180, 0.0), None);
        // native_et0_mm returns None on a temps-absent day.
        assert!(native_et0_mm(&DailyEntry::default(), 40.0, 180, 0.0).is_none());
    }

    #[test]
    fn today_range_resolves_from_the_forecast_or_not_at_all() {
        let (tmax, tmin, hum) = resolve_today_range(&fc_with(0.0));
        assert_eq!(tmax, Some(90.0));
        assert_eq!(tmin, Some(65.0));
        // fc_with's days carry no humidity (humidity_pct 0 = no coverage).
        assert_eq!(hum, None);

        // Nothing: None, never a fabricated 0°F/0°F range or 0% humidity.
        let (tmax, tmin, hum) = resolve_today_range(&ForecastSnapshot::default());
        assert_eq!((tmax, tmin, hum), (None, None, None));
    }

    #[test]
    fn partial_forecast_extremes_stay_independently_unknown_and_cannot_size_et0() {
        let mut fc = fc_with(0.0);
        fc.daily[0].temp_max_f = None;
        assert_eq!(resolve_today_range(&fc), (None, Some(65.0), None));
        assert!(native_et0_mm(&fc.daily[0], 40.0, 180, 0.0).is_none());
        fc.daily[0].temp_max_f = Some(0.0);
        fc.daily[0].temp_min_f = Some(-10.0);
        assert_eq!(resolve_today_range(&fc), (Some(0.0), Some(-10.0), None));
        fc.daily[0].temp_min_f = None;
        assert_eq!(resolve_today_range(&fc), (Some(0.0), None, None));
        assert!(native_et0_mm(&fc.daily[0], 40.0, 180, 0.0).is_none());
        // An old malformed range is not usable agronomic evidence either.
        fc.daily[0].temp_min_f = Some(75.0);
        assert!(native_et0_mm(&fc.daily[0], 40.0, 180, 0.0).is_none());
    }

    #[test]
    fn forecast_day_et0_mirrors_todays_forecast_ranks() {
        // Provider daily[1] ET0 (inches -> mm) ranks first, mirroring the
        // order of resolve_et0_today_mm, so "ET Tomorrow" agrees with "ET
        // Today" in method and units and the pair can no longer disagree
        // 25x across midnight.
        let got = forecast_day_et0_mm(
            &fc_with(0.18),
            1,
            40.0,
            chrono::NaiveDate::from_yo_opt(2026, 180).unwrap(),
            0.0,
            0.0,
        );
        assert!((got - 4.572).abs() < 1e-9, "provider daily wins: {got}");
        // No provider ET0 -> native compute from the day's temps.
        let native = forecast_day_et0_mm(
            &fc_with(0.0),
            1,
            40.0,
            chrono::NaiveDate::from_yo_opt(2026, 180).unwrap(),
            0.0,
            0.0,
        );
        assert!(native > 0.0, "native = {native}");
        // Day outside the forecast window -> the caller's fallback.
        let fb = forecast_day_et0_mm(
            &ForecastSnapshot::default(),
            1,
            40.0,
            chrono::NaiveDate::from_yo_opt(2026, 180).unwrap(),
            1.5,
            0.0,
        );
        assert!((fb - 1.5).abs() < 1e-9);
    }

    /// The model's own reading of today's rain, by calendar day.
    #[test]
    fn todays_rain_reads_from_the_forecast_row_for_today() {
        let mut fc = fc_with(0.0);
        fc.daily[0].precip_sum_in = Some(0.42);
        assert!((fc.today_precip_in().unwrap() - 0.42).abs() < 1e-9);
        assert_eq!(ForecastSnapshot::default().today_precip_in(), None);
    }
}

#[cfg(test)]
mod current_conditions_tests {
    use super::{forecast_is_stale, LiveReadings, FORECAST_MAX_AGE_S};
    use crate::forecast::snapshot::HourlyEntry;
    use crate::tempest::state::Snapshot as TempestSnapshot;

    use crate::weather::arbitration::LIVE_FRESHNESS_SECS;
    const AGES: [i64; 3] = [LIVE_FRESHNESS_SECS; 3];
    const NOW: i64 = 1_700_000_000;
    fn resolve_current_conditions(
        snapshot: &TempestSnapshot,
        hour: Option<&HourlyEntry>,
        now: i64,
        ages: [i64; 3],
    ) -> (f64, f64, f64, LiveReadings) {
        super::resolve_current_conditions(&super::test_current_samples(snapshot, ages), hour, now)
    }

    fn tempest(last_packet_epoch: i64) -> TempestSnapshot {
        TempestSnapshot {
            last_packet_epoch,
            // A full live station owns all engine-critical fields at this epoch.
            air_temp_live_epoch: last_packet_epoch,
            wind_live_epoch: last_packet_epoch,
            rh_live_epoch: last_packet_epoch,
            air_temp_f: 61.5,
            wind_avg_mph: 4.2,
            rh_pct: 71.0,
            ..Default::default()
        }
    }

    fn hour() -> HourlyEntry {
        HourlyEntry {
            temp_f: Some(55.0),
            wind_mph: Some(7.5),
            humidity_pct: Some(64),
            ..Default::default()
        }
    }

    #[test]
    fn fresh_station_drives_live_inputs() {
        let t = tempest(NOW - 90);
        let h = hour();
        let (temp, wind, rh, src) = resolve_current_conditions(&t, Some(&h), NOW, AGES);
        assert_eq!(src, LiveReadings::Station);
        assert_eq!(temp, 61.5);
        assert_eq!(wind, 4.2);
        assert_eq!(rh, 71.0);
    }

    #[test]
    fn absent_forecast_temperature_cannot_authorize_current_conditions() {
        let mut h = hour();
        h.temp_f = None;
        assert_eq!(
            resolve_current_conditions(&tempest(0), Some(&h), NOW, AGES).3,
            LiveReadings::Unavailable
        );
        // Future timestamps cannot turn missing forecast data into station proof.
        assert_eq!(
            resolve_current_conditions(&tempest(NOW + 60), Some(&h), NOW, AGES).3,
            LiveReadings::Unavailable
        );
        h.temp_f = Some(0.0);
        let (temp, _, _, source) = resolve_current_conditions(&tempest(0), Some(&h), NOW, AGES);
        assert_eq!(temp, 0.0);
        assert_eq!(source, LiveReadings::ForecastFallback);
        h.temp_f = Some(f64::NAN);
        assert_eq!(
            resolve_current_conditions(&tempest(0), Some(&h), NOW, AGES).3,
            LiveReadings::Unavailable
        );
    }

    #[test]
    fn missing_wind_or_humidity_is_not_calm_or_dry_but_reported_zero_is_known() {
        let mut h = hour();
        h.wind_mph = None;
        assert_eq!(
            resolve_current_conditions(&tempest(0), Some(&h), NOW, AGES).3,
            LiveReadings::Unavailable
        );
        h.wind_mph = Some(0.0);
        h.humidity_pct = None;
        assert_eq!(
            resolve_current_conditions(&tempest(0), Some(&h), NOW, AGES).3,
            LiveReadings::Unavailable
        );
        h.humidity_pct = Some(0);
        let (_, wind, humidity, source) =
            resolve_current_conditions(&tempest(0), Some(&h), NOW, AGES);
        assert_eq!((wind, humidity), (0.0, 0.0));
        assert_eq!(source, LiveReadings::ForecastFallback);
        h.humidity_pct = Some(101);
        assert_eq!(
            resolve_current_conditions(&tempest(0), Some(&h), NOW, AGES).3,
            LiveReadings::Unavailable
        );
    }

    #[test]
    fn partial_live_station_does_not_force_station_readings() {
        // The latent HIGH: a barometer-only live source keeps last_packet_epoch
        // fresh but provides no live air_temp/wind/rh. The engine must fall back
        // to the forecast for those fields PER FIELD, never treating the
        // forecast-filled / zero snapshot values as a live station reading.
        let mut t = tempest(NOW - 90);
        t.air_temp_live_epoch = 0;
        t.wind_live_epoch = 0;
        t.rh_live_epoch = 0;
        t.air_temp_f = 0.0;
        t.wind_avg_mph = 0.0;
        let h = hour();
        let (temp, wind, rh, src) = resolve_current_conditions(&t, Some(&h), NOW, AGES);
        assert_eq!(
            src,
            LiveReadings::ForecastFallback,
            "partial station != Station"
        );
        assert_eq!(temp, 55.0, "forecast temp, not the 0 snapshot value");
        assert_eq!(wind, 7.5);
        assert_eq!(rh, 64.0);
    }

    #[test]
    fn stale_station_falls_back_to_current_hour_forecast() {
        // Packet seen, but older than the recency window: the old
        // "ever-seen" check (last_packet_epoch > 0) would have kept the
        // dead station's readings live forever.
        let t = tempest(NOW - LIVE_FRESHNESS_SECS - 1);
        let h = hour();
        let (temp, wind, rh, src) = resolve_current_conditions(&t, Some(&h), NOW, AGES);
        assert_eq!(src, LiveReadings::ForecastFallback);
        assert_eq!(temp, 55.0);
        assert_eq!(wind, 7.5);
        assert_eq!(rh, 64.0);
    }

    #[test]
    fn never_seen_station_with_forecast_is_fallback() {
        let t = tempest(0);
        let h = hour();
        let (_, _, _, src) = resolve_current_conditions(&t, Some(&h), NOW, AGES);
        assert_eq!(src, LiveReadings::ForecastFallback);
    }

    #[test]
    fn no_station_and_no_forecast_is_unavailable() {
        let t = tempest(0);
        let (temp, wind, _, src) = resolve_current_conditions(&t, None, NOW, AGES);
        assert_eq!(src, LiveReadings::Unavailable);
        // Neutral zeros, never the old fabricated 70 °F.
        assert_eq!(temp, 0.0);
        assert_eq!(wind, 0.0);
    }

    #[test]
    fn configured_age_boundary_matches_arbitration() {
        let t = tempest(NOW - LIVE_FRESHNESS_SECS);
        let (_, _, _, src) = resolve_current_conditions(&t, None, NOW, AGES);
        assert_eq!(src, LiveReadings::Station);
        assert_eq!(
            resolve_current_conditions(&t, None, NOW + 1, AGES).3,
            LiveReadings::Unavailable
        );
    }

    #[test]
    fn configured_short_age_expires_only_its_field_and_uses_forecast_or_hold() {
        let t = tempest(NOW - 61);
        let h = hour();
        let (temp, wind, rh, src) = resolve_current_conditions(&t, Some(&h), NOW, [60, 600, 600]);
        assert_eq!((temp, wind, rh), (55.0, 4.2, 71.0));
        assert_eq!(src, LiveReadings::ForecastFallback);
        assert_eq!(
            resolve_current_conditions(&t, None, NOW, [60, 600, 600]).3,
            LiveReadings::Unavailable
        );
    }

    #[test]
    fn configured_long_age_is_not_overridden_by_a_tempest_timer() {
        let t = tempest(NOW - 900);
        assert_eq!(
            resolve_current_conditions(&t, None, NOW, [1200; 3]).3,
            LiveReadings::Station
        );
    }

    // Pin the forecast-staleness threshold at the assembly seam. A
    // stale forecast both gates the forward-looking rain SKIP rules and marks the
    // decision degraded, so the boundary behavior is safety-relevant.
    #[test]
    fn fresh_forecast_is_not_stale() {
        assert!(!forecast_is_stale(1_000, 1_000 + 3_600)); // 1h old
    }

    #[test]
    fn forecast_just_past_max_age_is_stale() {
        assert!(forecast_is_stale(1_000, 1_000 + FORECAST_MAX_AGE_S + 1));
    }

    #[test]
    fn forecast_exactly_at_max_age_is_still_fresh() {
        // `>` is strict: an age exactly at the bound is usable, not stale.
        assert!(!forecast_is_stale(1_000, 1_000 + FORECAST_MAX_AGE_S));
    }

    #[test]
    fn never_refreshed_forecast_is_stale() {
        // The "never refreshed" sentinel must fail safe regardless of `now`,
        // including a zero/negative clock that would make the age subtraction
        // misbehave without the explicit epoch <= 0 guard.
        assert!(forecast_is_stale(0, 99_999));
        assert!(forecast_is_stale(-1, 99_999));
        assert!(forecast_is_stale(0, 0));
        assert!(
            forecast_is_stale(100_001, 100_000),
            "future timestamps are not fresh evidence"
        );
    }
}

#[cfg(test)]
mod scientific_input_tests {
    use super::*;
    use crate::forecast::snapshot::DailyEntry;
    #[test]
    fn daily_peaks_are_not_penman_monteith_means() {
        let mut day = DailyEntry {
            temp_max_f: Some(86.0),
            temp_min_f: Some(64.4),
            wind_max_mph: Some(45.0),
            humidity_pct: Some(30),
            solar_rad_mj_m2_day: 22.0,
            ..Default::default()
        };
        let fallback = native_et0_mm(&day, 35.0, 180, 500.0).unwrap();
        day.wind_max_mph = Some(2.0);
        day.humidity_pct = Some(90);
        assert_eq!(native_et0_mm(&day, 35.0, 180, 500.0), Some(fallback));
        day.wind_mean_2m_ms = Some(1.5);
        day.humidity_mean_pct = Some(65.0);
        let mean_based = native_et0_mm(&day, 35.0, 180, 500.0).unwrap();
        assert!((mean_based - fallback).abs() > 0.1);
    }
    #[test]
    fn reported_zero_and_polar_night_do_not_invent_summer_evaporation() {
        let mut day = DailyEntry {
            et0_reported: true,
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            daily: vec![day.clone()],
            ..Default::default()
        };
        assert_eq!(resolve_et0_today_mm(0.0, &fc, 80.0, 355, 0.0), Some(0.0));
        day.et0_reported = false;
        day.temp_max_f = Some(5.0);
        day.temp_min_f = Some(-4.0);
        assert_eq!(native_et0_mm(&day, 80.0, 355, 0.0), Some(0.0));
        assert!(native_et0_mm(&DailyEntry::default(), 80.0, 355, 0.0).is_none());
    }
    #[test]
    fn forecast_ordinal_wraps_using_the_actual_year() {
        let day = DailyEntry {
            temp_max_f: Some(80.0),
            temp_min_f: Some(60.0),
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            daily: vec![day.clone(); 4],
            ..Default::default()
        };
        for year in [2024, 2025] {
            let date = chrono::NaiveDate::from_ymd_opt(year, 12, 31).unwrap();
            let actual = forecast_day_et0_mm(&fc, 3, -35.0, date, -1.0, 100.0);
            assert_eq!(Some(actual), native_et0_mm(&day, -35.0, 3, 100.0));
        }
    }
}
