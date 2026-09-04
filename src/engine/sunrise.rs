// NOAA Solar Calculator analytical sunrise + smart-morning target
// computation. Both smart_morning.rs (dispatch decision) and the HA
// refresher (next_run_epoch on the snapshot) need this; extracting
// here keeps the formula single-sourced.
//
// The smart_morning target follows IU's prior anchoring:
//   target_finish = sunrise - 15min   (anchor: finish, sun: sunrise, before: 00:15)
//   target_start  = target_finish - sequence_total_s
// where sequence_total_s is the sequence's TRUE wall time from
// scheduler::smart_morning::sequence_wall_seconds (cycle/soak plans laid
// out under the active policy: runs + soak gaps + 2s inter-zone
// preambles), clamped so the start never crosses into the previous
// local day.

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
///
/// Clamped to `date`'s local midnight: a soak-heavy plan whose wall time
/// exceeds the midnight-to-finish span would otherwise anchor its start
/// inside the PREVIOUS local day, where the day-keyed dedupe can never
/// fire it on time and the first after-midnight tick would mislabel the
/// whole sequence a catch-up run. Local midnight is the earliest
/// same-day start; the dispatcher warns when even that cannot finish by
/// the target.
pub fn smart_morning_target_start(
    date: NaiveDate,
    lat: f64,
    lon: f64,
    sequence_total_s: u64,
    cal: crate::engine::calendar::Calendar,
) -> Option<chrono::DateTime<Utc>> {
    let sunrise = sunrise_utc(date, lat, lon)?;
    let target_finish = sunrise - chrono::Duration::minutes(FINISH_BEFORE_SUNRISE_MIN);
    let start = target_finish - chrono::Duration::seconds(sequence_total_s as i64);
    match (cal.day_bounds_utc)(date) {
        Some((day_start, _)) if start < day_start => Some(day_start),
        _ => Some(start),
    }
}

/// Seconds available to the smart-morning sequence on `date`: the span
/// from the (midnight-clamped) target start to target_finish
/// (sunrise - 15min). The dispatcher's overshoot check and the tuning
/// report's raised-cap window test both read this one definition, so the
/// two can never disagree about what fits. None when sunrise does not
/// exist on `date` (polar latitudes).
pub fn smart_morning_available_s(
    date: NaiveDate,
    lat: f64,
    lon: f64,
    sequence_total_s: u64,
    cal: crate::engine::calendar::Calendar,
) -> Option<i64> {
    let sunrise = sunrise_utc(date, lat, lon)?;
    let target_finish = sunrise - chrono::Duration::minutes(FINISH_BEFORE_SUNRISE_MIN);
    let target_start = smart_morning_target_start(date, lat, lon, sequence_total_s, cal)?;
    Some((target_finish - target_start).num_seconds())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn sunrise_known_date_new_york() {
        // 2026-05-26 sunrise at New York City (40.7128, -74.006) is
        // ~09:31 UTC (05:31 EDT) per timeanddate.com (NOAA-based).
        let date = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        let sr = sunrise_utc(date, 40.7128, -74.006).expect("sunrise exists");
        let total_min = sr.hour() as i32 * 60 + sr.minute() as i32;
        let expected = 9 * 60 + 31;
        assert!((total_min - expected).abs() <= 3);
    }

    #[test]
    fn target_start_is_finish_minus_sequence() {
        // Asserts the finish-before + sequence delta, which is
        // independent of the actual sunrise time: 15 min finish-before +
        // 25 min sequence = 40 min before sunrise.
        let date = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        let sr = sunrise_utc(date, 40.7128, -74.006).unwrap();
        let target = smart_morning_target_start(
            date,
            40.7128,
            -74.006,
            25 * 60,
            crate::engine::calendar::Calendar::utc(),
        )
        .expect("target exists");
        let delta = (sr - target).num_minutes();
        // 15 min finish-before + 25 min sequence = 40 min.
        assert_eq!(delta, 40);
    }

    #[test]
    fn target_start_clamps_to_local_midnight() {
        // A 20h "sequence" is longer than any midnight-to-sunrise span, so the
        // unclamped start would land deep in the previous local day; the clamp
        // pins it to the date's own local midnight instead. Asserted via the
        // same calendar the call was given, which is UTC here, so the
        // assertion holds on any machine instead of inheriting the
        // runner's zone.
        let date = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        let target = smart_morning_target_start(
            date,
            40.7128,
            -74.006,
            20 * 3600,
            crate::engine::calendar::Calendar::utc(),
        )
        .expect("target exists");
        let cal = crate::engine::calendar::Calendar::utc();
        let (day_start, _) = (cal.day_bounds_utc)(date).expect("representable day");
        assert_eq!(target, day_start);
        // A plan that fits stays unclamped (the legacy arithmetic).
        let sr = sunrise_utc(date, 40.7128, -74.006).unwrap();
        let fits = smart_morning_target_start(
            date,
            40.7128,
            -74.006,
            25 * 60,
            crate::engine::calendar::Calendar::utc(),
        )
        .unwrap();
        assert_eq!((sr - fits).num_minutes(), 40);
    }

    #[test]
    fn polar_day_returns_none() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 21).unwrap();
        assert!(sunrise_utc(date, 80.0, 0.0).is_none());
        assert!(smart_morning_target_start(
            date,
            80.0,
            0.0,
            600,
            crate::engine::calendar::Calendar::utc()
        )
        .is_none());
        assert!(smart_morning_available_s(
            date,
            80.0,
            0.0,
            600,
            crate::engine::calendar::Calendar::utc()
        )
        .is_none());
    }

    #[test]
    fn available_seconds_match_the_dispatch_window_arithmetic() {
        // A plan that fits: start is unclamped, so the available span equals
        // the sequence itself (start = finish - sequence).
        let date = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        let seq = 25 * 60u64;
        let avail = smart_morning_available_s(
            date,
            40.7128,
            -74.006,
            seq,
            crate::engine::calendar::Calendar::utc(),
        )
        .unwrap();
        assert_eq!(avail, seq as i64, "unclamped start: available == sequence");
        // A 20h plan clamps the start to local midnight, so the available
        // span is midnight..sunrise-15min, strictly less than the sequence:
        // the overshoot condition the dispatcher warns on.
        let long = 20 * 3600u64;
        let avail_long = smart_morning_available_s(
            date,
            40.7128,
            -74.006,
            long,
            crate::engine::calendar::Calendar::utc(),
        )
        .unwrap();
        let sr = sunrise_utc(date, 40.7128, -74.006).unwrap();
        let finish = sr - chrono::Duration::minutes(FINISH_BEFORE_SUNRISE_MIN);
        let cal = crate::engine::calendar::Calendar::utc();
        let (day_start, _) = (cal.day_bounds_utc)(date).expect("representable day");
        assert_eq!(avail_long, (finish - day_start).num_seconds());
        assert!(
            avail_long < long as i64,
            "a 20h plan cannot fit the pre-sunrise span"
        );
    }
}
