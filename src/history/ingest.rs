// Per-zone run-edge detector. The refresher task already polls HA
// every 10s; on each cycle it calls `IngestState::observe(snapshot)`
// which compares the running flag of each zone against the previous
// observation and writes a row to SQLite when a zone goes from
// running→idle.
//
// Sub-10s blips are missed (acceptable; a tap-test for less than 10s
// isn't a real run). For runs that span the poll boundary, we record
// the start at the FIRST observation that saw the zone running and the
// duration up to the LAST observation that saw it running with KNOWN
// state (see RunLatch), so a carried-forward unknown gap never counts.

use crate::model::IrrigationSnapshot;
use crate::persistence::runs::{NewRun, RunsStore};
use crate::persistence::VerdictHistoryStore;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Per-zone run latch: set on the rising edge, cleared (and written out)
/// on the falling edge.
struct RunLatch {
    /// Epoch of the poll that first saw the zone running.
    start_epoch: i64,
    /// Metered volume integrated across the run so far, gallons, when
    /// the controller has a flow meter. None without one.
    volume_gal: Option<f64>,
    /// Whether a non-actuating dry-run controller was reporting it at the
    /// rising edge (the honest source label for the row).
    dry_run: bool,
    /// Epoch of the last poll that saw the zone running WITH
    /// running_known=true. Carried-forward unknown state (a cloud adapter
    /// that could not read live running state) never advances this, so the
    /// falling edge records only the VERIFIED running span: an hours-long
    /// unknown outage that ends in idle cannot inflate one run row with
    /// the whole outage as watering credit.
    last_known_running_epoch: i64,
}

#[derive(Default)]
pub struct IngestState {
    /// Per-zone slug → the active run latch. Absent means we last saw the
    /// zone idle (or never). On running→idle we take the latch, write the
    /// row (source 'dry_run' when the latch says the water was pretend),
    /// and clear.
    seen_running: HashMap<String, RunLatch>,
    /// Last observed (verdict, reason) pair. None until the first poll
    /// builds a valid skip_check; thereafter holds the most recent
    /// transition so we can detect changes against the next poll.
    last_decision: Option<(String, String)>,
    /// The previous poll's epoch, for integrating the sampled flow rate.
    last_poll_epoch: Option<i64>,
    /// Whether the flow-without-command alarm has been raised for the
    /// current episode, so a leak pushes once and not every ten seconds.
    flow_alarm_raised: bool,
}

/// Flow the meter has to read, gal/min, before water moving with no
/// zone commanded on counts as a leak rather than meter noise.
pub const FLOW_WITHOUT_COMMAND_GPM: f64 = 0.5;

impl IngestState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Which zone slugs are currently reported running by a SIMULATED
    /// (never-actuating) controller. A non-simulating DryRunController
    /// surfaces pretend_running through the native readback; without
    /// this check the observer would persist genuine-looking
    /// 'ha_refresher' rows for water that never fell, and the balance
    /// would credit it.
    pub async fn simulated_running_slugs(
        controllers: &crate::controllers::registry::ControllerRegistry,
    ) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for id in controllers.ids() {
            let Some(c) = controllers.get(&id) else {
                continue;
            };
            if !c.simulated() {
                continue;
            }
            if let Ok(status) = c.status().await {
                for z in status.zone_states {
                    if z.running {
                        out.insert(z.slug);
                    }
                }
            }
        }
        out
    }

    /// Inspect a freshly-built snapshot, write any completed runs and
    /// verdict transitions. `db` is the SQLite handle; `snapshot` is the
    /// in-memory state from the refresher's last successful poll.
    /// `simulated_running` marks zones whose running state comes from a
    /// dry-run controller this tick (empty on HA-sourced installs).
    ///
    /// Returns how many RUN rows were written this tick (falling edges),
    /// so the caller can invalidate anything cached over the runs table
    /// AFTER the new evidence is persisted, never a tick before it.
    pub async fn observe(
        &mut self,
        db: &Arc<Mutex<Connection>>,
        snapshot: &IrrigationSnapshot,
        simulated_running: &std::collections::HashSet<String>,
    ) -> usize {
        self.observe_with_push(db, snapshot, simulated_running, None)
            .await
    }

    /// `observe` with a push channel for the flow-without-command alarm.
    pub async fn observe_with_push(
        &mut self,
        db: &Arc<Mutex<Connection>>,
        snapshot: &IrrigationSnapshot,
        simulated_running: &std::collections::HashSet<String>,
        push: Option<&crate::push::PushDispatcher>,
    ) -> usize {
        let mut runs_written = 0usize;
        let now = snapshot.last_refresh_epoch;
        // The metered flow since the last poll, split across the zones
        // that were running for it. A meter reads the whole supply, so
        // two zones open at once share the volume evenly, which is the
        // honest answer when nothing finer is known.
        let dt_min = self
            .last_poll_epoch
            .map(|t| ((now - t).max(0) as f64) / 60.0)
            .unwrap_or(0.0);
        self.last_poll_epoch = Some(now);
        if let Some(gpm) = snapshot.flow_gpm.filter(|g| *g > 0.0) {
            let running: Vec<String> = self.seen_running.keys().cloned().collect();
            if !running.is_empty() && dt_min > 0.0 {
                let share = gpm * dt_min / running.len() as f64;
                for slug in running {
                    if let Some(latch) = self.seen_running.get_mut(&slug) {
                        latch.volume_gal = Some(latch.volume_gal.unwrap_or(0.0) + share);
                    }
                }
            }
        }
        // Water moving with nothing commanded on: a stuck valve or a
        // leak. Judged against the ledger too, so a run on a controller
        // that cannot report state is not mistaken for one.
        let anything_commanded = snapshot
            .zones
            .iter()
            .any(|z| z.is_running_or_unconfirmed() || z.ledger_running);
        match snapshot.flow_gpm {
            Some(gpm) if gpm >= FLOW_WITHOUT_COMMAND_GPM && !anything_commanded => {
                if !self.flow_alarm_raised {
                    self.flow_alarm_raised = true;
                    tracing::warn!(
                        gpm,
                        "flow meter reads water moving while no zone is commanded on"
                    );
                    if let Some(p) = push {
                        p.emit(crate::push::PushEvent::FlowWithoutCommand { gpm });
                    }
                }
            }
            _ => self.flow_alarm_raised = false,
        }
        for zone in &snapshot.zones {
            let was_running = self.seen_running.contains_key(&zone.slug);
            if zone.running && !was_running {
                // Start of a run. Latch whether the water is pretend at
                // the rising edge (the controller set can change while a
                // run is in flight; the entry state is the honest one).
                self.seen_running.insert(
                    zone.slug.clone(),
                    RunLatch {
                        start_epoch: now,
                        volume_gal: None,
                        dry_run: simulated_running.contains(&zone.slug),
                        last_known_running_epoch: now,
                    },
                );
            } else if zone.running && was_running {
                // Steady running: only a KNOWN observation extends the
                // verified span. Carried-forward unknown state (a cloud
                // adapter that could not read live running this poll)
                // leaves the latch's last-known epoch untouched, so an
                // unknown gap can never turn into watering credit.
                if zone.running_known {
                    if let Some(latch) = self.seen_running.get_mut(&zone.slug) {
                        // The moment the controller SAW the valve open, not
                        // the moment this pass asked. A cloud controller is
                        // read on the interval it declares, so crediting to
                        // `now` bills up to that interval of water against a
                        // valve that may have closed a minute ago. Never
                        // beyond now, and never backwards.
                        let seen_at = zone.running_observed_epoch.unwrap_or(now).min(now);
                        latch.last_known_running_epoch =
                            latch.last_known_running_epoch.max(seen_at);
                    }
                }
            } else if !zone.running && was_running {
                // End of a run, emit the row.
                let latch = self.seen_running.remove(&zone.slug).unwrap_or(RunLatch {
                    start_epoch: now,
                    volume_gal: None,
                    dry_run: false,
                    last_known_running_epoch: now,
                });
                // APPROXIMATE duration, bounded to the VERIFIED span: from
                // the first poll that saw the zone running to the last poll
                // that saw it running with known state. Quantized to the
                // ~10s refresher boundary and up to one poll short of the
                // true on-time. This matters most for a cycle-soak run,
                // whose valve toggles on/off per segment: the observer
                // records one such approximate row per ON segment rather
                // than a single whole-cycle row, so several short rows is
                // expected here and not a runtime error. Treat every
                // observer-written duration_s as approximate (~10s); the
                // scheduler's own rows (source "smart_morning") carry the
                // planned intent and are the
                // exact figure when one is needed.
                let duration = (latch.last_known_running_epoch - latch.start_epoch).max(0);
                let mut row = NewRun {
                    session_id: None,
                    zone_slug: zone.slug.clone(),
                    start_epoch: latch.start_epoch,
                    // Pretend water from a non-simulating dry-run
                    // controller is recorded honestly as such, never as
                    // watering evidence.
                    source: if latch.dry_run {
                        "dry_run".to_string()
                    } else {
                        "ha_refresher".to_string()
                    },
                    // The controller that reported it, written now rather
                    // than derived later from a config that may have
                    // changed. The historical placeholder stands for the
                    // rows nothing can attribute.
                    controller_id: zone
                        .controller_id
                        .clone()
                        .filter(|c| !c.is_empty())
                        .unwrap_or_else(|| "ha_service_call".to_string()),
                    planned_duration_s: duration.max(0) as u32,
                    skip_reason: None,
                    et0_mm: None,
                    etc_mm: None,
                    cycle_index: None,
                    cycle_count: None,
                };
                // Match exact command provenance, not a proximity heuristic.
                // A separate external valve episode receives its own identity.
                let store = RunsStore::new(db.clone());
                match store
                    .commands()
                    .attribution(
                        &row.zone_slug,
                        &row.controller_id,
                        row.start_epoch,
                        latch.last_known_running_epoch,
                    )
                    .await
                {
                    Ok(Some(a)) => {
                        row.session_id = a.session_id;
                        row.cycle_index = a.cycle_index;
                        row.cycle_count = a.cycle_count;
                    }
                    Ok(None) => {
                        row.session_id =
                            Some(crate::persistence::watering_commands::new_session_id())
                    }
                    Err(error) => {
                        tracing::warn!(%error, "observer session attribution unavailable")
                    }
                }
                // The gross depth its head put down over the verified span.
                let applied_mm = zone
                    .throughput_mm_hr
                    .filter(|_| !latch.dry_run)
                    .map(|t| t * duration as f64 / 3600.0);
                match RunsStore::new(db.clone())
                    .insert_observed(row, duration, applied_mm, latch.volume_gal)
                    .await
                {
                    Ok(()) => runs_written += 1,
                    Err(e) => tracing::warn!("history insert failed: {e:#}"),
                }
            }
        }

        // Persist verdict transitions. Compare both verdict and reason so
        // a "skip -> skip with new reason" still records (e.g. the reason
        // shifted from "Tomorrow rain" to "Live wind"). The very first
        // observation seeds last_decision without writing; we only care
        // about post-startup transitions to avoid a duplicate row on
        // every container restart.
        let verdict = snapshot.skip_check.verdict.clone();
        let reason = snapshot.skip_check.reason.clone();
        if verdict.is_empty() {
            return runs_written;
        }
        let current = (verdict, reason);
        let changed = match &self.last_decision {
            None => false,
            Some(prev) => *prev != current,
        };
        if changed {
            let (v, r) = current.clone();
            // Persist the structured trace captured at decision time so the
            // Rule Lab can replay why this day decided the way it did.
            let trace_json = snapshot
                .decision_trace
                .as_ref()
                .and_then(|t| serde_json::to_string(t).ok())
                .unwrap_or_default();
            if let Err(e) = VerdictHistoryStore::new(db.clone())
                .insert_transition(now, v, r, trace_json)
                .await
            {
                tracing::warn!("decision insert failed: {e:#}");
            }
        }
        self.last_decision = Some(current);
        runs_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ZoneState;

    fn mem() -> Arc<Mutex<Connection>> {
        let mut c = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut c).unwrap();
        Arc::new(Mutex::new(c))
    }

    fn snap_known(epoch: i64, running: bool, running_known: bool) -> IrrigationSnapshot {
        IrrigationSnapshot {
            last_refresh_epoch: epoch,
            zones: vec![ZoneState {
                slug: "front".into(),
                running,
                running_known,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn snap(epoch: i64, running: bool) -> IrrigationSnapshot {
        snap_known(epoch, running, true)
    }

    /// The tick sequence the balance-cache invalidation keys on: the
    /// rising edge writes nothing, the falling edge writes the run row
    /// and reports it, so the caller invalidates on the tick where the
    /// row actually exists (one tick after the run was last seen
    /// running), never a tick before.
    #[tokio::test]
    async fn observe_reports_run_rows_on_the_falling_edge() {
        let db = mem();
        let mut ingest = IngestState::new();
        let none = std::collections::HashSet::new();
        assert_eq!(ingest.observe(&db, &snap(1_000, false), &none).await, 0);
        // Rising edge: latched, nothing written yet.
        assert_eq!(ingest.observe(&db, &snap(1_010, true), &none).await, 0);
        // Still running: nothing written.
        assert_eq!(ingest.observe(&db, &snap(1_020, true), &none).await, 0);
        // Falling edge: the completed row lands and is reported.
        assert_eq!(ingest.observe(&db, &snap(1_030, false), &none).await, 1);
        // Idle again: nothing further.
        assert_eq!(ingest.observe(&db, &snap(1_040, false), &none).await, 0);
    }

    // The post-outage inflation scenario: a verified run (known ticks at
    // t=100 and t=160) is followed by hours of carried-forward UNKNOWN
    // running state (a cloud adapter that lost its running-state read);
    // when the state recovers to idle, the single row written must cover
    // only the VERIFIED span (60s), never the whole outage. Hours of
    // phantom watering credit here made the balance skip real watering.
    #[tokio::test]
    async fn outage_carry_forward_cannot_inflate_the_recorded_run() {
        let db = mem();
        let mut ingest = IngestState::new();
        let none = std::collections::HashSet::new();

        assert_eq!(
            ingest
                .observe(&db, &snap_known(100, true, true), &none)
                .await,
            0
        );
        assert_eq!(
            ingest
                .observe(&db, &snap_known(160, true, true), &none)
                .await,
            0
        );
        // Outage: running carried forward, state unknown for two hours.
        assert_eq!(
            ingest
                .observe(&db, &snap_known(200, true, false), &none)
                .await,
            0
        );
        assert_eq!(
            ingest
                .observe(&db, &snap_known(7_300, true, false), &none)
                .await,
            0
        );
        // Recovery shows idle: the falling edge writes exactly one row.
        assert_eq!(
            ingest
                .observe(&db, &snap_known(7_360, false, true), &none)
                .await,
            1
        );

        let runs = crate::persistence::runs::RunsStore::new(db.clone());
        let rows = runs.window(0, 100_000).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].start_epoch, 100);
        assert_eq!(
            rows[0].duration_s,
            Some(60),
            "only the verified running span is recorded, not the outage"
        );
    }
}
