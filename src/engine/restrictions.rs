// Jurisdictional watering-restriction evaluator. Pure functions over
// chrono::DateTime<Local> + the schema types, no I/O, no globals, so
// the rule logic is straightforward to unit-test against synthetic
// datetimes (DST/EST × Odd/Even × in/out of forbidden hours).
//
// The aggregator `evaluate` is the only thing skip_rules cares about:
// it ANDs every enabled, in-effective-window restriction and produces
// a `RestrictionVerdict` with the first matching skip reason plus the
// min-of-active per-zone duration cap.

use chrono::{Datelike, NaiveDate, Weekday};

use crate::engine::clock::{CivilDay, DecisionTime, Zoned};

use crate::config::schema::{
    AddressParity, DateParity, EffectiveWindow, Nth, NthWeekday, SprinklerType, WateringRestriction,
};
use crate::engine::ZoneSlug;

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
pub fn is_in_effective_window(day: CivilDay, w: &EffectiveWindow) -> bool {
    let date = day.naive();
    match w {
        EffectiveWindow::AllYear => true,
        // Both of these are the UNITED STATES rule. They delegate rather
        // than keeping a second copy of the arithmetic, so a jurisdiction
        // that states its own dates and a jurisdiction that inherits
        // Washington's go down the same path.
        EffectiveWindow::DstOnly => in_floating_range(date, &us_dst_start(), &us_dst_end(), false),
        EffectiveWindow::StandardOnly => {
            !in_floating_range(date, &us_dst_start(), &us_dst_end(), false)
        }
        EffectiveWindow::FloatingRange {
            start,
            end,
            wraps_year,
        } => in_floating_range(date, start, end, *wraps_year),
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

/// True when today's weekday is on the operator's allowed list.
///
/// `allowed_weekdays` binds every address. The odd and even rows bind
/// the matching parity. When the operator has not said which parity
/// they are, a rule whose two rows AGREE still binds, because it never
/// depended on parity in the first place: the "Two days a week" starter
/// wrote the same days into both rows and was inert on every default
/// install until the operator found the parity radio. Rows that differ
/// cannot decide without a parity and stand aside.
pub fn allowed_today(day: CivilDay, r: &WateringRestriction, parity: AddressParity) -> bool {
    let today_dow = day.weekday().num_days_from_sunday() as u8;
    if !r.allowed_weekdays.is_empty() && !r.allowed_weekdays.contains(&today_dow) {
        return false;
    }
    let allowed = match parity {
        AddressParity::Odd => &r.allowed_weekdays_odd,
        AddressParity::Even => &r.allowed_weekdays_even,
        AddressParity::NotApplicable => {
            if !r.allowed_weekdays_odd.is_empty()
                && same_days(&r.allowed_weekdays_odd, &r.allowed_weekdays_even)
            {
                &r.allowed_weekdays_odd
            } else {
                return true;
            }
        }
    };
    if allowed.is_empty() {
        return true;
    }
    allowed.contains(&today_dow)
}

/// Two weekday rows name the same days, in any order.
fn same_days(a: &[u8], b: &[u8]) -> bool {
    let mut a: Vec<u8> = a.to_vec();
    let mut b: Vec<u8> = b.to_vec();
    a.sort_unstable();
    a.dedup();
    b.sort_unstable();
    b.dedup();
    a == b
}

/// Why a DATE refuses, if it does: the calendar-date rotation and the
/// 31st rule. `None` = the date is fine.
pub fn date_fault(
    day: CivilDay,
    r: &WateringRestriction,
    parity: AddressParity,
) -> Option<&'static str> {
    let dom = day.naive().day();
    if r.skip_31st && dom == 31 {
        return Some("the 31st is never a watering day");
    }
    let odd = dom % 2 == 1;
    let ok = match r.date_parity {
        DateParity::Off => true,
        DateParity::OddDates => odd,
        DateParity::EvenDates => !odd,
        DateParity::MatchAddress => match parity {
            AddressParity::Odd => odd,
            AddressParity::Even => !odd,
            // Cannot decide without a parity; the settings page says so.
            AddressParity::NotApplicable => true,
        },
    };
    (!ok).then_some("today is not an allowed date for this address")
}

/// True when the week `day` falls in has already used its allowance.
///
/// Weeks run Sunday to Saturday, the same frame the weekday rows use.
/// `day` itself is not counted: a second run on a day that already
/// watered does not spend another day.
pub fn week_allowance_spent(day: CivilDay, r: &WateringRestriction, watered: &[CivilDay]) -> bool {
    let Some(max) = r.max_days_per_week else {
        return false;
    };
    let week_of = |d: CivilDay| {
        let n = d.naive();
        n - chrono::Duration::days(i64::from(n.weekday().num_days_from_sunday()))
    };
    let this_week = week_of(day);
    let mut spent: Vec<NaiveDate> = watered
        .iter()
        .copied()
        .filter(|d| *d != day && week_of(*d) == this_week)
        .map(|d| d.naive())
        .collect();
    spent.sort_unstable();
    spent.dedup();
    spent.len() >= usize::from(max)
}

/// The zone a restriction is being judged for, when one is.
#[derive(Debug, Clone, Copy)]
pub struct ZoneScope<'a> {
    pub slug: &'a str,
    pub sprinkler: SprinklerType,
}

/// Whether a restriction binds this zone at all.
///
/// A rule that exempts the zone's head, or names other zones, neither
/// skips the zone nor caps it. With no zone in hand, every rule applies:
/// that is the yard-wide answer.
pub fn applies_to_zone(r: &WateringRestriction, zone: Option<ZoneScope<'_>>) -> bool {
    let Some(z) = zone else {
        return true;
    };
    if r.exempt_sprinklers.contains(&z.sprinkler) {
        return false;
    }
    if r.zones.is_empty() {
        return true;
    }
    let mine = ZoneSlug::new(z.slug);
    r.zones.iter().any(|s| ZoneSlug::new(s) == mine)
}

/// True when `now`'s hour falls in `[forbidden_hour_start, forbidden_hour_end)`.
/// Supports wrap-around (e.g. start=22, end=6). When either bound is
/// `None`, this gate is inactive.
pub fn in_forbidden_hours(at: Zoned, r: &WateringRestriction) -> bool {
    let (start, end) = match (r.forbidden_hour_start, r.forbidden_hour_end) {
        (Some(s), Some(e)) => (s, e),
        _ => return false,
    };
    let h = at.hour() as u8;
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
/// The one entrance.
///
/// This used to be generic over `Tz: TimeZone` and take a `DateTime<Tz>`,
/// which is how two different wrong frames got in: its own tests handed
/// it `chrono::Local`, the machine's zone, and the engine handed it a
/// `FixedOffset` assembled from a field that could be, and was, left at
/// zero. There is one way to name an instant now, and only the deployment
/// calendar can build one.
///
/// A restriction is two kinds of fact wearing one name. The effective
/// window and the weekday are properties of a DATE. The forbidden-hours
/// window is a property of an INSTANT. A whole-day preview cell knows the
/// first and may not know the second, so they are decided separately.
///
/// Day facts fail CLOSED, hour facts fail OPEN. A false negative on the
/// hour gate is the safer error: `restrictions` is a protected rule an
/// operator cannot override, and the live dispatch path always has a real
/// clock, so no valve ever opens on the strength of an abstention.
pub fn evaluate(
    when: DecisionTime,
    restrictions: &[WateringRestriction],
    parity: AddressParity,
) -> RestrictionVerdict {
    evaluate_for(when, restrictions, parity, &[], None)
}

/// The full entrance: the yard's watered days this week, and the zone
/// under judgment when there is one.
///
/// `watered` feeds the days-per-week allowance. `zone` lets a rule that
/// exempts a head or names other zones stand aside for this zone; with
/// `None` every rule binds, which is the yard-wide answer the hero shows.
pub fn evaluate_for(
    when: DecisionTime,
    restrictions: &[WateringRestriction],
    parity: AddressParity,
    watered: &[CivilDay],
    zone: Option<ZoneScope<'_>>,
) -> RestrictionVerdict {
    let mut verdict = RestrictionVerdict::default();
    let Some(day) = when.day() else {
        return verdict;
    };
    let at = when.zoned();

    for r in restrictions {
        if !r.enabled {
            continue;
        }
        if !is_in_effective_window(day, &r.effective) {
            continue;
        }
        if !applies_to_zone(r, zone) {
            continue;
        }

        let day_fault = if !allowed_today(day, r, parity) {
            Some("today is not an allowed watering day")
        } else if let Some(f) = date_fault(day, r, parity) {
            Some(f)
        } else if week_allowance_spent(day, r, watered) {
            Some("this week's allowance of watering days is used up")
        } else {
            None
        };
        let bad_hour = at.is_some_and(|z| in_forbidden_hours(z, r));

        if (day_fault.is_some() || bad_hour) && verdict.reason.is_none() {
            verdict.skip = true;
            verdict.reason = Some(format_reason(r, day_fault, bad_hour));
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

fn format_reason(r: &WateringRestriction, day_fault: Option<&str>, bad_hour: bool) -> String {
    if let Some(fault) = day_fault {
        if bad_hour {
            format!(
                "Watering restriction ({}): {fault}, and inside the forbidden hours",
                r.name
            )
        } else {
            format!("Watering restriction ({}): {fault}", r.name)
        }
    } else {
        let s = r.forbidden_hour_start.unwrap_or(0);
        let e = r.forbidden_hour_end.unwrap_or(0);
        format!(
            "Watering restriction ({}): currently inside the forbidden window ({s:02}:00 to {e:02}:00)",
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
/// Daylight saving in the United States: second Sunday in March.
fn us_dst_start() -> NthWeekday {
    NthWeekday {
        month: 3,
        weekday: 0,
        nth: Nth::Second,
    }
}

/// Daylight saving in the United States ends: first Sunday in November.
fn us_dst_end() -> NthWeekday {
    NthWeekday {
        month: 11,
        weekday: 0,
        nth: Nth::First,
    }
}

/// Sunday-based weekday index to a chrono `Weekday`.
///
/// Written out rather than using chrono's `TryFrom<u8>`, which numbers
/// from MONDAY. The rest of this module numbers from Sunday, because that
/// is what `allowed_weekdays_odd` and `allowed_weekdays_even` use, and
/// mixing the two silently shifts every floating date by a day.
fn weekday_from_sunday_index(n: u8) -> Weekday {
    match n % 7 {
        0 => Weekday::Sun,
        1 => Weekday::Mon,
        2 => Weekday::Tue,
        3 => Weekday::Wed,
        4 => Weekday::Thu,
        5 => Weekday::Fri,
        _ => Weekday::Sat,
    }
}

/// Resolve a floating date within a calendar year.
fn resolve(year: i32, d: &NthWeekday) -> Option<NaiveDate> {
    let weekday = weekday_from_sunday_index(d.weekday);
    match d.nth {
        Nth::First => nth_weekday_of_month(year, d.month as u32, weekday, 1),
        Nth::Second => nth_weekday_of_month(year, d.month as u32, weekday, 2),
        Nth::Third => nth_weekday_of_month(year, d.month as u32, weekday, 3),
        Nth::Fourth => nth_weekday_of_month(year, d.month as u32, weekday, 4),
        // Count back from the fifth: a month has four or five, so
        // whichever of those exists is the last one.
        Nth::Last => nth_weekday_of_month(year, d.month as u32, weekday, 5)
            .or_else(|| nth_weekday_of_month(year, d.month as u32, weekday, 4)),
    }
}

/// `[start, end)` between two floating dates.
///
/// `wraps_year` is what a southern-hemisphere summer needs: October to
/// April is not a range within one calendar year, it is the complement of
/// April to October.
fn in_floating_range(
    date: NaiveDate,
    start: &NthWeekday,
    end: &NthWeekday,
    wraps_year: bool,
) -> bool {
    let year = date.year();
    match (resolve(year, start), resolve(year, end)) {
        (Some(s), Some(e)) if wraps_year => date >= s || date < e,
        (Some(s), Some(e)) => date >= s && date < e,
        _ => false,
    }
}

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
    use crate::engine::calendar::Calendar;

    /// US Eastern daylight time. A FIXED offset, so these fixtures answer
    /// the same on every machine.
    ///
    /// Every test in this module used to build a `DateTime<Local>`, which
    /// is the runner's zone, not the yard's. The evaluator was generic
    /// over the timezone, so it accepted that happily, and the suite
    /// therefore proved nothing about which frame production would use.
    /// It could not have caught the shipped defect, where the engine
    /// handed this evaluator UTC for a US Eastern deployment.
    const EDT: i32 = -4 * 3600;

    fn cal() -> Calendar {
        Calendar::fixed_offset(EDT).expect("a valid offset")
    }

    /// A local wall-clock reading in the fixture's zone.
    fn make(yyyy: i32, mm: u32, dd: u32, h: u32, mn: u32) -> DecisionTime {
        let naive = NaiveDate::from_ymd_opt(yyyy, mm, dd)
            .expect("valid date")
            .and_hms_opt(h, mn, 0)
            .expect("valid time");
        DecisionTime::at(cal(), naive.and_utc().timestamp() - EDT as i64)
    }

    fn day(yyyy: i32, mm: u32, dd: u32) -> CivilDay {
        CivilDay::from_naive(NaiveDate::from_ymd_opt(yyyy, mm, dd).expect("valid date"))
    }

    /// Odd addresses on odd dates, nobody on the 31st. An odd address
    /// on the 31st, an odd date by arithmetic, still does not water.
    #[test]
    fn an_odd_address_does_not_water_on_the_31st_under_an_odd_date_rule() {
        let rule = WateringRestriction {
            id: "date_rotation".into(),
            name: "Date rotation".into(),
            date_parity: DateParity::MatchAddress,
            skip_31st: true,
            ..Default::default()
        };
        let rules = [rule];
        let at31 = DecisionTime::Day(day(2026, 7, 31));
        let v = evaluate(at31, &rules, AddressParity::Odd);
        assert!(v.skip, "{v:?}");
        assert!(v.reason.as_deref().unwrap().contains("31st"), "{v:?}");
        // The 30th is even: refused for the odd address, allowed for the even.
        let at30 = DecisionTime::Day(day(2026, 7, 30));
        assert!(evaluate(at30, &rules, AddressParity::Odd).skip);
        assert!(!evaluate(at30, &rules, AddressParity::Even).skip);
        // The 29th is odd: the odd address waters.
        assert!(
            !evaluate(
                DecisionTime::Day(day(2026, 7, 29)),
                &rules,
                AddressParity::Odd
            )
            .skip
        );
    }

    /// The "Two days a week" starter wrote the same days into both parity
    /// rows and was inert on a default install because the parity radio
    /// said "not applicable". Rows that agree never depended on parity.
    #[test]
    fn identical_rows_bind_without_a_parity() {
        let rule = WateringRestriction {
            id: "two_days".into(),
            name: "Two days a week".into(),
            allowed_weekdays_odd: vec![3, 6],
            allowed_weekdays_even: vec![6, 3],
            ..Default::default()
        };
        // 2026-07-27 is a Monday.
        let monday = DecisionTime::Day(day(2026, 7, 27));
        let v = evaluate(
            monday,
            std::slice::from_ref(&rule),
            AddressParity::NotApplicable,
        );
        assert!(v.skip, "identical rows must bind: {v:?}");
        let wednesday = DecisionTime::Day(day(2026, 7, 29));
        assert!(
            !evaluate(
                wednesday,
                std::slice::from_ref(&rule),
                AddressParity::NotApplicable
            )
            .skip
        );
        // Rows that differ still cannot decide without a parity.
        let split = WateringRestriction {
            allowed_weekdays_even: vec![4, 0],
            ..rule
        };
        assert!(!evaluate(monday, &[split], AddressParity::NotApplicable).skip);
    }

    /// A parity-free row binds every address on its own.
    #[test]
    fn allowed_weekdays_binds_every_address() {
        let rule = WateringRestriction {
            id: "wed_sat".into(),
            name: "Wednesday and Saturday".into(),
            allowed_weekdays: vec![3, 6],
            ..Default::default()
        };
        let monday = DecisionTime::Day(day(2026, 7, 27));
        for parity in [
            AddressParity::Odd,
            AddressParity::Even,
            AddressParity::NotApplicable,
        ] {
            assert!(
                evaluate(monday, std::slice::from_ref(&rule), parity).skip,
                "{parity:?}"
            );
        }
    }

    /// Two days a week means two: once Monday and Tuesday have watered,
    /// Wednesday is refused and the following Sunday is a fresh week.
    #[test]
    fn a_week_allowance_holds_once_spent() {
        let rule = WateringRestriction {
            id: "two_per_week".into(),
            name: "Two days per week".into(),
            max_days_per_week: Some(2),
            ..Default::default()
        };
        let rules = [rule];
        let watered = [day(2026, 7, 27), day(2026, 7, 28)];
        let wed = DecisionTime::Day(day(2026, 7, 29));
        let v = evaluate_for(wed, &rules, AddressParity::NotApplicable, &watered, None);
        assert!(v.skip, "{v:?}");
        assert!(v.reason.as_deref().unwrap().contains("allowance"), "{v:?}");
        // A day that already watered does not spend another day.
        let tue = DecisionTime::Day(day(2026, 7, 28));
        assert!(!evaluate_for(tue, &rules, AddressParity::NotApplicable, &watered, None).skip);
        // The next week starts on Sunday.
        let sun = DecisionTime::Day(day(2026, 8, 2));
        assert!(!evaluate_for(sun, &rules, AddressParity::NotApplicable, &watered, None).skip);
    }

    /// A drip zone under a rule that exempts drip is not held by it, and
    /// is not capped by it either. A zone the rule does not name is the
    /// same. With no zone in hand, the rule binds the yard.
    #[test]
    fn an_exempted_zone_is_neither_held_nor_capped() {
        let rule = WateringRestriction {
            id: "schedule".into(),
            name: "Schedule".into(),
            allowed_weekdays: vec![3, 6],
            max_minutes_per_zone: Some(30),
            exempt_sprinklers: vec![SprinklerType::Drip],
            zones: vec!["front-yard".into(), "side".into()],
            ..Default::default()
        };
        let rules = [rule];
        let monday = DecisionTime::Day(day(2026, 7, 27));
        let yard = evaluate_for(monday, &rules, AddressParity::NotApplicable, &[], None);
        assert!(yard.skip);
        assert_eq!(yard.max_minutes_cap, Some(30));

        let beds = ZoneScope {
            slug: "front_yard",
            sprinkler: SprinklerType::Drip,
        };
        let v = evaluate_for(
            monday,
            &rules,
            AddressParity::NotApplicable,
            &[],
            Some(beds),
        );
        assert!(!v.skip, "drip is exempt: {v:?}");
        assert_eq!(v.max_minutes_cap, None);

        // Named with the operator's own spelling; the lookup normalizes.
        let front = ZoneScope {
            slug: "front_yard",
            sprinkler: SprinklerType::Spray,
        };
        assert!(
            evaluate_for(
                monday,
                &rules,
                AddressParity::NotApplicable,
                &[],
                Some(front)
            )
            .skip
        );
        let back = ZoneScope {
            slug: "back_yard",
            sprinkler: SprinklerType::Spray,
        };
        assert!(
            !evaluate_for(
                monday,
                &rules,
                AddressParity::NotApplicable,
                &[],
                Some(back)
            )
            .skip
        );
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
            ..Default::default()
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
            ..Default::default()
        }
    }

    /// Sunday is 0 here, as it is everywhere else in this module.
    /// chrono's own u8 conversion numbers from Monday, so borrowing it
    /// would shift every floating date by exactly one day, which is the
    /// sort of error that looks like a timezone bug for a week.
    #[test]
    fn the_weekday_index_counts_from_sunday() {
        assert_eq!(weekday_from_sunday_index(0), Weekday::Sun);
        assert_eq!(weekday_from_sunday_index(3), Weekday::Wed);
        assert_eq!(weekday_from_sunday_index(6), Weekday::Sat);
        // 5 September 2026 is a Saturday, index 6.
        assert_eq!(day(2026, 9, 5).weekday().num_days_from_sunday() as u8, 6);
    }

    /// The hardcoded window is the US one, and it is wrong everywhere
    /// else.
    ///
    /// A New South Wales district whose summer restriction runs October
    /// to April gets, under DstOnly, a window running March to November:
    /// very nearly the exact inverse of its own summer. FloatingRange
    /// lets a jurisdiction state its own dates.
    #[test]
    fn a_southern_hemisphere_summer_window_is_expressible() {
        let sydney = EffectiveWindow::FloatingRange {
            start: NthWeekday {
                month: 10,
                weekday: 0,
                nth: Nth::First,
            },
            end: NthWeekday {
                month: 4,
                weekday: 0,
                nth: Nth::First,
            },
            wraps_year: true,
        };
        // January is high summer in Sydney.
        assert!(is_in_effective_window(day(2026, 1, 15), &sydney));
        // December too, on the other side of the wrap.
        assert!(is_in_effective_window(day(2026, 12, 15), &sydney));
        // July is midwinter.
        assert!(!is_in_effective_window(day(2026, 7, 15), &sydney));

        // What the US variant would have claimed for the same yard, which
        // is the inverse in both seasons that matter.
        assert!(!is_in_effective_window(
            day(2026, 1, 15),
            &EffectiveWindow::DstOnly
        ));
        assert!(is_in_effective_window(
            day(2026, 7, 15),
            &EffectiveWindow::DstOnly
        ));
    }

    /// Europe changes its clocks on the LAST Sunday of a month, which no
    /// ordinal can name: March has four Sundays in some years and five in
    /// others.
    #[test]
    fn the_european_rule_needs_last_not_a_count() {
        let eu = EffectiveWindow::FloatingRange {
            start: NthWeekday {
                month: 3,
                weekday: 0,
                nth: Nth::Last,
            },
            end: NthWeekday {
                month: 10,
                weekday: 0,
                nth: Nth::Last,
            },
            wraps_year: false,
        };
        // 2026: last Sunday of March is the 29th, of October the 25th.
        assert!(!is_in_effective_window(day(2026, 3, 28), &eu));
        assert!(is_in_effective_window(day(2026, 3, 29), &eu));
        assert!(is_in_effective_window(day(2026, 10, 24), &eu));
        assert!(!is_in_effective_window(day(2026, 10, 25), &eu));

        // A five-Sunday March still resolves to the fifth, not the fourth.
        // 2027: last Sunday of March is the 28th.
        assert!(!is_in_effective_window(day(2027, 3, 27), &eu));
        assert!(is_in_effective_window(day(2027, 3, 28), &eu));
    }

    /// The two US variants delegate now. They must still answer exactly
    /// as they did, because real operators have them configured.
    #[test]
    fn the_us_variants_keep_their_answers_and_their_wire_shape() {
        for (d, in_dst) in [
            ((2026, 3, 7), false),
            ((2026, 3, 8), true),
            ((2026, 7, 4), true),
            ((2026, 10, 31), true),
            ((2026, 11, 1), false),
            ((2026, 12, 25), false),
        ] {
            let day = day(d.0, d.1, d.2);
            assert_eq!(
                is_in_effective_window(day, &EffectiveWindow::DstOnly),
                in_dst,
                "{d:?} DstOnly"
            );
            assert_eq!(
                is_in_effective_window(day, &EffectiveWindow::StandardOnly),
                !in_dst,
                "{d:?} StandardOnly is the complement"
            );
        }
        // A stored config must round-trip byte for byte. A serde mistake
        // here silently DISABLES a compliance rule rather than failing.
        let json = "{\"kind\":\"dst_only\"}";
        let w: EffectiveWindow = serde_json::from_str(json).expect("parses");
        assert_eq!(w, EffectiveWindow::DstOnly);
        assert_eq!(serde_json::to_string(&w).expect("serializes"), json);
    }

    #[test]
    fn dst_window_2026_runs_march_8_to_november_1() {
        // 2026 DST starts the 2nd Sun of March = 2026-03-08.
        // Standard time resumes 1st Sun of November = 2026-11-01.
        assert!(is_in_effective_window(
            day(2026, 3, 8),
            &EffectiveWindow::DstOnly
        ));
        assert!(!is_in_effective_window(
            day(2026, 3, 7),
            &EffectiveWindow::DstOnly
        ));
        assert!(is_in_effective_window(
            day(2026, 10, 31),
            &EffectiveWindow::DstOnly
        ));
        assert!(!is_in_effective_window(
            day(2026, 11, 1),
            &EffectiveWindow::DstOnly
        ));
    }

    #[test]
    fn standard_window_is_complement_of_dst() {
        assert!(is_in_effective_window(
            day(2026, 2, 14),
            &EffectiveWindow::StandardOnly
        ));
        assert!(is_in_effective_window(
            day(2026, 11, 1),
            &EffectiveWindow::StandardOnly
        ));
        assert!(!is_in_effective_window(
            day(2026, 7, 4),
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
        assert!(is_in_effective_window(day(2026, 2, 28), &w));
        assert!(!is_in_effective_window(day(2026, 3, 1), &w));
        // 2028 is a leap year: clamps to Feb 29.
        assert!(is_in_effective_window(day(2028, 2, 29), &w));
        assert!(!is_in_effective_window(day(2028, 3, 1), &w));
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
        assert!(is_in_effective_window(day(2028, 2, 29), &w));
        assert!(!is_in_effective_window(day(2028, 3, 1), &w));
        // Non-leap year: clamps to Feb 28, window still applies.
        assert!(is_in_effective_window(day(2026, 2, 28), &w));
        assert!(!is_in_effective_window(day(2026, 3, 1), &w));
        assert!(!is_in_effective_window(day(2026, 1, 31), &w));
    }

    #[test]
    fn date_range_clamps_jun_31_to_jun_30() {
        let w = EffectiveWindow::DateRange {
            start_month: 6,
            start_day: 1,
            end_month: 6,
            end_day: 31,
        };
        assert!(is_in_effective_window(day(2026, 6, 30), &w));
        assert!(is_in_effective_window(day(2026, 6, 1), &w));
        assert!(!is_in_effective_window(day(2026, 7, 1), &w));
        assert!(!is_in_effective_window(day(2026, 5, 31), &w));
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
        assert!(is_in_effective_window(day(2026, 2, 28), &w));
        assert!(is_in_effective_window(day(2026, 3, 15), &w));
        assert!(!is_in_effective_window(day(2026, 2, 27), &w));
        assert!(!is_in_effective_window(day(2026, 3, 16), &w));
    }

    #[test]
    fn forbidden_hour_window_inclusive_lower_exclusive_upper() {
        let r = sjrwmd_dst();
        // 09:59 = ok, 10:00 = forbidden, 15:59 = forbidden, 16:00 = ok.
        assert!(!in_forbidden_hours(
            make(2026, 5, 30, 9, 59).zoned().expect("a real instant"),
            &r
        ));
        assert!(in_forbidden_hours(
            make(2026, 5, 30, 10, 0).zoned().expect("a real instant"),
            &r
        ));
        assert!(in_forbidden_hours(
            make(2026, 5, 30, 15, 59).zoned().expect("a real instant"),
            &r
        ));
        assert!(!in_forbidden_hours(
            make(2026, 5, 30, 16, 0).zoned().expect("a real instant"),
            &r
        ));
    }

    #[test]
    fn allowed_today_respects_parity() {
        let r = sjrwmd_dst();
        // 2026-05-30 is a Saturday (DOW 6), allowed for odd.
        assert!(allowed_today(day(2026, 5, 30), &r, AddressParity::Odd));
        assert!(!allowed_today(day(2026, 5, 30), &r, AddressParity::Even));
        // 2026-05-31 is Sunday (DOW 0), allowed for even, not odd.
        assert!(allowed_today(day(2026, 5, 31), &r, AddressParity::Even));
        assert!(!allowed_today(day(2026, 5, 31), &r, AddressParity::Odd));
        // NotApplicable bypasses the gate entirely.
        assert!(allowed_today(
            day(2026, 5, 30),
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
