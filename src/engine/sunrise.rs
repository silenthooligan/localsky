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

use chrono::{NaiveDate, TimeZone, Timelike, Utc};

/// Width of the smart-morning finish offset: target_finish lands 15
/// minutes before sunrise, matching IU's `before: "00:15"` config.
pub const FINISH_BEFORE_SUNRISE_MIN: i64 = 15;

/// NOAA Solar Calculator analytical sunrise. Returns the UTC instant
/// of sunrise for the given local-civil-date at (lat_deg, lon_deg).
/// Uses the standard zenith angle for "official" sunrise (90.833°,
/// accounting for atmospheric refraction). Returns None at polar
/// latitudes where the sun doesn't rise/set on the given day.
pub fn sunrise_utc(date: NaiveDate, lat_deg: f64, lon_deg: f64) -> Option<chrono::DateTime<Utc>> {
    solar_event_utc(date, lat_deg, lon_deg, true)
}

/// Sunset uses the opposite hour angle in the same NOAA solar equations.
/// Reference: https://gml.noaa.gov/grad/solcalc/solareqns.PDF
pub fn sunset_utc(date: NaiveDate, lat_deg: f64, lon_deg: f64) -> Option<chrono::DateTime<Utc>> {
    solar_event_utc(date, lat_deg, lon_deg, false)
}

/// Official sunrise/sunset horizon in degrees of sun elevation: the sun's
/// centre 0.833° below the geometric horizon (refraction plus the solar
/// radius), the same zenith `sunrise_utc` and `sunset_utc` solve for.
pub const HORIZON_DEG: f64 = -0.833;

/// Reject corrupt coordinates before any trigonometry. Zero latitude or
/// longitude is valid; the configuration layer handles the unset marker.
pub fn valid_coordinates(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
}

/// NOAA/Meeus equation of time (minutes) and declination (radians).
/// Julian centuries keep the seasons continuous at leap days and year ends.
/// https://gml.noaa.gov/grad/solcalc/main.js
fn solar_terms(epoch: i64) -> (f64, f64) {
    let t = (epoch as f64 / 86_400.0 + 2_440_587.5 - 2_451_545.0) / 36_525.0;
    let l0 = (280.46646 + t * (36_000.769_83 + 0.0003032 * t))
        .rem_euclid(360.0)
        .to_radians();
    let m = (357.52911 + t * (35_999.050_29 - 0.0001537 * t)).to_radians();
    let e = 0.016708634 - t * (0.000042037 + 0.0000001267 * t);
    let centre = m.sin() * (1.914602 - t * (0.004817 + 0.000014 * t))
        + (2.0 * m).sin() * (0.019993 - 0.000101 * t)
        + (3.0 * m).sin() * 0.000289;
    let omega = (125.04 - 1934.136 * t).to_radians();
    let lambda = l0 + (centre - 0.00569 - 0.00478 * omega.sin()).to_radians();
    let seconds = 21.448 - t * (46.815 + t * (0.00059 - t * 0.001813));
    let obliquity = (23.0 + (26.0 + seconds / 60.0) / 60.0 + 0.00256 * omega.cos()).to_radians();
    let y = (obliquity / 2.0).tan().powi(2);
    let equation = y * (2.0 * l0).sin() - 2.0 * e * m.sin()
        + 4.0 * e * y * m.sin() * (2.0 * l0).cos()
        - 0.5 * y * y * (4.0 * l0).sin()
        - 1.25 * e * e * (2.0 * m).sin();
    (
        4.0 * equation.to_degrees(),
        (obliquity.sin() * lambda.sin()).asin(),
    )
}

/// The sun's geometric elevation above the horizon, in degrees, at `epoch`
/// for (lat_deg, lon_deg). The same NOAA terms as sunrise and sunset,
/// evaluated at the instant instead of solved for a zenith. Compare against
/// [`HORIZON_DEG`] for the official day/night boundary.
pub fn solar_elevation_deg(epoch: i64, lat_deg: f64, lon_deg: f64) -> Option<f64> {
    if !valid_coordinates(lat_deg, lon_deg) {
        return None;
    }
    let at = Utc.timestamp_opt(epoch, 0).single()?;
    let hour = at.hour() as f64 + at.minute() as f64 / 60.0 + at.second() as f64 / 3600.0;
    let (eq_time, decl) = solar_terms(epoch);
    let true_solar_min = hour * 60.0 + eq_time + 4.0 * lon_deg;
    let hour_angle = (true_solar_min / 4.0 - 180.0).to_radians();
    let lat = lat_deg.to_radians();
    let cos_zenith = lat.sin() * decl.sin() + lat.cos() * decl.cos() * hour_angle.cos();
    Some(90.0 - cos_zenith.clamp(-1.0, 1.0).acos().to_degrees())
}

/// Clear-sky global horizontal irradiance (W/m²) with the sun
/// `elevation_deg` above the horizon (Haurwitz, 1945). Zero once the sun is
/// down. A measured value divided by this says how much of the available
/// sunlight is getting through, independent of how high the sun is.
pub fn clear_sky_ghi_w_m2(elevation_deg: f64) -> f64 {
    if !elevation_deg.is_finite() || !(0.0..=90.0).contains(&elevation_deg) || elevation_deg == 0.0
    {
        return 0.0;
    }
    let cos_zenith = elevation_deg.to_radians().sin();
    1098.0 * cos_zenith * (-0.059 / cos_zenith).exp()
}

fn solar_event_utc(
    date: NaiveDate,
    lat_deg: f64,
    lon_deg: f64,
    rising: bool,
) -> Option<chrono::DateTime<Utc>> {
    if !valid_coordinates(lat_deg, lon_deg) {
        return None;
    }
    if let Some(at) = solar_event_estimate(date, lat_deg, lon_deg, rising) {
        let before = solar_elevation_deg(at.timestamp().checked_sub(60)?, lat_deg, lon_deg)?;
        let after = solar_elevation_deg(at.timestamp().checked_add(60)?, lat_deg, lon_deg)?;
        if if rising {
            before <= HORIZON_DEG && after >= HORIZON_DEG
        } else {
            before >= HORIZON_DEG && after <= HORIZON_DEG
        } {
            return Some(at);
        }
    }
    // Near a polar transition, noon's declination can imply no event even
    // though the changing declination crosses the horizon later that day.
    // Solve the actual elevation instead of inventing or dropping that event.
    solar_event_bracketed(date, lat_deg, lon_deg, rising)
}

fn solar_event_estimate(
    date: NaiveDate,
    lat_deg: f64,
    lon_deg: f64,
    rising: bool,
) -> Option<chrono::DateTime<Utc>> {
    if !valid_coordinates(lat_deg, lon_deg) {
        return None;
    }
    let midnight = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?);
    let mut event = midnight.checked_add_signed(chrono::Duration::hours(12))?;
    let lat_rad = lat_deg.to_radians();
    let zenith_rad = 90.833_f64.to_radians();
    // Re-evaluate at the event, not UTC noon. This matters near the poles and
    // date line, where the event may fall on a neighbouring UTC date.
    for _ in 0..6 {
        let (eq_time, decl) = solar_terms(event.timestamp());
        let cos_ha = (zenith_rad.cos() - lat_rad.sin() * decl.sin()) / (lat_rad.cos() * decl.cos());
        if !(-1.0..=1.0).contains(&cos_ha) {
            return None;
        }
        let ha_deg = cos_ha.acos().to_degrees();
        let minutes =
            720.0 - 4.0 * lon_deg - eq_time + if rising { -4.0 * ha_deg } else { 4.0 * ha_deg };
        event = midnight
            .checked_add_signed(chrono::Duration::seconds((minutes * 60.0).round() as i64))?;
    }
    Some(event)
}

fn solar_event_bracketed(
    date: NaiveDate,
    lat: f64,
    lon: f64,
    rising: bool,
) -> Option<chrono::DateTime<Utc>> {
    let midnight = date.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    let (equation, _) = solar_terms(midnight.checked_add(43_200)?);
    let noon = midnight.checked_add(((720.0 - 4.0 * lon - equation) * 60.0).round() as i64)?;
    let altitude = |at| solar_elevation_deg(at, lat, lon).map(|v| v - HORIZON_DEG);
    let mut points = Vec::with_capacity(56);
    for step in -26..=26 {
        let at = noon.checked_add(step * 1800)?;
        points.push((at, altitude(at)?));
    }
    // Refine turning points too: a polar day's entire daylight (or darkness)
    // can fit between two half-hour samples. A sign-only scan would miss it.
    let mut extrema = Vec::new();
    for window in points.windows(3) {
        let maximum = window[1].1 > window[0].1 && window[1].1 > window[2].1;
        let minimum = window[1].1 < window[0].1 && window[1].1 < window[2].1;
        if !maximum && !minimum {
            continue;
        }
        let (mut lo, mut hi) = (window[0].0, window[2].0);
        while hi - lo > 3 {
            let a = lo + (hi - lo) / 3;
            let b = hi - (hi - lo) / 3;
            if (altitude(a)? < altitude(b)?) == maximum {
                lo = a;
            } else {
                hi = b;
            }
        }
        let at = lo + (hi - lo) / 2;
        extrema.push((at, altitude(at)?));
    }
    points.extend(extrema);
    points.sort_by_key(|p| p.0);
    let mut crossings = Vec::new();
    for pair in points.windows(2) {
        let crosses = if rising {
            pair[0].1 < 0.0 && pair[1].1 >= 0.0
        } else {
            pair[0].1 > 0.0 && pair[1].1 <= 0.0
        };
        if !crosses {
            continue;
        }
        let (mut lo, mut hi) = (pair[0].0, pair[1].0);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if (altitude(mid)? < 0.0) == rising {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        crossings.push(hi);
    }
    let target = noon.checked_add(if rising { -21_600 } else { 21_600 })?;
    let epoch = crossings.into_iter().min_by_key(|at| at.abs_diff(target))?;
    Utc.timestamp_opt(epoch, 0).single()
}

/// Sunset normalized onto this deployment's civil day, including date-line
/// locations. The caller cannot substitute a process timezone or fixed hour.
pub fn sunset_on_local_day(
    day: crate::engine::clock::CivilDay,
    site: Site,
    cal: crate::engine::calendar::Calendar,
) -> Option<i64> {
    let (lat, lon) = site.location()?;
    solar_event_on_local_day(day, lat, lon, cal, false).map(|at| at.timestamp())
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
    let sunrise = sunrise_on_local_day(
        crate::engine::clock::CivilDay::from_naive(date),
        lat,
        lon,
        cal,
    )?;
    let target_finish = sunrise - chrono::Duration::minutes(FINISH_BEFORE_SUNRISE_MIN);
    let start = target_finish.checked_sub_signed(chrono::Duration::try_seconds(
        i64::try_from(sequence_total_s).ok()?,
    )?)?;
    match cal.day_bounds_datetime(date) {
        Some((day_start, _)) if target_finish <= day_start => None,
        Some((day_start, _)) if start < day_start => Some(day_start),
        _ => Some(start),
    }
}

/// Where the yard is and how long its sequence takes.
///
/// `location` is private and `new` maps (0.0, 0.0) to `None`, so an
/// unconfigured install cannot have its legal gates judged against a
/// fabricated sunrise in the Gulf of Guinea, which is a real place where
/// the sun rises perfectly well.
#[derive(Debug, Clone, Copy, Default)]
pub struct Site {
    location: Option<(f64, f64)>,
    pub sequence_total_s: u64,
}

impl Site {
    pub fn new(location: (f64, f64), sequence_total_s: u64) -> Self {
        Self {
            location: (valid_coordinates(location.0, location.1)
                && (location.0 != 0.0 || location.1 != 0.0))
                .then_some(location),
            sequence_total_s,
        }
    }

    pub fn location(&self) -> Option<(f64, f64)> {
        self.location
    }
}

/// Sunrise for a local civil day, normalized into that day.
///
/// `sunrise_utc` adds its result to UTC midnight of the date, because the
/// formula's output is minutes after UTC noon by construction. For a zone
/// near Greenwich that lands on the intended local day and nothing more
/// is needed. Far from Greenwich it can land on the neighbour: the
/// International Date Line zones (+14, +13, +12:45) are the ones this
/// actually moves.
///
/// Evaluate neighbouring UTC dates and select the event on the requested
/// local day. Shifting an already computed event by 24h would reuse the wrong
/// day's solar declination. Local midnight must not be added to the NOAA
/// formula; that would apply the timezone twice.
pub fn sunrise_on_local_day(
    day: crate::engine::clock::CivilDay,
    lat: f64,
    lon: f64,
    cal: crate::engine::calendar::Calendar,
) -> Option<chrono::DateTime<Utc>> {
    solar_event_on_local_day(day, lat, lon, cal, true)
}

fn solar_event_on_local_day(
    day: crate::engine::clock::CivilDay,
    lat: f64,
    lon: f64,
    cal: crate::engine::calendar::Calendar,
    rising: bool,
) -> Option<chrono::DateTime<Utc>> {
    for date in [
        Some(day.naive()),
        day.naive().pred_opt(),
        day.naive().succ_opt(),
    ]
    .into_iter()
    .flatten()
    {
        let Some(candidate) = solar_event_utc(date, lat, lon, rising) else {
            continue;
        };
        if cal.date_of(candidate.timestamp()) == Some(day) {
            return Some(candidate);
        }
    }
    None
}

/// The span the yard PLANS to be watering on `day`: `(start, finish)`
/// as UTC epochs, finish being the sunrise-minus-fifteen target.
///
/// A forecast gate that asks "will it be windy?" has to ask it about
/// these minutes and not about the whole day. The daily peak wind is a
/// figure for the afternoon; a yard that waters before dawn is in a
/// different atmosphere.
///
/// `None` when there is no location, or no
/// sunrise on this date.
pub fn planned_window(
    day: crate::engine::clock::CivilDay,
    site: Site,
    cal: crate::engine::calendar::Calendar,
) -> Option<(i64, i64)> {
    let (lat, lon) = site.location()?;
    let sunrise = sunrise_on_local_day(day, lat, lon, cal)?;
    let target_finish = sunrise - chrono::Duration::minutes(FINISH_BEFORE_SUNRISE_MIN);
    let start = smart_morning_target_start(day.naive(), lat, lon, site.sequence_total_s, cal)?;
    // Clamped to the day's own start, for the same reason the dispatcher
    // clamps: a sequence long enough to reach back past midnight would
    // otherwise be planned inside the PREVIOUS day.
    let start = match cal.day_start(day).instant() {
        Some(day_start) if start.timestamp() < day_start => day_start,
        _ => start.timestamp(),
    };
    Some((start, target_finish.timestamp()))
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
    let sunrise = sunrise_on_local_day(
        crate::engine::clock::CivilDay::from_naive(date),
        lat,
        lon,
        cal,
    )?;
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
        let (day_start, _) = cal.day_bounds_datetime(date).expect("representable day");
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
        let (day_start, _) = cal.day_bounds_datetime(date).expect("representable day");
        assert_eq!(avail_long, (finish - day_start).num_seconds());
        assert!(
            avail_long < long as i64,
            "a 20h plan cannot fit the pre-sunrise span"
        );
    }

    #[test]
    fn solar_elevation_agrees_with_sunrise_and_a_real_morning() {
        // A Florida site (the demo's) on the morning an overcast 07:30 read
        // as night, 2026-10-06: sunrise 07:20 EDT (11:20 UTC).
        let (lat, lon) = (28.54, -81.38);
        let date = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        for event in [sunrise_utc(date, lat, lon), sunset_utc(date, lat, lon)] {
            let at = solar_elevation_deg(event.unwrap().timestamp(), lat, lon).unwrap();
            assert!((at - HORIZON_DEG).abs() < 0.3, "{at}");
        }
        let half_past_seven = Utc
            .with_ymd_and_hms(2026, 10, 6, 11, 30, 0)
            .unwrap()
            .timestamp();
        let el = solar_elevation_deg(half_past_seven, lat, lon).unwrap();
        assert!((0.5..1.6).contains(&el), "{el}");
        let eight = solar_elevation_deg(half_past_seven + 1800, lat, lon).unwrap();
        assert!((6.5..8.5).contains(&eight), "{eight}");
        assert!(solar_elevation_deg(half_past_seven - 3 * 3600, lat, lon).unwrap() < -20.0);
    }

    #[test]
    fn clear_sky_irradiance_follows_sun_height() {
        assert_eq!(clear_sky_ghi_w_m2(-3.0), 0.0);
        assert_eq!(clear_sky_ghi_w_m2(0.0), 0.0);
        let low = clear_sky_ghi_w_m2(7.0);
        let high = clear_sky_ghi_w_m2(55.0);
        assert!((70.0..110.0).contains(&low), "{low}");
        assert!((800.0..870.0).contains(&high), "{high}");
    }

    #[test]
    fn date_line_and_fractional_zones_share_one_planning_window() {
        use crate::engine::{calendar::Calendar, clock::CivilDay};
        // Kiritimati, Auckland, Chatham, Kathmandu, Adelaide, Pago Pago.
        for (lat, lon, offset) in [
            (1.87, -157.43, 14 * 3600),
            (-36.85, 174.76, 13 * 3600),
            (-43.95, -176.56, 13 * 3600 + 45 * 60),
            (27.72, 85.32, 5 * 3600 + 45 * 60),
            (-34.93, 138.60, 10 * 3600 + 30 * 60),
            (-14.28, -170.70, -11 * 3600),
        ] {
            let cal = Calendar::fixed_offset(offset).unwrap();
            for date in [
                NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(),
                NaiveDate::from_ymd_opt(2028, 2, 29).unwrap(),
            ] {
                let day = CivilDay::from_naive(date);
                let rise = sunrise_on_local_day(day, lat, lon, cal).unwrap();
                let start = smart_morning_target_start(date, lat, lon, 1200, cal).unwrap();
                let plan = planned_window(day, Site::new((lat, lon), 1200), cal).unwrap();
                assert_eq!(cal.date_of(rise.timestamp()), Some(day));
                assert_eq!(cal.date_of(start.timestamp()), Some(day));
                assert_eq!(plan, (start.timestamp(), rise.timestamp() - 900));
                assert_eq!(
                    smart_morning_available_s(date, lat, lon, 1200, cal),
                    Some(1200)
                );
                assert!(
                    (solar_elevation_deg(rise.timestamp(), lat, lon).unwrap() - HORIZON_DEG).abs()
                        < 0.05
                );
                let set = sunset_on_local_day(day, Site::new((lat, lon), 0), cal).unwrap();
                assert_eq!(cal.date_of(set), Some(day));
                assert!((solar_elevation_deg(set, lat, lon).unwrap() - HORIZON_DEG).abs() < 0.05);
            }
        }
    }

    #[test]
    fn noaa_position_matches_the_nrel_spa_reference_example() {
        // Reda & Andreas, NREL/TP-560-34302: 2003-10-17 12:30:30 MST,
        // latitude 39.742476, longitude -105.1786. Geometric zenith ~50.128°;
        // apparent zenith 50.11162° includes atmospheric refraction.
        let at = Utc
            .with_ymd_and_hms(2003, 10, 17, 19, 30, 30)
            .unwrap()
            .timestamp();
        let elevation = solar_elevation_deg(at, 39.742476, -105.1786).unwrap();
        assert!((elevation - (90.0 - 50.128)).abs() < 0.05, "{elevation}");
    }

    #[test]
    fn southern_sunrise_and_sunset_match_independent_spa_fixtures() {
        // Published NREL SPA fixtures in pvlib's test_solarposition.py,
        // latitude -35°, longitude 0°. Allow 90s for the NOAA approximation.
        for (date, rise, set) in [
            ("1996-07-05", "07:08:15", "17:01:04"),
            ("2004-12-04", "04:38:57", "19:02:03"),
        ] {
            let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
            for (event, expected) in [
                (sunrise_utc(day, -35.0, 0.0), rise),
                (sunset_utc(day, -35.0, 0.0), set),
            ] {
                let expected = chrono::DateTime::parse_from_rfc3339(&format!("{date}T{expected}Z"))
                    .unwrap()
                    .timestamp();
                assert!((event.unwrap().timestamp() - expected).abs() < 90);
            }
        }
    }

    #[test]
    fn short_polar_transition_days_keep_real_horizon_crossings() {
        // These days have a real event even when UTC noon's declination
        // makes the analytic hour angle undefined. Cover both hemispheres,
        // both sides of the date line, and very short daylight/darkness.
        for (lat, lon, ordinal, rising) in [
            (70.0, -180.0, 16, true),
            (70.0, -180.0, 16, false),
            (70.0, 0.0, 136, true),
            (66.5, 180.0, 157, true),
            (-80.0, 180.0, 108, true),
            (-80.0, 180.0, 108, false),
            (-89.0, 180.0, 84, true),
            (-89.0, 180.0, 84, false),
        ] {
            let date = NaiveDate::from_yo_opt(2026, ordinal).unwrap();
            let at = solar_event_utc(date, lat, lon, rising).unwrap().timestamp();
            let before = solar_elevation_deg(at - 30, lat, lon).unwrap();
            let after = solar_elevation_deg(at + 30, lat, lon).unwrap();
            assert!(
                if rising {
                    before < HORIZON_DEG && after > HORIZON_DEG
                } else {
                    before > HORIZON_DEG && after < HORIZON_DEG
                },
                "{date} {lat} {lon} rising={rising}: {before} -> {after}"
            );
        }
    }

    #[test]
    fn invalid_coordinates_and_extreme_durations_cannot_make_solar_events() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 21).unwrap();
        for (lat, lon) in [
            (91.0, 0.0),
            (0.0, 181.0),
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
        ] {
            assert_eq!(solar_elevation_deg(1_700_000_000, lat, lon), None);
            assert_eq!(sunrise_utc(date, lat, lon), None);
            assert_eq!(Site::new((lat, lon), 0).location(), None);
        }
        assert_eq!(
            smart_morning_target_start(
                date,
                40.0,
                -74.0,
                u64::MAX,
                crate::engine::calendar::Calendar::utc()
            ),
            None
        );
    }
}
