// Jurisdictional watering-restriction evaluator. Pure functions over
// chrono::DateTime<Local> + the schema types, no I/O, no globals, so
// the rule logic is straightforward to unit-test against synthetic
// datetimes (DST/EST × Odd/Even × in/out of forbidden hours).
//
// The aggregator `evaluate` is the only thing skip_rules cares about:
// it ANDs every enabled, in-effective-window restriction and produces
// a `RestrictionVerdict` with the first matching skip reason plus the
// min-of-active per-zone duration cap.

use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Weekday};

use crate::config::schema::{AddressParity, EffectiveWindow, WateringRestriction};

/// Aggregated result over all enabled restrictions. `skip == true` means
/// at least one restriction is currently blocking irrigation; `reason`
/// carries the first matched restriction's name + cause. `max_minutes_cap`
/// is the tightest per-zone duration cap across all active restrictions
/// (independent of whether any of them produced a skip).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestrictionVerdict {
    pub skip: bool,
    pub reason: Option<String>,
    pub max_minutes_cap: Option<u32>,
}

/// True when `now`'s date falls inside `w`'s effective window. DST
/// rules follow US convention: 2nd Sunday of March → 1st Sunday of
/// November.
pub fn is_in_effective_window(now: DateTime<Local>, w: &EffectiveWindow) -> bool {
    let date = now.date_naive();
    match w {
        EffectiveWindow::AllYear => true,
        EffectiveWindow::DstOnly => match (
            nth_weekday_of_month(date.year(), 3, Weekday::Sun, 2),
            nth_weekday_of_month(date.year(), 11, Weekday::Sun, 1),
        ) {
            (Some(spring), Some(fall)) => date >= spring && date < fall,
            _ => false,
        },
        EffectiveWindow::StandardOnly => match (
            nth_weekday_of_month(date.year(), 3, Weekday::Sun, 2),
            nth_weekday_of_month(date.year(), 11, Weekday::Sun, 1),
        ) {
            (Some(spring), Some(fall)) => date < spring || date >= fall,
            _ => true,
        },
        EffectiveWindow::DateRange {
            start_month,
            start_day,
            end_month,
            end_day,
        } => {
            let yr = date.year();
            let start = ymd_clamped_to_month_end(yr, *start_month as u32, *start_day as u32);
            let end = ymd_clamped_to_month_end(yr, *end_month as u32, *end_day as u32);
            match (start, end) {
                (Some(s), Some(e)) if s <= e => date >= s && date <= e,
                // Wrap-around (e.g. Nov 15 → Feb 28): inside if before
                // end OR after start within the same calendar year.
                (Some(s), Some(e)) => date >= s || date <= e,
                _ => false,
            }
        }
    }
}

/// True when today's weekday is on the operator's allowed list for
/// their address parity. Empty allowed list = no restriction. When the
/// operator's parity is `NotApplicable`, weekday gates are no-ops (the
/// restriction can't decide either way).
pub fn allowed_today(now: DateTime<Local>, r: &WateringRestriction, parity: AddressParity) -> bool {
    let today_dow = now.weekday().num_days_from_sunday() as u8;
    let allowed = match parity {
        AddressParity::Odd => &r.allowed_weekdays_odd,
        AddressParity::Even => &r.allowed_weekdays_even,
        AddressParity::NotApplicable => return true,
    };
    if allowed.is_empty() {
        return true;
    }
    allowed.contains(&today_dow)
}

/// True when `now`'s hour falls in `[forbidden_hour_start, forbidden_hour_end)`.
/// Supports wrap-around (e.g. start=22, end=6). When either bound is
/// `None`, this gate is inactive.
pub fn in_forbidden_hours(now: DateTime<Local>, r: &WateringRestriction) -> bool {
    let (start, end) = match (r.forbidden_hour_start, r.forbidden_hour_end) {
        (Some(s), Some(e)) => (s, e),
        _ => return false,
    };
    let h = now.hour() as u8;
    if start <= end {
        h >= start && h < end
    } else {
        // Wrap: e.g. forbidden 22 .. 6 means 22, 23, 0, 1, 2, 3, 4, 5.
        h >= start || h < end
    }
}

/// Aggregate over every restriction in `restrictions`. First triggering
/// restriction supplies the skip reason; caps accumulate as the min
/// across every active restriction's `max_minutes_per_zone`.
pub fn evaluate(
    now: DateTime<Local>,
    restrictions: &[WateringRestriction],
    parity: AddressParity,
) -> RestrictionVerdict {
    let mut verdict = RestrictionVerdict::default();

    for r in restrictions {
        if !r.enabled {
            continue;
        }
        if !is_in_effective_window(now, &r.effective) {
            continue;
        }

        let bad_day = !allowed_today(now, r, parity);
        let bad_hour = in_forbidden_hours(now, r);

        if (bad_day || bad_hour) && verdict.reason.is_none() {
            verdict.skip = true;
            verdict.reason = Some(format_reason(r, bad_day, bad_hour));
        }

        if let Some(cap) = r.max_minutes_per_zone {
            verdict.max_minutes_cap = Some(match verdict.max_minutes_cap {
                Some(prev) => prev.min(cap),
                None => cap,
            });
        }
    }

    verdict
}

fn format_reason(r: &WateringRestriction, bad_day: bool, bad_hour: bool) -> String {
    if bad_day && bad_hour {
        format!(
            "Watering restriction ({}): not an allowed day and inside the forbidden hours",
            r.name
        )
    } else if bad_day {
        format!(
            "Watering restriction ({}): today is not an allowed watering day",
            r.name
        )
    } else {
        let s = r.forbidden_hour_start.unwrap_or(0);
        let e = r.forbidden_hour_end.unwrap_or(0);
        format!(
            "Watering restriction ({}): currently inside the forbidden window ({s:02}:00 – {e:02}:00)",
            r.name
        )
    }
}

/// Build a `NaiveDate`, clamping a day that overruns the month to the
/// month's last valid day (Feb 30 → Feb 28/29, Jun 31 → Jun 30). User
/// intent for a DateRange bound like "Feb 30" is "end of February";
/// without the clamp the date fails to construct and the restriction
/// silently never applies. An invalid month still yields `None`.
fn ymd_clamped_to_month_end(year: i32, month: u32, day: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(year, month, day).or_else(|| {
        let last = last_day_of_month(year, month)?;
        NaiveDate::from_ymd_opt(year, month, day.min(last))
    })
}

/// Last valid day number of `month` in `year` (handles leap February).
fn last_day_of_month(year: i32, month: u32) -> Option<u32> {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    Some(NaiveDate::from_ymd_opt(ny, nm, 1)?.pred_opt()?.day())
}

/// Helper: returns the `n`-th occurrence of `weekday` in `month` of
/// `year` (1-indexed). `nth_weekday_of_month(2026, 3, Sun, 2)` returns
/// the second Sunday of March 2026, i.e. DST start. Returns `None` if
/// the month has fewer than `n` matching weekdays.
fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, n: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let offset =
        (weekday.num_days_from_sunday() as i64 - first.weekday().num_days_from_sunday() as i64 + 7)
            % 7;
    let first_match = first.checked_add_signed(chrono::Duration::days(offset))?;
    let nth = first_match.checked_add_signed(chrono::Duration::days(7 * (n as i64 - 1)))?;
    if nth.month() != month {
        None
    } else {
        Some(nth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make(yyyy: i32, mm: u32, dd: u32, h: u32, mn: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(yyyy, mm, dd, h, mn, 0)
            .single()
            .expect("valid local datetime")
    }

    fn sjrwmd_dst() -> WateringRestriction {
        WateringRestriction {
            id: "sjrwmd_dst".into(),
            name: "St. Johns RWMD (DST)".into(),
            enabled: true,
            effective: EffectiveWindow::DstOnly,
            // DST window: odd watering Wed (3) + Sat (6); even Thu (4) + Sun (0)
            allowed_weekdays_odd: vec![3, 6],
            allowed_weekdays_even: vec![4, 0],
            forbidden_hour_start: Some(10),
            forbidden_hour_end: Some(16),
            max_minutes_per_zone: Some(60),
        }
    }

    fn sjrwmd_est() -> WateringRestriction {
        WateringRestriction {
            id: "sjrwmd_est".into(),
            name: "St. Johns RWMD (EST)".into(),
            enabled: true,
            effective: EffectiveWindow::StandardOnly,
            // EST: odd watering Sat (6); even Sun (0). Once a week.
            allowed_weekdays_odd: vec![6],
            allowed_weekdays_even: vec![0],
            forbidden_hour_start: Some(10),
            forbidden_hour_end: Some(16),
            max_minutes_per_zone: Some(60),
        }
    }

    #[test]
    fn dst_window_2026_runs_march_8_to_november_1() {
        // 2026 DST starts the 2nd Sun of March = 2026-03-08.
        // Standard time resumes 1st Sun of November = 2026-11-01.
        assert!(is_in_effective_window(
            make(2026, 3, 8, 6, 0),
            &EffectiveWindow::DstOnly
        ));
        assert!(!is_in_effective_window(
            make(2026, 3, 7, 6, 0),
            &EffectiveWindow::DstOnly
        ));
        assert!(is_in_effective_window(
            make(2026, 10, 31, 6, 0),
            &EffectiveWindow::DstOnly
        ));
        assert!(!is_in_effective_window(
            make(2026, 11, 1, 6, 0),
            &EffectiveWindow::DstOnly
        ));
    }

    #[test]
    fn standard_window_is_complement_of_dst() {
        assert!(is_in_effective_window(
            make(2026, 2, 14, 6, 0),
            &EffectiveWindow::StandardOnly
        ));
        assert!(is_in_effective_window(
            make(2026, 11, 1, 6, 0),
            &EffectiveWindow::StandardOnly
        ));
        assert!(!is_in_effective_window(
            make(2026, 7, 4, 6, 0),
            &EffectiveWindow::StandardOnly
        ));
    }

    #[test]
    fn date_range_clamps_feb_30_to_month_end() {
        // "Dec 1 → Feb 30" means "through the end of February".
        let w = EffectiveWindow::DateRange {
            start_month: 12,
            start_day: 1,
            end_month: 2,
            end_day: 30,
        };
        // 2026 is not a leap year: clamps to Feb 28.
        assert!(is_in_effective_window(make(2026, 2, 28, 6, 0), &w));
        assert!(!is_in_effective_window(make(2026, 3, 1, 6, 0), &w));
        // 2028 is a leap year: clamps to Feb 29.
        assert!(is_in_effective_window(make(2028, 2, 29, 6, 0), &w));
        assert!(!is_in_effective_window(make(2028, 3, 1, 6, 0), &w));
    }

    #[test]
    fn date_range_feb_29_end_works_on_leap_and_non_leap_years() {
        let w = EffectiveWindow::DateRange {
            start_month: 2,
            start_day: 1,
            end_month: 2,
            end_day: 29,
        };
        // Leap year: Feb 29 exists and is the last in-window day.
        assert!(is_in_effective_window(make(2028, 2, 29, 6, 0), &w));
        assert!(!is_in_effective_window(make(2028, 3, 1, 6, 0), &w));
        // Non-leap year: clamps to Feb 28, window still applies.
        assert!(is_in_effective_window(make(2026, 2, 28, 6, 0), &w));
        assert!(!is_in_effective_window(make(2026, 3, 1, 6, 0), &w));
        assert!(!is_in_effective_window(make(2026, 1, 31, 6, 0), &w));
    }

    #[test]
    fn date_range_clamps_jun_31_to_jun_30() {
        let w = EffectiveWindow::DateRange {
            start_month: 6,
            start_day: 1,
            end_month: 6,
            end_day: 31,
        };
        assert!(is_in_effective_window(make(2026, 6, 30, 6, 0), &w));
        assert!(is_in_effective_window(make(2026, 6, 1, 6, 0), &w));
        assert!(!is_in_effective_window(make(2026, 7, 1, 6, 0), &w));
        assert!(!is_in_effective_window(make(2026, 5, 31, 6, 0), &w));
    }

    #[test]
    fn date_range_clamped_start_day() {
        // Start day overruns too: "Feb 30 → Mar 15" behaves as Feb 28/29.
        let w = EffectiveWindow::DateRange {
            start_month: 2,
            start_day: 30,
            end_month: 3,
            end_day: 15,
        };
        assert!(is_in_effective_window(make(2026, 2, 28, 6, 0), &w));
        assert!(is_in_effective_window(make(2026, 3, 15, 6, 0), &w));
        assert!(!is_in_effective_window(make(2026, 2, 27, 6, 0), &w));
        assert!(!is_in_effective_window(make(2026, 3, 16, 6, 0), &w));
    }

    #[test]
    fn forbidden_hour_window_inclusive_lower_exclusive_upper() {
        let r = sjrwmd_dst();
        // 09:59 = ok, 10:00 = forbidden, 15:59 = forbidden, 16:00 = ok.
        assert!(!in_forbidden_hours(make(2026, 5, 30, 9, 59), &r));
        assert!(in_forbidden_hours(make(2026, 5, 30, 10, 0), &r));
        assert!(in_forbidden_hours(make(2026, 5, 30, 15, 59), &r));
        assert!(!in_forbidden_hours(make(2026, 5, 30, 16, 0), &r));
    }

    #[test]
    fn allowed_today_respects_parity() {
        let r = sjrwmd_dst();
        // 2026-05-30 is a Saturday (DOW 6), allowed for odd.
        assert!(allowed_today(
            make(2026, 5, 30, 6, 0),
            &r,
            AddressParity::Odd
        ));
        assert!(!allowed_today(
            make(2026, 5, 30, 6, 0),
            &r,
            AddressParity::Even
        ));
        // 2026-05-31 is Sunday (DOW 0), allowed for even, not odd.
        assert!(allowed_today(
            make(2026, 5, 31, 6, 0),
            &r,
            AddressParity::Even
        ));
        assert!(!allowed_today(
            make(2026, 5, 31, 6, 0),
            &r,
            AddressParity::Odd
        ));
        // NotApplicable bypasses the gate entirely.
        assert!(allowed_today(
            make(2026, 5, 30, 6, 0),
            &r,
            AddressParity::NotApplicable
        ));
    }

    #[test]
    fn sjrwmd_odd_summer_allowed_saturday_6am_no_skip() {
        let v = evaluate(
            make(2026, 5, 30, 6, 0),
            &[sjrwmd_dst(), sjrwmd_est()],
            AddressParity::Odd,
        );
        assert!(!v.skip, "Sat 6am DST should be allowed for odd, got {v:?}");
        assert_eq!(v.max_minutes_cap, Some(60));
    }

    #[test]
    fn sjrwmd_odd_summer_saturday_noon_skips_for_hours() {
        let v = evaluate(
            make(2026, 5, 30, 12, 0),
            &[sjrwmd_dst(), sjrwmd_est()],
            AddressParity::Odd,
        );
        assert!(v.skip);
        assert!(v.reason.as_deref().unwrap().contains("forbidden"));
    }

    #[test]
    fn sjrwmd_odd_tuesday_skips_for_weekday() {
        // 2026-06-02 is a Tuesday.
        let v = evaluate(
            make(2026, 6, 2, 6, 0),
            &[sjrwmd_dst(), sjrwmd_est()],
            AddressParity::Odd,
        );
        assert!(v.skip);
        assert!(v.reason.as_deref().unwrap().contains("allowed"));
    }

    #[test]
    fn cap_is_min_of_active_restrictions() {
        let mut tight = sjrwmd_dst();
        tight.max_minutes_per_zone = Some(30);
        let v = evaluate(
            make(2026, 5, 30, 6, 0),
            &[sjrwmd_dst(), tight],
            AddressParity::Odd,
        );
        assert_eq!(v.max_minutes_cap, Some(30));
    }

    #[test]
    fn disabled_restriction_is_ignored() {
        let mut r = sjrwmd_dst();
        r.enabled = false;
        let v = evaluate(make(2026, 6, 2, 12, 0), &[r], AddressParity::Odd);
        assert!(!v.skip);
        assert_eq!(v.max_minutes_cap, None);
    }

    #[test]
    fn out_of_effective_window_is_ignored() {
        // sjrwmd_est is StandardOnly; ask in July (DST), should not apply.
        let v = evaluate(
            make(2026, 7, 15, 12, 0),
            &[sjrwmd_est()],
            AddressParity::Odd,
        );
        assert!(!v.skip);
        assert_eq!(v.max_minutes_cap, None);
    }
}
