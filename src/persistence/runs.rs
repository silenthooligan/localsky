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
}

#[derive(Debug, Clone)]
pub struct NewRun {
    pub zone_slug: String,
    pub start_epoch: i64,
    pub source: String,         // "scheduler" | "manual" | "ha_external" | "controller_external"
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
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
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

    /// Mark a run as already completed (used by backfill).
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
                     applied_mm, cycle_index, cycle_count)
                 VALUES (?, ?, ?, ?, ?, ?, 'completed', ?, ?, ?, ?, ?, ?)",
                params![
                    zone, n.start_epoch, end_epoch, actual_duration_s, src,
                    ctrl, reason, n.et0_mm, n.etc_mm, applied_mm, n.cycle_index, n.cycle_count
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
                     applied_mm, cycle_index, cycle_count)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
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
                        applied_mm, cycle_index, cycle_count
                 FROM runs WHERE status IN ('running', 'intended')
                 ORDER BY start_epoch ASC",
            )?;
            let rows = stmt.query_map([], row_to_run)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| RunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| RunsError::Sqlite(e.to_string()))
    }

    /// All runs in [from_epoch, to_epoch). Used by the Gantt history.
    pub async fn window(
        &self,
        from_epoch: i64,
        to_epoch: i64,
    ) -> Result<Vec<RunRow>, RunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<RunRow>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, zone_slug, start_epoch, end_epoch, duration_s, source,
                        controller_id, status, skip_reason, et0_mm, etc_mm,
                        applied_mm, cycle_index, cycle_count
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
        let id = s.insert_intended(new_run("back_yard", 1700000000)).await.unwrap();
        s.mark_running(id).await.unwrap();

        let in_flight = s.in_flight().await.unwrap();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(in_flight[0].status, "running");

        s.mark_completed(id, 1700000600, 600, Some(7.0)).await.unwrap();
        let in_flight = s.in_flight().await.unwrap();
        assert!(in_flight.is_empty());
    }

    #[tokio::test]
    async fn mark_aborted_clears_in_flight() {
        let s = fresh_store().await;
        let id = s.insert_running(new_run("front_yard", 1700001000)).await.unwrap();
        s.mark_aborted(id, 1700001100).await.unwrap();
        let in_flight = s.in_flight().await.unwrap();
        assert!(in_flight.is_empty());
    }

    #[tokio::test]
    async fn window_queries() {
        let s = fresh_store().await;
        s.insert_completed(new_run("a", 1000), 1600, 600, Some(3.0)).await.unwrap();
        s.insert_completed(new_run("a", 2000), 2300, 300, Some(2.0)).await.unwrap();
        s.insert_completed(new_run("a", 3000), 3300, 300, Some(2.0)).await.unwrap();
        let win = s.window(1500, 2500).await.unwrap();
        assert_eq!(win.len(), 1);
        assert_eq!(win[0].start_epoch, 2000);
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
