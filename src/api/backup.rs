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
    /// Live runtime handles so a config-only restore HOT-APPLIES to the running
    /// engine (matching PUT /api/config), instead of only rewriting the file
    /// while the live WateringPolicy / schedules keep the pre-restore values.
    /// `None` in tests / demo (no live engine to re-apply into).
    pub runtime: Option<crate::runtime::RuntimeHandles>,
}

/// Upper bound on a restore upload (LS-API-09). Generous because a real
/// backup bundle is config + a VACUUM'd SQLite copy of the run history,
/// which grows with retention, but bounded so an anonymous/over-large body
/// cannot exhaust memory (post_restore + the Multipart extractor buffer
/// each field). 200 MiB comfortably fits a multi-year history DB; the
/// privileged gate already restricts this route to an authenticated/
/// trusted caller, so this cap is defense-in-depth, not the access gate.
const RESTORE_BODY_LIMIT: usize = 200 * 1024 * 1024;

/// Upper bound on the TOTAL bytes a restore request may DECOMPRESS out of
/// uploaded bundles. RESTORE_BODY_LIMIT caps only the COMPRESSED body; a
/// hostile gzip ("gzip bomb", a few MiB of gzipped zeros) can declare tar
/// entries of arbitrary size backed by almost no compressed input, so an
/// uncapped read_to_end would buffer unbounded gigabytes and OOM the
/// container. 1 GiB is 5x the compressed cap: comfortably above any real
/// bundle (a VACUUM'd multi-year history DB gzips well under the 200 MiB
/// body cap) while keeping the worst-case allocation bounded. Exceeding it
/// fails the restore with 422.
const RESTORE_DECOMPRESSED_LIMIT: u64 = 1024 * 1024 * 1024;

/// The files post_restore acts on out of an uploaded bundle.
#[derive(Debug, Default)]
struct BundleParts {
    config: Option<Vec<u8>>,
    db: Option<Vec<u8>>,
    manifest: Option<Vec<u8>>,
}

/// Unpack a backup bundle (the tar.gz from GET /backup) in memory, charging
/// every decompressed byte against `remaining` (a REQUEST-scoped budget, so
/// several bundle fields in one upload still share one ceiling). Each entry
/// is checked against the budget via its header size FIRST (the declared
/// size is attacker-controlled but tar reads never exceed it, so an oversized
/// declaration fails fast with nothing inflated), and the actual read is
/// clamped with take() as the belt-and-braces backstop. Unreadable entries
/// are skipped, matching the old loop.
fn unpack_bundle(data: &[u8], remaining: &mut u64) -> Result<BundleParts, (StatusCode, String)> {
    use std::io::Read;
    let gz = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(gz);
    let Ok(entries) = archive.entries() else {
        return Err((
            StatusCode::BAD_REQUEST,
            "not a localsky backup bundle".into(),
        ));
    };
    let too_big = || {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "bundle decompresses past the {} MiB limit and was rejected (gzip bomb guard); \
                 a real LocalSky backup never gets this large",
                RESTORE_DECOMPRESSED_LIMIT / (1024 * 1024)
            ),
        )
    };
    let mut parts = BundleParts::default();
    for entry in entries.flatten() {
        let path = entry
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if entry.header().size().unwrap_or(u64::MAX) > *remaining {
            return Err(too_big());
        }
        let mut buf = Vec::new();
        let mut limited = entry.take(remaining.saturating_add(1));
        if limited.read_to_end(&mut buf).is_err() {
            continue;
        }
        if buf.len() as u64 > *remaining {
            return Err(too_big());
        }
        *remaining -= buf.len() as u64;
        match path.as_str() {
            "localsky.toml" => parts.config = Some(buf),
            "irrigation.db" => parts.db = Some(buf),
            "manifest.json" => parts.manifest = Some(buf),
            _ => {}
        }
    }
    Ok(parts)
}

/// Schema probe for an uploaded database file: open it READ-ONLY and require
/// the LocalSky migrations ledger (`schema_migrations`, created by M0001 in
/// persistence::runner) to exist. The 16-byte magic only proves "some SQLite
/// file"; without this probe any foreign .db swaps in at boot, HistoryDb::
/// open then fails, and the instance comes up without persistence and with
/// an EMPTY controller registry (no watering dispatches). Read-only open of
/// a freshly written, sibling-less copy is safe for both delete-journal and
/// WAL files and creates no -wal/-shm side files.
fn probe_localsky_db(path: &str) -> Result<(), String> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("db is not readable as a SQLite database: {e}"))?;
    let migrations: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("db is not readable as a SQLite database: {e}"))?;
    if migrations == 0 {
        return Err(
            "db is a SQLite file but not a LocalSky database (it has no schema_migrations \
             table); upload the irrigation.db from a LocalSky backup bundle"
                .into(),
        );
    }
    Ok(())
}

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

/// Delete-on-drop guard for the on-disk temp files backing a backup
/// download. The bundle's guard rides inside the response body stream, so
/// the temp file is removed when the download completes AND when the client
/// disconnects early (the stream is dropped either way). Removal failures
/// are ignored: names are unique per request, so a leftover can never be
/// picked up by a later backup.
struct TempFileGuard(String);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Per-process sequence for unique backup temp names, so two concurrent
/// downloads (or an early-drop cleanup racing a fresh request) never touch
/// each other's files.
static BACKUP_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn get_backup(State(s): State<BackupApiState>) -> Response {
    // DISK-STAGED, STREAMED response: the old path buffered the FULL VACUUM'd
    // DB plus the whole gzipped tarball in RAM (DB-size + compressed-size
    // resident at once), which grows without bound with history retention and
    // OOMs small self-host boxes (HAOS / Pi). Now the DB copy lands on disk,
    // the tar.gz is built on disk, and the finished file streams out as the
    // body; peak memory is one 64 KiB chunk regardless of history size.
    let token = format!(
        "{}-{}",
        std::process::id(),
        BACKUP_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let db_tmp = format!("{}.backup-db-{token}.tmp", s.db_path);
    let bundle_tmp = format!("{}.backup-bundle-{token}.tmp", s.db_path);
    // The VACUUM copy only lives for this handler (its bytes are tarred into
    // the bundle before we respond); the bundle guard is handed to the
    // response stream below so it outlives the handler until the download
    // finishes or the client vanishes. Both also clean up every early-error
    // return in between.
    let db_tmp_guard = TempFileGuard(db_tmp.clone());
    let bundle_guard = TempFileGuard(bundle_tmp.clone());

    // Consistent DB copy: VACUUM INTO a temp file under the data dir. Disk
    // only; never read into memory.
    let db_copy: Option<String> = if let Some(db) = &s.db {
        let db = db.clone();
        let tmp_clone = db_tmp.clone();
        let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let conn = db.blocking_lock();
            let _ = std::fs::remove_file(&tmp_clone);
            conn.execute("VACUUM INTO ?1", rusqlite::params![tmp_clone])
                .map_err(|e| e.to_string())
                .map(|_| ())
        })
        .await;
        match res {
            Ok(Ok(())) => Some(db_tmp.clone()),
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

    // Build the tar.gz ON DISK: small entries (manifest, config) from memory,
    // the DB streamed file-to-file (tar::Builder::append_data copies from any
    // Read in chunks), the gzip encoder writing straight to the bundle temp.
    let build = {
        let bundle_tmp = bundle_tmp.clone();
        let db_copy = db_copy.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let out = std::fs::File::create(&bundle_tmp).map_err(|e| e.to_string())?;
            let gz = flate2::write::GzEncoder::new(out, flate2::Compression::default());
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
            if let Some(path) = &db_copy {
                let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
                let len = f.metadata().map_err(|e| e.to_string())?.len();
                let mut h = tar::Header::new_gnu();
                h.set_size(len);
                h.set_mode(0o600);
                h.set_mtime(chrono::Utc::now().timestamp() as u64);
                h.set_cksum();
                tar.append_data(&mut h, "irrigation.db", &mut f)
                    .map_err(|e| e.to_string())?;
            }
            let gz = tar.into_inner().map_err(|e| e.to_string())?;
            gz.finish().map_err(|e| e.to_string())?;
            Ok(())
        })
        .await
    };
    match build {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")),
    }
    // The VACUUM copy's bytes are inside the bundle now; free the disk before
    // a potentially slow download.
    drop(db_tmp_guard);

    // Stream the finished bundle. tokio-util's ReaderStream is NOT a declared
    // dependency, so the file is chunked through futures::stream::try_unfold
    // (futures is already a direct dep) into axum's Body::from_stream. The
    // bundle guard is threaded through the stream state: it drops (deleting
    // the temp file) when the final chunk is served OR when the stream itself
    // is dropped mid-download.
    let file = match tokio::fs::File::open(&bundle_tmp).await {
        Ok(f) => f,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("bundle open: {e}"),
            )
        }
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("bundle stat: {e}"),
            )
        }
    };
    let stream =
        futures::stream::try_unfold((file, bundle_guard), |(mut file, guard)| async move {
            use tokio::io::AsyncReadExt;
            let mut chunk = vec![0u8; 64 * 1024];
            let n = file.read(&mut chunk).await?;
            if n == 0 {
                // Download complete: dropping the guard deletes the temp file.
                Ok::<_, std::io::Error>(None)
            } else {
                chunk.truncate(n);
                Ok(Some((axum::body::Bytes::from(chunk), (file, guard))))
            }
        });

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
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

async fn post_restore(State(s): State<BackupApiState>, mut multipart: Multipart) -> Response {
    let mut config_bytes: Option<Vec<u8>> = None;
    let mut db_bytes: Option<Vec<u8>> = None;
    let mut manifest_bytes: Option<Vec<u8>> = None;
    // Request-wide decompression budget shared by every bundle field (see
    // RESTORE_DECOMPRESSED_LIMIT): repeating the bundle field cannot mint a
    // fresh budget per field.
    let mut decompressed_budget: u64 = RESTORE_DECOMPRESSED_LIMIT;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let fname = field.file_name().unwrap_or("").to_string();
        let Ok(data) = field.bytes().await else {
            return err(StatusCode::BAD_REQUEST, "upload read failed");
        };
        match name.as_str() {
            "bundle" => {
                // tar.gz from GET /backup: unpack in memory, capped against
                // gzip bombs (unpack_bundle).
                match unpack_bundle(data.as_ref(), &mut decompressed_budget) {
                    Ok(parts) => {
                        if parts.config.is_some() {
                            config_bytes = parts.config;
                        }
                        if parts.db.is_some() {
                            db_bytes = parts.db;
                        }
                        if parts.manifest.is_some() {
                            manifest_bytes = parts.manifest;
                        }
                    }
                    Err((status, msg)) => return err(status, msg),
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
    let mut config_restart: Option<crate::runtime::ConfigApplyOutcome> = None;
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
        // Snapshot the running config BEFORE overwrite so the hot-apply diff +
        // restart-required set are computed against the pre-restore state.
        let prev_cfg = s.cfg_store.load().await.ok();
        if let Err(e) = s.cfg_store.save(&cfg).await {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("config save: {e}"),
            );
        }
        applied_config = true;
        // Hot-apply to the LIVE engine (matches PUT /api/config). Without this
        // the file changed but the running WateringPolicy / skip thresholds /
        // schedules / priorities / chains kept their pre-restore values until a
        // manual restart, so a restored stricter-restriction or different-schedule
        // config silently kept watering on the OLD rules while the settings UI
        // showed the new ones. The returned outcome tells the caller which
        // boot-bound parts (connections, zones, mode) still need a restart.
        if let Some(h) = &s.runtime {
            config_restart = Some(crate::runtime::apply_runtime_config(
                h,
                prev_cfg.as_ref(),
                &cfg,
            ));
        }
    }

    let mut staged_db = false;
    if let Some(bytes) = db_bytes {
        // Cheap first gate: SQLite magic.
        if !bytes.starts_with(b"SQLite format 3\0") {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "db is not a SQLite file");
        }
        // Schema probe: the magic only proves "some SQLite file", and
        // apply_staged_restore swaps unconditionally at boot, so a wrong .db
        // upload would boot the instance without persistence (and an empty
        // controller registry). Write to a probe temp, verify it is a
        // LocalSky database (probe_localsky_db), and only then rename into
        // the boot-swap slot; a failed probe can never leave a foreign db
        // staged.
        let stage = format!("{}.restore", s.db_path);
        let probe = format!("{}.restore-probe", s.db_path);
        if let Err(e) = tokio::fs::write(&probe, &bytes).await {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("db stage: {e}"));
        }
        let probe_clone = probe.clone();
        let verdict = tokio::task::spawn_blocking(move || probe_localsky_db(&probe_clone)).await;
        match verdict {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                let _ = tokio::fs::remove_file(&probe).await;
                return err(StatusCode::UNPROCESSABLE_ENTITY, msg);
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&probe).await;
                return err(StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}"));
            }
        }
        if let Err(e) = tokio::fs::rename(&probe, &stage).await {
            let _ = tokio::fs::remove_file(&probe).await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("db stage: {e}"));
        }
        staged_db = true;
    }

    // Surface the bundle's manifest (version / created_at / flags) so the
    // caller sees exactly which backup it just restored and a UI can warn
    // about a version skew against this binary. Best-effort: a hand-rolled
    // bundle without one (or with junk) yields null, never an error.
    let bundle_manifest: Option<serde_json::Value> = manifest_bytes
        .as_deref()
        .and_then(|b| serde_json::from_slice(b).ok());

    let cfg_needs_restart = config_restart
        .as_ref()
        .map(|o| o.restart_required)
        .unwrap_or(false);
    let cfg_restart_reasons: Vec<String> = config_restart
        .map(|o| o.restart_reasons)
        .unwrap_or_default();
    let restart_required = staged_db || cfg_needs_restart;
    let note = if staged_db {
        "restart the container to swap in the restored database"
    } else if cfg_needs_restart {
        "config restored and hot-applied; some changes (connections, zones, or mode) still need a restart"
    } else if applied_config {
        "config restored and hot-applied to the running engine"
    } else {
        "nothing to restore"
    };
    Json(serde_json::json!({
        "ok": true,
        "config_applied": applied_config,
        "db_staged": staged_db,
        "restart_required": restart_required,
        "restart_reasons": cfg_restart_reasons,
        "bundle_manifest": bundle_manifest,
        "note": note,
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
            runtime: None,
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

    fn test_dir(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "localsky-backup-test-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state_for(dir: &std::path::Path) -> BackupApiState {
        BackupApiState {
            cfg_store: Arc::new(FileConfigStore::new(dir.join("localsky.toml"))),
            db: None,
            db_path: dir.join("irrigation.db").to_string_lossy().to_string(),
            snapshots: None,
            runtime: None,
        }
    }

    /// Bytes of a minimal LocalSky-shaped SQLite database (has the
    /// schema_migrations ledger the restore probe requires).
    fn localsky_db_bytes(dir: &std::path::Path) -> Vec<u8> {
        let p = dir.join("donor.db");
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations(version TEXT PRIMARY KEY, name TEXT, applied_at TEXT);
             INSERT INTO schema_migrations VALUES('M0001','baseline schema','2026-01-01');",
        )
        .unwrap();
        drop(conn);
        std::fs::read(&p).unwrap()
    }

    /// Build a tar.gz bundle the same shape GET /backup produces.
    fn build_bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar_b = tar::Builder::new(gz);
        for (name, bytes) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o600);
            h.set_mtime(0);
            h.set_cksum();
            tar_b.append_data(&mut h, *name, *bytes).unwrap();
        }
        tar_b.into_inner().unwrap().finish().unwrap()
    }

    /// Wrap raw bytes in a single-field multipart/form-data extractor, so
    /// post_restore can be exercised directly.
    async fn multipart_with(field: &str, filename: &str, bytes: &[u8]) -> Multipart {
        use axum::extract::FromRequest;
        let boundary = "LSBOUNDARY";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; \
                 filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = axum::http::Request::builder()
            .method("POST")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(body))
            .unwrap();
        Multipart::from_request(req, &()).await.unwrap()
    }

    async fn json_body(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ---- gzip-bomb guard (decompression budget) ----

    #[test]
    fn restore_bundle_gzip_bomb_is_rejected_before_inflating() {
        // A hostile bundle: a syntactically valid tar header DECLARING an
        // 8 GiB entry, gzipped, with no data behind it. The header-size check
        // must 422 without inflating anything (the take() clamp backstops a
        // stream that somehow got past it).
        use std::io::Write;
        let mut h = tar::Header::new_gnu();
        h.set_path("irrigation.db").unwrap();
        h.set_size(8 * 1024 * 1024 * 1024);
        h.set_mode(0o600);
        h.set_cksum();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(h.as_bytes()).unwrap();
        let bomb = gz.finish().unwrap();
        assert!(bomb.len() < 1024, "the bomb itself is tiny");

        let mut budget = RESTORE_DECOMPRESSED_LIMIT;
        let (status, msg) = unpack_bundle(&bomb, &mut budget).unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(msg.contains("limit"), "message names the cap: {msg}");
    }

    #[test]
    fn restore_bundle_budget_is_cumulative_across_entries() {
        // Entries individually under the remaining budget are still charged
        // against it: with most of the request budget spent, a bundle whose
        // TOTAL exceeds what is left is rejected.
        let bundle = build_bundle(&[
            ("manifest.json", &[b'a'; 600][..]),
            ("irrigation.db", &[b'b'; 600][..]),
        ]);
        let mut budget: u64 = 1000;
        let (status, _) = unpack_bundle(&bundle, &mut budget).unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn restore_bundle_unpacks_within_budget_and_captures_manifest() {
        let db = b"SQLite format 3\0fakebytes";
        let bundle = build_bundle(&[
            ("manifest.json", br#"{"version":"1.2.3"}"#.as_slice()),
            ("localsky.toml", b"schema_version = 1\n".as_slice()),
            ("irrigation.db", db.as_slice()),
        ]);
        let mut budget = RESTORE_DECOMPRESSED_LIMIT;
        let parts = unpack_bundle(&bundle, &mut budget).unwrap();
        assert_eq!(
            parts.config.as_deref(),
            Some(b"schema_version = 1\n".as_slice())
        );
        assert_eq!(parts.db.as_deref(), Some(db.as_slice()));
        assert!(parts.manifest.is_some());
        assert!(
            budget < RESTORE_DECOMPRESSED_LIMIT,
            "decompressed bytes are charged against the budget"
        );
    }

    // ---- restore schema probe ----

    #[test]
    fn db_probe_accepts_localsky_and_rejects_foreign_or_corrupt() {
        let dir = test_dir("probe");

        // LocalSky-shaped db passes.
        let ok = dir.join("ok.db");
        let conn = Connection::open(&ok).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations(version TEXT PRIMARY KEY, name TEXT, applied_at TEXT);",
        )
        .unwrap();
        drop(conn);
        assert!(probe_localsky_db(ok.to_str().unwrap()).is_ok());

        // A valid SQLite file with a foreign schema fails with the friendly
        // message.
        let alien = dir.join("alien.db");
        let conn = Connection::open(&alien).unwrap();
        conn.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);")
            .unwrap();
        drop(conn);
        let msg = probe_localsky_db(alien.to_str().unwrap()).unwrap_err();
        assert!(msg.contains("not a LocalSky database"), "{msg}");

        // Magic-prefixed garbage fails too (the magic alone proves nothing).
        let junk = dir.join("junk.db");
        let mut bytes = b"SQLite format 3\0".to_vec();
        bytes.extend_from_slice(&[0xAB; 4096]);
        std::fs::write(&junk, &bytes).unwrap();
        assert!(probe_localsky_db(junk.to_str().unwrap()).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_rejects_foreign_sqlite_db_and_stages_nothing() {
        let dir = test_dir("foreigndb");

        // A real SQLite file that is NOT LocalSky: passes the magic check,
        // must fail the schema probe.
        let alien_path = dir.join("alien.db");
        let conn = Connection::open(&alien_path).unwrap();
        conn.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);")
            .unwrap();
        drop(conn);
        let alien = std::fs::read(&alien_path).unwrap();
        assert!(alien.starts_with(b"SQLite format 3\0"));

        let state = state_for(&dir);
        let db_path = state.db_path.clone();
        let mp = multipart_with("db", "alien.db", &alien).await;
        let resp = post_restore(State(state), mp).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let v = json_body(resp).await;
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("not a LocalSky database"),
            "friendly rejection: {v}"
        );
        assert!(
            !std::path::Path::new(&format!("{db_path}.restore")).exists(),
            "a rejected db must never be staged for the boot swap"
        );
        assert!(
            !std::path::Path::new(&format!("{db_path}.restore-probe")).exists(),
            "probe temp must be cleaned up"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_bundle_stages_localsky_db_and_surfaces_manifest() {
        let dir = test_dir("bundlerestore");
        let db_bytes = localsky_db_bytes(&dir);
        let bundle = build_bundle(&[
            (
                "manifest.json",
                br#"{"service":"localsky","version":"9.9.9"}"#.as_slice(),
            ),
            ("irrigation.db", db_bytes.as_slice()),
        ]);

        let state = state_for(&dir);
        let db_path = state.db_path.clone();
        let mp = multipart_with("bundle", "backup.tar.gz", &bundle).await;
        let resp = post_restore(State(state), mp).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["db_staged"], true);
        assert_eq!(v["restart_required"], true);
        assert_eq!(
            v["bundle_manifest"]["version"], "9.9.9",
            "the bundled manifest is surfaced so the caller sees which backup landed: {v}"
        );
        assert!(
            std::path::Path::new(&format!("{db_path}.restore")).exists(),
            "a LocalSky db stages for the boot swap"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- disk-staged, streamed backup download ----

    #[tokio::test]
    async fn get_backup_streams_bundle_and_cleans_temp_files() {
        use std::io::Read;

        let dir = test_dir("stream");
        std::fs::write(dir.join("localsky.toml"), "schema_version = 1\n").unwrap();
        let db_path = dir.join("irrigation.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations(version TEXT PRIMARY KEY, name TEXT, applied_at TEXT);
             INSERT INTO schema_migrations VALUES('M0001','baseline schema','2026-01-01');",
        )
        .unwrap();

        let state = BackupApiState {
            cfg_store: Arc::new(FileConfigStore::new(dir.join("localsky.toml"))),
            db: Some(Arc::new(Mutex::new(conn))),
            db_path: db_path.to_string_lossy().to_string(),
            snapshots: None,
            runtime: None,
        };

        let resp = get_backup(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "application/gzip"
        );
        let disp = headers
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(disp.starts_with("attachment; filename=\"localsky-backup-"));
        assert!(disp.ends_with(".tar.gz\""));
        let declared_len: u64 = headers
            .get(header::CONTENT_LENGTH)
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            body.len() as u64,
            declared_len,
            "Content-Length matches the streamed body"
        );

        // Well-formed tar.gz with all three entries; the db entry is the
        // VACUUM'd copy (real SQLite bytes).
        let gz = flate2::read::GzDecoder::new(body.as_ref());
        let mut archive = tar::Archive::new(gz);
        let mut names = Vec::new();
        let mut db_bytes = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().to_string();
            if name == "irrigation.db" {
                entry.read_to_end(&mut db_bytes).unwrap();
            }
            names.push(name);
        }
        for expected in ["manifest.json", "localsky.toml", "irrigation.db"] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert!(db_bytes.starts_with(b"SQLite format 3\0"));

        // Both disk stages (VACUUM copy + bundle temp) are gone once the
        // stream completed; nothing accumulates across backups.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".backup-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn get_backup_cleans_temp_file_on_early_body_drop() {
        let dir = test_dir("earlydrop");
        std::fs::write(dir.join("localsky.toml"), "schema_version = 1\n").unwrap();
        let state = state_for(&dir);

        let resp = get_backup(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Client vanishes before reading a single byte: dropping the response
        // drops the body stream, whose state owns the delete-on-drop guard.
        drop(resp);

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".backup-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left after early drop: {leftovers:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
