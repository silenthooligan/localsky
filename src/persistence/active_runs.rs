// Active-run safety ledger (P0-1b). A commanded-valve table with persisted
// shutoff deadlines, enforced by the deadline reaper (controllers::reaper)
// independent of any controller's own shutoff. Deliberately separate from the
// `runs` history table: `runs` records what HAPPENED (via the run-edge observer);
// `active_runs` records what is currently COMMANDED ON and when it must be closed.

use std::sync::Arc;

use rusqlite::{params, Connection};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum ActiveRunsError {
    #[error("sqlite: {0}")]
    Sqlite(String),
}

/// One commanded-ON zone and the wall-clock epoch by which it must be closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub zone_slug: String,
    pub controller_id: String,
    pub off_deadline_epoch: i64,
}

#[derive(Clone)]
pub struct ActiveRunsStore {
    conn: Arc<Mutex<Connection>>,
}

impl ActiveRunsStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Arm (or re-arm) a zone's shutoff deadline on a successful run_zone.
    /// DEADLINE-MONOTONIC on conflict: keep MAX(existing, new) off-deadline (and
    /// the earliest start). The dispatch `zone_run_lock` only serializes the
    /// commanding INSTANT, not the run DURATION, so a zone can legitimately have
    /// two overlapping commanded-on windows (a smart-morning cycle plus a manual
    /// run of the same zone). A plain INSERT OR REPLACE let the shorter, later
    /// arm SHRINK the deadline, so the reaper would fire mid-cycle and cut a
    /// still-running smart-morning segment short (and drop the backstop for the
    /// rest of the cycle). Keeping the MAX means an overlapping run can only
    /// EXTEND the shutoff window, never retract it: the ledger always reflects
    /// the latest instant any commanded-on window must be closed by, which is
    /// the fail-safe direction. Completion still clears the row via `disarm`.
    pub async fn arm(
        &self,
        zone_slug: String,
        controller_id: String,
        started_epoch: i64,
        off_deadline_epoch: i64,
    ) -> Result<(), ActiveRunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO active_runs
                    (zone_slug, controller_id, started_epoch, off_deadline_epoch)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(zone_slug) DO UPDATE SET
                    off_deadline_epoch = MAX(off_deadline_epoch, excluded.off_deadline_epoch),
                    started_epoch = MIN(started_epoch, excluded.started_epoch),
                    controller_id = excluded.controller_id",
                params![zone_slug, controller_id, started_epoch, off_deadline_epoch],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| ActiveRunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| ActiveRunsError::Sqlite(e.to_string()))
    }

    /// Disarm a zone: explicit Stop, or a successful reap.
    pub async fn disarm(&self, zone_slug: &str) -> Result<(), ActiveRunsError> {
        let c = self.conn.clone();
        let zone = zone_slug.to_string();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "DELETE FROM active_runs WHERE zone_slug = ?1",
                params![zone],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| ActiveRunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| ActiveRunsError::Sqlite(e.to_string()))
    }

    /// Every armed run whose deadline has passed; the reaper enforces these.
    pub async fn due(&self, now_epoch: i64) -> Result<Vec<ActiveRun>, ActiveRunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<ActiveRun>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT zone_slug, controller_id, off_deadline_epoch
                 FROM active_runs WHERE off_deadline_epoch <= ?1
                 ORDER BY off_deadline_epoch ASC",
            )?;
            let rows = stmt
                .query_map(params![now_epoch], |r| {
                    Ok(ActiveRun {
                        zone_slug: r.get(0)?,
                        controller_id: r.get(1)?,
                        off_deadline_epoch: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| ActiveRunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| ActiveRunsError::Sqlite(e.to_string()))
    }

    /// Clear the whole ledger. Called at boot AFTER reconcile_stop_all has
    /// physically closed every valve, so persisted deadlines do not re-fire
    /// against valves already known off. Returns rows cleared.
    pub async fn clear_all(&self) -> Result<usize, ActiveRunsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let conn = c.blocking_lock();
            let n = conn.execute("DELETE FROM active_runs", [])?;
            Ok(n)
        })
        .await
        .map_err(|e| ActiveRunsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| ActiveRunsError::Sqlite(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> ActiveRunsStore {
        let mut c = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut c).unwrap();
        ActiveRunsStore::new(Arc::new(Mutex::new(c)))
    }

    #[tokio::test]
    async fn arm_due_disarm_lifecycle() {
        let s = mem();
        // Arm two zones: one already past deadline, one in the future.
        s.arm("a".into(), "ctrl".into(), 100, 130).await.unwrap();
        s.arm("b".into(), "ctrl".into(), 100, 900).await.unwrap();

        // At now=200, only "a" (deadline 130) is due.
        let due = s.due(200).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].zone_slug, "a");
        assert_eq!(due[0].controller_id, "ctrl");

        // Disarm "a"; now nothing is due at 200.
        s.disarm("a").await.unwrap();
        assert!(s.due(200).await.unwrap().is_empty());

        // "b" becomes due once now passes its deadline.
        assert_eq!(s.due(1000).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn arm_extends_to_a_later_deadline() {
        let s = mem();
        s.arm("z".into(), "ctrl".into(), 100, 130).await.unwrap();
        // Re-arm the same zone with a LATER deadline: the window extends.
        s.arm("z".into(), "ctrl".into(), 500, 800).await.unwrap();
        assert!(
            s.due(200).await.unwrap().is_empty(),
            "old deadline extended"
        );
        assert_eq!(s.due(900).await.unwrap()[0].off_deadline_epoch, 800);
    }

    #[tokio::test]
    async fn arm_is_deadline_monotonic_overlap_cannot_shrink_backstop() {
        let s = mem();
        // A smart-morning cycle arms a long whole-cycle shutoff deadline.
        s.arm("z".into(), "cycle".into(), 100, 900).await.unwrap();
        // An overlapping manual run of the SAME zone arms a SHORTER deadline.
        // It must NOT shrink the backstop, or the reaper would fire at 560 and
        // cut the still-running cycle short, dropping shutoff coverage for the
        // rest of the cycle (the exact stuck-valve case this ledger prevents).
        s.arm("z".into(), "manual".into(), 500, 560).await.unwrap();
        assert!(
            s.due(600).await.unwrap().is_empty(),
            "an overlapping shorter run must not shrink the shutoff deadline"
        );
        // The original MAX deadline is still enforced later.
        let due = s.due(1000).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].off_deadline_epoch, 900);
        // controller_id follows the latest commander.
        assert_eq!(due[0].controller_id, "manual");
    }

    #[tokio::test]
    async fn clear_all_empties_the_ledger() {
        let s = mem();
        s.arm("a".into(), "ctrl".into(), 100, 130).await.unwrap();
        s.arm("b".into(), "ctrl".into(), 100, 140).await.unwrap();
        assert_eq!(s.clear_all().await.unwrap(), 2);
        assert!(s.due(1000).await.unwrap().is_empty());
    }
}
