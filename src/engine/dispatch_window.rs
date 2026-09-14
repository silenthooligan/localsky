// Where in the day the yard waters.
//
// Smart morning was the only window: finish fifteen minutes before
// sunrise, start as early as the sequence needs. That is the right
// window for most of the year in most places. It is the wrong window on
// a continental October or April morning, when the pre-dawn hour sits
// at 30 F and the freeze gate refuses it while by ten the lawn is at 50 F
// in full sun and would take water gladly. A cool-season lawn in Denver
// skipped every morning of both shoulder seasons on that rule.
//
// So the window is chosen, not assumed. The pre-dawn window stays the
// default. When the forecast puts the pre-dawn hours below the freeze
// threshold, the first later hour that clears the threshold for the run
// and a few hours after it, and that the restrictions allow, becomes
// the day's window instead. Every consumer asks this one function: the
// refresher's verdict, the strip's cells, next_run, and the dispatcher,
// so they cannot name different windows for the same morning.

use serde::{Deserialize, Serialize};

use crate::config::schema::{AddressParity, WateringRestriction};
use crate::engine::calendar::Calendar;
use crate::engine::clock::{CivilDay, DecisionTime};
use crate::engine::restrictions;
use crate::engine::sunrise::{planned_window, Site, FINISH_BEFORE_SUNRISE_MIN};
use crate::forecast::snapshot::ForecastSnapshot;

/// How long after the valves close the air has to stay above the freeze
/// threshold. Water needs time to move off the leaf and into the soil
/// before a freeze can turn it to ice on the blade.
pub const FREEZE_TAIL_S: i64 = 3 * 3600;

/// Which window the yard is planned into.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    /// Finish fifteen minutes before sunrise. The default.
    #[default]
    PreDawn,
    /// After sunrise, because the pre-dawn hours were below the freeze
    /// threshold and this hour clears it.
    PostSunrise,
}

/// The span the yard plans to water in on a day.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub start: i64,
    pub finish: i64,
    pub kind: WindowKind,
    /// Forecast minimum from `start` through `finish + FREEZE_TAIL_S`,
    /// when the hourly series covers it. None past the series.
    pub min_temp_f: Option<f64>,
}

impl Window {
    pub fn span(self) -> (i64, i64) {
        (self.start, self.finish)
    }
}

/// The day's chosen window as the refresher publishes it on the
/// snapshot, so the dispatcher executes the decision the verdict was
/// made under rather than re-deriving one from data it does not hold.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlannedWindow {
    pub day: CivilDay,
    pub start: i64,
    pub finish: i64,
    pub kind: WindowKind,
    #[serde(default)]
    pub min_temp_f: Option<f64>,
}

impl PlannedWindow {
    pub fn of(day: CivilDay, w: Window) -> Self {
        Self {
            day,
            start: w.start,
            finish: w.finish,
            kind: w.kind,
            min_temp_f: w.min_temp_f,
        }
    }
}

/// The restriction facts a window has to satisfy.
#[derive(Debug, Clone, Copy)]
pub struct Rules<'a> {
    pub restrictions: &'a [WateringRestriction],
    pub parity: AddressParity,
    pub watered: &'a [CivilDay],
}

impl Rules<'_> {
    fn permit(self, cal: Calendar, epoch: i64) -> bool {
        !restrictions::evaluate_for(
            DecisionTime::at(cal, epoch),
            self.restrictions,
            self.parity,
            self.watered,
            None,
        )
        .skip
    }
}

/// The window the yard waters in on `day`.
///
/// Pre-dawn unless the hourly forecast puts the pre-dawn hours (through
/// the tail after them) below `freeze_f`, in which case the first later
/// hour that clears the threshold and the restrictions is chosen. When
/// no later hour clears it, or the hourly series does not reach the
/// day, the pre-dawn window is returned and the freeze gate judges it
/// exactly as before. `None` only when no window exists at all: no
/// location, or no sunrise on this date.
pub fn choose(
    day: CivilDay,
    site: Site,
    cal: Calendar,
    fc: &ForecastSnapshot,
    freeze_f: f64,
    rules: Rules<'_>,
) -> Option<Window> {
    let (start, finish) = planned_window(day, site, cal)?;
    let pre = Window {
        start,
        finish,
        kind: WindowKind::PreDawn,
        min_temp_f: fc.min_temp_over(start, finish + FREEZE_TAIL_S),
    };
    let Some(cold) = pre.min_temp_f.filter(|t| *t < freeze_f) else {
        return Some(pre);
    };
    let _ = cold;

    let seq = i64::from(site.sequence_total_s.min(i64::MAX as u64) as u32);
    let sunrise = finish + FINISH_BEFORE_SUNRISE_MIN * 60;
    let (_, day_end) = cal.day_bounds_utc(day)?;
    // The same drying interval must finish before local sunset. A fixed
    // 15:00 cutoff both ignores seasonal daylight and moves on DST days
    // when expressed as seconds since midnight.
    let Some(sunset) = crate::engine::sunrise::sunset_on_local_day(day, site, cal) else {
        return Some(pre);
    };
    let latest_start = sunset - FREEZE_TAIL_S - seq;
    // Whole hours from sunrise, because the forecast is hourly and a
    // window that starts on the hour reads the hours it is in.
    let mut t = (sunrise + 3599).div_euclid(3600) * 3600;
    while t <= latest_start && t + seq <= day_end {
        let min = fc.min_temp_over(t, t + seq + FREEZE_TAIL_S);
        let Some(min) = min else {
            // Past the hourly series: nothing further is knowable.
            break;
        };
        if min >= freeze_f && rules.permit(cal, t) && rules.permit(cal, t + seq) {
            return Some(Window {
                start: t,
                finish: t + seq,
                kind: WindowKind::PostSunrise,
                min_temp_f: Some(min),
            });
        }
        t += 3600;
    }
    Some(pre)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::snapshot::HourlyEntry;

    /// Denver, mid October. Mountain daylight time is UTC-6.
    const DENVER: (f64, f64) = (39.74, -104.99);
    /// Thursday 15 October 2026, 00:00 MDT.
    const OCT15_MIDNIGHT_MDT: i64 = 1_792_044_000;

    fn mdt() -> Calendar {
        Calendar::fixed_offset(-6 * 3600).expect("MDT")
    }

    fn oct15() -> CivilDay {
        mdt().date_of(OCT15_MIDNIGHT_MDT + 3600).expect("a day")
    }

    /// A continental October: 30 F through the pre-dawn hours, warming
    /// five degrees an hour from eight, freezing again after dark.
    fn october_hours(days: i64) -> Vec<HourlyEntry> {
        (0..24 * days)
            .map(|i| {
                let h = i % 24;
                let temp = if h < 8 {
                    30.0
                } else if h <= 16 {
                    30.0 + (h - 7) as f64 * 5.0
                } else {
                    75.0 - (h - 16) as f64 * 6.0
                };
                HourlyEntry {
                    time_epoch: OCT15_MIDNIGHT_MDT + i * 3600,
                    temp_f: Some(temp),
                    ..Default::default()
                }
            })
            .collect()
    }

    fn no_rules() -> Rules<'static> {
        Rules {
            restrictions: &[],
            parity: AddressParity::NotApplicable,
            watered: &[],
        }
    }

    #[test]
    fn late_freeze_recovery_follows_seasonal_daylight_in_each_location() {
        // Northern and southern summer evenings, plus half/quarter-hour
        // offsets. In summer 16:00 can be safe; the old fixed 15:00 cutoff
        // refused it regardless of the hours of daylight still available.
        for (location, offset, date) in [
            ((40.71, -74.01), -4 * 3600, (2026, 6, 21)),
            ((-33.87, 151.21), 11 * 3600, (2026, 12, 21)),
            ((-43.95, -176.56), 13 * 3600 + 45 * 60, (2026, 12, 21)),
        ] {
            let cal = Calendar::fixed_offset(offset).unwrap();
            let date = chrono::NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap();
            let noon =
                date.and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp() - i64::from(offset);
            let day = cal.date_of(noon).unwrap();
            let (midnight, _) = cal.day_bounds_utc(day).unwrap();
            let fc = ForecastSnapshot {
                hourly: (0..48)
                    .map(|h| HourlyEntry {
                        time_epoch: midnight + h * 3600,
                        temp_f: Some(if h < 16 { 20.0 } else { 55.0 }),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            };
            let site = Site::new(location, 30 * 60);
            let window = choose(day, site, cal, &fc, 38.0, no_rules()).unwrap();
            assert_eq!(window.kind, WindowKind::PostSunrise, "{location:?}");
            assert!(
                window.finish + FREEZE_TAIL_S
                    <= crate::engine::sunrise::sunset_on_local_day(day, site, cal).unwrap()
            );
            assert!(cal.at(window.start).unwrap().hour() >= 16);
        }
    }

    #[test]
    fn short_winter_day_does_not_water_into_darkness() {
        let cal = Calendar::fixed_offset(-5 * 3600).unwrap();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 12, 21).unwrap();
        let noon = date.and_hms_opt(17, 0, 0).unwrap().and_utc().timestamp();
        let day = cal.date_of(noon).unwrap();
        let (midnight, _) = cal.day_bounds_utc(day).unwrap();
        let fc = ForecastSnapshot {
            hourly: (0..48)
                .map(|h| HourlyEntry {
                    time_epoch: midnight + h * 3600,
                    temp_f: Some(if h < 14 { 20.0 } else { 55.0 }),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let window = choose(
            day,
            Site::new((40.71, -74.01), 30 * 60),
            cal,
            &fc,
            38.0,
            no_rules(),
        )
        .unwrap();
        assert_eq!(window.kind, WindowKind::PreDawn);
    }

    /// The Denver October morning waters, after sunrise, at the first
    /// hour that clears 38 F for the run and three hours beyond it.
    #[test]
    fn a_freezing_dawn_moves_the_window_after_sunrise() {
        let fc = ForecastSnapshot {
            hourly: october_hours(2),
            ..Default::default()
        };
        let site = Site::new(DENVER, 30 * 60);
        let w = choose(oct15(), site, mdt(), &fc, 38.0, no_rules()).expect("a window");
        assert_eq!(w.kind, WindowKind::PostSunrise, "{w:?}");
        // 09:00 reads 40 F; the run and its tail run to 12:30, all above 38.
        assert_eq!(w.start, OCT15_MIDNIGHT_MDT + 9 * 3600, "{w:?}");
        assert_eq!(w.finish, w.start + 30 * 60);
        assert_eq!(w.min_temp_f, Some(40.0));
    }

    /// A mild morning keeps the pre-dawn window.
    #[test]
    fn a_mild_dawn_keeps_the_pre_dawn_window() {
        let mut fc = ForecastSnapshot {
            hourly: october_hours(2),
            ..Default::default()
        };
        for h in &mut fc.hourly {
            h.temp_f = h.temp_f.map(|temp| temp + 20.0);
        }
        let site = Site::new(DENVER, 30 * 60);
        let w = choose(oct15(), site, mdt(), &fc, 38.0, no_rules()).expect("a window");
        assert_eq!(w.kind, WindowKind::PreDawn);
        assert_eq!(w.span(), planned_window(oct15(), site, mdt()).unwrap());
    }

    /// Past the hourly series nothing is knowable, so the pre-dawn
    /// window stands and the freeze gate judges it as it always did.
    #[test]
    fn without_hourly_coverage_the_pre_dawn_window_stands() {
        let fc = ForecastSnapshot::default();
        let site = Site::new(DENVER, 30 * 60);
        let w = choose(oct15(), site, mdt(), &fc, 38.0, no_rules()).expect("a window");
        assert_eq!(w.kind, WindowKind::PreDawn);
        assert_eq!(w.min_temp_f, None);
    }

    /// The post-sunrise hour has to be legal too: with a 09:00 to 16:00
    /// ban the search runs past it, and 16:00 is after the latest start,
    /// so the day falls back to the pre-dawn window and its freeze.
    #[test]
    fn the_later_hour_must_clear_the_restrictions_as_well() {
        use crate::config::schema::{EffectiveWindow, WateringRestriction};
        let fc = ForecastSnapshot {
            hourly: october_hours(2),
            ..Default::default()
        };
        let ban = [WateringRestriction {
            id: "midday".into(),
            name: "No daytime watering".into(),
            effective: EffectiveWindow::AllYear,
            forbidden_hour_start: Some(9),
            forbidden_hour_end: Some(16),
            ..Default::default()
        }];
        let rules = Rules {
            restrictions: &ban,
            parity: AddressParity::NotApplicable,
            watered: &[],
        };
        let site = Site::new(DENVER, 30 * 60);
        let w = choose(oct15(), site, mdt(), &fc, 38.0, rules).expect("a window");
        assert_eq!(w.kind, WindowKind::PreDawn, "{w:?}");
        // And with the ban ending at eleven, eleven is the answer.
        let short = [WateringRestriction {
            forbidden_hour_end: Some(11),
            ..ban[0].clone()
        }];
        let rules = Rules {
            restrictions: &short,
            ..rules
        };
        let w = choose(oct15(), site, mdt(), &fc, 38.0, rules).expect("a window");
        assert_eq!(w.kind, WindowKind::PostSunrise);
        assert_eq!(w.start, OCT15_MIDNIGHT_MDT + 11 * 3600, "{w:?}");
    }
}
