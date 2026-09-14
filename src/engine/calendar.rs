// The deployment's calendar, as something the engine is HANDED rather
// than something it reaches out and reads.
//
// Four engine modules used to call `crate::timeutil` directly for "what
// local day is this instant" and "what is the yard's UTC offset". That is
// ambient state: it resolves against a process-wide timezone the engine
// does not own, which made the engine's answers depend on where the
// process runs. Two consequences, both of which bit:
//
//   * Tests inherited the runner's clock. Fixtures that pinned a
//     midnight-to-sunrise window passed on a machine whose zone matched
//     the fixture's coordinates and failed in the UTC build container,
//     twice, on unrelated changes.
//   * The modules could not compile for the browser, because resolving a
//     named zone needs the full timezone database.
//
// The first shape of this type was a pair of plain `fn` pointers. That
// answered "which day" but not "what offset was in force", so callers
// that needed an offset kept one alongside as a separate integer, and a
// separate integer is a thing that can be left at zero. It was, and the
// watering week reported every day of the week blocked by a midday ban it
// was reading in UTC.
//
// So a `Calendar` is now plain data, and it answers the offset question
// itself. Two shapes:
//
//   * `Fixed` is an offset in seconds. Pure data, which is what finally
//     makes `Calendar::fixed_offset(secs)` real; the old fn-pointer shape
//     promised it in prose for months and could never supply it, because
//     a plain fn pointer captures nothing.
//   * `Zone` is a real named zone, reached through a `&'static ZoneOps`
//     table the server side owns. The reference keeps `Calendar` small
//     and `Copy`, the ops are plain fns so nothing captures, and the
//     timezone database never enters the browser build.
//
// Everything else is derived from those two questions, so `local_date`
// and the offset can no longer disagree: they are the same answer read
// two ways.

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use crate::engine::clock::{CivilDay, DayMarker, LocalStart, Zoned};

/// How instants map to calendar days, and what offset is in force, for
/// THIS deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calendar(Kind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fixed { offset_s: i32 },
    Zone(&'static ZoneOps),
}

/// The two questions a real named zone can answer that a fixed offset
/// cannot. Supplied by the server, which owns the timezone database.
///
/// Both are plain `fn` pointers, so a `ZoneOps` is a constant and a
/// `Calendar` holding one stays 16 bytes and `Copy`.
#[derive(Debug)]
pub struct ZoneOps {
    /// The UTC offset in force AT `epoch`, in seconds.
    ///
    /// Per-instant, not per-deployment. A yard on a zone that observes
    /// daylight saving has two offsets in a year, and a seven day
    /// forecast can straddle the change. Sampling once and applying the
    /// result to every forward day is how a November morning gets judged
    /// with an August offset, and under a jurisdiction whose rules differ
    /// by season it picks the wrong RULE SET, not merely the wrong hour.
    pub offset_at: fn(i64) -> Option<i32>,
    /// When a local calendar day begins, including the two cases where
    /// local midnight is not a single instant.
    pub day_start: fn(NaiveDate) -> LocalStart,
}

impl PartialEq for ZoneOps {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for ZoneOps {}

impl Calendar {
    /// A calendar with no offset. Every instant maps to its UTC day. The
    /// default for tests, because it is the one answer that cannot vary
    /// by machine.
    pub const fn utc() -> Self {
        Self(Kind::Fixed { offset_s: 0 })
    }

    /// A calendar at a constant offset from UTC, in SECONDS.
    ///
    /// Seconds rather than hours because half-hour and quarter-hour zones
    /// are real: India is +05:30, Nepal +05:45, the Chatham Islands
    /// +12:45. `None` for an offset outside a day.
    pub const fn fixed_offset(offset_s: i32) -> Option<Self> {
        if offset_s <= -86_400 || offset_s >= 86_400 {
            None
        } else {
            Some(Self(Kind::Fixed { offset_s }))
        }
    }

    /// A calendar backed by a real named zone.
    pub const fn zone(ops: &'static ZoneOps) -> Self {
        Self(Kind::Zone(ops))
    }

    /// The UTC offset in force at `epoch`, in seconds.
    pub fn offset_at(self, epoch: i64) -> Option<i32> {
        match self.0 {
            Kind::Fixed { offset_s } => Some(offset_s),
            Kind::Zone(ops) => (ops.offset_at)(epoch),
        }
    }

    /// Mint a [`Zoned`]: an instant carrying the offset in force at it.
    ///
    /// This is the ONLY producer of a `Zoned`, and a `Zoned` is the only
    /// type that answers `.hour()`. That is the whole type wall: an hour
    /// cannot be read off an instant without going through the
    /// deployment's calendar, so an offset can never be invented next to
    /// an instant that does not have it.
    pub fn at(self, epoch: i64) -> Option<Zoned> {
        Zoned::from_parts(epoch, self.offset_at(epoch)?)
    }

    /// The civil day an instant falls in. Derived from [`Calendar::at`],
    /// so the day and the offset are the same answer read two ways and
    /// cannot drift apart.
    pub fn date_of(self, epoch: i64) -> Option<CivilDay> {
        self.at(epoch).map(Zoned::day)
    }

    /// The one safe operation on a forecast provider's day marker.
    ///
    /// Correct for every shipped provider, because every one of them
    /// stamps an instant that falls inside the day it labels, however
    /// much their chosen hours differ. Reading the hour off a marker is
    /// correct for none of them, which is why [`DayMarker`] does not
    /// offer it.
    pub fn day_of(self, marker: DayMarker) -> Option<CivilDay> {
        self.date_of(marker.provenance_epoch_not_an_instant()?)
    }

    /// When a local calendar day begins.
    pub fn day_start(self, day: CivilDay) -> LocalStart {
        match self.0 {
            Kind::Fixed { offset_s } => match day.naive().and_hms_opt(0, 0, 0) {
                Some(naive) => LocalStart::At(naive.and_utc().timestamp() - offset_s as i64),
                None => LocalStart::Unrepresentable,
            },
            Kind::Zone(ops) => (ops.day_start)(day.naive()),
        }
    }

    /// The `[start, end)` UTC instants of a local calendar day.
    ///
    /// Note the behavior change from the fn-pointer shape: a day whose
    /// local midnight is skipped or repeated by a clock change still
    /// EXISTS, and this now says so. The old shape returned `None` there,
    /// and its two production readers disagreed about what that meant,
    /// one dropping a safety clamp and the other re-arming a dispatch
    /// that had already run.
    pub fn day_bounds_utc(self, day: CivilDay) -> Option<(i64, i64)> {
        let start = self.day_start(day).instant()?;
        let end = self.day_start(day.succ()?).instant()?;
        Some((start, end))
    }

    /// The local calendar day of an instant, as a bare `NaiveDate`.
    ///
    /// Kept for callers that key rows and buckets by date. New code
    /// should prefer [`Calendar::date_of`], whose `CivilDay` cannot be
    /// mistaken for an instant.
    pub fn local_date(self, epoch: i64) -> Option<NaiveDate> {
        self.date_of(epoch).map(CivilDay::naive)
    }

    /// The `[start, end)` bounds of a day as `DateTime<Utc>`.
    ///
    /// Kept for the chrono-shaped callers; same answer as
    /// [`Calendar::day_bounds_utc`].
    pub fn day_bounds_datetime(self, day: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let (start, end) = self.day_bounds_utc(CivilDay::from_naive(day))?;
        Some((
            Utc.timestamp_opt(start, 0).single()?,
            Utc.timestamp_opt(end, 0).single()?,
        ))
    }
}

impl Default for Calendar {
    /// UTC. A caller that forgets to supply the deployment's calendar
    /// gets a deterministic answer rather than whatever zone the process
    /// happens to be running in.
    fn default() -> Self {
        Self::utc()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_utc_calendar_is_the_same_everywhere() {
        let c = Calendar::utc();
        // 03:00 UTC on a known day. In UTC that instant belongs to that
        // day; on a machine west of Greenwich an ambient calendar would
        // call it the day before, which is the whole point of pinning it.
        let day = NaiveDate::from_ymd_opt(2026, 5, 26).unwrap();
        let epoch = day.and_hms_opt(3, 0, 0).unwrap().and_utc().timestamp();
        let d = c.local_date(epoch).expect("representable");
        assert_eq!(d, day);
        let (start, end) = c.day_bounds_datetime(d).expect("representable day");
        assert_eq!(end.timestamp() - start.timestamp(), 86_400);
        assert!(start.timestamp() <= epoch && epoch < end.timestamp());
    }

    /// The constructor the module header promised for months and the old
    /// shape could never supply, because a plain fn pointer captures
    /// nothing. The timezone matrix needs it for the zones that have no
    /// daylight saving to model.
    #[test]
    fn fixed_offset_is_real_now_and_takes_seconds() {
        assert!(Calendar::fixed_offset(5 * 3600 + 1800).is_some(), "+05:30");
        assert!(Calendar::fixed_offset(-7 * 3600).is_some(), "Phoenix");
        assert!(Calendar::fixed_offset(12 * 3600 + 2700).is_some(), "+12:45");
        assert_eq!(Calendar::fixed_offset(86_400), None);
        assert_eq!(Calendar::fixed_offset(-86_400), None);
        assert_eq!(Calendar::utc(), Calendar::fixed_offset(0).expect("valid"));
    }

    /// The reported defect, at the level of the calendar. The same
    /// instant is a banned hour in UTC and a legal one in the yard.
    #[test]
    fn the_offset_travels_with_the_instant() {
        const NWS_SEPT_5: i64 = 1_788_602_400;
        let utc = Calendar::utc();
        let yard = Calendar::fixed_offset(-4 * 3600).expect("valid");

        assert_eq!(utc.at(NWS_SEPT_5).expect("representable").hour(), 10);
        assert_eq!(yard.at(NWS_SEPT_5).expect("representable").hour(), 6);
        // Both agree on the day here; only the hour differs.
        assert_eq!(
            utc.date_of(NWS_SEPT_5),
            yard.date_of(NWS_SEPT_5),
            "5 September either way"
        );
    }

    /// Four providers stamp four different hours on the row for the same
    /// day. The calendar recovers the same day from all of them, which is
    /// the property that makes reading the hour off a marker unnecessary.
    #[test]
    fn every_provider_day_anchor_resolves_to_the_same_civil_day() {
        let yard = Calendar::fixed_offset(-4 * 3600).expect("valid");
        let midnight_local = 1_788_580_800; // 00:00 EDT, Open-Meteo shape
        let expected = yard.date_of(midnight_local).expect("representable");
        for (label, epoch) in [
            ("Open-Meteo 00:00 local", midnight_local),
            ("NWS 06:00 local", midnight_local + 6 * 3600),
            ("met.no 12:00 local", midnight_local + 12 * 3600),
            ("NWS lone night 18:00 local", midnight_local + 18 * 3600),
            ("last second of the day", midnight_local + 86_399),
        ] {
            let marker = DayMarker::inside_local_day(epoch);
            assert_eq!(
                yard.day_of(marker),
                Some(expected),
                "{label} should label the same civil day"
            );
        }
    }

    #[test]
    fn a_fixed_offset_day_starts_at_local_midnight() {
        let kolkata = Calendar::fixed_offset(5 * 3600 + 1800).expect("valid");
        let day = CivilDay::from_naive(NaiveDate::from_ymd_opt(2026, 9, 5).unwrap());
        let start = kolkata.day_start(day).instant().expect("day exists");
        let z = kolkata.at(start).expect("representable");
        assert_eq!(z.hour(), 0);
        assert_eq!(z.minute(), 0);
        assert_eq!(z.day(), day);
        let (s, e) = kolkata.day_bounds_utc(day).expect("bounded");
        assert_eq!(e - s, 86_400);
        assert_eq!(s, start);
    }

    #[test]
    fn an_unknown_marker_resolves_to_no_day() {
        assert_eq!(Calendar::utc().day_of(DayMarker::unknown()), None);
    }
}
