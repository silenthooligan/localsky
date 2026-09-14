use super::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::engine::{calendar::Calendar, clock::DayMarker};

#[test]
fn a_future_scenario_checks_forecast_age_at_issue_evaluation_time() {
    let now = 1_788_609_600;
    let future = now + 3 * 86_400;
    let mut fc = ForecastSnapshot {
        last_refresh_epoch: now,
        daily: vec![DailyEntry {
            day_marker: DayMarker::inside_local_day(future),
            precip_sum_in: Some(0.4),
            precip_probability_max: Some(50),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(fc.scenario_rain_in(future, now, Calendar::utc()), Some(0.2));
    assert_eq!(
        fc.planning_precip_weighted_in(24, future),
        None,
        "live dispatch still requires current hourly evidence"
    );
    fc.last_refresh_epoch = now - 2 * 86_400;
    assert_eq!(
        fc.scenario_rain_in(future, now, Calendar::utc()),
        None,
        "a scenario must not revive an already stale forecast"
    );
}

fn day_forecast(start: i64, hours: i64) -> ForecastSnapshot {
    ForecastSnapshot {
        daily: vec![DailyEntry {
            day_marker: DayMarker::inside_local_day(start + 3600),
            et0_in: 0.24,
            et0_reported: true,
            ..Default::default()
        }],
        hourly: (0..hours)
            .map(|i| HourlyEntry {
                time_epoch: start + i * 3600,
                et0_in: 0.01,
                et0_reported: true,
                wind_mph: Some(if i == 12 { 24.0 } else { 0.0 }),
                humidity_pct: Some(60),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn remaining_et_needs_complete_coverage_and_prorates_current_hour() {
    let start = 1_749_945_600;
    let mut fc = day_forecast(start, 24);
    let expected = 12.5 * 0.01 * 25.4;
    assert!(
        (fc.et0_spent_with_evidence(start + 45_000, Calendar::utc())
            .unwrap()
            - expected)
            .abs()
            < 1e-9
    );
    fc.hourly[20].et0_in = 0.0;
    fc.hourly[20].et0_reported = false;
    assert!(fc
        .et0_spent_with_evidence(start + 45_000, Calendar::utc())
        .is_none());
    fc.hourly[20].et0_reported = true;
    assert!(fc
        .et0_spent_with_evidence(start + 45_000, Calendar::utc())
        .is_some());
    fc.hourly.push(fc.hourly[20].clone());
    assert!(fc
        .et0_spent_with_evidence(start + 45_000, Calendar::utc())
        .is_none());
}

#[test]
fn means_need_a_whole_day_and_distinguish_calm_from_absence() {
    let start = 1_749_945_600;
    let mut fc = day_forecast(start, 24);
    fc.backfill_daily_et0_means(Calendar::utc(), 2.0);
    assert!((fc.daily[0].wind_mean_2m_ms.unwrap() - crate::units::mph_to_ms(1.0)).abs() < 1e-9);
    assert_eq!(fc.daily[0].humidity_mean_pct, Some(60.0));
    fc.hourly[12].wind_mph = None;
    fc.backfill_daily_et0_means(Calendar::utc(), 2.0);
    assert_eq!(fc.daily[0].wind_mean_2m_ms, None);
    assert_eq!(fc.daily[0].humidity_mean_pct, Some(60.0));
    fc.hourly[12].wind_mph = Some(0.0);
    fc.backfill_daily_et0_means(Calendar::utc(), 2.0);
    assert_eq!(fc.daily[0].wind_mean_2m_ms, Some(0.0));
    fc.hourly.pop();
    fc.backfill_daily_et0_means(Calendar::utc(), 2.0);
    assert_eq!(fc.daily[0].humidity_mean_pct, None);
}

#[test]
fn legacy_zero_stays_unknown_but_reported_zero_survives_serde() {
    let missing = DailyEntry::default();
    assert_eq!(missing.reference_et0_mm(), None);
    let zero = DailyEntry {
        et0_reported: true,
        ..Default::default()
    };
    let decoded: DailyEntry = serde_json::from_str(&serde_json::to_string(&zero).unwrap()).unwrap();
    assert_eq!(decoded.reference_et0_mm(), Some(0.0));
    let legacy = DailyEntry {
        et0_in: 0.1,
        ..Default::default()
    };
    assert_eq!(legacy.reference_et0_mm(), Some(2.54));
}
