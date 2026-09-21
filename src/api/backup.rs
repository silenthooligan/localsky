// Backup and restore.
//
//   GET  /api/v1/backup            -> tar.gz: localsky.toml +
//                                     localsky.ledger.toml + irrigation.db
//                                     (VACUUM INTO consistent copy) +
//                                     manifest.json (version/schema/created)
//   POST /api/v1/backup/restore    -> multipart upload of a bundle (or a
//                                     bare localsky.toml). A config-only
//                                     upload applies immediately through
//                                     the normal snapshot machinery. A
//                                     bundle with a database stages ALL of
//                                     it (config, ledger, db) as
//                                     <file>.restore with a durable marker.
//                                     One boot coordinator verifies the full
//                                     set before activating it; interrupted
//                                     publication or activation refuses boot.
//   GET  /api/v1/backup/snapshots  -> the config snapshot history, the
//                                     same rows and the same `ts` keys as
//                                     GET /config/snapshots, because it is
//                                     the same on-disk history POST
//                                     /config/rollback restores from.
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
use crate::persistence::restore_probe::probe_localsky_db;
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
// Legacy file-layout primitive used only by the tests below. Production boot
// must use config::restore::activate_at_boot so all files share one marker.
#[cfg(test)]
fn apply_staged_restore(db_path: &str) -> std::io::Result<Option<String>> {
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
    ledger: Option<Vec<u8>>,
    db: Option<Vec<u8>>,
    manifest: Option<Vec<u8>>,
}

/// Unpack a backup bundle (the tar.gz from GET /backup) in memory, charging
/// every decompressed byte against `remaining` (a REQUEST-scoped budget, so
/// several bundle fields in one upload still share one ceiling). Each entry
/// is checked against the budget via its header size FIRST (the declared
/// size is attacker-controlled but tar reads never exceed it, so an oversized
/// declaration fails fast with nothing inflated), and the actual read is
/// clamped with take() as the belt-and-braces backstop. A truncated or
/// duplicate part rejects the whole upload, never a partial restore.
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
    for entry in entries {
        let entry = entry.map_err(|e| (StatusCode::BAD_REQUEST, format!("bundle entry: {e}")))?;
        let path = entry
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if entry.header().size().unwrap_or(u64::MAX) > *remaining {
            return Err(too_big());
        }
        let mut buf = Vec::new();
        let mut limited = entry.take(remaining.saturating_add(1));
        limited
            .read_to_end(&mut buf)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("bundle read: {e}")))?;
        if buf.len() as u64 > *remaining {
            return Err(too_big());
        }
        *remaining -= buf.len() as u64;
        let slot = match path.as_str() {
            "localsky.toml" => &mut parts.config,
            "localsky.ledger.toml" => &mut parts.ledger,
            "irrigation.db" => &mut parts.db,
            "manifest.json" => &mut parts.manifest,
            _ => continue,
        };
        if slot.replace(buf).is_some() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("duplicate bundle part: {path}"),
            ));
        }
    }
    Ok(parts)
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

fn operation_err(
    status: StatusCode,
    error: &(dyn std::error::Error + 'static),
    operation: &'static str,
) -> Response {
    let failure = crate::diagnostics::from_error(error, operation);
    tracing::error!(%failure, operation, "backup or restore operation failed");
    (status, Json(serde_json::json!({ "error": failure.to_string(), "diagnostic": crate::failure::FailureRecord::now(failure) }))).into_response()
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
        let res =
            tokio::task::spawn_blocking(move || -> Result<(), Box<crate::failure::Failure>> {
                let conn = db.blocking_lock();
                let _ = std::fs::remove_file(&tmp_clone);
                conn.execute("VACUUM INTO ?1", rusqlite::params![tmp_clone])
                    .map_err(|e| {
                        Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
                    })
                    .map(|_| ())
            })
            .await;
        match res {
            Ok(Ok(())) => Some(db_tmp.clone()),
            Ok(Err(e)) => return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "db copy"),
            Err(e) => return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "join"),
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
    // The server-owned record beside it, so a restore onto new hardware
    // carries the migration state and the seeding record with the config.
    let ledger_toml: Option<Vec<u8>> = tokio::fs::read_to_string(s.cfg_store.ledger_path())
        .await
        .ok()
        .map(String::into_bytes);

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
        tokio::task::spawn_blocking(move || -> Result<(), Box<crate::failure::Failure>> {
            let out = std::fs::File::create(&bundle_tmp).map_err(|e| {
                Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
            })?;
            let gz = flate2::write::GzEncoder::new(out, flate2::Compression::default());
            let mut tar = tar::Builder::new(gz);
            let mut add = |name: &str, bytes: &[u8]| -> Result<(), Box<crate::failure::Failure>> {
                let mut h = tar::Header::new_gnu();
                h.set_size(bytes.len() as u64);
                h.set_mode(0o600);
                h.set_mtime(chrono::Utc::now().timestamp() as u64);
                h.set_cksum();
                tar.append_data(&mut h, name, bytes).map_err(|e| {
                    Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
                })
            };
            add(
                "manifest.json",
                serde_json::to_vec_pretty(&manifest)
                    .map_err(|e| {
                        Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
                    })?
                    .as_slice(),
            )?;
            if let Some(cfg) = &config_toml {
                add("localsky.toml", cfg)?;
            }
            if let Some(l) = &ledger_toml {
                add("localsky.ledger.toml", l)?;
            }
            if let Some(path) = &db_copy {
                let mut f = std::fs::File::open(path).map_err(|e| {
                    Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
                })?;
                let len = f
                    .metadata()
                    .map_err(|e| {
                        Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
                    })?
                    .len();
                let mut h = tar::Header::new_gnu();
                h.set_size(len);
                h.set_mode(0o600);
                h.set_mtime(chrono::Utc::now().timestamp() as u64);
                h.set_cksum();
                tar.append_data(&mut h, "irrigation.db", &mut f)
                    .map_err(|e| {
                        Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
                    })?;
            }
            let gz = tar.into_inner().map_err(|e| {
                Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
            })?;
            gz.finish().map_err(|e| {
                Box::new(crate::diagnostics::from_error(&e, "backup bundle creation"))
            })?;
            Ok(())
        })
        .await
    };
    match build {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return operation_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e,
                "backup bundle creation",
            )
        }
        Err(e) => return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "join"),
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
        Err(e) => return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "bundle open"),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "bundle stat"),
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

fn merge_bundle_parts(target: &mut BundleParts, incoming: BundleParts) -> Result<(), String> {
    for (name, slot, value) in [
        ("config", &mut target.config, incoming.config),
        ("ledger", &mut target.ledger, incoming.ledger),
        ("db", &mut target.db, incoming.db),
        ("manifest", &mut target.manifest, incoming.manifest),
    ] {
        if let Some(value) = value {
            if slot.replace(value).is_some() {
                return Err(format!("duplicate restore part: {name}"));
            }
        }
    }
    Ok(())
}

async fn post_restore(State(s): State<BackupApiState>, mut multipart: Multipart) -> Response {
    let mut parts = BundleParts::default();
    let mut decompressed_budget = RESTORE_DECOMPRESSED_LIMIT;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => return operation_err(StatusCode::BAD_REQUEST, &e, "upload read"),
        };
        let name = field.name().unwrap_or("").to_string();
        let Ok(data) = field.bytes().await else {
            return err(StatusCode::BAD_REQUEST, "upload read failed");
        };
        let incoming = match name.as_str() {
            "bundle" => match unpack_bundle(data.as_ref(), &mut decompressed_budget) {
                Ok(parts) => parts,
                Err((status, msg)) => return err(status, msg),
            },
            "config" => BundleParts {
                config: Some(data.to_vec()),
                ..Default::default()
            },
            "db" => BundleParts {
                db: Some(data.to_vec()),
                ..Default::default()
            },
            _ => continue,
        };
        if let Err(msg) = merge_bundle_parts(&mut parts, incoming) {
            return err(StatusCode::BAD_REQUEST, msg);
        }
    }
    if parts.config.is_none() && parts.db.is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "nothing to restore; send bundle=, config=, or db=",
        );
    }

    // Validate the ENTIRE upload before taking a writer lock or touching any
    // live/staged file. Scratch probe files have unique names and drop guards;
    // rejected uploads cannot replace an earlier accepted restore.
    let config_text = match parts.config.as_deref() {
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(_) => return err(StatusCode::UNPROCESSABLE_ENTITY, "config is not UTF-8"),
        },
        None => None,
    };
    let config = if let Some(text) = config_text {
        let cfg: crate::config::schema::Config = match toml::from_str(text) {
            Ok(cfg) => cfg,
            Err(e) => return operation_err(StatusCode::UNPROCESSABLE_ENTITY, &e, "config parse"),
        };
        if cfg.schema_version > crate::config::schema::CURRENT_SCHEMA_VERSION {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "config schema is newer than this LocalSky supports",
            );
        }
        let report = crate::config::validate::validate(&cfg);
        if !report.ok() {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "config_invalid", "validation": report,
                })),
            )
                .into_response();
        }
        Some(cfg)
    } else {
        None
    };
    let ledger_text = match parts.ledger.as_deref() {
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(_) => return err(StatusCode::UNPROCESSABLE_ENTITY, "ledger is not UTF-8"),
        },
        None => None,
    };
    let ledger = if let Some(text) = ledger_text {
        let ledger = match toml::from_str::<crate::config::ledger::Ledger>(text) {
            Ok(ledger) => ledger,
            Err(e) => {
                return operation_err(StatusCode::UNPROCESSABLE_ENTITY, &e, "restore ledger parse")
            }
        };
        if config.is_none() {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "a bundled ledger requires its config",
            );
        }
        Some(ledger)
    } else {
        None
    };
    let probe_guard = if let Some(bytes) = parts.db.as_ref() {
        if !bytes.starts_with(b"SQLite format 3\0") {
            return err(StatusCode::UNPROCESSABLE_ENTITY, "db is not a SQLite file");
        }
        let probe = format!(
            "{}.restore-probe-{}-{}",
            s.db_path,
            std::process::id(),
            BACKUP_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let guard = TempFileGuard(probe.clone());
        if let Err(e) = tokio::fs::write(&probe, bytes).await {
            return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "db probe write");
        }
        let verdict = tokio::task::spawn_blocking(move || probe_localsky_db(&probe)).await;
        match verdict {
            Ok(Ok(())) => Some(guard),
            Ok(Err(error)) => {
                let mut response = serde_json::json!({ "error": error.to_string(), "diagnostic": crate::failure::FailureRecord::now(error.diagnostic()) });
                response["code"] = serde_json::json!("restore_database_invalid");
                return (StatusCode::UNPROCESSABLE_ENTITY, Json(response)).into_response();
            }
            Err(e) => return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "db probe"),
        }
    } else {
        None
    };

    let accepted = AcceptedRestore {
        config,
        config_text: config_text.map(str::to_owned),
        ledger,
        ledger_text: ledger_text.map(str::to_owned),
        probe_guard,
        bundle_manifest: parts
            .manifest
            .as_deref()
            .and_then(|bytes| serde_json::from_slice(bytes).ok()),
    };
    // An accepted restore owns its scratch file and both writer guards until
    // commit/rollback and runtime publication finish. Dropping the HTTP future
    // only detaches this task; it cannot cancel an already-started file write.
    match tokio::spawn(commit_restore(s, accepted)).await {
        Ok(response) => response,
        Err(e) => operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "restore transaction"),
    }
}

struct AcceptedRestore {
    config: Option<crate::config::schema::Config>,
    config_text: Option<String>,
    ledger: Option<crate::config::ledger::Ledger>,
    ledger_text: Option<String>,
    probe_guard: Option<TempFileGuard>,
    bundle_manifest: Option<serde_json::Value>,
}

async fn commit_restore(s: BackupApiState, accepted: AcceptedRestore) -> Response {
    let AcceptedRestore {
        config,
        config_text,
        ledger,
        ledger_text,
        probe_guard,
        bundle_manifest,
    } = accepted;
    // Config-only and database-bearing restores use the same read-modify-write
    // serialization as settings/wizard/rollback writers, including publication
    // of the resulting runtime hold before another writer can enter.
    let _write_guard = s.cfg_store.begin_write().await;
    let staged_db = probe_guard.is_some();
    let staged_config = staged_db && config.is_some();
    let mut applied_config = false;
    let mut restart_reasons = Vec::new();
    if let Some(probe) = probe_guard.as_ref() {
        // Finish entered commands, then latch the hold before any stage can
        // change. Both the hold and command barrier protect the mutation phase.
        let order = s
            .runtime
            .as_ref()
            .map(|h| h.dispatch_context.command_order());
        let _commands = match order.as_ref() {
            Some(order) => Some(order.write().await),
            None => None,
        };
        if let Some(h) = &s.runtime {
            h.dispatch_context.restart_hold().latch([
                "An accepted database restore requires LocalSky to restart before new watering can start.".to_string(),
            ]);
        }
        if let Err(e) = s
            .cfg_store
            .stage_restore_bundle(
                config_text.as_deref(),
                ledger_text.as_deref(),
                &s.db_path,
                &probe.0,
            )
            .await
        {
            if let Some(h) = &s.runtime {
                h.dispatch_context.restart_hold().latch([
                    "A restore staging write failed. Check recovery files and restart LocalSky before starting new watering.".to_string(),
                ]);
            }
            return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "restore stage");
        }
        restart_reasons.push(if staged_config {
            "A restored configuration and History database are staged. Restart LocalSky to apply the backup."
        } else {
            "A restored History database is staged. Restart LocalSky to apply the backup."
        }.to_string());
        if let Some(h) = &s.runtime {
            h.dispatch_context.restart_hold().latch(restart_reasons);
            restart_reasons = h.dispatch_context.restart_hold().reasons();
        }
    } else if let Some(cfg) = config.as_ref() {
        let prev_cfg = s.cfg_store.load().await.ok();
        let hot_restore = match s
            .cfg_store
            .restore_config_pair(cfg, ledger, &s.db_path)
            .await
        {
            Ok(restore) => restore,
            Err(e) => {
                hold_failed_config_restore(&s).await;
                return operation_err(StatusCode::INTERNAL_SERVER_ERROR, &e, "config restore");
            }
        };
        applied_config = true;
        if let Some(h) = &s.runtime {
            restart_reasons = crate::runtime::apply_runtime_config(h, prev_cfg.as_ref(), cfg)
                .await
                .restart_reasons;
        }
        match tokio::task::spawn_blocking(move || hot_restore.complete()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                hold_failed_config_restore(&s).await;
                return operation_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &error,
                    "config restore completion",
                );
            }
            Err(error) => {
                hold_failed_config_restore(&s).await;
                return operation_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &error,
                    "config restore worker",
                );
            }
        }
    }
    let restart_required = staged_db || !restart_reasons.is_empty();
    let note = if staged_db {
        "backup staged; new watering is held until restart applies the restore"
    } else if restart_required {
        "config restored and hot-applied; new watering is held until restart applies pending connections"
    } else {
        "config restored and hot-applied to the running engine"
    };
    Json(serde_json::json!({
        "ok": true,
        "config_applied": applied_config,
        "config_staged": staged_config,
        "db_staged": staged_db,
        "restart_required": restart_required,
        "restart_reasons": restart_reasons,
        "bundle_manifest": bundle_manifest,
        "note": note,
    }))
    .into_response()
}

async fn hold_failed_config_restore(s: &BackupApiState) {
    if let Some(h) = &s.runtime {
        let order = h.dispatch_context.command_order();
        let _commands = order.write().await;
        h.dispatch_context.restart_hold().latch([
            "Configuration restore did not finish. Restart LocalSky to resume its verified recovery journal before starting new watering.".to_string(),
        ]);
    }
}

/// GET /api/v1/backup/snapshots -> the config snapshot history.
///
/// Proxies the ConfigStore, so this is byte-for-byte the response
/// GET /api/v1/config/snapshots gives: the on-disk
/// <config_dir>/snapshots/<ts>.toml files every save writes and
/// POST /api/v1/config/rollback restores from. That is the ONLY config
/// history the product has.
///
/// It used to list the `config_snapshots` SQLite table instead. Nothing
/// ever inserted into that table, so this route answered an empty list on
/// installs with twenty restore points. See src/persistence/
/// config_snapshots.rs for why that store is retired rather than fed.
///
/// The `ts` key is load-bearing, not cosmetic: it is the id
/// POST /config/rollback takes, so a caller can pipe a row from here
/// straight back into a rollback.
async fn get_snapshots(State(s): State<BackupApiState>) -> Response {
    match s.cfg_store.list_snapshots().await {
        Ok(list) => {
            let snapshots: Vec<_> = list
                .into_iter()
                .map(|v| {
                    serde_json::json!({
                        "ts": v.version,
                        "applied_at_epoch": v.applied_at_epoch,
                        "schema_version": v.schema_version,
                        "note": v.note,
                    })
                })
                .collect();
            Json(serde_json::json!({ "snapshots": snapshots })).into_response()
        }
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
        cfg.notifications.ntfy = Some(NtfyConfig {
            base_url: "https://ntfy.example.com".into(),
            topic: "lawn".into(),
            auth_token: Some("tk_ntfy_secret".into()),
        });
        std::fs::write(&src_cfg_path, toml::to_string_pretty(&cfg).unwrap()).unwrap();

        let src_state = BackupApiState {
            cfg_store: Arc::new(FileConfigStore::new(&src_cfg_path)),
            db: None,
            db_path: dir
                .join("source/irrigation.db")
                .to_string_lossy()
                .to_string(),
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
            bundled.contains("tk_ntfy_secret"),
            "backup must contain the real ntfy token"
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
        let ntfy = loaded.notifications.ntfy.as_ref().expect("ntfy config");
        assert_eq!(
            ntfy.auth_token.as_deref(),
            Some("tk_ntfy_secret"),
            "restored ntfy token must be the REAL value"
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
            runtime: None,
        }
    }

    fn runtime_for(cfg: &crate::config::schema::Config) -> crate::runtime::RuntimeHandles {
        use arc_swap::ArcSwap;
        crate::runtime::RuntimeHandles {
            dispatch_context: crate::controllers::ZoneLocks::default(),
            tempest_store: Arc::new(crate::tempest::state::TempestStore::new()),
            forecast_priority: Arc::new(ArcSwap::from_pointee(std::collections::HashMap::new())),
            watering_policy: Arc::new(ArcSwap::from_pointee(
                crate::refresher::WateringPolicy::from_config(cfg),
            )),
            manual_schedules: Arc::new(ArcSwap::from_pointee(Vec::new())),
            source_reachable: crate::sources::SourceReachability::default(),
            source_last_seen: Some(crate::sources::SourceLastSeen::default()),
            push: None,
        }
    }

    /// A real current-schema database, created by the same runner as boot.
    fn localsky_db_bytes(dir: &std::path::Path) -> Vec<u8> {
        let p = dir.join("donor.db");
        let mut conn = Connection::open(&p).unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
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

    #[tokio::test]
    async fn duplicate_bundle_config_rejects_the_whole_upload_without_staging() {
        let dir = test_dir("duplicate-config");
        let state = state_for(&dir);
        let config = toml::to_string_pretty(&sited_config()).unwrap();
        let bundle = build_bundle(&[
            ("localsky.toml", config.as_bytes()),
            ("localsky.toml", b"invalid later config"),
        ]);
        let resp = post_restore(
            State(state.clone()),
            multipart_with("bundle", "backup.tar.gz", &bundle).await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!state.cfg_store.path().exists());
        assert!(!crate::config::store::staged_path(state.cfg_store.path()).exists());
    }

    // ---- restore schema probe ----

    #[test]
    fn db_probe_accepts_localsky_and_rejects_foreign_or_corrupt() {
        let dir = test_dir("probe");

        // A real fully migrated LocalSky database passes.
        let ok = dir.join("ok.db");
        let mut conn = Connection::open(&ok).unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
        drop(conn);
        assert!(probe_localsky_db(ok.to_str().unwrap()).is_ok());

        // A valid SQLite file with a foreign schema fails with the friendly
        // message.
        let alien = dir.join("alien.db");
        let conn = Connection::open(&alien).unwrap();
        conn.execute_batch("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT);")
            .unwrap();
        drop(conn);
        let msg = probe_localsky_db(alien.to_str().unwrap())
            .unwrap_err()
            .to_string();
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
    async fn restore_rejects_malformed_future_or_inconsistent_migration_history() {
        for (name, mutation) in [
            (
                "ledger-columns",
                "DROP TABLE schema_migrations; CREATE TABLE schema_migrations(version TEXT);",
            ),
            (
                "future-version",
                "INSERT INTO schema_migrations VALUES ('M9999', 'future', 1);",
            ),
            (
                "history-gap",
                "DELETE FROM schema_migrations WHERE version = 'M0004';",
            ),
            (
                "false-name",
                "UPDATE schema_migrations SET name = 'forged' WHERE version = 'M0003';",
            ),
            (
                "bad-timestamp",
                "UPDATE schema_migrations SET applied_at = 'not an epoch' WHERE version = 'M0003';",
            ),
            ("empty-history", "DELETE FROM schema_migrations;"),
            ("missing-table", "DROP TABLE runs;"),
            ("missing-index", "DROP INDEX uq_runs_zone_start_ctrl;"),
            // The claimed prefix has all its required columns, but a later
            // unrecorded ALTER makes the normal pending migration fail.
            (
                "unrecorded-alter",
                "DELETE FROM schema_migrations WHERE version IN ('M0019', 'M0020');",
            ),
        ] {
            let dir = test_dir(&format!("reject-{name}"));
            let donor = dir.join("donor.db");
            let mut conn = Connection::open(&donor).unwrap();
            crate::persistence::run_migrations(&mut conn).unwrap();
            conn.execute_batch(mutation).unwrap();
            drop(conn);
            let original = std::fs::read(&donor).unwrap();
            let state = state_for(&dir);
            let stage = format!("{}.restore", state.db_path);
            std::fs::write(&stage, b"previous accepted stage").unwrap();
            let resp = post_restore(
                State(state),
                multipart_with("db", "irrigation.db", &original).await,
            )
            .await;
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{name}");
            assert_eq!(
                std::fs::read(&stage).unwrap(),
                b"previous accepted stage",
                "{name}"
            );
            assert_eq!(
                std::fs::read(&donor).unwrap(),
                original,
                "probe changed uploaded bytes: {name}"
            );
            assert!(
                !std::fs::read_dir(&dir).unwrap().flatten().any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .contains("restore-probe"))
            );
        }
    }

    #[test]
    fn a_one_table_fixture_or_forged_complete_ledger_is_not_a_valid_database() {
        let dir = test_dir("forged-ledger");
        let path = dir.join("fake.db");
        let conn = Connection::open(&path).unwrap();
        // This was the old positive fixture; it proves neither real ledger
        // column types nor a usable LocalSky schema.
        conn.execute_batch(
            "CREATE TABLE schema_migrations(version TEXT PRIMARY KEY, name TEXT, applied_at TEXT);
            INSERT INTO schema_migrations VALUES('M0001', 'baseline schema', '2026-01-01');",
        )
        .unwrap();
        assert!(probe_localsky_db(path.to_str().unwrap()).is_err());
        conn.execute_batch("DROP TABLE schema_migrations;").unwrap();
        conn.execute_batch(crate::persistence::MIGRATIONS[0].sql)
            .unwrap();
        for migration in crate::persistence::MIGRATIONS {
            conn.execute(
                "INSERT INTO schema_migrations VALUES (?1, ?2, 1)",
                [migration.version, migration.name],
            )
            .unwrap();
        }
        drop(conn);
        let err = probe_localsky_db(path.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not match its migration history"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn legitimate_old_database_migrates_on_probe_but_stages_original_bytes() {
        let dir = test_dir("old-schema-probe");
        let donor = dir.join("old.db");
        let mut conn = Connection::open(&donor).unwrap();
        let older_count = crate::persistence::MIGRATIONS.len() - 2;
        for migration in crate::persistence::MIGRATIONS.iter().take(older_count) {
            let tx = conn.transaction().unwrap();
            tx.execute_batch(migration.sql).unwrap();
            tx.execute(
                "INSERT INTO schema_migrations VALUES (?1, ?2, 1)",
                [migration.version, migration.name],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        conn.execute(
            "INSERT INTO runs(zone_slug, start_epoch) VALUES ('fixture_yard', 123)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO schema_migrations VALUES ('M0007_legacy', 'push_subscriptions (legacy store)', 1)", []).unwrap();
        drop(conn);
        let original = std::fs::read(&donor).unwrap();
        let state = state_for(&dir);
        let stage = format!("{}.restore", state.db_path);
        let resp = post_restore(
            State(state),
            multipart_with("db", "irrigation.db", &original).await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(std::fs::read(&stage).unwrap(), original);
        assert_eq!(std::fs::read(&donor).unwrap(), original);
        let uploaded = Connection::open(&stage).unwrap();
        let count: i64 = uploaded
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count,
            (older_count + 1) as i64,
            "only the disposable proof was upgraded; the historical legacy marker is retained"
        );
        let zone: String = uploaded
            .query_row(
                "SELECT zone_slug FROM runs WHERE start_epoch = 123",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(zone, "fixture_yard");
        assert!(
            !std::fs::read_dir(&dir).unwrap().flatten().any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("migration-proof"))
        );
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

    /// A config that passes : the only requirement is a location
    /// that is not null island.
    fn sited_config() -> crate::config::schema::Config {
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location = crate::config::schema::Location {
            lat: 28.5,
            lon: -81.4,
            elevation_m: None,
        };
        cfg
    }

    #[tokio::test]
    async fn invalid_restore_parts_preserve_live_files_and_every_existing_stage() {
        let dir = test_dir("invalid-all-parts");
        let mut state = state_for(&dir);
        let cfg = sited_config();
        state.cfg_store.save(&cfg).await.unwrap();
        state
            .cfg_store
            .update_ledger(|l| l.seeded_source_ids.push("current".into()))
            .await
            .unwrap();
        state.runtime = Some(runtime_for(&cfg));
        let paths = [
            state.cfg_store.path().to_path_buf(),
            state.cfg_store.ledger_path().to_path_buf(),
            std::path::PathBuf::from(&state.db_path),
            crate::config::store::staged_path(state.cfg_store.path()),
            crate::config::store::staged_path(state.cfg_store.ledger_path()),
            std::path::PathBuf::from(format!("{}.restore", state.db_path)),
        ];
        for (index, path) in paths.iter().enumerate().skip(2) {
            std::fs::write(path, format!("previous bytes {index}")).unwrap();
        }
        let before: Vec<_> = paths
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let config = toml::to_string_pretty(&cfg).unwrap();
        let db = localsky_db_bytes(&dir);
        let foreign_path = dir.join("foreign.db");
        Connection::open(&foreign_path)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated(id INTEGER);")
            .unwrap();
        let foreign = std::fs::read(foreign_path).unwrap();
        let mut future = cfg.clone();
        future.schema_version = crate::config::schema::CURRENT_SCHEMA_VERSION + 1;
        let future = toml::to_string_pretty(&future).unwrap();
        let cases = [
            (
                config.as_bytes(),
                b"seeded_source_ids = []".as_slice(),
                b"not sqlite".as_slice(),
            ),
            (
                config.as_bytes(),
                b"seeded_source_ids = []".as_slice(),
                b"SQLite format 3\0broken".as_slice(),
            ),
            (
                config.as_bytes(),
                b"seeded_source_ids = []".as_slice(),
                foreign.as_slice(),
            ),
            (
                config.as_bytes(),
                b"seeded_source_ids = [".as_slice(),
                db.as_slice(),
            ),
            (
                b"not = [".as_slice(),
                b"seeded_source_ids = []".as_slice(),
                db.as_slice(),
            ),
            (
                future.as_bytes(),
                b"seeded_source_ids = []".as_slice(),
                db.as_slice(),
            ),
        ];
        for (config, ledger, db) in cases {
            let bundle = build_bundle(&[
                ("localsky.toml", config),
                ("localsky.ledger.toml", ledger),
                ("irrigation.db", db),
            ]);
            let resp = post_restore(
                State(state.clone()),
                multipart_with("bundle", "backup.tar.gz", &bundle).await,
            )
            .await;
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
            for (path, expected) in paths.iter().zip(&before) {
                assert_eq!(
                    &std::fs::read(path).unwrap(),
                    expected,
                    "{} changed after rejected upload",
                    path.display()
                );
            }
            assert!(!state
                .runtime
                .as_ref()
                .unwrap()
                .dispatch_context
                .restart_hold()
                .is_pending());
            assert!(!std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains("restore-probe")));
        }
    }

    #[tokio::test]
    async fn restore_stage_io_failure_rolls_back_earlier_slots_and_holds_new_watering() {
        let dir = test_dir("stage-rollback");
        let mut state = state_for(&dir);
        let cfg = sited_config();
        state.cfg_store.save(&cfg).await.unwrap();
        state.runtime = Some(runtime_for(&cfg));
        let config_stage = crate::config::store::staged_path(state.cfg_store.path());
        let ledger_stage = crate::config::store::staged_path(state.cfg_store.ledger_path());
        let db_stage = format!("{}.restore", state.db_path);
        std::fs::write(&config_stage, b"previous config stage").unwrap();
        std::fs::write(&db_stage, b"previous database stage").unwrap();
        // Config publishes first; this later non-file destination forces the
        // real handler's ordinary-I/O rollback after the first replacement.
        std::fs::create_dir(&ledger_stage).unwrap();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let db = localsky_db_bytes(&dir);
        let bundle = build_bundle(&[
            ("localsky.toml", text.as_bytes()),
            ("localsky.ledger.toml", b"seeded_source_ids = []"),
            ("irrigation.db", &db),
        ]);
        let resp = post_restore(
            State(state.clone()),
            multipart_with("bundle", "backup.tar.gz", &bundle).await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            std::fs::read(&config_stage).unwrap(),
            b"previous config stage"
        );
        assert_eq!(
            std::fs::read(&db_stage).unwrap(),
            b"previous database stage"
        );
        assert!(ledger_stage.is_dir());
        assert!(state
            .runtime
            .as_ref()
            .unwrap()
            .dispatch_context
            .restart_hold()
            .is_pending());
    }

    #[tokio::test]
    async fn db_only_restore_uses_writer_and_command_order_then_latches_shared_restart_hold() {
        let dir = test_dir("restore-order-hold");
        let mut state = state_for(&dir);
        let cfg = sited_config();
        state.cfg_store.save(&cfg).await.unwrap();
        state.runtime = Some(runtime_for(&cfg));
        let runtime = state.runtime.as_ref().unwrap();
        let order = runtime.dispatch_context.command_order();
        let entered_command = order.read().await;
        let writer = state.cfg_store.begin_write().await;
        let db = localsky_db_bytes(&dir);
        let mp = multipart_with("db", "irrigation.db", &db).await;
        let restore = post_restore(State(state.clone()), mp);
        tokio::pin!(restore);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut restore)
                .await
                .is_err()
        );
        assert!(!std::path::Path::new(&format!("{}.restore", state.db_path)).exists());
        drop(writer);
        // Once the config writer releases, an entered valve command still
        // finishes before the restore can publish stages or latch the hold.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut restore)
                .await
                .is_err()
        );
        assert!(!runtime.dispatch_context.restart_hold().is_pending());
        drop(entered_command);
        let resp = restore.await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["db_staged"], true);
        assert_eq!(body["restart_required"], true);
        let reasons = runtime.dispatch_context.restart_hold().reasons();
        assert!(!reasons.is_empty());
        assert_eq!(body["restart_reasons"], serde_json::json!(reasons));
        let later = crate::runtime::apply_runtime_config(runtime, Some(&cfg), &cfg).await;
        assert_eq!(
            later.restart_reasons, reasons,
            "later saves preserve the staged restore hold"
        );
    }

    #[tokio::test]
    async fn cancelled_restore_request_keeps_accepted_transaction_and_probe_alive() {
        let dir = test_dir("restore-request-cancel");
        let mut state = state_for(&dir);
        let cfg = sited_config();
        state.cfg_store.save(&cfg).await.unwrap();
        state.runtime = Some(runtime_for(&cfg));
        let runtime = state.runtime.as_ref().unwrap();
        let order = runtime.dispatch_context.command_order();
        let entered_command = order.read().await;
        let db = localsky_db_bytes(&dir);
        let mp = multipart_with("db", "irrigation.db", &db).await;
        let request = tokio::spawn(post_restore(State(state.clone()), mp));

        // Wait until the accepted transaction owns the config writer and is
        // waiting for the already-entered controller command to finish.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(20),
                    state.cfg_store.begin_write(),
                )
                .await
                {
                    Err(_) => break,
                    Ok(guard) => drop(guard),
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("accepted transaction takes the writer lock");
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(30),
                state.cfg_store.begin_write(),
            )
            .await
            .is_err(),
            "HTTP cancellation must not release the accepted transaction's writer"
        );
        assert!(std::fs::read_dir(&dir).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".restore-probe-")));
        assert!(!runtime.dispatch_context.restart_hold().is_pending());
        let db_stage = format!("{}.restore", state.db_path);
        assert!(!std::path::Path::new(&db_stage).exists());

        drop(entered_command);
        // Taking this writer proves the detached transaction has finished its
        // stage/hold publication, without needing its discarded HTTP response.
        let _finished = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.cfg_store.begin_write(),
        )
        .await
        .expect("accepted restore finishes after the entered command");
        assert_eq!(std::fs::read(&db_stage).unwrap(), db);
        assert!(runtime.dispatch_context.restart_hold().is_pending());
        assert!(!std::fs::read_dir(&dir).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".restore-probe-")));
        assert_eq!(
            state
                .cfg_store
                .load()
                .await
                .unwrap()
                .deployment
                .display_name,
            cfg.deployment.display_name,
            "a DB-only restore leaves the live configuration unchanged"
        );
    }

    #[tokio::test]
    async fn successful_db_only_restore_replaces_prior_request_without_inheriting_its_config() {
        let dir = test_dir("replace-stage-set");
        let state = state_for(&dir);
        let config_stage = crate::config::store::staged_path(state.cfg_store.path());
        let ledger_stage = crate::config::store::staged_path(state.cfg_store.ledger_path());
        std::fs::write(&config_stage, b"earlier config").unwrap();
        std::fs::write(&ledger_stage, b"earlier ledger").unwrap();
        let db = localsky_db_bytes(&dir);
        let resp = post_restore(
            State(state.clone()),
            multipart_with("db", "irrigation.db", &db).await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!config_stage.exists());
        assert!(!ledger_stage.exists());
        assert_eq!(
            std::fs::read(format!("{}.restore", state.db_path)).unwrap(),
            db
        );
    }

    // The ledger is a separate file: a config-only restore of a document
    // with no records in it cannot drop the running install's records.
    #[tokio::test]
    async fn restoring_a_config_cannot_drop_the_ledger() {
        let dir = test_dir("ledger-survives-restore");
        let state = state_for(&dir);
        state.cfg_store.save(&sited_config()).await.unwrap();
        state
            .cfg_store
            .update_ledger(|l| {
                for id in crate::ha_adopt::ENTITIES {
                    l.ha_adoption.push(crate::model::HaAdoptedHelper {
                        entity: id.to_string(),
                        outcome: crate::ha_adopt::OUTCOME_NOT_FOUND.to_string(),
                        target: crate::ha_adopt::target_of(id).to_string(),
                        adopted_value: None,
                        observed_value: None,
                        previous_value: None,
                        epoch: 1,
                    });
                }
                l.seeded_source_ids.push("nws".into());
            })
            .await
            .unwrap();

        let mut old = sited_config();
        old.engine.skip_rules.max_wind_mph = 22.0;
        let body = toml::to_string_pretty(&old).unwrap();
        let cfg_store = state.cfg_store.clone();
        let mp = multipart_with("config", "localsky.toml", body.as_bytes()).await;
        let resp = post_restore(State(state), mp).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["config_applied"], true);

        let after = cfg_store.load().await.unwrap();
        assert_eq!(after.engine.skip_rules.max_wind_mph, 22.0);
        let ledger = cfg_store.ledger();
        assert_eq!(ledger.ha_adoption.len(), crate::ha_adopt::ENTITIES.len());
        assert!(ledger.seeded_source_ids.contains(&"nws".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A bundle with a database stages everything it carries; nothing is
    // applied until the boot that swaps all of it in.
    #[tokio::test]
    async fn successful_threshold_only_restore_clears_fence_without_requiring_restart() {
        let dir = test_dir("hot-restore-success");
        let mut state = state_for(&dir);
        let cfg = sited_config();
        state.cfg_store.save(&cfg).await.unwrap();
        state.runtime = Some(runtime_for(&cfg));
        let mut restored = cfg;
        restored.engine.skip_rules.max_wind_mph = 24.0;
        let text = toml::to_string_pretty(&restored).unwrap();
        let response = post_restore(
            State(state.clone()),
            multipart_with("config", "localsky.toml", text.as_bytes()).await,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["restart_required"], false);
        assert!(!state
            .runtime
            .as_ref()
            .unwrap()
            .dispatch_context
            .restart_hold()
            .is_pending());
        assert!(crate::config::restore::activate_at_boot(
            state.cfg_store.path(),
            std::path::Path::new(&state.db_path),
        )
        .unwrap()
        .is_none());
        assert_eq!(
            state
                .cfg_store
                .load()
                .await
                .unwrap()
                .engine
                .skip_rules
                .max_wind_mph,
            24.0
        );
    }

    #[tokio::test]
    async fn config_only_ledger_error_is_detected_before_either_live_file_changes() {
        let dir = test_dir("hot-restore-ledger-failure");
        let mut state = state_for(&dir);
        let cfg = sited_config();
        state.cfg_store.save(&cfg).await.unwrap();
        state.runtime = Some(runtime_for(&cfg));
        // An invalid ledger target must be found before the first config write.
        std::fs::create_dir(state.cfg_store.ledger_path()).unwrap();
        let mut restored = cfg;
        restored.engine.skip_rules.max_wind_mph = 31.0;
        let text = toml::to_string_pretty(&restored).unwrap();
        let bundle = build_bundle(&[
            ("localsky.toml", text.as_bytes()),
            (
                "localsky.ledger.toml",
                b"seeded_source_ids = ['restored']\n",
            ),
        ]);
        let response = post_restore(
            State(state.clone()),
            multipart_with("bundle", "backup.tar.gz", &bundle).await,
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            crate::config::loader::load_from_path(state.cfg_store.path())
                .unwrap()
                .engine
                .skip_rules
                .max_wind_mph,
            10.0,
        );
        assert!(state
            .runtime
            .as_ref()
            .unwrap()
            .dispatch_context
            .restart_hold()
            .is_pending());
        assert!(crate::config::restore::activate_at_boot(
            state.cfg_store.path(),
            std::path::Path::new(&state.db_path),
        )
        .unwrap()
        .is_none());
        assert!(
            !std::path::Path::new(&state.db_path).exists(),
            "refusing boot must not create an empty history DB"
        );
    }

    #[tokio::test]
    async fn a_bundle_with_a_database_stages_config_and_ledger_for_the_boot() {
        let dir = test_dir("stage-together");
        let state = state_for(&dir);
        let running = sited_config();
        state.cfg_store.save(&running).await.unwrap();

        let mut restored = sited_config();
        restored.engine.skip_rules.max_wind_mph = 33.0;
        let cfg_text = toml::to_string_pretty(&restored).unwrap();
        let ledger_text = "seeded_source_ids = [\"met_no\"]\n";
        let db = localsky_db_bytes(&dir);
        let bundle = build_bundle(&[
            ("localsky.toml", cfg_text.as_bytes()),
            ("localsky.ledger.toml", ledger_text.as_bytes()),
            ("irrigation.db", &db),
        ]);
        let cfg_store = state.cfg_store.clone();
        let db_path = state.db_path.clone();
        let mp = multipart_with("bundle", "backup.tar.gz", &bundle).await;
        let resp = post_restore(State(state), mp).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["config_applied"], false);
        assert_eq!(body["config_staged"], true);
        assert_eq!(body["db_staged"], true);
        assert_eq!(body["restart_required"], true);

        // Nothing moved yet.
        let live = cfg_store.load().await.unwrap();
        assert_eq!(
            live.engine.skip_rules.max_wind_mph,
            running.engine.skip_rules.max_wind_mph
        );
        assert!(cfg_store.ledger().seeded_source_ids.is_empty());
        assert!(crate::config::store::staged_path(cfg_store.path()).exists());
        assert!(std::path::Path::new(&format!("{db_path}.restore")).exists());

        // Production boot verifies/activates the complete marked set before
        // opening either configuration or history, then completes the marker.
        let activated = crate::config::restore::activate_at_boot(
            cfg_store.path(),
            std::path::Path::new(&db_path),
        )
        .unwrap()
        .unwrap();
        let restored_db = crate::persistence::HistoryDb::open(db_path.clone().into()).unwrap();
        let after = cfg_store.load().await.unwrap();
        assert_eq!(after.engine.skip_rules.max_wind_mph, 33.0);
        assert_eq!(cfg_store.ledger().seeded_source_ids, vec!["met_no"]);
        assert!(std::path::Path::new(&db_path).exists());
        activated.complete().unwrap();
        drop(restored_db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- disk-staged, streamed backup download ----

    #[tokio::test]
    async fn get_backup_streams_bundle_and_cleans_temp_files() {
        use std::io::Read;

        let dir = test_dir("stream");
        std::fs::write(dir.join("localsky.toml"), "schema_version = 1\n").unwrap();
        let db_path = dir.join("irrigation.db");
        let mut conn = Connection::open(&db_path).unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();

        let state = BackupApiState {
            cfg_store: Arc::new(FileConfigStore::new(dir.join("localsky.toml"))),
            db: Some(Arc::new(Mutex::new(conn))),
            db_path: db_path.to_string_lossy().to_string(),
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

    // ---- the documented config history is the on-disk one ----

    /// The endpoint must report the restore points that EXIST. It used to
    /// list the `config_snapshots` SQLite table, which no production code
    /// has ever inserted into, so it answered `{"snapshots": []}` on an
    /// install with twenty restore points while docs and its own header
    /// promised the history behind POST /config/rollback. This drives the
    /// route against a store that has one real snapshot on disk: against
    /// the old table-backed reader the list comes back empty and the
    /// length assertion fails.
    #[tokio::test]
    async fn backup_snapshots_lists_the_on_disk_config_history() {
        use crate::config::schema::Config;

        let dir = test_dir("snapshots");
        let store = FileConfigStore::new(dir.join("localsky.toml"));

        // Two saves through the same ConfigStore::save the settings PUT
        // uses: the first lands the file, the second snapshots it. That is
        // exactly how a restore point comes into existence.
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.deployment.display_name = "before".into();
        store.save(&cfg).await.unwrap();
        cfg.deployment.display_name = "after".into();
        store.save(&cfg).await.unwrap();

        let on_disk = store.list_snapshots().await.unwrap();
        assert_eq!(on_disk.len(), 1, "the second save snapshotted the first");

        let state = BackupApiState {
            cfg_store: Arc::new(store),
            db: None,
            db_path: dir.join("irrigation.db").to_string_lossy().to_string(),
            runtime: None,
        };
        let resp = get_snapshots(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let rows = v["snapshots"].as_array().expect("snapshots array");

        assert_eq!(
            rows.len(),
            on_disk.len(),
            "the endpoint must report the restore points that exist, not an empty list"
        );
        // Same `ts` key GET /config/snapshots emits, because it is the id
        // POST /config/rollback accepts. A row from here must be pipeable
        // straight back into a rollback.
        assert_eq!(
            rows[0]["ts"].as_u64().unwrap(),
            on_disk[0].version as u64,
            "ts must be the snapshot id rollback takes"
        );
        assert_eq!(
            rows[0]["applied_at_epoch"].as_i64().unwrap(),
            on_disk[0].applied_at_epoch
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
