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
    /// INSERT OR REPLACE keyed by zone_slug: a fresh run supersedes any stale row
    /// (a zone cannot run twice at once; the dispatch path also serializes Run).
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
                "INSERT OR REPLACE INTO active_runs
                    (zone_slug, controller_id, started_epoch, off_deadline_epoch)
                 VALUES (?1, ?2, ?3, ?4)",
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
    async fn arm_replaces_stale_row_for_same_zone() {
        let s = mem();
        s.arm("z".into(), "ctrl".into(), 100, 130).await.unwrap();
        // Re-arm the same zone with a later deadline (a fresh run supersedes).
        s.arm("z".into(), "ctrl".into(), 500, 800).await.unwrap();
        assert!(s.due(200).await.unwrap().is_empty(), "old deadline gone");
        assert_eq!(s.due(900).await.unwrap()[0].off_deadline_epoch, 800);
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
