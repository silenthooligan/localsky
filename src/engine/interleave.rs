// Cycle/soak interleaving planner. Pure scheduling math for the smart-morning
// dispatcher: given each zone's cycle-and-soak plan (engine::cycle_soak), lay
// the segments out on the single shared valve timeline.
//
// Two policies:
//   * Serial: the legacy layout. Zone 1 runs ALL its segments (idling through
//     every soak) before zone 2 starts; an inter-zone preamble separates
//     zones. Must reproduce the legacy dispatcher's arithmetic exactly.
//   * Interleaved (opt-in via engine.interleave_cycles): other zones' cycles
//     run DURING a zone's soak window (classic OpenSprinkler-style
//     interleaving), shortening the total wall time. Still strictly one valve
//     at a time; ControllerCaps.multi_zone_parallel is deliberately not
//     consulted (parallel valves are out of scope).
//
// Invariants the interleaved policy guarantees (unit-tested below):
//   * no two runs overlap; zone switches keep the preamble gap;
//   * every soak is honored as a MINIMUM (it may stretch, never shrink);
//   * per-zone segment order is preserved;
//   * makespan never exceeds the serial makespan;
//   * a single zone (or a plan with no soaks) degenerates to the serial
//     layout.
//
// Selection rule: greedy earliest-start simulation. Among zones with segments
// remaining, dispatch the one whose next segment can START earliest, where
// start = max(zone ready time, valve free time, + preamble when switching
// zones); ties break on earliest ready time, then zone order. With real soak
// gaps (>= 1 min, always longer than the 2 s preamble) this is identical to
// picking the earliest-READY zone; the start-time form additionally keeps a
// zero-soak zone's segments contiguous (there is no idle window to give
// away), which is what makes the makespan bound hold in that degenerate
// shape.

use crate::engine::cycle_soak::CycleSegment;

/// One zone's cycle-and-soak plan, tagged with the caller's zone index (the
/// dispatcher's dispatch-list position; opaque to the planner beyond
/// identity). Zone ORDER for tie-breaks is the slice order.
#[derive(Debug, Clone)]
pub struct ZonePlan {
    pub zone_idx: usize,
    pub segments: Vec<CycleSegment>,
}

/// One planned valve opening. `start_offset_s` is seconds from sequence
/// start; the executor treats planned offsets as estimates and re-derives
/// real times from the dispatch clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub zone_idx: usize,
    pub seg_idx: usize,
    pub run_seconds: u32,
    pub start_offset_s: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Serial,
    Interleaved,
}

/// Lay the zones' segments out on the shared valve timeline under `policy`.
/// Steps are returned in dispatch order (non-decreasing start offsets).
pub fn plan(zones: &[ZonePlan], policy: Policy, preamble_s: u64) -> Vec<Step> {
    match policy {
        Policy::Serial => {
            let mut out = Vec::new();
            let mut t: u64 = 0;
            let mut first = true;
            for zp in zones {
                if zp.segments.is_empty() {
                    continue;
                }
                if !first {
                    t += preamble_s;
                }
                first = false;
                for (i, seg) in zp.segments.iter().enumerate() {
                    out.push(Step {
                        zone_idx: zp.zone_idx,
                        seg_idx: i,
                        run_seconds: seg.run_seconds,
                        start_offset_s: t,
                    });
                    t += seg.run_seconds as u64 + seg.soak_seconds as u64;
                }
            }
            out
        }
        Policy::Interleaved => {
            let total: usize = zones.iter().map(|z| z.segments.len()).sum();
            let mut next_seg: Vec<usize> = vec![0; zones.len()];
            let mut ready: Vec<u64> = vec![0; zones.len()];
            let mut valve_free: u64 = 0;
            // Position (in `zones`) of the previously dispatched step, for
            // the switch-only preamble.
            let mut last: Option<usize> = None;
            let mut out = Vec::with_capacity(total);
            for _ in 0..total {
                // Lexicographic min over (start, ready, position) encodes
                // the selection rule + both tie-breaks.
                let mut best: Option<(u64, u64, usize)> = None;
                for (pos, zp) in zones.iter().enumerate() {
                    if next_seg[pos] >= zp.segments.len() {
                        continue;
                    }
                    let base = match last {
                        Some(l) if l != pos => valve_free + preamble_s,
                        _ => valve_free,
                    };
                    let key = (ready[pos].max(base), ready[pos], pos);
                    if best.is_none_or(|b| key < b) {
                        best = Some(key);
                    }
                }
                let Some((start, _, pos)) = best else {
                    break;
                };
                let seg = zones[pos].segments[next_seg[pos]];
                out.push(Step {
                    zone_idx: zones[pos].zone_idx,
                    seg_idx: next_seg[pos],
                    run_seconds: seg.run_seconds,
                    start_offset_s: start,
                });
                let end = start + seg.run_seconds as u64;
                ready[pos] = end + seg.soak_seconds as u64;
                valve_free = end;
                last = Some(pos);
                next_seg[pos] += 1;
            }
            out
        }
    }
}

/// End of the last step (its start + run), i.e. when the final valve closes.
/// Trailing soak is excluded: split() always ends a plan on soak 0, and the
/// water is on the ground once the last run finishes.
pub fn makespan_s(steps: &[Step]) -> u64 {
    steps
        .iter()
        .map(|s| s.start_offset_s + s.run_seconds as u64)
        .max()
        .unwrap_or(0)
}

/// (first start offset, last end offset) of one zone's steps, or None when
/// the zone has no steps in the plan.
pub fn zone_span_s(steps: &[Step], zone_idx: usize) -> Option<(u64, u64)> {
    let mut span: Option<(u64, u64)> = None;
    for s in steps.iter().filter(|s| s.zone_idx == zone_idx) {
        let end = s.start_offset_s + s.run_seconds as u64;
        span = Some(match span {
            None => (s.start_offset_s, end),
            Some((first, last)) => (first.min(s.start_offset_s), last.max(end)),
        });
    }
    span
}

/// Replay the executor's dynamic timing rule over the REMAINING steps of a
/// mid-flight sequence, projecting when `target_zone`'s last remaining step
/// ENDS (valve commanded off). Used to arm/extend the per-zone whole-cycle
/// shutoff deadline from live state instead of the stale planned offsets.
///
/// `remaining` is (zone_idx, run_seconds, soak_seconds) in EXECUTION order
/// (the executor follows planned order, so this is a replay, not a re-pick).
/// Offsets are seconds from "now": `zone_ready_in_s` gives each zone's
/// earliest next-dispatch offset (absent zones count as 0). The first
/// remaining step is the one about to dispatch, so its zone's ready offset
/// should be 0 and no preamble is charged before it. Returns None when the
/// zone has no remaining steps.
pub fn project_zone_end(
    remaining: &[(usize, u32, u32)],
    zone_ready_in_s: &[(usize, u64)],
    preamble_s: u64,
    target_zone: usize,
) -> Option<u64> {
    let mut ready: Vec<(usize, u64)> = zone_ready_in_s.to_vec();
    let mut valve_free: u64 = 0;
    let mut last: Option<usize> = None;
    let mut target_end: Option<u64> = None;
    for &(zone, run, soak) in remaining {
        let zone_ready = ready
            .iter()
            .find(|(z, _)| *z == zone)
            .map(|(_, r)| *r)
            .unwrap_or(0);
        let base = match last {
            Some(l) if l != zone => valve_free + preamble_s,
            _ => valve_free,
        };
        let start = zone_ready.max(base);
        let end = start + run as u64;
        if zone == target_zone {
            target_end = Some(end);
        }
        let next_ready = end + soak as u64;
        match ready.iter_mut().find(|(z, _)| *z == zone) {
            Some(slot) => slot.1 = next_ready,
            None => ready.push((zone, next_ready)),
        }
        valve_free = end;
        last = Some(zone);
    }
    target_end
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRE: u64 = 2;

    fn seg(run: u32, soak: u32) -> CycleSegment {
        CycleSegment {
            run_seconds: run,
            soak_seconds: soak,
        }
    }

    fn zp(zone_idx: usize, segs: &[(u32, u32)]) -> ZonePlan {
        ZonePlan {
            zone_idx,
            segments: segs.iter().map(|&(r, s)| seg(r, s)).collect(),
        }
    }

    /// Every invariant the interleaved policy promises: no overlap (with the
    /// preamble on zone switches), per-zone segment order + soak minimums,
    /// and totals preserved (every segment appears exactly once).
    fn assert_valid(steps: &[Step], zones: &[ZonePlan], preamble: u64) {
        for w in steps.windows(2) {
            let prev_end = w[0].start_offset_s + w[0].run_seconds as u64;
            let min_gap = if w[0].zone_idx == w[1].zone_idx {
                0
            } else {
                preamble
            };
            assert!(
                w[1].start_offset_s >= prev_end + min_gap,
                "overlap/preamble violation: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
        for zone in zones {
            let zsteps: Vec<&Step> = steps
                .iter()
                .filter(|s| s.zone_idx == zone.zone_idx)
                .collect();
            assert_eq!(zsteps.len(), zone.segments.len(), "segment count");
            for (k, s) in zsteps.iter().enumerate() {
                assert_eq!(s.seg_idx, k, "segment order");
                assert_eq!(s.run_seconds, zone.segments[k].run_seconds, "run length");
            }
            for k in 0..zsteps.len().saturating_sub(1) {
                let end_k = zsteps[k].start_offset_s + zsteps[k].run_seconds as u64;
                let min_next = end_k + zone.segments[k].soak_seconds as u64;
                assert!(
                    zsteps[k + 1].start_offset_s >= min_next,
                    "soak minimum shrunk for zone {} between segments {} and {}",
                    zone.zone_idx,
                    k,
                    k + 1
                );
            }
        }
    }

    // Serial offsets must match the legacy dispatcher arithmetic exactly:
    // within a zone, consecutive segments are separated by run + soak; a
    // preamble separates zones; no preamble before the first zone.
    #[test]
    fn serial_matches_legacy_arithmetic() {
        let zones = vec![
            zp(0, &[(600, 1800), (600, 0)]),
            zp(1, &[(300, 0)]),
            zp(2, &[(120, 600), (120, 600), (120, 0)]),
        ];
        let steps = plan(&zones, Policy::Serial, PRE);
        let offsets: Vec<(usize, usize, u64)> = steps
            .iter()
            .map(|s| (s.zone_idx, s.seg_idx, s.start_offset_s))
            .collect();
        assert_eq!(
            offsets,
            vec![
                (0, 0, 0),
                (0, 1, 2400), // 600 run + 1800 soak
                (1, 0, 3002), // zone 0 ends 3000, + preamble
                (2, 0, 3304), // zone 1 ends 3302, + preamble
                (2, 1, 4024), // 120 + 600
                (2, 2, 4744),
            ]
        );
        // Legacy total: runs + soaks + preamble * (zones - 1).
        assert_eq!(makespan_s(&steps), 1860 + 3000 + 2 * PRE);
        assert_eq!(zone_span_s(&steps, 0), Some((0, 3000)));
        assert_eq!(zone_span_s(&steps, 2), Some((3304, 4864)));
        assert_eq!(zone_span_s(&steps, 9), None);
    }

    // A soak window long enough to host another zone's whole cycle: the
    // interleaved makespan must be strictly shorter than serial, with exact
    // greedy placement.
    #[test]
    fn interleave_hosts_cycle_inside_soak() {
        let zones = vec![zp(0, &[(900, 1800), (900, 0)]), zp(1, &[(600, 0)])];
        let serial = plan(&zones, Policy::Serial, PRE);
        let inter = plan(&zones, Policy::Interleaved, PRE);
        assert_valid(&inter, &zones, PRE);
        let offsets: Vec<(usize, usize, u64)> = inter
            .iter()
            .map(|s| (s.zone_idx, s.seg_idx, s.start_offset_s))
            .collect();
        // Zone 1's run fits inside zone 0's soak (902..1502 within 900..2700).
        assert_eq!(offsets, vec![(0, 0, 0), (1, 0, 902), (0, 1, 2700)]);
        assert_eq!(makespan_s(&inter), 3600);
        assert_eq!(makespan_s(&serial), 900 + 1800 + 900 + PRE + 600);
        assert!(makespan_s(&inter) < makespan_s(&serial));
    }

    // A guest run longer than the soak stretches the soak (never shrinks it)
    // and still beats serial.
    #[test]
    fn interleave_soak_stretches_for_long_guest() {
        let zones = vec![zp(0, &[(600, 300), (600, 0)]), zp(1, &[(1200, 0)])];
        let serial = plan(&zones, Policy::Serial, PRE);
        let inter = plan(&zones, Policy::Interleaved, PRE);
        assert_valid(&inter, &zones, PRE);
        // Zone 1 runs 602..1802; zone 0's second segment waits for the valve
        // + preamble (1804) even though its soak expired at 1500.
        let z0_second = inter
            .iter()
            .find(|s| s.zone_idx == 0 && s.seg_idx == 1)
            .unwrap();
        assert_eq!(z0_second.start_offset_s, 1804);
        assert!(makespan_s(&inter) <= makespan_s(&serial));
    }

    // Zero-soak segments leave no idle window to give away: the zone stays
    // contiguous and the interleaved plan equals the serial plan.
    #[test]
    fn interleave_zero_soak_stays_contiguous() {
        let zones = vec![zp(0, &[(600, 0), (600, 0)]), zp(1, &[(600, 0)])];
        let serial = plan(&zones, Policy::Serial, PRE);
        let inter = plan(&zones, Policy::Interleaved, PRE);
        assert_eq!(inter, serial);
        assert_eq!(makespan_s(&inter), makespan_s(&serial));
    }

    // A single zone degenerates identically under both policies.
    #[test]
    fn single_zone_identical_under_both_policies() {
        let zones = vec![zp(0, &[(600, 1800), (600, 1800), (600, 0)])];
        let serial = plan(&zones, Policy::Serial, PRE);
        let inter = plan(&zones, Policy::Interleaved, PRE);
        assert_eq!(inter, serial);
        assert_eq!(makespan_s(&serial), 3 * 600 + 2 * 1800);
    }

    // All-single-segment plans (no soak anywhere) reproduce the serial zone
    // order and spacing under the interleaved policy.
    #[test]
    fn interleave_no_soak_anywhere_matches_serial() {
        let zones = vec![zp(0, &[(600, 0)]), zp(1, &[(600, 0)]), zp(2, &[(600, 0)])];
        let serial = plan(&zones, Policy::Serial, PRE);
        let inter = plan(&zones, Policy::Interleaved, PRE);
        assert_eq!(inter, serial);
        let starts: Vec<u64> = inter.iter().map(|s| s.start_offset_s).collect();
        assert_eq!(starts, vec![0, 602, 1204]);
    }

    // Property battery across mixed shapes: invariants hold and the
    // interleaved makespan never exceeds serial.
    #[test]
    fn interleave_makespan_never_exceeds_serial() {
        let shapes: Vec<Vec<ZonePlan>> = vec![
            vec![zp(0, &[(900, 1800), (900, 0)]), zp(1, &[(600, 0)])],
            vec![zp(0, &[(600, 60), (600, 0)]), zp(1, &[(600, 0)])],
            vec![zp(0, &[(600, 300), (600, 0)]), zp(1, &[(1200, 0)])],
            vec![
                zp(0, &[(900, 1800), (900, 0)]),
                zp(1, &[(300, 600), (300, 0)]),
                zp(2, &[(400, 0)]),
            ],
            vec![
                zp(0, &[(120, 600), (120, 600), (120, 0)]),
                zp(1, &[(120, 600), (120, 600), (120, 0)]),
            ],
            vec![zp(0, &[]), zp(1, &[(600, 0)])],
        ];
        for zones in &shapes {
            let serial = plan(zones, Policy::Serial, PRE);
            let inter = plan(zones, Policy::Interleaved, PRE);
            assert_valid(&inter, zones, PRE);
            assert!(
                makespan_s(&inter) <= makespan_s(&serial),
                "interleave regressed makespan for {zones:?}"
            );
        }
    }

    #[test]
    fn empty_input_plans_nothing() {
        assert!(plan(&[], Policy::Serial, PRE).is_empty());
        assert!(plan(&[], Policy::Interleaved, PRE).is_empty());
        assert_eq!(makespan_s(&[]), 0);
    }

    // A zone with an empty plan is skipped without charging a preamble.
    #[test]
    fn serial_skips_empty_zone_without_preamble() {
        let zones = vec![zp(0, &[]), zp(1, &[(600, 0)])];
        let steps = plan(&zones, Policy::Serial, PRE);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].start_offset_s, 0);
    }

    // project_zone_end replays the executor's rule over remaining steps.
    #[test]
    fn project_zone_end_replays_remaining_steps() {
        // About to dispatch zone 0's first segment; the plan continues with
        // zone 1 inside the soak, then zone 0's second segment.
        let remaining = [(0usize, 900u32, 1800u32), (1, 600, 0), (0, 900, 0)];
        // Zone 0 ends at 2700 (its soak dominates), zone 1 at 1502.
        assert_eq!(project_zone_end(&remaining, &[], PRE, 0), Some(2700 + 900));
        assert_eq!(project_zone_end(&remaining, &[], PRE, 1), Some(1502));
        assert_eq!(project_zone_end(&remaining, &[], PRE, 7), None);
        assert_eq!(project_zone_end(&[], &[], PRE, 0), None);
    }

    // Live ready offsets delay a mid-soak zone but not the others.
    #[test]
    fn project_zone_end_honors_live_ready_offsets() {
        let remaining = [(0usize, 900u32, 1800u32), (1, 600, 0), (0, 900, 0)];
        // Zone 1 is still mid-soak from an earlier segment: its step slides
        // to 2000..2600, and zone 0's finale still lands at its soak expiry.
        let end0 = project_zone_end(&remaining, &[(1, 2000)], PRE, 0);
        assert_eq!(end0, Some(2700 + 900));
        let end1 = project_zone_end(&remaining, &[(1, 2000)], PRE, 1);
        assert_eq!(end1, Some(2600));
    }
}
