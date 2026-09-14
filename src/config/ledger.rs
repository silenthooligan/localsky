// The server-owned record beside localsky.toml.
//
// Some facts about an install are LocalSky's to keep and nobody's to
// edit: which config migrations have run, which forecast authorities the
// boot seeder has already handled (so a source the owner deleted stays
// deleted), which source priorities the one-time repair has evaluated,
// and which Home Assistant helpers the 0.7.22 migration recorded. They
// used to live inside the config document, where six whole-config write
// paths each had to remember to carry them across or silently lose them.
// They live here now, in `localsky.ledger.toml`, and no config write can
// touch them because they are not in the document.

#[cfg(feature = "ssr")]
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::HaAdoptedHelper;

/// One config migration that has run, so it never runs again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedMigration {
    pub id: String,
    pub applied_at_epoch: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub migrations: Vec<AppliedMigration>,
    /// Region forecast authorities the boot seeder has handled, whether it
    /// appended them or found them already present. Never re-seeded.
    #[serde(default)]
    pub seeded_source_ids: Vec<String>,
    /// Sources the one-time flat-default priority repair has evaluated.
    #[serde(default)]
    pub priority_repaired_ids: Vec<String>,
    /// The Home Assistant helpers the 0.7.22 migration recorded, for the
    /// migration notice. Nothing appends to this any more.
    #[serde(default)]
    pub ha_adoption: Vec<HaAdoptedHelper>,
}

impl Ledger {
    pub fn has_migration(&self, id: &str) -> bool {
        self.migrations.iter().any(|m| m.id == id)
    }

    pub fn record_migration(&mut self, id: &str, now_epoch: i64) {
        if !self.has_migration(id) {
            self.migrations.push(AppliedMigration {
                id: id.to_string(),
                applied_at_epoch: now_epoch,
            });
        }
    }

    /// Union in ids from another record (a bundle's ledger on restore).
    pub fn absorb_seeded(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            if !self.seeded_source_ids.contains(&id) {
                self.seeded_source_ids.push(id);
            }
        }
    }
}

/// The file beside the config. Server only: the browser never reads it.
#[cfg(feature = "ssr")]
impl Ledger {
    /// `localsky.ledger.toml` beside the config file.
    pub fn path_for(config_path: &Path) -> PathBuf {
        let stem = config_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("localsky");
        config_path.with_file_name(format!("{stem}.ledger.toml"))
    }

    /// The ledger on disk, or an empty one when there is none. A file that
    /// exists but does not parse is reported and treated as empty rather
    /// than blocking the boot: every record here is a "do not repeat"
    /// marker, and repeating a seed is recoverable where refusing to
    /// start is not.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Ledger>(&text) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "ledger does not parse; treating it as empty");
                    Ledger::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "ledger unreadable; treating it as empty");
                Ledger::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::config::store::write_atomic_durable(path, text.as_bytes())
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;

    #[test]
    fn the_ledger_sits_beside_the_config() {
        assert_eq!(
            Ledger::path_for(Path::new("/data/localsky.toml")),
            PathBuf::from("/data/localsky.ledger.toml")
        );
    }

    #[test]
    fn a_missing_or_broken_ledger_reads_as_empty() {
        let dir = std::env::temp_dir().join(format!("localsky-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("localsky.ledger.toml");
        assert_eq!(Ledger::load(&p), Ledger::default());
        std::fs::write(&p, "not = [toml").unwrap();
        assert_eq!(Ledger::load(&p), Ledger::default());
        let mut l = Ledger::default();
        l.record_migration("x", 5);
        l.record_migration("x", 6);
        l.absorb_seeded(["nws".to_string(), "nws".to_string()]);
        l.save(&p).unwrap();
        assert_eq!(Ledger::load(&p), l);
        assert_eq!(l.migrations.len(), 1);
        assert_eq!(l.seeded_source_ids, vec!["nws"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
