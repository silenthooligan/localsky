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
//
// SECURITY: the bundled localsky.toml is FULL FIDELITY (real secrets, not
// redacted) because a backup must restore a working config onto a fresh
// instance, which has nothing to un-redact against. The route is guarded:
// auth::middleware treats every /api/backup* method as PRIVILEGED, so even
// in the default AuthMode::Disabled posture only an authenticated/trusted
// caller can download it, and the public demo 403s the whole surface. The
// bundle therefore contains real credentials + the history DB and must be
// stored securely. (The config/raw + wizard/draft reads remain redacted:
// they are VIEWS, not backups.)

use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, Multipart, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use rusqlite::Connection;
use tokio::sync::Mutex;
use tower_http::limit::RequestBodyLimitLayer;

use crate::config::FileConfigStore;
use crate::persistence::ConfigSnapshotStore;
use crate::ports::config_store::ConfigStore;

/// Boot-time swap of a staged restore (<db>.restore) into place. Called
/// by main BEFORE anything opens the live DB. Moves the live file aside
/// (timestamped .pre-restore), deletes the old -wal/-shm siblings, then
/// renames the staged file in. Returns the aside path when a swap
/// happened, None when nothing was staged.
///
/// The -wal/-shm deletion is load-bearing: SQLite associates journal
/// files by NAME, so a leftover <db>-wal from the previous database
/// would be replayed into the freshly restored .db on first open,
/// corrupting it. The staged file came from VACUUM INTO (or an upload
/// of one), which is self-contained, so nothing is lost by deleting.
pub fn apply_staged_restore(db_path: &str) -> std::io::Result<Option<String>> {
    let stage = format!("{db_path}.restore");
    if !std::path::Path::new(&stage).exists() {
        return Ok(None);
    }
    let aside = format!("{db_path}.pre-restore.{}", chrono::Utc::now().timestamp());
    if std::path::Path::new(db_path).exists() {
        std::fs::rename(db_path, &aside)?;
    }
    for ext in ["-wal", "-shm"] {
        let sibling = format!("{db_path}{ext}");
        if std::path::Path::new(&sibling).exists() {
            std::fs::remove_file(&sibling)?;
        }
    }
    std::fs::rename(&stage, db_path)?;
    Ok(Some(aside))
}

#[derive(Clone)]
pub struct BackupApiState {
    pub cfg_store: Arc<FileConfigStore>,
    pub db: Option<Arc<Mutex<Connection>>>,
    pub db_path: String,
    pub snapshots: Option<ConfigSnapshotStore>,
}

/// Upper bound on a restore upload (LS-API-09). Generous because a real
/// backup bundle is config + a VACUUM'd SQLite copy of the run history,
/// which grows with retention, but bounded so an anonymous/over-large body
/// cannot exhaust memory (post_restore + the Multipart extractor buffer
/// each field). 200 MiB comfortably fits a multi-year history DB; the
/// privileged gate already restricts this route to an authenticated/
/// trusted caller, so this cap is defense-in-depth, not the access gate.
const RESTORE_BODY_LIMIT: usize = 200 * 1024 * 1024;

pub fn router(state: BackupApiState) -> Router {
    Router::new()
        .route("/", get(get_backup))
        .route("/restore", post(post_restore))
        .route("/snapshots", get(get_snapshots))
        .with_state(state)
        // Bound the restore upload. RequestBodyLimitLayer caps the body
        // regardless of how it is consumed (Multipart streams it), short-
        // circuiting on Content-Length and on the wrapped body stream.
        // DefaultBodyLimit::disable() lifts axum's stock 2 MiB extractor
        // cap (which Multipart honors) so the explicit 200 MiB layer below
        // is the single effective limit for a legitimate large backup.
        .layer(RequestBodyLimitLayer::new(RESTORE_BODY_LIMIT))
        .layer(DefaultBodyLimit::disable())
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

    // FULL-FIDELITY CONFIG (security wave 3, corrected): the bundle tars the
    // REAL localsky.toml, secrets and all. A backup is a disaster-recovery
    // artifact: restoring it onto a FRESH instance must reproduce a working
    // config, and a fresh target has nothing to un-redact against, so a
    // redacted bundle would write the "***redacted***" sentinel as each
    // secret and silently break the restored instance. The config-leak
    // finding (LS-API-03) is closed not by redacting the bundle but by the
    // PRIVILEGED GATE in auth::middleware: GET /api/backup requires an
    // authenticated/trusted caller even in the default AuthMode::Disabled
    // posture, and the public demo 403s the whole backup surface. The
    // config/raw + wizard/draft READ paths stay redacted (they are VIEWS,
    // not backups); only the backup ships real secrets, and only to a
    // caller already proven authorized to take it.
    //
    // SECURITY: the resulting bundle contains real secrets (HA token, MQTT /
    // SMTP passwords, OpenSprinkler hash, LLM key, webhook URLs) and the
    // history DB. Store it somewhere secure and treat it like a credential.
    //
    // If the file can't be read we withhold the config from the bundle
    // (None); the DB + manifest still go out and `includes_config` is false.
    let config_toml: Option<Vec<u8>> = match tokio::fs::read_to_string(s.cfg_store.path()).await {
        Ok(raw) => Some(raw.into_bytes()),
        Err(_) => None,
    };

    let manifest = serde_json::json!({
        "service": "localsky",
        "version": env!("CARGO_PKG_VERSION"),
        "created_at_epoch": chrono::Utc::now().timestamp(),
        "includes_db": db_copy.is_some(),
        "includes_config": config_toml.is_some(),
        // The bundled config is FULL FIDELITY: real secrets, not redacted.
        // It restores cleanly onto a fresh box. Flag stays for restore UIs
        // so they can warn the operator to store the bundle securely.
        "config_secrets_redacted": false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_restore_swap_removes_old_wal_and_shm() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-backup-test-{}-walshm",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("irrigation.db");
        let db = db.to_str().unwrap().to_string();

        std::fs::write(&db, b"OLD-DB").unwrap();
        std::fs::write(format!("{db}-wal"), b"OLD-WAL").unwrap();
        std::fs::write(format!("{db}-shm"), b"OLD-SHM").unwrap();
        std::fs::write(format!("{db}.restore"), b"NEW-DB").unwrap();

        let aside = apply_staged_restore(&db).unwrap().expect("swap happened");

        assert_eq!(std::fs::read(&db).unwrap(), b"NEW-DB");
        assert_eq!(std::fs::read(&aside).unwrap(), b"OLD-DB");
        assert!(
            !std::path::Path::new(&format!("{db}-wal")).exists(),
            "old WAL must not be replayed into the restored db"
        );
        assert!(!std::path::Path::new(&format!("{db}-shm")).exists());
        assert!(!std::path::Path::new(&format!("{db}.restore")).exists());
    }

    #[test]
    fn staged_restore_noop_without_stage_file() {
        let dir =
            std::env::temp_dir().join(format!("localsky-backup-test-{}-noop", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("irrigation.db");
        std::fs::write(&db, b"LIVE").unwrap();
        let res = apply_staged_restore(db.to_str().unwrap()).unwrap();
        assert!(res.is_none());
        assert_eq!(std::fs::read(&db).unwrap(), b"LIVE");
    }

    #[test]
    fn staged_restore_onto_fresh_install_works() {
        let dir =
            std::env::temp_dir().join(format!("localsky-backup-test-{}-fresh", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("irrigation.db");
        let db = db.to_str().unwrap().to_string();
        std::fs::write(format!("{db}.restore"), b"NEW-DB").unwrap();
        let aside = apply_staged_restore(&db).unwrap().expect("swap happened");
        assert_eq!(std::fs::read(&db).unwrap(), b"NEW-DB");
        assert!(!std::path::Path::new(&aside).exists(), "no old db to keep");
    }

    /// Disaster-recovery contract: a backup taken from a configured
    /// instance, restored onto a FRESH instance, must reproduce the SAME
    /// config WITH REAL SECRETS. This proves the bundle is full fidelity
    /// (not redacted) and that the restore parse/save path lands the real
    /// secret bytes on disk. If the bundle were redacted, a fresh restore
    /// would write the "***redacted***" sentinel (nothing to un-redact
    /// against on a clean target) and the restored instance would be broken.
    #[tokio::test]
    async fn backup_restore_roundtrip_preserves_real_secrets_on_fresh_instance() {
        use crate::config::schema::*;
        use std::io::Read;

        let dir = std::env::temp_dir().join(format!(
            "localsky-backup-test-{}-roundtrip",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // ----- The SOURCE (configured) instance -----
        let src_cfg_path = dir.join("source/localsky.toml");
        std::fs::create_dir_all(src_cfg_path.parent().unwrap()).unwrap();

        let mut cfg = Config::default();
        cfg.deployment.location = Location {
            lat: 28.5,
            lon: -81.4,
            elevation_m: None,
        };
        cfg.controllers.push(ControllerEntry {
            id: "os_main".into(),
            default: true,
            enabled: true,
            controller: ControllerKind::OpensprinklerDirect(OpenSprinklerDirectConfig {
                host: "10.0.0.10".into(),
                port: 80,
                password_md5: "abc123md5hash".into(),
                poll_interval_s: 10,
            }),
        });
        cfg.notifications.email = Some(EmailConfig {
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            username: "smtp_user_secret".into(),
            password: "smtp_pass_secret".into(),
            from_address: "a@example.com".into(),
            to_address: "b@example.com".into(),
            starttls: true,
        });
        std::fs::write(&src_cfg_path, toml::to_string_pretty(&cfg).unwrap()).unwrap();

        let src_state = BackupApiState {
            cfg_store: Arc::new(FileConfigStore::new(&src_cfg_path)),
            db: None,
            db_path: dir
                .join("source/irrigation.db")
                .to_string_lossy()
                .to_string(),
            snapshots: None,
        };

        // ----- Take the backup -----
        let resp = get_backup(State(src_state)).await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();

        // Unpack the tar.gz and pull localsky.toml back out (this is exactly
        // what post_restore's bundle branch does).
        let gz = flate2::read::GzDecoder::new(bytes.as_ref());
        let mut archive = tar::Archive::new(gz);
        let mut bundled_config: Option<String> = None;
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path == "localsky.toml" {
                let mut s = String::new();
                entry.read_to_string(&mut s).unwrap();
                bundled_config = Some(s);
            }
        }
        let bundled = bundled_config.expect("bundle contains localsky.toml");

        // The bundle is FULL FIDELITY: real secrets are present, no sentinel.
        assert!(
            bundled.contains("abc123md5hash"),
            "backup must contain the real OpenSprinkler password_md5"
        );
        assert!(
            bundled.contains("smtp_pass_secret"),
            "backup must contain the real SMTP password"
        );
        assert!(
            bundled.contains("smtp_user_secret"),
            "backup must contain the real SMTP username"
        );
        assert!(
            !bundled.contains(crate::api::config::SECRET_REDACTED_SENTINEL),
            "a backup must NOT carry the redaction sentinel"
        );

        // ----- Restore onto a FRESH instance -----
        // Mirror post_restore's config branch: parse -> validate -> save to a
        // clean store. The fresh target has NO prior config to un-redact
        // against, so this is the exact disaster-recovery scenario.
        let fresh_cfg_path = dir.join("fresh/localsky.toml");
        std::fs::create_dir_all(fresh_cfg_path.parent().unwrap()).unwrap();
        let fresh_store = FileConfigStore::new(&fresh_cfg_path);
        assert!(
            !fresh_store.is_initialized(),
            "fresh instance starts with no config"
        );

        let restored: Config = toml::from_str(&bundled).expect("bundled TOML re-parses");
        let report = crate::config::validate::validate(&restored);
        assert!(report.ok(), "restored config must validate: {report:?}");
        fresh_store.save(&restored).await.expect("restore save");

        // ----- Verify the restored instance has the REAL secrets -----
        let loaded = fresh_store.load().await.expect("fresh load after restore");
        let ControllerKind::OpensprinklerDirect(os) = &loaded.controllers[0].controller else {
            panic!("expected opensprinkler_direct controller");
        };
        assert_eq!(
            os.password_md5, "abc123md5hash",
            "restored OpenSprinkler secret must be the REAL value, not a sentinel"
        );
        let email = loaded.notifications.email.as_ref().expect("email config");
        assert_eq!(
            email.password, "smtp_pass_secret",
            "restored SMTP password must be the REAL value"
        );
        assert_eq!(
            email.username, "smtp_user_secret",
            "restored SMTP username must be the REAL value"
        );
        // And nothing on the restored instance is a redaction sentinel.
        let on_disk = std::fs::read_to_string(&fresh_cfg_path).unwrap();
        assert!(
            !on_disk.contains(crate::api::config::SECRET_REDACTED_SENTINEL),
            "restored config on disk must contain no sentinel"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
