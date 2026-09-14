// When the yard next waters.
//
// This lived in the refresher as a purely astronomical calculation: work
// out today's sunrise-derived start, and if it has passed, return
// tomorrow's. It had no idea watering restrictions existed. On a Sunday
// evening, for a yard whose district allows Thursday and Sunday, it
// published Monday: a day that yard is forbidden to water. The morning
// gate caught it and skipped, so no valve opened illegally, but the time
// on screen was a day the operator may not use.
//
// The module that owns "may we water" should also answer "when do we
// next water", or the two drift. So it does.

use crate::config::schema::{AddressParity, WateringRestriction};
use crate::engine::calendar::Calendar;
use crate::engine::clock::{CivilDay, DecisionTime, Zoned};
use crate::engine::restrictions;
use crate::engine::sunrise::Site;

/// Days to look ahead before giving up. A fortnight covers every
/// once-a-week and twice-a-week schedule a district imposes, with room
/// for a seasonal window that has just closed.
pub const DEFAULT_HORIZON_DAYS: u16 = 14;

/// The answer, including the reasons there might not be one.
///
/// The old shape returned `0` for "no location", "polar latitude" and
/// "date arithmetic overflowed" alike, and the hero rendered all three
/// identically to a fully restricted week.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextRun {
    At {
        day: CivilDay,
        start_epoch: i64,
    },
    /// Every day in the horizon is refused by a restriction.
    NoLegalDay {
        horizon_days: u16,
    },
    /// The sun does not rise, so there is no morning to aim at.
    NoSunrise {
        from: CivilDay,
        horizon_days: u16,
    },
    /// No location configured, so no sunrise can be computed for anywhere
    /// in particular.
    NoLocation,
}

impl NextRun {
    /// The epoch, or 0. Preserves the sentinel the snapshot has always
    /// put on the wire; callers that want to tell the three no-answer
    /// cases apart should match the enum instead.
    pub fn start_epoch(&self) -> i64 {
        match self {
            Self::At { start_epoch, .. } => *start_epoch,
            _ => 0,
        }
    }
}

/// The next morning this yard both CAN and MAY water.
///
/// Walks forward a day at a time from `from`, asking the same two
/// questions the morning itself will ask: is there a planned start on
/// this day that has not already passed, and do the restrictions permit
/// it. Because the restriction check happens at the planned start rather
/// than at midnight or at noon, an overnight ban and a midday ban both
/// give the right answer.
pub fn next_run(
    cal: Calendar,
    from: Zoned,
    site: Site,
    rules: &[WateringRestriction],
    parity: AddressParity,
    watered: &[crate::engine::clock::CivilDay],
    fc: &crate::forecast::snapshot::ForecastSnapshot,
    freeze_f: f64,
    horizon_days: u16,
) -> NextRun {
    if site.location().is_none() {
        return NextRun::NoLocation;
    }
    let start_day = from.day();
    let mut day = start_day;
    let mut saw_a_sunrise = false;

    for _ in 0..=horizon_days {
        // The same window the dispatcher will use: pre-dawn, or the first
        // post-sunrise hour that clears a freezing morning.
        let window = crate::engine::dispatch_window::choose(
            day,
            site,
            cal,
            fc,
            freeze_f,
            crate::engine::dispatch_window::Rules {
                restrictions: rules,
                parity,
                watered,
            },
        );
        if let Some(w) = window {
            saw_a_sunrise = true;
            let start = w.start;
            // Today's window may already have passed.
            if start > from.epoch() {
                let permitted = !restrictions::evaluate_for(
                    DecisionTime::at(cal, start),
                    rules,
                    parity,
                    watered,
                    None,
                )
                .skip;
                if permitted {
                    return NextRun::At {
                        day,
                        start_epoch: start,
                    };
                }
            }
        }
        match day.succ() {
            Some(d) => day = d,
            None => break,
        }
    }

    if saw_a_sunrise {
        NextRun::NoLegalDay { horizon_days }
    } else {
        NextRun::NoSunrise {
            from: start_day,
            horizon_days,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::EffectiveWindow;

    const EDT: i32 = -4 * 3600;
    /// Jacksonville, Florida.
    const YARD: (f64, f64) = (30.33, -81.66);

    fn cal() -> Calendar {
        Calendar::fixed_offset(EDT).expect("a valid offset")
    }

    fn site() -> Site {
        Site::new(YARD, 30 * 60)
    }

    fn at(epoch: i64) -> Zoned {
        cal().at(epoch).expect("representable")
    }

    /// The operator's own district: an even address waters Thursday and
    /// Sunday, never between 10:00 and 16:00.
    fn sjrwmd() -> Vec<WateringRestriction> {
        vec![WateringRestriction {
            id: "sjrwmd_dst".into(),
            name: "St. Johns RWMD".into(),
            enabled: true,
            effective: EffectiveWindow::DstOnly,
            allowed_weekdays_odd: vec![3, 6],
            allowed_weekdays_even: vec![4, 0],
            forbidden_hour_start: Some(10),
            forbidden_hour_end: Some(16),
            max_minutes_per_zone: Some(60),
            ..Default::default()
        }]
    }

    /// Sunday evening, 2026-09-06 20:00 EDT.
    const SUNDAY_EVENING: i64 = 1_788_753_600;

    /// The defect: astronomy alone publishes tomorrow, and tomorrow is a
    /// day this yard is forbidden to water.
    #[test]
    fn next_run_skips_past_days_the_district_forbids() {
        let r = next_run(
            cal(),
            at(SUNDAY_EVENING),
            site(),
            &sjrwmd(),
            AddressParity::Even,
            &[],
            &crate::forecast::snapshot::ForecastSnapshot::default(),
            38.0,
            DEFAULT_HORIZON_DAYS,
        );
        let NextRun::At { day, start_epoch } = r else {
            panic!("expected a run, got {r:?}");
        };
        // Thursday 10 September, not Monday the 7th.
        assert_eq!(day.weekday(), chrono::Weekday::Thu);
        assert_eq!(day.naive().to_string(), "2026-09-10");
        // And at a legal hour, comfortably outside 10:00 to 16:00.
        let z = cal().at(start_epoch).expect("representable");
        assert!(z.hour() < 10, "planned start at {}:00", z.hour());
    }

    /// With no restrictions the answer is still the next astronomical
    /// morning, so this change cannot move an unrestricted install.
    #[test]
    fn an_unrestricted_yard_waters_the_next_morning() {
        let r = next_run(
            cal(),
            at(SUNDAY_EVENING),
            site(),
            &[],
            AddressParity::NotApplicable,
            &[],
            &crate::forecast::snapshot::ForecastSnapshot::default(),
            38.0,
            DEFAULT_HORIZON_DAYS,
        );
        let NextRun::At { day, .. } = r else {
            panic!("expected a run, got {r:?}");
        };
        assert_eq!(day.naive().to_string(), "2026-09-07");
    }

    /// An unconfigured install says so, rather than returning a zero that
    /// reads the same as a fully restricted week.
    #[test]
    fn no_location_is_its_own_answer() {
        let r = next_run(
            cal(),
            at(SUNDAY_EVENING),
            Site::new((0.0, 0.0), 1800),
            &[],
            AddressParity::NotApplicable,
            &[],
            &crate::forecast::snapshot::ForecastSnapshot::default(),
            38.0,
            DEFAULT_HORIZON_DAYS,
        );
        assert_eq!(r, NextRun::NoLocation);
        assert_eq!(r.start_epoch(), 0);
    }

    /// A rule nobody can ever satisfy terminates instead of running to
    /// the end of time, and names what happened.
    #[test]
    fn a_yard_with_no_legal_day_says_so() {
        let never = vec![WateringRestriction {
            id: "never".into(),
            name: "Total ban".into(),
            enabled: true,
            effective: EffectiveWindow::AllYear,
            allowed_weekdays_odd: vec![],
            allowed_weekdays_even: vec![],
            forbidden_hour_start: Some(0),
            forbidden_hour_end: Some(24),
            max_minutes_per_zone: None,
            ..Default::default()
        }];
        let r = next_run(
            cal(),
            at(SUNDAY_EVENING),
            site(),
            &never,
            AddressParity::NotApplicable,
            &[],
            &crate::forecast::snapshot::ForecastSnapshot::default(),
            38.0,
            7,
        );
        assert_eq!(r, NextRun::NoLegalDay { horizon_days: 7 });
    }

    /// Above the Arctic circle in December there is no morning to aim
    /// at, which is a different answer from "every day is banned".
    #[test]
    fn a_polar_winter_is_not_a_restriction_problem() {
        let polar = Site::new((78.2, 15.6), 30 * 60); // Longyearbyen
                                                      // 15 December 2026, 12:00 UTC.
        let r = next_run(
            Calendar::utc(),
            Calendar::utc().at(1_797_768_000).expect("representable"),
            polar,
            &[],
            AddressParity::NotApplicable,
            &[],
            &crate::forecast::snapshot::ForecastSnapshot::default(),
            38.0,
            5,
        );
        assert!(
            matches!(r, NextRun::NoSunrise { .. }),
            "expected NoSunrise, got {r:?}"
        );
    }
}
