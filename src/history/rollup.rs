// Shared watering-evidence rollup. ONE definition of "which run rows
// are watering evidence" and "how overlapping rows reduce to events",
// compiled for BOTH the server (the balance's applied-irrigation term,
// the tuning report) and the WASM client (the history page and zone
// detail day buckets), so no minutes surface can disagree with what the
// balance credits. engine::tuning re-exports these for its callers.

use crate::history::types::RunRecord;

/// Segments closer than this gap (previous end to next start) merge
/// into one irrigation event: cycle-soak splits one morning's watering
/// into several valve-open windows.
pub const EVENT_CLUSTER_GAP_S: i64 = 2 * 3600;

/// One valve-open interval from the runs history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunSegment {
    pub start_epoch: i64,
    pub end_epoch: i64,
}

/// A same-morning cluster of run segments: one irrigation event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IrrigationEvent {
    /// First segment's start.
    pub start_epoch: i64,
    /// Last segment's end.
    pub end_epoch: i64,
    /// Interval-UNION valve-open time across the clustered segments
    /// (seconds). See `cluster_events`.
    pub valve_open_s: i64,
}

/// Watering evidence per the run-history semantics: run-edge observer
/// rows (source ha_refresher) plus manual API/scheduler rows. Skip
/// markers, dry-run rows, and the transient intended/running states
/// never count. `controller_external` (the never-wired run_history
/// backfill) stays excluded until an ingest path actually produces it;
/// revisit this filter when wiring one.
pub fn is_watering_row(source: &str, status: &str) -> bool {
    status == "completed"
        && (source == "ha_refresher" || source == "manual" || source.starts_with("manual:"))
}

/// [`is_watering_row`] plus the LEGACY fallbacks, the form every real
/// consumer uses. Two legacy shapes exist:
///   - wire rows from a pre-1.21 server carry EMPTY source/status
///     (additive serde defaults);
///   - v0.1 rows migrated by M0003 carry source 'unknown' with status
///     'completed' on watering AND skip rows alike.
/// Both are judged by the historical skip_reason test (a skip marker
/// carries a reason; watering does not). 'unknown' is never
/// whitelisted wholesale: without the skip_reason consult, migrated
/// skip markers would count as watering.
pub fn is_watering_evidence(source: &str, status: &str, skip_reason: Option<&str>) -> bool {
    let legacy_source = source.is_empty() || source == "unknown";
    let legacy_status = status.is_empty() || status == "completed";
    if legacy_source && legacy_status {
        return skip_reason.is_none();
    }
    is_watering_row(source, status)
}

/// The record-level form of [`is_watering_evidence`] for wire
/// `RunRecord`s.
pub fn is_watering_record(r: &RunRecord) -> bool {
    is_watering_evidence(&r.source, &r.status, r.skip_reason.as_deref())
}

/// Cluster watering segments into same-morning irrigation events.
/// Segments closer than `EVENT_CLUSTER_GAP_S` (measured from the previous
/// segment's end to the next segment's start) merge into one event.
///
/// valve_open_s is the interval-UNION coverage of the cluster, not a raw
/// sum: a manual run is persisted twice (the manual completed row plus
/// the run-edge observer's row for the same physical valve activity),
/// and summing both would double the minutes and halve every backed-out
/// rate. Segments are sorted by start, so counting only the portion past
/// the cluster's current end is exactly the union length; disjoint
/// cycle/soak observer segments still sum as before.
pub fn cluster_events(segments: &[RunSegment]) -> Vec<IrrigationEvent> {
    let mut segs: Vec<RunSegment> = segments
        .iter()
        .copied()
        .filter(|s| s.end_epoch >= s.start_epoch)
        .collect();
    segs.sort_by_key(|s| s.start_epoch);
    let mut events: Vec<IrrigationEvent> = Vec::new();
    for s in segs {
        match events.last_mut() {
            Some(ev) if s.start_epoch - ev.end_epoch <= EVENT_CLUSTER_GAP_S => {
                ev.valve_open_s += (s.end_epoch - s.start_epoch.max(ev.end_epoch)).max(0);
                ev.end_epoch = ev.end_epoch.max(s.end_epoch);
            }
            _ => events.push(IrrigationEvent {
                start_epoch: s.start_epoch,
                end_epoch: s.end_epoch,
                valve_open_s: s.end_epoch - s.start_epoch,
            }),
        }
    }
    events
}

/// The applied-irrigation evidence inside a window: union valve-open
/// seconds and event count. Segments are pre-truncated to
/// `[window_start, window_end]` BEFORE clustering (the union of
/// truncated segments equals the truncated union), so a cycle-soak
/// event straddling the window edge contributes only its in-window
/// coverage. Gross depth = valve_open_s / 3600 x throughput_mm_hr,
/// applied by the caller (no capture factor: gross-in credit against a
/// gross weekly target).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WindowedApplied {
    pub valve_open_s: i64,
    pub events: u32,
}

pub fn applied_in_window(
    segments: &[RunSegment],
    window_start: i64,
    window_end: i64,
) -> WindowedApplied {
    let truncated: Vec<RunSegment> = segments
        .iter()
        .filter_map(|s| {
            let start = s.start_epoch.max(window_start);
            let end = s.end_epoch.min(window_end);
            (end > start).then_some(RunSegment {
                start_epoch: start,
                end_epoch: end,
            })
        })
        .collect();
    let events = cluster_events(&truncated);
    WindowedApplied {
        valve_open_s: events.iter().map(|e| e.valve_open_s).sum(),
        events: events.len() as u32,
    }
}

/// Union valve-open evidence bucketed per day: `applied_in_window` run
/// against each frame in `day_bounds` (epoch pairs, one per local day,
/// typically midnight-to-midnight in the configured timezone). A run
/// straddling a boundary splits at it, so each day counts only its own
/// coverage and the per-day figures sum to the whole-window union;
/// duplicate manual + observer rows still count once inside each day.
/// The soil replay multiplies each day's valve seconds by throughput
/// and capture efficiency to get that day's net applied depth.
///
/// Day frames come from the caller: this module compiles for the WASM
/// client too, where the configured-timezone clock helpers do not
/// exist. The `events` count in each bucket describes that day alone,
/// so a straddling run counts as an event on both sides of midnight;
/// consumers wanting event counts should use the whole-window read.
pub fn applied_per_day(segments: &[RunSegment], day_bounds: &[(i64, i64)]) -> Vec<WindowedApplied> {
    day_bounds
        .iter()
        .map(|&(start, end)| applied_in_window(segments, start, end))
        .collect()
}

/// Union-clustered watering events per zone from wire run records.
/// The minutes any surface derives from these agree with the balance's
/// applied-irrigation credit (same filter, same union), so the history
/// charts can never show more watering than the balance counts.
pub fn watering_events_per_zone(
    runs: &[RunRecord],
) -> std::collections::BTreeMap<String, Vec<IrrigationEvent>> {
    let mut segments: std::collections::BTreeMap<String, Vec<RunSegment>> =
        std::collections::BTreeMap::new();
    for r in runs {
        if !is_watering_record(r) {
            continue;
        }
        segments
            .entry(r.zone.replace('-', "_"))
            .or_default()
            .push(RunSegment {
                start_epoch: r.start_epoch,
                end_epoch: r.start_epoch + r.duration_s.max(0),
            });
    }
    segments
        .into_iter()
        .map(|(zone, segs)| (zone, cluster_events(&segs)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: i64, dur: i64) -> RunSegment {
        RunSegment {
            start_epoch: start,
            end_epoch: start + dur,
        }
    }

    /// Window-edge clamp: an event straddling the window start counts
    /// only its in-window coverage; one fully outside contributes
    /// nothing; overlapping manual + observer rows still count once.
    #[test]
    fn applied_in_window_clamps_to_the_window_edges() {
        let w_start = 100_000;
        let w_end = 200_000;
        let segments = [
            // Straddles the start: 600 of 1200 seconds inside.
            seg(w_start - 600, 1200),
            // Fully inside.
            seg(w_start + 10_000, 900),
            // The same physical run persisted twice (manual + observer).
            seg(w_start + 50_000, 1200),
            seg(w_start + 50_010, 1200),
            // Fully before the window.
            seg(w_start - 90_000, 3600),
            // Straddles the end: 300 inside.
            seg(w_end - 300, 900),
        ];
        let a = applied_in_window(&segments, w_start, w_end);
        // 600 + 900 + union(1210) + 300
        assert_eq!(a.valve_open_s, 600 + 900 + 1210 + 300);
        assert_eq!(a.events, 4);
    }

    #[test]
    fn applied_in_window_empty_and_disjoint() {
        assert_eq!(applied_in_window(&[], 0, 1000), WindowedApplied::default());
        let outside = [seg(5000, 100)];
        assert_eq!(
            applied_in_window(&outside, 0, 1000),
            WindowedApplied::default()
        );
    }

    /// The per-day split preserves the whole-window union: a run
    /// straddling midnight contributes its pre-midnight seconds to the
    /// first day and the rest to the second, duplicate manual +
    /// observer rows count once per day, and the day figures sum to the
    /// single-window figure for the same segments.
    #[test]
    fn applied_per_day_splits_at_the_boundary_and_sums_to_the_window() {
        let d0 = 100_000; // day frames: [d0,d1), [d1,d2)
        let d1 = d0 + 86_400;
        let d2 = d1 + 86_400;
        let segments = [
            // Straddles midnight: 600 s on each side.
            seg(d1 - 600, 1200),
            // The same physical run persisted twice (manual + observer),
            // fully inside day one.
            seg(d0 + 10_000, 1200),
            seg(d0 + 10_010, 1200),
            // A cycle-soak pair inside day two.
            seg(d1 + 40_000, 600),
            seg(d1 + 42_000, 600),
            // Fully outside both frames.
            seg(d2 + 5_000, 900),
        ];
        let days = applied_per_day(&segments, &[(d0, d1), (d1, d2)]);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].valve_open_s, 1210 + 600, "union pair + straddle");
        assert_eq!(days[1].valve_open_s, 600 + 600 + 600);
        let whole = applied_in_window(&segments, d0, d2);
        assert_eq!(
            days[0].valve_open_s + days[1].valve_open_s,
            whole.valve_open_s,
            "day figures sum to the window union"
        );
    }

    /// Empty inputs stay empty on both axes: no frames yields no
    /// buckets, and a frame with no coverage yields a zero bucket.
    #[test]
    fn applied_per_day_empty_inputs() {
        assert!(applied_per_day(&[seg(0, 100)], &[]).is_empty());
        let days = applied_per_day(&[], &[(0, 86_400)]);
        assert_eq!(days, vec![WindowedApplied::default()]);
    }

    #[test]
    fn watering_record_fallback_and_dry_run_exclusion() {
        let watering = RunRecord {
            zone: "back_yard".into(),
            start_epoch: 0,
            duration_s: 600,
            skip_reason: None,
            source: "ha_refresher".into(),
            status: "completed".into(),
        };
        assert!(is_watering_record(&watering));
        let dry = RunRecord {
            source: "dry_run".into(),
            ..watering.clone()
        };
        assert!(!is_watering_record(&dry), "pretend water never counts");
        let skip = RunRecord {
            source: "smart_morning".into(),
            status: "skipped".into(),
            skip_reason: Some("rain".into()),
            ..watering.clone()
        };
        assert!(!is_watering_record(&skip));
        // Legacy wire rows (no source/status): the historical rule.
        let legacy = RunRecord {
            source: String::new(),
            status: String::new(),
            ..watering.clone()
        };
        assert!(is_watering_record(&legacy));
        let legacy_skip = RunRecord {
            skip_reason: Some("wind".into()),
            ..legacy.clone()
        };
        assert!(!is_watering_record(&legacy_skip));
        // v0.1 rows migrated by M0003: source 'unknown' with status
        // 'completed' on BOTH watering and skip rows, so only the
        // skip_reason distinguishes them. Watering counts; skips do not.
        let migrated = RunRecord {
            source: "unknown".into(),
            status: "completed".into(),
            ..watering.clone()
        };
        assert!(is_watering_record(&migrated), "migrated watering counts");
        let migrated_skip = RunRecord {
            skip_reason: Some("Rain expected within 4h".into()),
            ..migrated.clone()
        };
        assert!(
            !is_watering_record(&migrated_skip),
            "migrated skip markers never count as watering"
        );
        // 'unknown' is not whitelisted wholesale: a non-completed status
        // stays excluded.
        let migrated_aborted = RunRecord {
            source: "unknown".into(),
            status: "aborted".into(),
            ..watering.clone()
        };
        assert!(!is_watering_record(&migrated_aborted));
    }

    /// Surface agreement on a manual+observer fixture: the minutes any
    /// history surface derives from `watering_events_per_zone` equal the
    /// valve seconds `applied_in_window` credits to the balance for the
    /// same rows and window. One filter, one clustering, one total.
    #[test]
    fn history_buckets_equal_the_balance_credit() {
        let mk = |start: i64, dur: i64, source: &str| RunRecord {
            zone: "back_yard".into(),
            start_epoch: start,
            duration_s: dur,
            skip_reason: None,
            source: source.into(),
            status: "completed".into(),
        };
        let runs = vec![
            // A manual run and its observer twin.
            mk(10_000, 1200, "manual"),
            mk(10_010, 1200, "ha_refresher"),
            // A cycle-soak morning: three observer segments.
            mk(200_000, 600, "ha_refresher"),
            mk(201_800, 600, "ha_refresher"),
            mk(203_600, 600, "ha_refresher"),
            // Pretend water: excluded on both sides.
            mk(300_000, 900, "dry_run"),
        ];
        let chart_seconds: i64 = watering_events_per_zone(&runs)
            .values()
            .flatten()
            .map(|e| e.valve_open_s)
            .sum();
        let segments: Vec<RunSegment> = runs
            .iter()
            .filter(|r| is_watering_record(r))
            .map(|r| RunSegment {
                start_epoch: r.start_epoch,
                end_epoch: r.start_epoch + r.duration_s,
            })
            .collect();
        let balance = applied_in_window(&segments, 0, 1_000_000);
        assert_eq!(chart_seconds, balance.valve_open_s);
        assert_eq!(chart_seconds, 1210 + 1800, "union pair + three segments");
        assert_eq!(balance.events, 2);
    }

    #[test]
    fn per_zone_events_union_duplicate_rows() {
        let mk = |zone: &str, start: i64, dur: i64, source: &str| RunRecord {
            zone: zone.into(),
            start_epoch: start,
            duration_s: dur,
            skip_reason: None,
            source: source.into(),
            status: "completed".into(),
        };
        let runs = vec![
            mk("back_yard", 1_000, 1200, "manual"),
            mk("back_yard", 1_010, 1200, "ha_refresher"),
            mk("front_yard", 1_000, 600, "ha_refresher"),
            mk("front_yard", 500_000, 600, "ha_refresher"),
        ];
        let by_zone = watering_events_per_zone(&runs);
        let back = &by_zone["back_yard"];
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].valve_open_s, 1210, "union, not the 2400 sum");
        assert_eq!(by_zone["front_yard"].len(), 2);
    }
}
