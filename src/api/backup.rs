// Backup and restore.
//
//   GET  /api/v1/backup            -> tar.gz: localsky.toml + irrigation.db
//                                     (VACUUM INTO consistent copy) +
//                                     manifest.json (version/schema/created)
//   POST /api/v1/backup/restore    -> multipart upload of a bundle (or a
//                                     bare localsky.toml). Config applies
//                                     immediately through the normal
//                                     snapshot machinery; a DB stages to
//                                     <db>.restore and swaps at next boot.
//   GET  /api/v1/backup/snapshots  -> the config_snapshots history (id +
//                                     stamp) driving POST /config/rollback.
//
// The bundle deliberately EXCLUDES /data/keys (VAPID private key) and
// instance-id: restoring a config onto new hardware should mint a new
// identity, and a push key inside a casually shared backup is a leak.

use std::sync::Arc;

use axum::{
    extract::{Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::config::FileConfigStore;
use crate::persistence::ConfigSnapshotStore;
use crate::ports::config_store::ConfigStore;

#[derive(Clone)]
pub struct BackupApiState {
    pub cfg_store: Arc<FileConfigStore>,
    pub db: Option<Arc<Mutex<Connection>>>,
    pub db_path: String,
    pub snapshots: Option<ConfigSnapshotStore>,
}

pub fn router(state: BackupApiState) -> Router {
    Router::new()
        .route("/", get(get_backup))
        .route("/restore", post(post_restore))
        .route("/snapshots", get(get_snapshots))
        .with_state(state)
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

async fn get_backup(State(s): State<BackupApiState>) -> Response {
    // Consistent DB copy: VACUUM INTO a temp file under the data dir.
    let db_copy: Option<Vec<u8>> = if let Some(db) = &s.db {
        let db = db.clone();
        let tmp = format!("{}.backup-tmp", s.db_path);
        let tmp_clone = tmp.clone();
        let res = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
            let conn = db.blocking_lock();
            let _ = std::fs::remove_file(&tmp_clone);
            conn.execute("VACUUM INTO ?1", rusqlite::params![tmp_clone])
                .map_err(|e| e.to_string())?;
            drop(conn);
            let bytes = std::fs::read(&tmp_clone).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&tmp_clone);
            Ok(bytes)
        })
        .await;
        match res {
            Ok(Ok(bytes)) => Some(bytes),
            Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("db copy: {e}")),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")),
        }
    } else {
        None
    };

    let config_toml = tokio::fs::read(s.cfg_store.path()).await.ok();

    let manifest = serde_json::json!({
        "service": "localsky",
        "version": env!("CARGO_PKG_VERSION"),
        "created_at_epoch": chrono::Utc::now().timestamp(),
        "includes_db": db_copy.is_some(),
        "includes_config": config_toml.is_some(),
    });

    let tarball = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut add = |name: &str, bytes: &[u8]| -> Result<(), String> {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o600);
            h.set_mtime(chrono::Utc::now().timestamp() as u64);
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
        if let Some(db) = &db_copy {
            add("irrigation.db", db)?;
        }
        let gz = tar.into_inner().map_err(|e| e.to_string())?;
        gz.finish().map_err(|e| e.to_string())
    })
    .await;

    match tarball {
        Ok(Ok(bytes)) => {
            let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            let filename = format!(
                "localsky-backup-{}-{stamp}.tar.gz",
                env!("CARGO_PKG_VERSION")
            );
            (
                [
                    (header::CONTENT_TYPE, "application/gzip".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")),
    }
}

async fn post_restore(State(s): State<BackupApiState>, mut multipart: Multipart) -> Response {
    let mut config_bytes: Option<Vec<u8>> = None;
    let mut db_bytes: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let fname = field.file_name().unwrap_or("").to_string();
        let Ok(data) = field.bytes().await else {
            return err(StatusCode::BAD_REQUEST, "upload read failed");
        };
        match name.as_str() {
            "bundle" => {
                // tar.gz from GET /backup: unpack in memory.
                let gz = flate2::read::GzDecoder::new(data.as_ref());
                let mut archive = tar::Archive::new(gz);
                let Ok(entries) = archive.entries() else {
                    return err(StatusCode::BAD_REQUEST, "not a localsky backup bundle");
                };
                for entry in entries.flatten() {
                    let path = entry
                        .path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let mut buf = Vec::new();
                    use std::io::Read;
                    let mut entry = entry;
                    if entry.read_to_end(&mut buf).is_err() {
                        continue;
                    }
                    match path.as_str() {
                        "localsky.toml" => config_bytes = Some(buf),
                        "irrigation.db" => db_bytes = Some(buf),
                        _ => {}
                    }
                }
            }
            "config" => config_bytes = Some(data.to_vec()),
            "db" => db_bytes = Some(data.to_vec()),
            other => {
                tracing::debug!(field = other, file = fname, "restore: ignoring field");
            }
        }
    }

    if config_bytes.is_none() && db_bytes.is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "nothing to restore; send bundle=, config=, or db=",
        );
    }

    let mut applied_config = false;
    if let Some(bytes) = config_bytes {
        let Ok(text) = String::from_utf8(bytes) else {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "config is not UTF-8");
        };
        let cfg: crate::config::schema::Config = match toml::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                return err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("config parse: {e}"),
                )
            }
        };
        let report = crate::config::validate::validate(&cfg);
        if !report.ok() {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "config_invalid",
                    "validation": report,
                })),
            )
                .into_response();
        }
        if let Err(e) = s.cfg_store.save(&cfg).await {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config save: {e}"),
            );
        }
        applied_config = true;
    }

    let mut staged_db = false;
    if let Some(bytes) = db_bytes {
        // Sanity: SQLite magic.
        if !bytes.starts_with(b"SQLite format 3\0") {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "db is not a SQLite file");
        }
        let stage = format!("{}.restore", s.db_path);
        if let Err(e) = tokio::fs::write(&stage, &bytes).await {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("db stage: {e}"));
        }
        staged_db = true;
    }

    Json(serde_json::json!({
        "ok": true,
        "config_applied": applied_config,
        "db_staged": staged_db,
        "restart_required": staged_db,
        "note": if staged_db {
            "restart the container to swap in the restored database"
        } else {
            "config applied; engine picks it up on the next tick"
        },
    }))
    .into_response()
}

async fn get_snapshots(State(s): State<BackupApiState>) -> Response {
    let Some(snaps) = &s.snapshots else {
        return Json(serde_json::json!({ "snapshots": [] })).into_response();
    };
    match snaps.list().await {
        Ok(list) => Json(serde_json::json!({ "snapshots": list })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
