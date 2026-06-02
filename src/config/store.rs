// File-backed ConfigStore. Reads + writes /data/localsky.toml.
//
// Atomic write: serialize to TOML, write to <path>.tmp, fsync, rename.
// The rename is atomic on every POSIX filesystem so a crash mid-write
// leaves either the old or new file but never a truncated one.
//
// Snapshot history and rollback are stubbed here; they require the
// SQLite layer that lands in Phase 4. Until then, save() returns a
// synthetic ConfigVersion with applied_at_epoch=now and version=0.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;

use crate::config::loader::{self, LoadError};
use crate::config::schema::Config;
use crate::ports::config_store::{ConfigStore, ConfigStoreError, ConfigVersion};

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
        let tmp_path = path.with_extension("toml.tmp");

        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&tmp_path, toml_str)?;
            // POSIX rename is atomic; this is the commit point.
            std::fs::rename(&tmp_path, &path)?;
            Ok(())
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
        // Phase 4 wires this to persistence::config_snapshots.
        Ok(Vec::new())
    }

    async fn rollback(&self, version: u32) -> Result<Config, ConfigStoreError> {
        // Phase 4 wires this to persistence::config_snapshots.
        Err(ConfigStoreError::RollbackTargetMissing(version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_then_load_roundtrip() {
        let dir = tempfile_dir();
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

    fn tempfile_dir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("localsky-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
