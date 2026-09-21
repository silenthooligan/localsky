// File-backed ConfigStore. Reads + writes /data/localsky.toml.
//
// Durable atomic write: serialize to TOML, write to <path>.tmp, fsync
// the file, rename over the target, fsync the directory. The rename is
// atomic on every POSIX filesystem so a crash mid-write leaves either
// the old or new file but never a truncated one; the two fsyncs make
// the new content + the rename itself survive power loss.
//
// Snapshots: every successful save first copies the previous on-disk
// file to <config_dir>/snapshots/<unix_ts>.toml (newest 20 kept).
// list_snapshots() enumerates that directory; rollback(ts) validates
// the snapshot parses, snapshots the current config, then swaps.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use crate::config::ledger::Ledger;
use crate::config::loader::{self, LoadError};
use crate::config::schema::Config;
use crate::ports::config_store::{ConfigStore, ConfigStoreError, ConfigVersion};

/// Snapshot retention: newest N kept, older pruned on each save.
const SNAPSHOT_KEEP: usize = 20;

pub struct FileConfigStore {
    path: PathBuf,
    /// `localsky.ledger.toml`, the server-owned record beside the config.
    ledger_path: PathBuf,
    /// Serializes WRITERS (save / save_raw_toml / rollback all funnel through
    /// here). Without it two concurrent saves (a settings PUT racing a wizard
    /// apply or a backup restore) interleave on the SHARED <path>.toml.tmp:
    /// writer B's File::create truncates the tmp mid-write of A, and A's
    /// rename can then commit B's torn bytes over localsky.toml. Readers are
    /// unaffected (the rename stays atomic); this only queues writers.
    save_lock: Arc<tokio::sync::Mutex<()>>,
    /// Serializes whole READ-MODIFY-WRITE sequences, one level above
    /// save_lock (which only queues the final file writes and cannot stop
    /// two handlers from loading the same base config and silently
    /// clobbering each other's changes on save). Every handler that loads
    /// the config, mutates it, and saves it holds this via `begin_write`
    /// for the full sequence: config PUT, raw PUT, rollback, the soil
    /// probe removal, and the tuning-report apply. Distinct from
    /// save_lock so a guarded sequence can still call save() without
    /// deadlocking (tokio Mutex is not reentrant).
    write_lock: tokio::sync::Mutex<()>,
}

impl FileConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path: PathBuf = path.into();
        Self {
            ledger_path: Ledger::path_for(&path),
            path,
            save_lock: Arc::new(tokio::sync::Mutex::new(())),
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn ledger_path(&self) -> &Path {
        &self.ledger_path
    }

    /// The ledger as it stands on disk (empty when there is none).
    pub fn ledger(&self) -> Ledger {
        Ledger::load(&self.ledger_path)
    }

    /// Read-modify-write the ledger under the save lock. The closure's
    /// return value is handed back.
    pub async fn update_ledger<T>(
        &self,
        f: impl FnOnce(&mut Ledger) -> T,
    ) -> Result<T, ConfigStoreError> {
        let _guard = self.save_lock.lock().await;
        let mut ledger = Ledger::load(&self.ledger_path);
        let out = f(&mut ledger);
        let path = self.ledger_path.clone();
        tokio::task::spawn_blocking(move || ledger.save(&path))
            .await
            .map_err(|e| ConfigStoreError::io(&e, "config.ledger join error"))?
            .map_err(|e| ConfigStoreError::io(&e, "config.ledger ledger write"))?;
        Ok(out)
    }

    /// Run the recorded config migrations against the raw document. Writes
    /// the document and the ledger back only when something changed, and
    /// says which migrations ran. Pure I/O on the raw TOML: `${VAR}`
    /// references are never expanded here, so nothing secret is written.
    fn migrate_on_disk(path: &Path, ledger_path: &Path) -> Result<Vec<&'static str>, LoadError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(LoadError::NotFound(path.display().to_string()))
            }
            Err(e) => return Err(LoadError::Io(path.display().to_string(), e)),
        };
        let mut doc: toml::Table = toml::from_str(&raw)?;
        let version = crate::config::migrate::document_version(&doc);
        if version > crate::config::schema::CURRENT_SCHEMA_VERSION {
            return Err(LoadError::SchemaTooNew {
                found: version,
                known: crate::config::schema::CURRENT_SCHEMA_VERSION,
            });
        }
        let mut ledger = Ledger::load(ledger_path);
        let applied =
            crate::config::migrate::migrate(&mut doc, &mut ledger, Utc::now().timestamp());
        if applied.is_empty() {
            return Ok(applied);
        }
        let text = toml::to_string_pretty(&doc)
            .map_err(|e| LoadError::Validation(format!("toml serialize: {e}")))?;
        snapshot_current_blocking(path)
            .map_err(|e| LoadError::Io(path.display().to_string(), e))?;
        // The ledger first: a crash between the two leaves a document that
        // still carries its records and a ledger that already has them,
        // which the next load unions harmlessly. The other order could
        // lose the records.
        ledger
            .save(ledger_path)
            .map_err(|e| LoadError::Io(ledger_path.display().to_string(), e))?;
        write_atomic_durable(path, text.as_bytes())
            .map_err(|e| LoadError::Io(path.display().to_string(), e))?;
        tracing::info!(
            migrations = %applied.join(", "),
            config = %path.display(),
            ledger = %ledger_path.display(),
            "config migrated and written back"
        );
        Ok(applied)
    }

    /// Legacy file-layout test primitive. Production boot must activate the
    /// complete marked set through config::restore::activate_at_boot.
    #[cfg(test)]
    fn apply_staged_restore(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut swapped = Vec::new();
        for live in [self.path.clone(), self.ledger_path.clone()] {
            let stage = staged_path(&live);
            if !stage.exists() {
                continue;
            }
            if live.exists() {
                snapshot_current_blocking(&live)?;
            }
            std::fs::rename(&stage, &live)?;
            swapped.push(live);
        }
        Ok(swapped)
    }

    /// Stage a document (and a ledger) to swap in at the next boot, the
    /// same way a restored database is staged, so the two move together.
    #[cfg(test)]
    async fn stage_restore(
        &self,
        config_text: &str,
        ledger_text: Option<&str>,
    ) -> Result<(), ConfigStoreError> {
        let _guard = self.save_lock.lock().await;
        let cfg_stage = staged_path(&self.path);
        let ledger_stage = staged_path(&self.ledger_path);
        let (cfg_bytes, ledger_bytes) = (
            config_text.as_bytes().to_vec(),
            ledger_text.map(|t| t.as_bytes().to_vec()),
        );
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            write_atomic_durable(&cfg_stage, &cfg_bytes)?;
            match ledger_bytes {
                Some(b) => write_atomic_durable(&ledger_stage, &b)?,
                None => {
                    let _ = std::fs::remove_file(&ledger_stage);
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| ConfigStoreError::io(&e, "config.stage_restore join error"))?
        .map_err(|e| ConfigStoreError::io(&e, "config.stage_restore stage"))
    }

    /// Publish a fully validated database-bearing restore under the file writer
    /// lock and a durable fail-closed marker. The caller holds begin_write
    /// across staging and the runtime hold.
    /// Prepare every file first, then replace the three stage slots; ordinary
    /// rename failures roll back earlier slots. These are separate renames, not
    /// a crash-atomic transaction. Boot verifies and activates the marked set
    /// before opening any live state, refusing interrupted sets.
    pub async fn stage_restore_bundle(
        &self,
        config_text: Option<&str>,
        ledger_text: Option<&str>,
        db_path: &str,
        db_probe: &str,
    ) -> Result<(), ConfigStoreError> {
        let config_live = self.path.clone();
        let db_live = PathBuf::from(db_path);
        let config_stage = staged_path(&self.path);
        let ledger_stage = staged_path(&self.ledger_path);
        let db_stage = staged_path(Path::new(db_path));
        let db_probe = PathBuf::from(db_probe);
        let config = config_text.map(str::to_owned);
        let ledger = ledger_text.map(str::to_owned);
        self.run_restore_stage(move || -> std::io::Result<()> {
            let token = format!("{:032x}", rand::random::<u128>());
            let mut prepared = PreparedRestoreFiles(Vec::new());
            let mut entries = Vec::new();
            for (stage, text) in [(config_stage, config), (ledger_stage, ledger)] {
                let candidate = if let Some(text) = text {
                    let candidate = restore_sibling(&stage, &format!("new-{token}"));
                    prepared.0.push(candidate.clone());
                    write_atomic_durable(&candidate, text.as_bytes())?;
                    Some(candidate)
                } else {
                    None
                };
                // Missing parts replace any previous request's staged part;
                // a DB-only upload must never inherit an older staged config.
                entries.push((stage, candidate));
            }
            std::fs::File::open(&db_probe)?.sync_all()?;
            entries.push((db_stage, Some(db_probe)));
            let candidates = std::array::from_fn(|n| entries[n].1.clone());
            let publication = super::restore::Publication::begin(
                &config_live,
                &db_live,
                &candidates,
                token.clone(),
            )?;
            publication.finish()
        })
        .await
    }

    /// Restore config and ledger under the same disk-writer lock. The worker
    /// retains that lock through its actual completion even if its waiter drops.
    pub(crate) async fn restore_config_pair(
        &self,
        cfg: &Config,
        bundled: Option<Ledger>,
        db_path: &str,
    ) -> Result<super::restore::HotRestore, ConfigStoreError> {
        loader::validate(cfg).map_err(map_load_err)?;
        let config_text = toml::to_string_pretty(cfg)
            .map_err(|e| ConfigStoreError::io(&e, "config.restore_config_pair"))?;
        let config_path = self.path.clone();
        let ledger_path = self.ledger_path.clone();
        let database_path = PathBuf::from(db_path);
        let guard = self.save_lock.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> std::io::Result<_> {
            let _guard = guard;
            let original = match std::fs::read_to_string(&ledger_path) {
                Ok(text) => Some(text),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            let ledger_text = if let Some(mut bundled) = bundled {
                let current = original
                    .as_deref()
                    .map(toml::from_str::<Ledger>)
                    .transpose()
                    .map_err(std::io::Error::other)?
                    .unwrap_or_default();
                bundled.absorb_seeded(current.seeded_source_ids);
                Some(toml::to_string_pretty(&bundled).map_err(std::io::Error::other)?)
            } else {
                original
            };
            snapshot_current_blocking(&config_path)?;
            let mut restore = super::restore::HotRestore::begin(
                &config_path,
                &database_path,
                config_text,
                ledger_text,
            )?;
            restore.install()?;
            Ok(restore)
        })
        .await
        .map_err(|e| ConfigStoreError::io(&e, "config.restore_config_pair join error"))?
        .map_err(|e| ConfigStoreError::io(&e, "config.restore_config_pair config restore"))
    }

    /// Once the blocking stage writer starts, cancellation of its awaiting
    /// task must not let another writer enter before it has finished rollback
    /// or publication. The worker owns this guard through its actual return.
    async fn run_restore_stage(
        &self,
        stage: impl FnOnce() -> std::io::Result<()> + Send + 'static,
    ) -> Result<(), ConfigStoreError> {
        let guard = self.save_lock.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            stage()
        })
        .await
        .map_err(|e| ConfigStoreError::io(&e, "config.run_restore_stage join error"))?
        .map_err(|e| ConfigStoreError::io(&e, "config.run_restore_stage restore staging"))
    }

    /// Take the config read-modify-write guard. Hold the returned guard
    /// across the ENTIRE load-verify-mutate-validate-save sequence so
    /// concurrent config writers queue instead of clobbering each other's
    /// loads. save()/save_raw_toml()/rollback() may be called while
    /// holding it (they use the separate save_lock internally).
    pub async fn begin_write(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().await
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns true when the configured path does not yet exist. Lets
    /// main.rs decide whether to boot into wizard mode vs. load.
    pub fn is_initialized(&self) -> bool {
        self.path.exists()
    }

    /// Synchronous best-effort read of the current on-disk config.
    /// Used by SSR components that need to consult the live config
    /// inside a Leptos `view!` (which can't await). Returns `None` on
    /// any error (missing file, parse failure, env-var expansion etc.)
    /// so the caller can fall back gracefully.
    pub fn load_blocking(&self) -> Option<crate::config::Config> {
        loader::load_from_path(&self.path).ok()
    }

    fn snapshots_dir(path: &Path) -> PathBuf {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join("snapshots")
    }

    /// Write raw TOML text (already validated by the caller) with the
    /// same snapshot + durability guarantees as save(). Used by the
    /// PUT /api/config/raw editor so direct edits also get rollback
    /// points and fsynced writes.
    pub async fn save_raw_toml(&self, body: String) -> Result<(), ConfigStoreError> {
        let _guard = self.save_lock.lock().await;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            snapshot_current_blocking(&path)?;
            write_atomic_durable(&path, body.as_bytes())
        })
        .await
        .map_err(|e| ConfigStoreError::io(&e, "config.save_raw_toml join error"))?
        .map_err(|e| ConfigStoreError::io(&e, "config.save_raw_toml write"))
    }
}

fn restore_sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{suffix}"));
    PathBuf::from(name)
}

struct PreparedRestoreFiles(Vec<PathBuf>);
impl Drop for PreparedRestoreFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(restore_sibling(path, "tmp"));
        }
    }
}

/// `<file>.restore`: where a restore waits for the next boot.
pub fn staged_path(live: &Path) -> PathBuf {
    let name = live
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("localsky.toml");
    live.with_file_name(format!("{name}.restore"))
}

/// Atomic + durable file replace: tmp write, fsync, rename, dir fsync.
pub(crate) fn write_atomic_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp_path = {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        path.with_file_name(format!("{name}.tmp"))
    };
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        // Flush file content to stable storage BEFORE the rename makes
        // it visible; otherwise a crash can leave a zero-length file
        // under the final name.
        f.sync_all()?;
    }
    // POSIX rename is atomic; this is the commit point.
    std::fs::rename(&tmp_path, path)?;
    // fsync the directory so the rename itself is durable. Best-effort:
    // opening a directory for fsync is fine on Linux but not portable
    // everywhere (e.g. Windows), and the content fsync above already
    // covers the worst case (stale-but-valid old file).
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Copy the current on-disk config (if any) into snapshots/<ts>.toml
/// and prune to the newest SNAPSHOT_KEEP. Returns the snapshot ts.
fn snapshot_current_blocking(config_path: &Path) -> std::io::Result<Option<u64>> {
    let bytes = match std::fs::read(config_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let dir = FileConfigStore::snapshots_dir(config_path);
    std::fs::create_dir_all(&dir)?;
    let mut ts = Utc::now().timestamp().max(0) as u64;
    // Bump on collision so rapid saves within one second keep distinct
    // snapshots instead of overwriting each other.
    while dir.join(format!("{ts}.toml")).exists() {
        ts += 1;
    }
    write_atomic_durable(&dir.join(format!("{ts}.toml")), &bytes)?;
    prune_snapshots_blocking(&dir)?;
    Ok(Some(ts))
}

fn snapshot_timestamps(dir: &Path) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<u64> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            name.strip_suffix(".toml")?.parse::<u64>().ok()
        })
        .collect();
    // Newest first.
    out.sort_unstable_by(|a, b| b.cmp(a));
    out
}

fn prune_snapshots_blocking(dir: &Path) -> std::io::Result<()> {
    for ts in snapshot_timestamps(dir).iter().skip(SNAPSHOT_KEEP) {
        let _ = std::fs::remove_file(dir.join(format!("{ts}.toml")));
    }
    Ok(())
}

fn map_load_err(e: LoadError) -> ConfigStoreError {
    match e {
        LoadError::NotFound(_) => ConfigStoreError::NotFound,
        LoadError::Io(_, ioe) => ConfigStoreError::io(&ioe, "config read file"),
        LoadError::Parse(e) => ConfigStoreError::Parse(Box::new(crate::diagnostics::from_error(
            &e,
            "config parse TOML",
        ))),
        LoadError::UnsetEnvVar(v) => {
            ConfigStoreError::Validation(format!("env var ${{{v}}} unset"))
        }
        LoadError::Validation(m) => ConfigStoreError::Validation(m),
        LoadError::SchemaTooNew { found, known } => ConfigStoreError::Migration(format!(
            "config schema_version {found} is newer than binary supports ({known})"
        )),
    }
}

#[async_trait]
impl ConfigStore for FileConfigStore {
    async fn load(&self) -> Result<Config, ConfigStoreError> {
        // Loads may migrate both files. Serialize with saves and ledger
        // updates so a read cannot overwrite a concurrently saved config.
        let _guard = self.save_lock.lock().await;
        let path = self.path.clone();
        let ledger_path = self.ledger_path.clone();
        tokio::task::spawn_blocking(move || {
            Self::migrate_on_disk(&path, &ledger_path)?;
            loader::load_from_path(&path)
        })
        .await
        .map_err(|e| ConfigStoreError::io(&e, "config.load join error"))?
        .map_err(map_load_err)
    }

    async fn save(&self, cfg: &Config) -> Result<ConfigVersion, ConfigStoreError> {
        // Validate before touching disk so a bad PUT can't corrupt the
        // on-disk file with un-loadable garbage.
        loader::validate(cfg).map_err(map_load_err)?;

        let toml_str = toml::to_string_pretty(cfg)
            .map_err(|e| ConfigStoreError::io(&e, "config.save toml serialize"))?;

        // Queue behind any concurrent writer (see save_lock).
        let _guard = self.save_lock.lock().await;
        let path = self.path.clone();

        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            // Snapshot the previous file first so rollback has a target.
            snapshot_current_blocking(&path)?;
            write_atomic_durable(&path, toml_str.as_bytes())
        })
        .await
        .map_err(|e| ConfigStoreError::io(&e, "config.save join error"))?
        .map_err(|e| ConfigStoreError::io(&e, "config.save write"))?;

        Ok(ConfigVersion {
            version: 0,
            applied_at_epoch: Utc::now().timestamp(),
            schema_version: cfg.schema_version,
            note: None,
        })
    }

    async fn list_snapshots(&self) -> Result<Vec<ConfigVersion>, ConfigStoreError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let dir = FileConfigStore::snapshots_dir(&path);
            snapshot_timestamps(&dir)
                .into_iter()
                .map(|ts| {
                    // schema_version is informational here; a snapshot
                    // that fails to parse still lists (rollback will
                    // reject it with a real error).
                    let schema_version = std::fs::read_to_string(dir.join(format!("{ts}.toml")))
                        .ok()
                        .and_then(|s| toml::from_str::<SchemaVersionOnly>(&s).ok())
                        .map(|v| v.schema_version)
                        .unwrap_or(0);
                    ConfigVersion {
                        version: ts as u32,
                        applied_at_epoch: ts as i64,
                        schema_version,
                        note: None,
                    }
                })
                .collect()
        })
        .await
        .map_err(|e| ConfigStoreError::io(&e, "config.list_snapshots join error"))
    }

    async fn rollback(&self, version: u32) -> Result<Config, ConfigStoreError> {
        let _guard = self.save_lock.lock().await;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> Result<Config, ConfigStoreError> {
            let snap_path = FileConfigStore::snapshots_dir(&path).join(format!("{version}.toml"));
            if !snap_path.exists() {
                return Err(ConfigStoreError::RollbackTargetMissing(version));
            }
            // Validate the snapshot fully parses BEFORE swapping; a
            // corrupt snapshot must never replace a working config.
            let cfg = loader::load_from_path(&snap_path).map_err(map_load_err)?;
            let bytes = std::fs::read(&snap_path)
                .map_err(|e| ConfigStoreError::io(&e, "config.rollback read snapshot"))?;
            // Snapshot the current config first so a rollback is itself
            // rollback-able.
            snapshot_current_blocking(&path)
                .map_err(|e| ConfigStoreError::io(&e, "config.rollback snapshot current"))?;
            write_atomic_durable(&path, &bytes)
                .map_err(|e| ConfigStoreError::io(&e, "config.rollback write"))?;
            Ok(cfg)
        })
        .await
        .map_err(|e| ConfigStoreError::io(&e, "config.rollback join error"))?
    }
}

#[derive(serde::Deserialize)]
struct SchemaVersionOnly {
    #[serde(default)]
    schema_version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_toml_exposes_offset_without_configuration_secrets() {
        let error = toml::from_str::<toml::Value>("token = [private-secret").unwrap_err();
        let mapped = map_load_err(LoadError::Parse(error));
        assert!(matches!(&mapped, ConfigStoreError::Parse(_)));
        let failure = mapped.diagnostic();
        assert_eq!(failure.code, crate::failure::FailureCode::TomlParse);
        assert!(failure.byte_offset.is_some());
        assert!(!mapped.to_string().contains("private-secret"));
    }

    #[test]
    fn read_permission_failure_preserves_os_evidence() {
        let mapped = map_load_err(LoadError::Io(
            "private-config-path".into(),
            std::io::Error::from_raw_os_error(13),
        ));
        let failure = mapped.diagnostic();
        assert_eq!(failure.os_code, Some(13));
        assert_eq!(failure.operation, "config read file");
        assert!(!mapped.to_string().contains("private-config-path"));
    }

    #[tokio::test]
    async fn future_schema_load_refuses_without_rewriting_config_or_ledger() {
        let dir = tempfile_dir("future-schema");
        let path = dir.join("localsky.toml");
        let store = FileConfigStore::new(&path);
        let original = format!(
            "schema_version = {}\nfuture_setting = 'preserve me'\n",
            crate::config::schema::CURRENT_SCHEMA_VERSION + 1
        );
        std::fs::write(&path, &original).unwrap();
        let ledger = "seeded_source_ids = ['owner-choice']\n";
        std::fs::write(&store.ledger_path, ledger).unwrap();
        assert!(matches!(
            store.load().await,
            Err(ConfigStoreError::Migration(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&store.ledger_path).unwrap(), ledger);
        assert!(store.list_snapshots().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn migration_load_waits_for_the_current_writer() {
        let dir = tempfile_dir("migration-serialization");
        let path = dir.join("localsky.toml");
        let store = FileConfigStore::new(&path);
        std::fs::write(&path, "schema_version = 1\n").unwrap();
        let guard = store.save_lock.lock().await;
        let load = store.load();
        tokio::pin!(load);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut load)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "schema_version = 1\n"
        );
        drop(guard);
        assert_eq!(
            load.await.unwrap().schema_version,
            crate::config::schema::CURRENT_SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn cancelled_restore_waiter_keeps_file_writers_queued_until_worker_finishes() {
        let dir = tempfile_dir("restore-cancel-writer");
        let path = dir.join("localsky.toml");
        let store = Arc::new(FileConfigStore::new(&path));
        let worker_store = store.clone();
        let stage_path = staged_path(&path);
        let worker_stage = stage_path.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(async move {
            worker_store
                .run_restore_stage(move || {
                    let _ = started_tx.send(());
                    // A real blocking stage operation is in progress when its
                    // awaiting task is cancelled. The worker cannot be aborted.
                    release_rx.recv().unwrap();
                    std::fs::write(worker_stage, b"finished restore stage")
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .expect("stage worker starts")
            .unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());

        let cfg = Config::default();
        let next_save = store.save(&cfg);
        tokio::pin!(next_save);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut next_save)
                .await
                .is_err(),
            "cancelling the waiter must not release the worker's save lock"
        );
        assert!(!path.exists());
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut next_save)
            .await
            .expect("next save resumes after stage completion")
            .unwrap();
        assert_eq!(
            std::fs::read(stage_path).unwrap(),
            b"finished restore stage"
        );
        assert!(path.exists());
    }

    #[tokio::test]
    async fn save_then_load_roundtrip() {
        let dir = tempfile_dir("roundtrip");
        let path = dir.join("localsky.toml");
        let store = FileConfigStore::new(&path);

        let mut cfg = Config::default();
        cfg.deployment.location.lat = 30.07;
        cfg.deployment.location.lon = -81.47;
        cfg.deployment.display_name = "Test".into();

        let v = store.save(&cfg).await.unwrap();
        assert_eq!(v.schema_version, cfg.schema_version);

        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.deployment.location.lat, 30.07);
        assert_eq!(loaded.deployment.display_name, "Test");
    }

    #[tokio::test]
    async fn save_snapshots_previous_config() {
        let dir = tempfile_dir("snap-on-save");
        let path = dir.join("localsky.toml");
        let store = FileConfigStore::new(&path);

        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.deployment.display_name = "v1".into();
        store.save(&cfg).await.unwrap();
        // First save: nothing to snapshot (no previous file).
        assert!(store.list_snapshots().await.unwrap().is_empty());

        cfg.deployment.display_name = "v2".into();
        store.save(&cfg).await.unwrap();
        let snaps = store.list_snapshots().await.unwrap();
        assert_eq!(snaps.len(), 1, "second save snapshots the first");
        assert_eq!(snaps[0].schema_version, cfg.schema_version);
    }

    #[tokio::test]
    async fn snapshots_prune_to_twenty() {
        let dir = tempfile_dir("snap-prune");
        let path = dir.join("localsky.toml");
        let store = FileConfigStore::new(&path);

        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        for i in 0..25 {
            cfg.deployment.display_name = format!("v{i}");
            store.save(&cfg).await.unwrap();
        }
        let snaps = store.list_snapshots().await.unwrap();
        assert_eq!(snaps.len(), SNAPSHOT_KEEP);
        // Newest first.
        assert!(snaps[0].version >= snaps[SNAPSHOT_KEEP - 1].version);
    }

    #[tokio::test]
    async fn rollback_roundtrip_restores_previous_config() {
        let dir = tempfile_dir("rollback");
        let path = dir.join("localsky.toml");
        let store = FileConfigStore::new(&path);

        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.deployment.display_name = "old".into();
        store.save(&cfg).await.unwrap();

        cfg.deployment.display_name = "new".into();
        store.save(&cfg).await.unwrap();

        let snaps = store.list_snapshots().await.unwrap();
        assert_eq!(snaps.len(), 1);
        let ts = snaps[0].version;

        let restored = store.rollback(ts).await.unwrap();
        assert_eq!(restored.deployment.display_name, "old");
        let on_disk = store.load().await.unwrap();
        assert_eq!(on_disk.deployment.display_name, "old");
        // The pre-rollback config got snapshotted too.
        let snaps = store.list_snapshots().await.unwrap();
        assert_eq!(snaps.len(), 2);
    }

    #[tokio::test]
    async fn rollback_missing_target_errors() {
        let dir = tempfile_dir("rollback-missing");
        let store = FileConfigStore::new(dir.join("localsky.toml"));
        let err = store.rollback(12345).await.unwrap_err();
        assert!(matches!(err, ConfigStoreError::RollbackTargetMissing(_)));
    }

    /// begin_write serializes whole read-modify-write sequences: while
    /// one holder has the guard, a second writer queues (try_lock fails),
    /// and save() still works under the guard (separate save_lock, no
    /// deadlock).
    #[tokio::test]
    async fn begin_write_serializes_read_modify_write() {
        let dir = tempfile_dir("write-guard");
        let store = FileConfigStore::new(dir.join("localsky.toml"));
        let guard = store.begin_write().await;
        assert!(
            store.write_lock.try_lock().is_err(),
            "a concurrent writer must queue while the guard is held"
        );
        // save() under the guard must not deadlock.
        let cfg = Config::default();
        store.save(&cfg).await.unwrap();
        drop(guard);
        assert!(store.write_lock.try_lock().is_ok());
    }

    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("localsky-config-test-{}-{tag}", std::process::id()));
        // Fresh dir per test so snapshot counts are deterministic.
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::ports::config_store::ConfigStore;

    fn dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("localsky-store-mig-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const V1: &str = r#"
schema_version = 1
seeded_source_ids = ["nws"]

[deployment.location]
lat = 29.65
lon = -82.32

[[sources]]
id = "open_meteo"
priority = 40
enabled = true
kind = "open_meteo"
[sources.config]
past_days = 1

[[ha_adoption]]
entity = "input_boolean.irrigation_pause"
outcome = "adopted"
target = "irrigation_control.is_paused"
epoch = 5
"#;

    /// The plan's criterion: a v1 file migrates to v2 on the first load
    /// and re-saves byte-identically on the second; the records are in the
    /// ledger and out of the document, where no config write can drop them.
    #[tokio::test]
    async fn a_v1_file_migrates_to_v2_and_resaves_byte_identically() {
        let d = dir("v1");
        let path = d.join("localsky.toml");
        std::fs::write(&path, V1).unwrap();
        let store = FileConfigStore::new(&path);

        let cfg = store.load().await.unwrap();
        assert_eq!(cfg.schema_version, 2);
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("schema_version = 2"), "{first}");
        for gone in ["seeded_source_ids", "ha_adoption"] {
            assert!(
                !first.contains(gone),
                "{gone} still in the document:\n{first}"
            );
        }
        assert!(first.contains("past_days = 3"), "{first}");
        let ledger = store.ledger();
        assert_eq!(ledger.seeded_source_ids, vec!["nws"]);
        assert_eq!(ledger.ha_adoption.len(), 1);
        assert_eq!(ledger.migrations.len(), 2);

        // Second load: nothing to migrate, nothing rewritten.
        let again = store.load().await.unwrap();
        assert_eq!(again.schema_version, 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        assert_eq!(store.ledger(), ledger);

        // A whole-config save of the loaded config cannot touch the ledger:
        // the records are not in the document it writes.
        store.save(&again).await.unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("ha_adoption"));
        assert_eq!(store.ledger(), ledger);
        // And a save is a fixed point: saving what was just saved changes
        // no byte.
        let reloaded = store.load().await.unwrap();
        store.save(&reloaded).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A staged restore of both files swaps both in, before anything reads
    /// them, and keeps the previous document as a snapshot.
    #[tokio::test]
    async fn a_staged_restore_swaps_config_and_ledger_together() {
        let d = dir("stage");
        let path = d.join("localsky.toml");
        let store = FileConfigStore::new(&path);
        std::fs::write(&path, "schema_version = 2\n").unwrap();
        store
            .stage_restore(
                "schema_version = 2\n[deployment]\ndisplay_name = \"Restored\"\n",
                Some("seeded_source_ids = [\"met_no\"]\n"),
            )
            .await
            .unwrap();
        assert!(staged_path(&path).exists());
        let swapped = store.apply_staged_restore().unwrap();
        assert_eq!(swapped.len(), 2);
        assert!(!staged_path(&path).exists());
        assert!(std::fs::read_to_string(&path).unwrap().contains("Restored"));
        assert_eq!(store.ledger().seeded_source_ids, vec!["met_no"]);
        assert!(store.apply_staged_restore().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
