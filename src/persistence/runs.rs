// Runs table CRUD. Wraps the v2 `runs` schema introduced by M0003.
//
// The store fulfills three workflows:
//   1. Scheduler/controller dispatch: insert_intended -> mark_running ->
//      mark_completed (or mark_aborted) as the controller reports state.
//   2. Backfill: insert_completed for past runs discovered via
//      controller.run_history() on boot.
//   3. Read/render: window(from, to) for the Gantt; in_flight() for the
//      scheduler's restart-recovery pass.

use std::sync::Arc;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum RunsError {
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("not found: id={0}")]
    NotFound(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRow {
    #[serde(default)]
    pub session_id: Option<String>,
    pub id: i64,
    pub zone_slug: String,
    pub start_epoch: i64,
    pub end_epoch: Option<i64>,
    pub duration_s: Option<u32>,
    pub source: String,
    pub controller_id: String,
    pub status: String,
    pub skip_reason: Option<String>,
    pub et0_mm: Option<f64>,
    pub etc_mm: Option<f64>,
    pub applied_mm: Option<f64>,
    pub cycle_index: Option<u32>,
    pub cycle_count: Option<u32>,
    /// Why the row ended the way it did when that is not a skip: "ended
    /// by restart", "stopped by the reaper at its deadline". None on an
    /// ordinary completed run.
    pub note: Option<String>,
    /// Metered volume across the run, when the controller has a flow
    /// meter. None without one.
    pub volume_gal: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct NewRun {
    pub session_id: Option<String>,
    pub zone_slug: String,
    pub start_epoch: i64,
    pub source: String, // "scheduler" | "manual" | "ha_external" | "controller_external"
    pub controller_id: String,
    pub planned_duration_s: u32, // becomes duration_s on completion
    pub skip_reason: Option<String>,
    pub et0_mm: Option<f64>,
    pub etc_mm: Option<f64>,
    pub cycle_index: Option<u32>,
    pub cycle_count: Option<u32>,
}

#[derive(Clone)]
pub struct RunsStore {
    conn: Arc<Mutex<Connection>>,
}

impl RunsStore {
    pub fn daily(&self) -> super::daily_irrigation::DailyIrrigationStore {
        super::daily_irrigation::DailyIrrigationStore::new(self.conn.clone())
    }

    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub fn commands(&self) -> super::watering_commands::WateringCommands {
        super::watering_commands::WateringCommands::new(self.conn.clone())
    }

    /// Decision evidence shares the run history database, so dispatch and replay
    /// always address the same durable local instance.
    pub fn soil_decisions(&self) -> super::soil_decisions::SoilDecisionsStore {
        super::soil_decisions::SoilDecisionsStore::new(self.conn.clone())
    }

    /// Mark a run as intended (queued by the scheduler but not yet
    /// dispatched). Returns the row id.
    pub async fn insert_intended(&self, n: NewRun) -> Result<i64, RunsError> {
        self.insert_with_status(n, "intended", None).await
    }

    /// Mark a run as actively running. Use when the controller confirms
    /// dispatch. Returns the row id.
    pub async fn insert_running(&self, n: NewRun) -> Result<i64, RunsError> {
        self.insert_with_status(n, "running", None).await
    }

    /// Mark a run as skipped before dispatch (used when a regulatory
    /// watering restriction or another gate blocks a scheduled run).
    /// `skip_reason` should be human-readable since it surfaces in the
    /// history Gantt + per-zone history strip. The `end_epoch` matches
    /// `start_epoch` so the strip renders a zero-width skip marker.
    pub async fn insert_skipped(&self, n: NewRun, skip_reason: String) -> Result<i64, RunsError> {
        let mut n = n;
        let start = n.start_epoch;
        n.skip_reason = Some(skip_reason);
        self.insert_with_status(n, "skipped", Some(start)).await
    }

    /// Mark a run as already completed (used by backfill).
    /// A run that ended before its time, recorded with what it actually
    /// applied and a note saying why: the boot pass converting an armed
    /// deadline it found, or the reaper closing a valve at the deadline.
    /// Status `aborted`, skip_reason None, so the water counts.
    pub async fn insert_aborted(
        &self,
        n: NewRun,
        end_epoch: i64,
        note: &str,
    ) -> Result<i64, RunsError> {
        let c = self.conn.clone();
        let note = note.to_string();
        let duration = (end_epoch - n.start_epoch).max(0);
        let id = tokio::task::spawn_blocking(move || -> rusqlite::Result<i64> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO runs
                    (zone_slug, start_epoch, end_epoch, duration_s, source,
                     controller_id, status, skip_reason, et0_mm, etc_mm,
                     applied_mm, cycle_index, cycle_count, note, session_id)
                 VALUES (?, ?, ?, ?, ?, ?, 'aborted', NULL, ?, ?, NULL, ?, ?, ?, ?)",
                params![
                    n.zone_slug,
                    n.start_epoch,
                    end_epoch,
                    duration,
                    n.source,
                    n.controller_id,
                    n.et0_mm,
                    n.etc_mm,
                    n.cycle_index,
                    n.cycle_count,
                    note,
                    n.session_id,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))?;
        Ok(id)
    }

    pub async fn insert_completed(
        &self,
        n: NewRun,
        end_epoch: i64,
        actual_duration_s: u32,
        applied_mm: Option<f64>,
    ) -> Result<i64, RunsError> {
        let c = self.conn.clone();
        let zone = n.zone_slug.clone();
        let src = n.source.clone();
        let ctrl = n.controller_id.clone();
        let reason = n.skip_reason.clone();
        let id = tokio::task::spawn_blocking(move || -> rusqlite::Result<i64> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO runs
                    (zone_slug, start_epoch, end_epoch, duration_s, source,
                     controller_id, status, skip_reason, et0_mm, etc_mm,
                     applied_mm, cycle_index, cycle_count, session_id)
                 VALUES (?, ?, ?, ?, ?, ?, 'completed', ?, ?, ?, ?, ?, ?, ?)",
                params![
                    zone,
                    n.start_epoch,
                    end_epoch,
                    actual_duration_s,
                    src,
                    ctrl,
                    reason,
                    n.et0_mm,
                    n.etc_mm,
                    applied_mm,
                    n.cycle_index,
                    n.cycle_count,
                    n.session_id,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))?;
        Ok(id)
    }

    async fn insert_with_status(
        &self,
        n: NewRun,
        status: &'static str,
        end_epoch: Option<i64>,
    ) -> Result<i64, RunsError> {
        let c = self.conn.clone();
        let id = tokio::task::spawn_blocking(move || -> rusqlite::Result<i64> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO runs
                    (zone_slug, start_epoch, end_epoch, duration_s, source,
                     controller_id, status, skip_reason, et0_mm, etc_mm,
                     applied_mm, cycle_index, cycle_count, session_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?)",
                params![
                    n.zone_slug,
                    n.start_epoch,
                    end_epoch,
                    n.planned_duration_s,
                    n.source,
                    n.controller_id,
                    status,
                    n.skip_reason,
                    n.et0_mm,
                    n.etc_mm,
                    n.cycle_index,
                    n.cycle_count,
                    n.session_id,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))?;
        Ok(id)
    }

    /// Transition an `intended` row to `running`.
    pub async fn mark_running(&self, id: i64) -> Result<(), RunsError> {
        self.update_status(id, "running", None, None, None).await
    }

    /// Finalize a `running` row: set end_epoch, duration_s, applied_mm,
    /// and status='completed'.
    pub async fn mark_completed(
        &self,
        id: i64,
        end_epoch: i64,
        actual_duration_s: u32,
        applied_mm: Option<f64>,
    ) -> Result<(), RunsError> {
        self.update_status(
            id,
            "completed",
            Some(end_epoch),
            Some(actual_duration_s),
            applied_mm,
        )
        .await
    }

    /// Mark a stale `running` row as aborted (e.g., scheduler restart
    /// after the controller lost it).
    pub async fn mark_aborted(&self, id: i64, end_epoch: i64) -> Result<(), RunsError> {
        self.update_status(id, "aborted", Some(end_epoch), None, None)
            .await
    }

    /// End a dispatcher's prewritten segment when boot confirms a stop.
    /// Updating its own interval avoids leaving its planned tail credited
    /// alongside a second, shorter restart row. Completed observer rows are
    /// measurements and are never rewritten here.
    pub async fn abort_dispatched_segment_at_restart(
        &self,
        id: i64,
        now_epoch: i64,
    ) -> Result<bool, RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<bool> {
            let conn = c.blocking_lock();
            let changed = conn.execute(
                "UPDATE runs SET status = 'aborted', end_epoch = ?2,
                    duration_s = ?2 - start_epoch, applied_mm = NULL,
                    note = 'ended by restart'
                 WHERE id = ?1 AND status = 'completed'
                   AND (source = 'manual' OR source LIKE 'manual:%' OR source = 'smart_morning')
                   AND start_epoch <= ?2 AND end_epoch > ?2",
                params![id, now_epoch],
            )?;
            Ok(changed > 0)
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }

    async fn update_status(
        &self,
        id: i64,
        status: &'static str,
        end_epoch: Option<i64>,
        duration_s: Option<u32>,
        applied_mm: Option<f64>,
    ) -> Result<(), RunsError> {
        let c = self.conn.clone();
        let changed = tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let conn = c.blocking_lock();
            conn.execute(
                "UPDATE runs
                 SET status = ?,
                     end_epoch = COALESCE(?, end_epoch),
                     duration_s = COALESCE(?, duration_s),
                     applied_mm = COALESCE(?, applied_mm)
                 WHERE id = ?",
                params![status, end_epoch, duration_s, applied_mm, id],
            )
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))?;
        if changed == 0 {
            return Err(RunsError::NotFound(id));
        }
        Ok(())
    }

    /// On-boot reconciliation: any run still flagged 'running' or
    /// 'intended' must have been interrupted by a container restart, OOM,
    /// or host reboot, since the in-process scheduler is the only writer
    /// for those states. Mark them aborted in one transaction, stamp
    /// end_epoch = now, and tag skip_reason so the Gantt and history
    /// surface the cause.
    ///
    /// All currently-in-flight runs (status = 'running' or 'intended').
    /// The scheduler queries this on boot to reconcile with the
    /// controllers; entries older than the controller's grace window are
    /// candidates for mark_aborted.
    pub async fn in_flight(&self) -> Result<Vec<RunRow>, RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<RunRow>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, zone_slug, start_epoch, end_epoch, duration_s, source,
                        controller_id, status, skip_reason, et0_mm, etc_mm,
                        applied_mm, cycle_index, cycle_count, note, volume_gal, session_id
                 FROM runs WHERE status IN ('running', 'intended')
                 ORDER BY start_epoch ASC",
            )?;
            let rows = stmt
                .query_map([], row_to_run)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }

    /// Truncate any OPEN prewritten LocalSky run row for `zone_slug` at `now_epoch`.
    /// Manual and smart-morning dispatch pre-write their rows as completed for the full
    /// planned duration (the controller owns the shutoff timer), so an
    /// early Stop must shrink the row to the real span; otherwise the
    /// balance and every history surface credit water that never fell.
    /// Only rows whose span contains `now_epoch` are touched. Returns
    /// the number of rows truncated.
    pub async fn truncate_active(
        &self,
        zone_slug: &str,
        now_epoch: i64,
    ) -> Result<usize, RunsError> {
        let c = self.conn.clone();
        let zone = zone_slug.to_string();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let conn = c.blocking_lock();
            conn.execute(
                "UPDATE watering_commands SET end_epoch=?2
                WHERE state='confirmed' AND zone_slug=?1 AND start_epoch<=?2 AND end_epoch>?2",
                params![zone, now_epoch],
            )?;
            conn.execute(
                "UPDATE runs
                 SET end_epoch = ?2, duration_s = ?2 - start_epoch,
                     note = CASE WHEN note IS NULL OR note = '' THEN 'Stopped early by LocalSky'
                         ELSE note || '; Stopped early by LocalSky' END
                 WHERE zone_slug = ?1 AND status = 'completed'
                   AND (source = 'manual' OR source LIKE 'manual:%' OR source = 'smart_morning')
                   AND start_epoch <= ?2 AND end_epoch > ?2",
                params![zone, now_epoch],
            )
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }

    /// [`Self::truncate_active`] across every zone OF ONE CONTROLLER: the
    /// device-wide-stop path (a per_zone_stop=false controller's zone stop
    /// halts every zone on that device, so every open prewritten row on it must
    /// shrink to the real span; other controllers' runs continue and keep
    /// their planned credit).
    pub async fn truncate_active_for_controller(
        &self,
        controller_id: &str,
        now_epoch: i64,
    ) -> Result<usize, RunsError> {
        let c = self.conn.clone();
        let ctrl = controller_id.to_string();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let conn = c.blocking_lock();
            conn.execute(
                "UPDATE watering_commands SET end_epoch=?2
                WHERE state='confirmed' AND controller_id=?1 AND start_epoch<=?2 AND end_epoch>?2",
                params![ctrl, now_epoch],
            )?;
            conn.execute(
                "UPDATE runs
                 SET end_epoch = ?2, duration_s = ?2 - start_epoch,
                     note = CASE WHEN note IS NULL OR note = '' THEN 'Stopped early by LocalSky'
                         ELSE note || '; Stopped early by LocalSky' END
                 WHERE controller_id = ?1 AND status = 'completed'
                   AND (source = 'manual' OR source LIKE 'manual:%' OR source = 'smart_morning')
                   AND start_epoch <= ?2 AND end_epoch > ?2",
                params![ctrl, now_epoch],
            )
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }

    /// [`Self::truncate_active`] across every zone (the StopAll path).
    pub async fn truncate_active_all(&self, now_epoch: i64) -> Result<usize, RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let conn = c.blocking_lock();
            conn.execute(
                "UPDATE watering_commands SET end_epoch=?1
                WHERE state='confirmed' AND start_epoch<=?1 AND end_epoch>?1",
                params![now_epoch],
            )?;
            conn.execute(
                "UPDATE runs
                 SET end_epoch = ?1, duration_s = ?1 - start_epoch,
                     note = CASE WHEN note IS NULL OR note = '' THEN 'Stopped early by LocalSky'
                         ELSE note || '; Stopped early by LocalSky' END
                 WHERE status = 'completed'
                   AND (source = 'manual' OR source LIKE 'manual:%' OR source = 'smart_morning')
                   AND start_epoch <= ?1 AND end_epoch > ?1",
                params![now_epoch],
            )
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }

    /// All runs in [from_epoch, to_epoch). Used by the Gantt history.
    /// A run the OBSERVER saw rather than one LocalSky dispatched: the
    /// refresher watched a zone go from running to not, and this is the
    /// row for it. Deduped by the table's own key, so a restart that
    /// re-observes the same edge writes nothing.
    ///
    /// A dispatched run has a row from the moment it is commanded (that
    /// is `insert_intended` through `mark_completed`); this one appears
    /// whole, after the fact, which is why the status is decided here
    /// from whether a reason came with it.
    pub async fn insert_observed(
        &self,
        n: NewRun,
        duration_s: i64,
        applied_mm: Option<f64>,
        volume_gal: Option<f64>,
    ) -> Result<(), RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO runs
                    (zone_slug, start_epoch, end_epoch, duration_s,
                     source, controller_id, status, skip_reason,
                     applied_mm, volume_gal, cycle_index, cycle_count, session_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                         CASE WHEN ?7 IS NULL THEN 'completed' ELSE 'aborted' END,
                         ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    n.zone_slug,
                    n.start_epoch,
                    n.start_epoch + duration_s,
                    duration_s,
                    n.source,
                    n.controller_id,
                    n.skip_reason,
                    applied_mm,
                    volume_gal,
                    n.cycle_index,
                    n.cycle_count,
                    n.session_id,
                ],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }

    /// Drop runs that started before `cutoff_epoch`. Only called when
    /// `[persistence] runs_retention_days` is set; the default keeps
    /// everything, which is what makes a multi-year trend possible.
    pub async fn prune_older_than(&self, cutoff_epoch: i64) -> Result<usize, RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let conn = c.blocking_lock();
            conn.execute(
                "DELETE FROM watering_commands WHERE end_epoch < ?",
                [cutoff_epoch],
            )?;
            conn.execute(
                "DELETE FROM runs WHERE start_epoch < ?",
                params![cutoff_epoch],
            )
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }

    pub async fn window(&self, from_epoch: i64, to_epoch: i64) -> Result<Vec<RunRow>, RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<RunRow>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, zone_slug, start_epoch, end_epoch, duration_s, source,
                        controller_id, status, skip_reason, et0_mm, etc_mm,
                        applied_mm, cycle_index, cycle_count, note, volume_gal, session_id
                 FROM runs WHERE start_epoch >= ? AND start_epoch < ?
                 ORDER BY start_epoch ASC",
            )?;
            let rows = stmt
                .query_map(params![from_epoch, to_epoch], row_to_run)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        session_id: r.get(16)?,
        id: r.get(0)?,
        zone_slug: r.get(1)?,
        start_epoch: r.get(2)?,
        end_epoch: r.get(3)?,
        duration_s: r.get::<_, Option<i64>>(4)?.map(|v| v as u32),
        source: r.get(5)?,
        controller_id: r.get(6)?,
        status: r.get(7)?,
        skip_reason: r.get(8)?,
        et0_mm: r.get(9)?,
        etc_mm: r.get(10)?,
        applied_mm: r.get(11)?,
        cycle_index: r.get::<_, Option<i64>>(12)?.map(|v| v as u32),
        cycle_count: r.get::<_, Option<i64>>(13)?.map(|v| v as u32),
        note: r.get(14)?,
        volume_gal: r.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;

    async fn fresh_store() -> RunsStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        RunsStore::new(Arc::new(Mutex::new(c)))
    }

    fn new_run(zone: &str, start: i64) -> NewRun {
        NewRun {
            session_id: None,
            zone_slug: zone.into(),
            start_epoch: start,
            source: "scheduler".into(),
            controller_id: "os_main".into(),
            planned_duration_s: 600,
            skip_reason: None,
            et0_mm: Some(5.5),
            etc_mm: Some(5.0),
            cycle_index: None,
            cycle_count: None,
        }
    }

    #[tokio::test]
    async fn intended_then_running_then_completed() {
        let s = fresh_store().await;
        let id = s
            .insert_intended(new_run("back_yard", 1700000000))
            .await
            .unwrap();
        s.mark_running(id).await.unwrap();

        let in_flight = s.in_flight().await.unwrap();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(in_flight[0].status, "running");

        s.mark_completed(id, 1700000600, 600, Some(7.0))
            .await
            .unwrap();
        let in_flight = s.in_flight().await.unwrap();
        assert!(in_flight.is_empty());
    }

    #[tokio::test]
    async fn mark_aborted_clears_in_flight() {
        let s = fresh_store().await;
        let id = s
            .insert_running(new_run("front_yard", 1700001000))
            .await
            .unwrap();
        s.mark_aborted(id, 1700001100).await.unwrap();
        let in_flight = s.in_flight().await.unwrap();
        assert!(in_flight.is_empty());
    }

    #[tokio::test]
    async fn window_queries() {
        let s = fresh_store().await;
        s.insert_completed(new_run("a", 1000), 1600, 600, Some(3.0))
            .await
            .unwrap();
        s.insert_completed(new_run("a", 2000), 2300, 300, Some(2.0))
            .await
            .unwrap();
        s.insert_completed(new_run("a", 3000), 3300, 300, Some(2.0))
            .await
            .unwrap();
        let win = s.window(1500, 2500).await.unwrap();
        assert_eq!(win.len(), 1);
        assert_eq!(win[0].start_epoch, 2000);
    }

    /// An early Stop truncates the pre-written manual row to the real
    /// span: only manual-family rows whose span contains the stop
    /// instant, never other sources or already-ended rows.
    #[tokio::test]
    async fn truncate_active_shrinks_the_open_manual_row_only() {
        let s = fresh_store().await;
        // Open manual row: dispatched at 1000 for a planned hour.
        let manual = NewRun {
            source: "manual".into(),
            ..new_run("front", 1000)
        };
        s.insert_completed(manual, 1000 + 3600, 3600, None)
            .await
            .unwrap();
        // An observer row for another zone, same span: untouched.
        let observer = NewRun {
            source: "ha_refresher".into(),
            ..new_run("side", 1000)
        };
        s.insert_completed(observer, 1000 + 3600, 3600, None)
            .await
            .unwrap();
        // A manual row that already ended: untouched.
        let done = NewRun {
            source: "manual:sched1".into(),
            ..new_run("front", 100)
        };
        s.insert_completed(done, 400, 300, None).await.unwrap();

        let n = s.truncate_active("front", 1120).await.unwrap();
        assert_eq!(n, 1, "exactly the open manual row");
        let rows = s.window(0, 10_000).await.unwrap();
        let front_open = rows
            .iter()
            .find(|r| r.zone_slug == "front" && r.start_epoch == 1000)
            .unwrap();
        assert_eq!(front_open.end_epoch, Some(1120));
        assert_eq!(front_open.duration_s, Some(120));
        let side = rows.iter().find(|r| r.zone_slug == "side").unwrap();
        assert_eq!(side.duration_s, Some(3600), "other sources untouched");
        let done = rows
            .iter()
            .find(|r| r.zone_slug == "front" && r.start_epoch == 100)
            .unwrap();
        assert_eq!(done.duration_s, Some(300), "ended rows untouched");
        // StopAll variant truncates across zones.
        let manual_b = NewRun {
            source: "manual".into(),
            ..new_run("side", 5000)
        };
        s.insert_completed(manual_b, 5000 + 3600, 3600, None)
            .await
            .unwrap();
        assert_eq!(s.truncate_active_all(5060).await.unwrap(), 1);
    }

    // Device-wide stop bookkeeping: stopping one zone on a controller with
    // no per-zone stop halts EVERY zone on that device, so every open
    // manual row on that controller shrinks, while another controller's
    // concurrent manual run keeps its credit.
    #[tokio::test]
    async fn truncate_active_for_controller_shrinks_sibling_rows_only() {
        let s = fresh_store().await;
        // Zone A: open manual run on the cloud device (planned full hour).
        let a = NewRun {
            source: "manual".into(),
            controller_id: "rachio_main".into(),
            ..new_run("front", 1000)
        };
        s.insert_completed(a, 1000 + 3600, 3600, None)
            .await
            .unwrap();
        // Zone B on the SAME device, also open (the zone the user stopped).
        let b = NewRun {
            source: "manual:sched1".into(),
            controller_id: "rachio_main".into(),
            ..new_run("back", 1100)
        };
        s.insert_completed(b, 1100 + 3600, 3600, None)
            .await
            .unwrap();
        // An open manual run on a DIFFERENT controller: still watering.
        let other = NewRun {
            source: "manual".into(),
            controller_id: "os_main".into(),
            ..new_run("garden", 1000)
        };
        s.insert_completed(other, 1000 + 3600, 3600, None)
            .await
            .unwrap();

        let n = s
            .truncate_active_for_controller("rachio_main", 1300)
            .await
            .unwrap();
        assert_eq!(n, 2, "both of the stopped device's open rows shrink");
        let rows = s.window(0, 10_000).await.unwrap();
        let front = rows.iter().find(|r| r.zone_slug == "front").unwrap();
        assert_eq!(front.duration_s, Some(300));
        let back = rows.iter().find(|r| r.zone_slug == "back").unwrap();
        assert_eq!(back.duration_s, Some(200));
        let garden = rows.iter().find(|r| r.zone_slug == "garden").unwrap();
        assert_eq!(
            garden.duration_s,
            Some(3600),
            "the other controller's run keeps its planned credit"
        );
    }

    #[tokio::test]
    async fn early_stops_truncate_smart_morning_rows_and_name_the_interruption() {
        for scope in ["zone", "controller", "all"] {
            let store = fresh_store().await;
            for (zone, controller) in [("front", "main"), ("back", "main"), ("beds", "other")] {
                store
                    .insert_completed(
                        NewRun {
                            source: "smart_morning".into(),
                            controller_id: controller.into(),
                            ..new_run(zone, 1000)
                        },
                        1600,
                        600,
                        None,
                    )
                    .await
                    .unwrap();
            }
            let count = match scope {
                "zone" => store.truncate_active("front", 1100).await.unwrap(),
                "controller" => store
                    .truncate_active_for_controller("main", 1100)
                    .await
                    .unwrap(),
                _ => store.truncate_active_all(1100).await.unwrap(),
            };
            assert_eq!(
                count,
                match scope {
                    "zone" => 1,
                    "controller" => 2,
                    _ => 3,
                }
            );
            for row in store.window(0, 2000).await.unwrap() {
                let stopped = row.zone_slug == "front"
                    || scope == "all"
                    || (scope == "controller" && row.controller_id == "main");
                assert_eq!(row.duration_s, Some(if stopped { 100 } else { 600 }));
                assert_eq!(
                    row.note.as_deref(),
                    stopped.then_some("Stopped early by LocalSky")
                );
            }
        }
    }

    #[tokio::test]
    async fn duplicate_zone_start_ctrl_is_ignored() {
        let s = fresh_store().await;
        let _id1 = s.insert_running(new_run("a", 5000)).await.unwrap();
        // Same zone + start + controller -> INSERT OR IGNORE.
        let _id2 = s.insert_running(new_run("a", 5000)).await.unwrap();
        let in_flight = s.in_flight().await.unwrap();
        assert_eq!(in_flight.len(), 1, "uq_runs_zone_start_ctrl should dedupe");
    }
}
