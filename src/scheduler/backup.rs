// Scheduled local backups (pre-launch audit follow-up, B-1).
//
// An optional always-on task that writes a full backup bundle to a local
// directory on an interval and prunes old ones, so a self-hoster who never
// clicks "Download backup" still has one. OFF by default and env-driven, which
// keeps it out of the /api/config schema (and its snapshot/redaction contract)
// and mirrors the update-check's env-compat pattern:
//
//   LOCALSKY_AUTO_BACKUP_HOURS  interval in hours (unset / <= 0 = disabled)
//   LOCALSKY_BACKUP_DIR         output dir (default: <data dir>/backups)
//   LOCALSKY_BACKUP_KEEP        how many newest bundles to retain (default 7)
//
// The bundle format is IDENTICAL to GET /api/backup (a tar.gz of manifest.json
// + localsky.toml + irrigation.db), so a scheduled bundle restores through the
// same POST /api/v1/backup/restore path. It is FULL-FIDELITY (real secrets),
// so the output directory inherits the data dir's protection; treat bundles as
// credentials. The DB copy uses VACUUM INTO for a consistent snapshot, exactly
// like the HTTP handler.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::config::FileConfigStore;

fn env_hours() -> Option<f64> {
    let raw = std::env::var("LOCALSKY_AUTO_BACKUP_HOURS").ok()?;
    let h: f64 = raw.trim().parse().ok()?;
    (h > 0.0).then_some(h)
}

fn backup_dir(db_path: &str) -> PathBuf {
    if let Ok(d) = std::env::var("LOCALSKY_BACKUP_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    // Default: <data dir>/backups, derived from the history DB's parent dir.
    Path::new(db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups")
}

fn keep_count() -> usize {
    std::env::var("LOCALSKY_BACKUP_KEEP")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(7)
}

/// Spawn the scheduled-backup task. A no-op (returns without spawning) unless
/// `LOCALSKY_AUTO_BACKUP_HOURS` is set to a positive number.
pub fn spawn(cfg_store: Arc<FileConfigStore>, db: Option<Arc<Mutex<Connection>>>, db_path: String) {
    let Some(hours) = env_hours() else {
        return;
    };
    let dir = backup_dir(&db_path);
    let keep = keep_count();
    // Floor the interval at 60s so a fat-fingered tiny value can't spin.
    let interval = Duration::from_secs((hours * 3600.0) as u64).max(Duration::from_secs(60));
    tracing::info!(
        interval_hours = hours,
        dir = %dir.display(),
        keep,
        "scheduled backups enabled"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // interval fires immediately on the first tick; consume it so we do not
        // snapshot a just-started (possibly mid-restore) instance at boot.
        tick.tick().await;
        loop {
            tick.tick().await;
            // Supervise the pass: a panic (a corrupt DB, a full disk mid-tar)
            // must not kill the loop for the process lifetime. Mirrors the
            // reaper / dispatcher supervisors.
            use futures::FutureExt;
            let outcome = std::panic::AssertUnwindSafe(run_once(
                cfg_store.as_ref(),
                db.as_ref(),
                &db_path,
                &dir,
                keep,
            ))
            .catch_unwind()
            .await;
            match outcome {
                Ok(Ok(path)) => {
                    tracing::info!(bundle = %path.display(), "scheduled backup written")
                }
                Ok(Err(e)) => tracing::error!(error = %e, "scheduled backup failed"),
                Err(_) => tracing::error!("scheduled backup task PANICKED; continuing next tick"),
            }
        }
    });
}

/// Take one backup: VACUUM the DB to a temp, tar.gz {manifest, config, db} to a
/// timestamped file in `dir`, prune to `keep` newest. Returns the bundle path.
async fn run_once(
    cfg_store: &FileConfigStore,
    db: Option<&Arc<Mutex<Connection>>>,
    db_path: &str,
    dir: &Path,
    keep: usize,
) -> Result<PathBuf, String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let ts = chrono::Utc::now().timestamp();
    let out = dir.join(format!("localsky-backup-{ts}.tar.gz"));

    // Consistent DB copy via VACUUM INTO a temp file (disk only), same as the
    // HTTP handler. `None` DB (no persistence mounted) yields a config-only
    // bundle, which still restores.
    let db_tmp = format!("{db_path}.sched-backup-{ts}.tmp");
    let db_copy: Option<String> = if let Some(db) = db {
        let db = db.clone();
        let tmp = db_tmp.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let conn = db.blocking_lock();
            let _ = std::fs::remove_file(&tmp);
            conn.execute("VACUUM INTO ?1", rusqlite::params![tmp])
                .map_err(|e| e.to_string())
                .map(|_| ())
        })
        .await
        .map_err(|e| format!("join: {e}"))??;
        Some(db_tmp.clone())
    } else {
        None
    };

    let config_toml: Option<Vec<u8>> = tokio::fs::read_to_string(cfg_store.path())
        .await
        .ok()
        .map(|s| s.into_bytes());

    let manifest = serde_json::json!({
        "service": "localsky",
        "version": env!("CARGO_PKG_VERSION"),
        "created_at_epoch": ts,
        "includes_db": db_copy.is_some(),
        "includes_config": config_toml.is_some(),
        "config_secrets_redacted": false,
    });

    let out_build = out.clone();
    let db_copy_build = db_copy.clone();
    let build = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let f = std::fs::File::create(&out_build).map_err(|e| e.to_string())?;
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut add = |name: &str, bytes: &[u8]| -> Result<(), String> {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o600);
            h.set_mtime(ts as u64);
            h.set_cksum();
            tar.append_data(&mut h, name, bytes)
                .map_err(|e| e.to_string())
        };
        add(
            "manifest.json",
            serde_json::to_vec_pretty(&manifest)
                .map_err(|e| e.to_string())?
                .as_slice(),
        )?;
        if let Some(cfg) = &config_toml {
            add("localsky.toml", cfg)?;
        }
        if let Some(path) = &db_copy_build {
            let mut fdb = std::fs::File::open(path).map_err(|e| e.to_string())?;
            let len = fdb.metadata().map_err(|e| e.to_string())?.len();
            let mut h = tar::Header::new_gnu();
            h.set_size(len);
            h.set_mode(0o600);
            h.set_mtime(ts as u64);
            h.set_cksum();
            tar.append_data(&mut h, "irrigation.db", &mut fdb)
                .map_err(|e| e.to_string())?;
        }
        let gz = tar.into_inner().map_err(|e| e.to_string())?;
        gz.finish().map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
    .map_err(|e| format!("join: {e}"));

    // Always free the VACUUM temp, whether or not the tar succeeded.
    if let Some(p) = &db_copy {
        let _ = tokio::fs::remove_file(p).await;
    }
    build??;

    prune(dir, keep).await;
    Ok(out)
}

/// Keep the `keep` newest `localsky-backup-*.tar.gz` files in `dir`, delete the rest.
async fn prune(dir: &Path, keep: usize) {
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(r) => r,
        Err(_) => return,
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("localsky-backup-") || !name.ends_with(".tar.gz") {
            continue;
        }
        if let Ok(meta) = e.metadata().await {
            if let Ok(modified) = meta.modified() {
                entries.push((modified, e.path()));
            }
        }
    }
    if entries.len() <= keep {
        return;
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0)); // newest first
    for (_, path) in entries.into_iter().skip(keep) {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!(path = %path.display(), error = %e, "scheduled backup prune: could not remove old bundle");
        }
    }
}
