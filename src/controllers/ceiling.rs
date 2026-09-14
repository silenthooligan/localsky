// The per-zone daily valve-open ceiling, checked in one place.
//
// The two-hour cap bounds one command. Nothing bounded a day: a manual
// run after a full smart morning, a retried catch-up, a schedule the
// operator forgot about and the morning it overlaps, each within its
// own cap and together an afternoon of water on one zone. The ceiling
// here is judged against the runs table, which every path writes to,
// so the paths cannot disagree about how much a zone has already had.
// It reduces those rows the way the balance does, as an interval union
// rather than a sum, because one manual run leaves two of them.

use crate::history::rollup::{union_intervals, RunSegment};
use crate::persistence::runs::NewRun;
use crate::persistence::RunsStore;

/// A zone may be open for this many times its single-run cap in one
/// local day. Four full passes is a heavy establishment day; a fifth
/// is a fault somewhere, and the row this writes says so.
pub const DAILY_CEILING_FACTOR: u32 = 4;

/// The ceiling for a zone whose single-run cap is `max_duration_s`.
pub fn daily_ceiling_s(max_duration_s: u32) -> u32 {
    max_duration_s.saturating_mul(DAILY_CEILING_FACTOR)
}

/// What the day has already spent against the ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub used_s: u32,
    pub cap_s: u32,
}

impl Usage {
    pub fn remaining_s(self) -> u32 {
        self.cap_s.saturating_sub(self.used_s)
    }
}

/// The ceiling's answer for a requested run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Run for this many seconds, the request trimmed to what the day
    /// has left when it had to be.
    Allow(u32),
    /// The day is spent. The caller records the refusal as a skip row
    /// through [`record_refusal`] so History says why nothing happened.
    Refuse(Usage),
    /// The allowance cannot be verified. Missing evidence never means unused.
    Unavailable,
}

pub const HISTORY_UNAVAILABLE_REASON: &str =
    "Watering held: today's water use could not be verified from History";

/// Seconds this zone has been (or is committed to be) open today: the
/// interval UNION of the day's rows, never their sum. A manual run is
/// persisted TWICE wherever the controller reads its own state back --
/// the dispatcher's completed row plus the run-edge observer's row for
/// the same physical valve window, a poll apart and so distinct under
/// the runs table's (zone, start, controller) uniqueness -- and adding
/// the two charged the day twice for water that fell once, binding the
/// ceiling at half the water it advertises and telling the owner a
/// figure that was not true.
///
/// What a row contributes, in the order the match tests it. Skip
/// markers and dry-run rows contribute nothing at all.
///   - An `end_epoch` wins where the row has one: start to that end.
///     Every writer but one derives it from the same window as
///     `duration_s`; `mark_aborted` is the exception, stamping the real
///     end and leaving `duration_s` at the stale plan, so taking the
///     end charges what the valve actually did. `watered_since` and the
///     balance's evidence read pick the end first for the same reason,
///     and `mark_aborted` has no production caller today, so nothing
///     yet observes the difference between the two.
///   - No end but a `duration_s`: the whole COMMANDED window, not the
///     seconds elapsed so far. The controller owns the shutoff timer,
///     so that water is already on its way to the ground; a ceiling
///     that waited for it to land would hand out a second run against
///     the same allowance.
///   - Neither: charged as far as `now_epoch`.
/// No shipped path writes an open row today (`insert_running` and
/// `insert_intended` have no callers), so both no-end arms serve legacy
/// rows and whoever wires the intended/running lifecycle the table was
/// built for. Do not read them as describing what is on disk now.
///
/// A row is charged WHOLE to the day it started in, midnight or not.
/// `RunsStore::window` selects on `start_epoch` alone, so tomorrow's
/// read never sees a row that began before its midnight: clipping the
/// tail at the day edge charges those seconds to no day at all and
/// quietly widens tonight's ceiling by however much crossed over. The
/// balance's per-day buckets DO split at midnight, but they are handed
/// the whole multi-day span so both halves land somewhere; this read
/// holds one day's rows and gets no second chance, so it over-charges
/// the starting day, which is the direction that refuses.
pub async fn used_today_s(runs: &RunsStore, slug: &str, now_epoch: i64) -> Option<u32> {
    let cal = crate::timeutil::deployment_calendar();
    let day = cal.date_of(now_epoch)?;
    let (start, end) = cal.day_bounds_utc(day)?;
    let rows = match runs.window(start, end).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(zone = slug, error = %e, "daily ceiling: runs read failed; holding watering");
            return None;
        }
    };
    let segments: Vec<RunSegment> = rows
        .iter()
        .filter(|r| r.zone_slug == slug)
        .filter(|r| !r.source.starts_with("dry_run"))
        .filter(|r| {
            crate::history::rollup::is_watering_evidence(
                &r.source,
                &r.status,
                r.skip_reason.as_deref(),
            ) || matches!(r.status.as_str(), "running" | "intended")
        })
        .map(|r| RunSegment {
            session_id: r.session_id.clone(),
            start_epoch: r.start_epoch,
            // Precedence and reasons: see the note above.
            end_epoch: match (r.end_epoch, r.duration_s) {
                (Some(e), _) => e,
                (None, Some(d)) => r.start_epoch + i64::from(d),
                (None, None) => now_epoch,
            },
        })
        .collect();
    // The union, NOT truncated at the day edge. Clusters are disjoint
    // by construction, so summing their union coverage is the union of
    // the day's rows; a row that crosses local midnight is charged here
    // in full because no other day's read will ever see it.
    Some(
        union_intervals(&segments)
            .iter()
            .map(|e| e.valve_open_s)
            .sum::<i64>()
            .clamp(0, i64::from(u32::MAX)) as u32,
    )
}

/// Judge a requested run against the zone's daily ceiling.
///
/// Without readable history the allowance is unknown, so watering is held.
pub async fn admit(
    runs: Option<&RunsStore>,
    slug: &str,
    cap_s: u32,
    requested_s: u32,
    now_epoch: i64,
) -> Admission {
    let Some(rs) = runs else {
        return Admission::Unavailable;
    };
    let Some(used_s) = used_today_s(rs, slug, now_epoch).await else {
        return Admission::Unavailable;
    };
    let usage = Usage { used_s, cap_s };
    let remaining = usage.remaining_s();
    if remaining == 0 {
        return Admission::Refuse(usage);
    }
    if requested_s > remaining {
        tracing::warn!(
            zone = slug,
            requested_s,
            remaining_s = remaining,
            used_s,
            cap_s,
            "daily ceiling: trimming the run to what the day has left"
        );
    }
    Admission::Allow(requested_s.min(remaining))
}

/// The sentence a refused run leaves in History.
pub fn refusal_reason(usage: Usage) -> String {
    format!(
        "Daily ceiling reached: {} min already today against a {} min limit",
        usage.used_s / 60,
        usage.cap_s / 60
    )
}

/// Record a refused run as a skip row so the day's silence has a reason.
pub async fn record_refusal(
    runs: &RunsStore,
    slug: &str,
    controller_id: &str,
    source: &str,
    requested_s: u32,
    usage: Usage,
    now_epoch: i64,
) {
    record_hold(
        runs,
        slug,
        controller_id,
        source,
        requested_s,
        refusal_reason(usage),
        now_epoch,
    )
    .await;
}

pub async fn record_unavailable(
    runs: &RunsStore,
    slug: &str,
    controller_id: &str,
    source: &str,
    requested_s: u32,
    now_epoch: i64,
) {
    record_hold(
        runs,
        slug,
        controller_id,
        source,
        requested_s,
        HISTORY_UNAVAILABLE_REASON.into(),
        now_epoch,
    )
    .await;
}

async fn record_hold(
    runs: &RunsStore,
    slug: &str,
    controller_id: &str,
    source: &str,
    requested_s: u32,
    reason: String,
    now_epoch: i64,
) {
    let row = NewRun {
        session_id: None,
        zone_slug: slug.to_string(),
        start_epoch: now_epoch,
        source: source.to_string(),
        controller_id: controller_id.to_string(),
        planned_duration_s: requested_s,
        skip_reason: None,
        et0_mm: None,
        etc_mm: None,
        cycle_index: None,
        cycle_count: None,
    };
    if let Err(e) = runs.insert_skipped(row, reason).await {
        tracing::warn!(zone = slug, error = %e, "daily ceiling: refusal row insert failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> RunsStore {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut c).unwrap();
        RunsStore::new(std::sync::Arc::new(tokio::sync::Mutex::new(c)))
    }

    fn row(zone: &str, source: &str, start: i64, secs: u32) -> NewRun {
        NewRun {
            session_id: None,
            zone_slug: zone.into(),
            start_epoch: start,
            source: source.into(),
            controller_id: "os".into(),
            planned_duration_s: secs,
            skip_reason: None,
            et0_mm: None,
            etc_mm: None,
            cycle_index: None,
            cycle_count: None,
        }
    }

    /// A manual run after a full morning is trimmed to what the day has
    /// left, and once the day is spent it is refused with a visible row.
    #[tokio::test]
    async fn a_manual_run_after_a_full_morning_is_capped_with_a_visible_row() {
        let rs = store();
        let now = chrono::Utc::now().timestamp();
        // Noon today, so the day's window holds every row below.
        let cal = crate::timeutil::deployment_calendar();
        let day = cal.date_of(now).unwrap();
        let (day_start, _) = cal.day_bounds_utc(day).unwrap();
        let noon = day_start + 12 * 3600;
        let cap = daily_ceiling_s(3600);
        // A full morning: three completed hours, plus a dry-run row that
        // must not count.
        for i in 0..3 {
            let start = noon - 6 * 3600 + i * 3600;
            rs.insert_completed(
                row("front", "smart_morning", start, 3600),
                start + 3600,
                3600,
                None,
            )
            .await
            .unwrap();
        }
        rs.insert_completed(
            row("front", "dry_run", noon - 3 * 3600, 3600),
            noon - 2 * 3600,
            3600,
            None,
        )
        .await
        .unwrap();
        assert_eq!(used_today_s(&rs, "front", noon).await.unwrap(), 3 * 3600);
        // Another zone's rows are its own business.
        assert_eq!(used_today_s(&rs, "back", noon).await.unwrap(), 0);

        // The fourth hour fits; a request for two is trimmed to one.
        assert_eq!(
            admit(Some(&rs), "front", cap, 7200, noon).await,
            Admission::Allow(3600)
        );
        rs.insert_completed(row("front", "manual", noon, 3600), noon + 3600, 3600, None)
            .await
            .unwrap();
        // The day is spent: refused, and the refusal leaves a row that
        // says so.
        let later = noon + 2 * 3600;
        let Admission::Refuse(usage) = admit(Some(&rs), "front", cap, 600, later).await else {
            panic!("the fifth hour must be refused");
        };
        assert_eq!(
            usage,
            Usage {
                used_s: 4 * 3600,
                cap_s: cap
            }
        );
        record_refusal(&rs, "front", "os", "manual", 600, usage, later).await;
        let rows = rs.window(day_start, day_start + 86_400).await.unwrap();
        let refusal = rows
            .iter()
            .find(|r| r.skip_reason.is_some())
            .expect("a refusal row");
        assert_eq!(
            refusal.skip_reason.as_deref(),
            Some("Daily ceiling reached: 240 min already today against a 240 min limit")
        );
        // A refusal row spends nothing itself.
        assert_eq!(used_today_s(&rs, "front", later).await.unwrap(), 4 * 3600);
    }

    /// One manual run on a controller that reads its own state back
    /// leaves TWO rows for one valve window: the dispatcher's completed
    /// row at the started epoch, and the observer's row for the same
    /// water a poll later. The day may only be charged for the water
    /// that actually fell, or the ceiling binds at half its allowance.
    #[tokio::test]
    async fn a_manual_run_persisted_twice_is_charged_once() {
        let rs = store();
        let now = chrono::Utc::now().timestamp();
        let cal = crate::timeutil::deployment_calendar();
        let day = cal.date_of(now).unwrap();
        let (day_start, _) = cal.day_bounds_utc(day).unwrap();
        // A 20 min zone, so an 80 min day.
        let cap = daily_ceiling_s(20 * 60);
        assert_eq!(cap, 80 * 60);

        // 06:00, smart morning on a readback controller: record_row is
        // false there, so the observer writes the only row.
        let morning = day_start + 6 * 3600;
        rs.insert_observed(
            row("front", "ha_refresher", morning, 1200),
            1200,
            None,
            None,
        )
        .await
        .unwrap();

        // 10:00 and 12:00, hand-started 20 min runs. Each writes the
        // dispatcher's row at dispatch AND the observer's row on the
        // falling edge, one poll later and a poll short.
        for start in [day_start + 10 * 3600, day_start + 12 * 3600] {
            rs.insert_completed(
                row("front", "manual", start, 1200),
                start + 1200,
                1200,
                None,
            )
            .await
            .unwrap();
            rs.insert_observed(
                row("front", "ha_refresher", start + 10, 1190),
                1190,
                None,
                None,
            )
            .await
            .unwrap();
        }

        // Three 20 min runs is 60 min of water, not the 100 min those
        // five rows sum to.
        let afternoon = day_start + 13 * 3600;
        assert_eq!(
            used_today_s(&rs, "front", afternoon).await.unwrap(),
            60 * 60
        );

        // So the day still has its fourth pass to give.
        assert_eq!(
            admit(Some(&rs), "front", cap, 20 * 60, afternoon).await,
            Admission::Allow(20 * 60)
        );
    }

    /// The refusal the owner reads states the figure the refusal was
    /// decided on: four passes of water, not the eight rows they left.
    #[tokio::test]
    async fn a_refusal_states_the_de_duplicated_minutes() {
        let rs = store();
        let now = chrono::Utc::now().timestamp();
        let cal = crate::timeutil::deployment_calendar();
        let day = cal.date_of(now).unwrap();
        let (day_start, _) = cal.day_bounds_utc(day).unwrap();
        let cap = daily_ceiling_s(20 * 60);
        // Four hand-started 20 min runs, each persisted twice: the day
        // is honestly spent, at 80 min and not at 40.
        for i in 0..4 {
            let start = day_start + (6 + 2 * i) * 3600;
            rs.insert_completed(
                row("front", "manual", start, 1200),
                start + 1200,
                1200,
                None,
            )
            .await
            .unwrap();
            rs.insert_observed(
                row("front", "ha_refresher", start + 10, 1190),
                1190,
                None,
                None,
            )
            .await
            .unwrap();
        }
        let evening = day_start + 15 * 3600;
        let Admission::Refuse(usage) = admit(Some(&rs), "front", cap, 600, evening).await else {
            panic!("the fifth pass must be refused");
        };
        assert_eq!(
            usage,
            Usage {
                used_s: 80 * 60,
                cap_s: cap
            }
        );
        assert_eq!(
            refusal_reason(usage),
            "Daily ceiling reached: 80 min already today against a 80 min limit"
        );
    }

    /// A run started before local midnight is charged WHOLE to the day
    /// it began. The runs query selects on `start_epoch`, so tomorrow's
    /// read never sees the row; clipping its tail at midnight charged
    /// those seconds to no day at all and handed the rest of the night
    /// headroom the zone had already spent.
    #[tokio::test]
    async fn a_run_crossing_midnight_is_charged_whole_to_the_day_it_started() {
        let rs = store();
        let now = chrono::Utc::now().timestamp();
        let cal = crate::timeutil::deployment_calendar();
        let day = cal.date_of(now).unwrap();
        let (day_start, day_end) = cal.day_bounds_utc(day).unwrap();
        // A 2 h zone, so an 8 h day.
        let cap = daily_ceiling_s(2 * 3600);
        assert_eq!(cap, 8 * 3600);

        // Three 2 h passes through the day: six of the eight hours.
        for i in 0..3 {
            let start = day_start + (6 + 4 * i) * 3600;
            rs.insert_completed(
                row("front", "manual", start, 7200),
                start + 7200,
                7200,
                None,
            )
            .await
            .unwrap();
        }

        // A fourth dispatched an hour before local midnight. The
        // dispatcher writes end = start + planned at command time, so
        // an hour of this row lies on the far side of midnight.
        let late = day_end - 3600;
        rs.insert_completed(row("front", "manual", late, 7200), late + 7200, 7200, None)
            .await
            .unwrap();

        // Half an hour in, the whole two hours counts and the day is
        // spent. Clipped at midnight it read seven hours, and the
        // ceiling let another pass through on the strength of it.
        let half_past = late + 1800;
        assert_eq!(
            used_today_s(&rs, "front", half_past).await.unwrap(),
            8 * 3600
        );
        assert_eq!(
            admit(Some(&rs), "front", cap, 900, half_past).await,
            Admission::Refuse(Usage {
                used_s: 8 * 3600,
                cap_s: cap
            })
        );

        // And tomorrow does not inherit the tail: the row started
        // before tomorrow's midnight, so tomorrow's window never
        // selects it. That is exactly why today must charge it all.
        assert_eq!(used_today_s(&rs, "front", day_end + 1800).await.unwrap(), 0);
    }

    /// A run still in flight is charged the window it was COMMANDED
    /// for, not the seconds elapsed so far -- the controller owns the
    /// shutoff timer, so that water is already on its way down -- and
    /// that window is not clipped at midnight either. Once the run is
    /// over, the row's own end is what the day is charged.
    #[tokio::test]
    async fn an_in_flight_run_charges_its_commanded_window() {
        let rs = store();
        let now = chrono::Utc::now().timestamp();
        let cal = crate::timeutil::deployment_calendar();
        let day = cal.date_of(now).unwrap();
        let (day_start, day_end) = cal.day_bounds_utc(day).unwrap();
        // A 1 h zone, so a 4 h day.
        let cap = daily_ceiling_s(3600);
        assert_eq!(cap, 4 * 3600);

        // Two completed hours earlier in the day.
        for i in 0..2 {
            let start = day_start + (6 + 4 * i) * 3600;
            rs.insert_completed(
                row("front", "manual", start, 3600),
                start + 3600,
                3600,
                None,
            )
            .await
            .unwrap();
        }

        // A third commanded an hour before midnight and still open:
        // end_epoch NULL, duration_s the planned two hours.
        let late = day_end - 3600;
        let id = rs
            .insert_running(row("front", "manual", late, 7200))
            .await
            .unwrap();

        // Ten minutes in, the day is committed to the whole two hours,
        // which spends the ceiling exactly. Counting the 600 s elapsed,
        // or clipping the hour past midnight, leaves room for another
        // pass tonight that the zone has no allowance for.
        let ten_min_in = late + 600;
        assert_eq!(
            used_today_s(&rs, "front", ten_min_in).await.unwrap(),
            4 * 3600
        );
        assert_eq!(
            admit(Some(&rs), "front", cap, 900, ten_min_in).await,
            Admission::Refuse(Usage {
                used_s: 4 * 3600,
                cap_s: cap
            })
        );

        // Cut short at fifteen minutes. `mark_aborted` stamps the real
        // end and leaves duration_s at the stale plan, so the end wins
        // and the day is charged what the valve actually did -- the
        // same precedence the balance's evidence read uses.
        rs.mark_aborted(id, late + 900).await.unwrap();
        assert_eq!(
            used_today_s(&rs, "front", late + 1800).await.unwrap(),
            2 * 3600 + 900
        );
    }

    /// Missing persistence cannot certify an unused allowance.
    #[tokio::test]
    async fn missing_history_holds_the_request() {
        assert_eq!(
            admit(None, "front", 100, 900, 0).await,
            Admission::Unavailable
        );
    }

    #[tokio::test]
    async fn unreadable_history_is_not_an_empty_day() {
        let broken = RunsStore::new(std::sync::Arc::new(tokio::sync::Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        )));
        assert_eq!(
            admit(Some(&broken), "front", 7200, 900, 1_700_000_000).await,
            Admission::Unavailable
        );
        let empty = store();
        assert_eq!(
            admit(Some(&empty), "front", 7200, 900, 1_700_000_000).await,
            Admission::Allow(900)
        );
    }
}
