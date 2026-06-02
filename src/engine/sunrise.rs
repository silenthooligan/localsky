// NOAA Solar Calculator analytical sunrise + smart-morning target
// computation. Both smart_morning.rs (dispatch decision) and the HA
// refresher (next_run_epoch on the snapshot) need this; extracting
// here keeps the formula single-sourced.
//
// The smart_morning target follows IU's prior anchoring:
//   target_finish = sunrise - 15min   (anchor: finish, sun: sunrise, before: 00:15)
//   target_start  = target_finish - sequence_total_s
// where sequence_total_s = sum(zone.planned_run_seconds) plus 2s
// inter-zone preamble per gap.

use chrono::{Datelike, NaiveDate, TimeZone, Utc};

/// Width of the smart-morning finish offset: target_finish lands 15
/// minutes before sunrise, matching IU's `before: "00:15"` config.
pub const FINISH_BEFORE_SUNRISE_MIN: i64 = 15;

/// NOAA Solar Calculator analytical sunrise. Returns the UTC instant
/// of sunrise for the given local-civil-date at (lat_deg, lon_deg).
/// Uses the standard zenith angle for "official" sunrise (90.833°,
/// accounting for atmospheric refraction). Returns None at polar
/// latitudes where the sun doesn't rise/set on the given day.
pub fn sunrise_utc(date: NaiveDate, lat_deg: f64, lon_deg: f64) -> Option<chrono::DateTime<Utc>> {
    let doy = date.ordinal() as f64;
    let gamma = 2.0 * std::f64::consts::PI / 365.0 * (doy - 1.0);

    let eq_time = 229.18
        * (0.000075 + 0.001868 * gamma.cos()
            - 0.032077 * gamma.sin()
            - 0.014615 * (2.0 * gamma).cos()
            - 0.040849 * (2.0 * gamma).sin());

    let decl = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin()
        - 0.006758 * (2.0 * gamma).cos()
        + 0.000907 * (2.0 * gamma).sin()
        - 0.002697 * (3.0 * gamma).cos()
        + 0.00148 * (3.0 * gamma).sin();

    let lat_rad = lat_deg.to_radians();
    let zenith_rad = 90.833_f64.to_radians();

    let cos_ha = (zenith_rad.cos() - lat_rad.sin() * decl.sin()) / (lat_rad.cos() * decl.cos());
    if !(-1.0..=1.0).contains(&cos_ha) {
        return None;
    }
    let ha_deg = cos_ha.acos().to_degrees();

    let solar_noon_utc_min = 720.0 - 4.0 * lon_deg - eq_time;
    let sunrise_utc_min = solar_noon_utc_min - 4.0 * ha_deg;

    let secs = (sunrise_utc_min * 60.0) as i64;
    let midnight_utc = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?);
    Some(midnight_utc + chrono::Duration::seconds(secs))
}

/// UTC epoch of the smart-morning dispatch start for `date`. Returns
/// None when sunrise doesn't exist on `date` (polar latitudes).
pub fn smart_morning_target_start(
    date: NaiveDate,
    lat: f64,
    lon: f64,
    sequence_total_s: u64,
) -> Option<chrono::DateTime<Utc>> {
    let sunrise = sunrise_utc(date, lat, lon)?;
    let target_finish = sunrise - chrono::Duration::minutes(FINISH_BEFORE_SUNRISE_MIN);
    Some(target_finish - chrono::Duration::seconds(sequence_total_s as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn sunrise_known_date_aperture_labs() {
        // 2026-05-26 sunrise at 30.0738, -81.4716 is ~10:25 UTC per
        // timeanddate.com (NOAA-based). Same regression test as
        // smart_morning's pre-extraction copy.
        let date = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        let sr = sunrise_utc(date, 30.0737788, -81.4715974).expect("sunrise exists");
        let total_min = sr.hour() as i32 * 60 + sr.minute() as i32;
        let expected = 10 * 60 + 25;
        assert!((total_min - expected).abs() <= 3);
    }

    #[test]
    fn target_start_is_finish_minus_sequence() {
        // sunrise 10:25 UTC, sequence 25 min, target_finish = 10:10 UTC,
        // target_start = 09:45 UTC.
        let date = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        let sr = sunrise_utc(date, 30.0737788, -81.4715974).unwrap();
        let target = smart_morning_target_start(date, 30.0737788, -81.4715974, 25 * 60)
            .expect("target exists");
        let delta = (sr - target).num_minutes();
        // 15 min finish-before + 25 min sequence = 40 min.
        assert_eq!(delta, 40);
    }

    #[test]
    fn polar_day_returns_none() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 21).unwrap();
        assert!(sunrise_utc(date, 80.0, 0.0).is_none());
        assert!(smart_morning_target_start(date, 80.0, 0.0, 600).is_none());
    }
}
