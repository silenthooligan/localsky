// Boot phase 2: the data directory.
//
// The demo switch, the history database (with a staged restore swapped
// in before anything opens the live file) and the per-install identity.
// Everything later reads persistence through the one connection handed
// out here; a missing or unopenable /data leaves `history_conn` None and
// the instance runs without persistence rather than refusing to boot, except
// during restore: every activation/open failure refuses startup.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::persistence::HistoryDb;
use anyhow::Context;

/// What the data directory yields.
pub struct Storage {
    /// `LOCALSKY_DEMO=1`: the demo feeder writes synthetic weather,
    /// irrigation and forecast snapshots into the stores; the live data
    /// paths (listener, sources, schedulers) are not spawned, and the
    /// read-only gate answers 403 to every mutation.
    pub demo_mode: bool,
    /// The SQLite file (`HISTORY_DB_PATH`, default `/data/irrigation.db`).
    pub history_path: String,
    /// The directory the database lives in: the instance id, the forecast
    /// cache and the staged restore files sit beside it.
    pub data_dir: PathBuf,
    /// The one connection every store shares; None without persistence.
    pub history_conn: Option<Arc<Mutex<Connection>>>,
    /// Applying stays durable until both database open and config load finish.
    pub restore: Option<crate::config::restore::ActivatedRestore>,
}

pub fn open() -> anyhow::Result<Storage> {
    let demo_mode = std::env::var("LOCALSKY_DEMO").ok().as_deref() == Some("1");
    let history_path =
        std::env::var("HISTORY_DB_PATH").unwrap_or_else(|_| "/data/irrigation.db".to_string());
    let data_dir = Path::new(&history_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/data"));

    // Stable per-install identity (mDNS TXT uuid, HACS unique_id, the
    // outbound User-Agent), persisted next to the DB so it survives a
    // config restore onto new hardware. Resolved once, before any task
    // that sends a request exists.
    crate::instance::init(&data_dir);

    // One coordinator verifies the complete marked bundle before any live
    // state is opened. Partial publication/activation refuses startup, so a
    // failed rename can never become a fresh empty history database.
    let config_path =
        std::env::var("CONFIG_PATH").unwrap_or_else(|_| "/data/localsky.toml".to_string());
    let restore = crate::config::restore::activate_at_boot(
        Path::new(&config_path),
        Path::new(&history_path),
    )?;
    let history_conn = match HistoryDb::open(history_path.clone().into()) {
        Ok(db) => Some(db.handle()),
        Err(e) if restore.is_some() => {
            return Err(e).context("restored history could not open; startup refused, restore marker and recovery files retained");
        }
        Err(e) => {
            tracing::warn!(
                "history db open failed at {history_path:?}: {e:#}; running without persistence"
            );
            None
        }
    };
    Ok(Storage {
        demo_mode,
        history_path,
        data_dir,
        history_conn,
        restore,
    })
}
