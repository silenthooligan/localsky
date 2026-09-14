// Opening the database.
//
// One file holds every table LocalSky keeps: runs, verdict history,
// sensor readings, active-run deadlines, config snapshots, push
// subscriptions. This opens it, sets the pragmas every reader depends on
// and applies the versioned migrations; the stores beside it own the
// tables themselves.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct HistoryDb {
    conn: Arc<Mutex<Connection>>,
}

impl HistoryDb {
    /// Open or create the SQLite file at the given path and run all
    /// versioned migrations. Idempotent: re-opening an already-migrated
    /// database is a no-op.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut conn =
            Connection::open(&path).with_context(|| format!("open sqlite at {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        // Wait out a concurrent writer instead of failing the statement. The
        // refresher + the chatty Ecowitt/webhook ingest contend on this file;
        // without a busy timeout a SQLITE_BUSY drops a reading or a decision row.
        conn.busy_timeout(std::time::Duration::from_secs(5)).ok();
        // Apply v2 migrations. This evolves a legacy v0.1 database to
        // the v2 schema in-place (runs gets zone_slug etc; decisions
        // gets renamed to verdict_history) and creates fresh tables on
        // a clean install.
        let applied = crate::persistence::run_migrations(&mut conn)
            .with_context(|| "applying schema migrations")?;
        if !applied.is_empty() {
            tracing::info!(applied = ?applied, "applied schema migrations");
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn handle(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }
}
