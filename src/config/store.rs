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

use async_trait::async_trait;
use chrono::Utc;

use crate::config::loader::{self, LoadError};
use crate::config::schema::Config;
use crate::ports::config_store::{ConfigStore, ConfigStoreError, ConfigVersion};

/// Snapshot retention: newest N kept, older pruned on each save.
const SNAPSHOT_KEEP: usize = 20;

pub struct FileConfigStore {
    path: PathBuf,
}

impl FileConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
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
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            snapshot_current_blocking(&path)?;
            write_atomic_durable(&path, body.as_bytes())
        })
        .await
        .map_err(|e| ConfigStoreError::Io(format!("join error: {e}")))?
        .map_err(|e| ConfigStoreError::Io(format!("write: {e}")))
    }
}

/// Atomic + durable file replace: tmp write, fsync, rename, dir fsync.
fn write_atomic_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp_path = path.with_extension("toml.tmp");
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
        LoadError::Io(_, ioe) => ConfigStoreError::Io(ioe.to_string()),
        LoadError::Parse(e) => ConfigStoreError::Validation(format!("toml parse: {e}")),
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
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || loader::load_from_path(&path))
            .await
            .map_err(|e| ConfigStoreError::Io(format!("join error: {e}")))?
            .map_err(map_load_err)
    }

    async fn save(&self, cfg: &Config) -> Result<ConfigVersion, ConfigStoreError> {
        // Validate before touching disk so a bad PUT can't corrupt the
        // on-disk file with un-loadable garbage.
        loader::validate(cfg).map_err(map_load_err)?;

        let toml_str = toml::to_string_pretty(cfg)
            .map_err(|e| ConfigStoreError::Io(format!("toml serialize: {e}")))?;

        let path = self.path.clone();

        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            // Snapshot the previous file first so rollback has a target.
            snapshot_current_blocking(&path)?;
            write_atomic_durable(&path, toml_str.as_bytes())
        })
        .await
        .map_err(|e| ConfigStoreError::Io(format!("join error: {e}")))?
        .map_err(|e| ConfigStoreError::Io(format!("write: {e}")))?;

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
        .map_err(|e| ConfigStoreError::Io(format!("join error: {e}")))
    }

    async fn rollback(&self, version: u32) -> Result<Config, ConfigStoreError> {
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
                .map_err(|e| ConfigStoreError::Io(format!("read snapshot: {e}")))?;
            // Snapshot the current config first so a rollback is itself
            // rollback-able.
            snapshot_current_blocking(&path)
                .map_err(|e| ConfigStoreError::Io(format!("snapshot current: {e}")))?;
            write_atomic_durable(&path, &bytes)
                .map_err(|e| ConfigStoreError::Io(format!("write: {e}")))?;
            Ok(cfg)
        })
        .await
        .map_err(|e| ConfigStoreError::Io(format!("join error: {e}")))?
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

    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("localsky-config-test-{}-{tag}", std::process::id()));
        // Fresh dir per test so snapshot counts are deterministic.
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
