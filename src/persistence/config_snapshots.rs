// Persisted config snapshot store. Saves Config blobs into the
// config_snapshots table on every write so /api/config/rollback can
// restore an older version. Trigger M0002 caps retention at 20.

use std::sync::Arc;

use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::config::schema::Config;
use crate::ports::config_store::{ConfigStoreError, ConfigVersion};

#[derive(Clone)]
pub struct ConfigSnapshotStore {
    conn: Arc<Mutex<Connection>>,
}

impl ConfigSnapshotStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Insert a new snapshot. Returns the assigned version number.
    pub async fn save(
        &self,
        cfg: &Config,
        note: Option<&str>,
    ) -> Result<ConfigVersion, ConfigStoreError> {
        let blob = serde_json::to_string(cfg)
            .map_err(|e| ConfigStoreError::Io(format!("json serialize: {e}")))?;
        let note_s = note.map(|s| s.to_string());
        let schema_v = cfg.schema_version;

        let c = self.conn.clone();
        let row = tokio::task::spawn_blocking(move || -> rusqlite::Result<ConfigVersion> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO config_snapshots(applied_at, schema_version, note, blob) VALUES (?, ?, ?, ?)",
                params![now_epoch(), schema_v as i64, note_s, blob],
            )?;
            let v: i64 = conn.last_insert_rowid();
            let applied: i64 = conn
                .query_row("SELECT applied_at FROM config_snapshots WHERE version = ?", params![v], |r| r.get(0))?;
            Ok(ConfigVersion {
                version: v as u32,
                applied_at_epoch: applied,
                schema_version: schema_v,
                note: None,
            })
        })
        .await
        .map_err(|e| ConfigStoreError::Io(format!("join: {e}")))?
        .map_err(|e| ConfigStoreError::Io(format!("insert: {e}")))?;
        Ok(row)
    }

    pub async fn list(&self) -> Result<Vec<ConfigVersion>, ConfigStoreError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<ConfigVersion>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT version, applied_at, schema_version, note FROM config_snapshots ORDER BY version DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(ConfigVersion {
                        version: r.get::<_, i64>(0)? as u32,
                        applied_at_epoch: r.get(1)?,
                        schema_version: r.get::<_, i64>(2)? as u32,
                        note: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| ConfigStoreError::Io(format!("join: {e}")))?
        .map_err(|e| ConfigStoreError::Io(format!("query: {e}")))
    }

    pub async fn load(&self, version: u32) -> Result<Config, ConfigStoreError> {
        let c = self.conn.clone();
        let blob = tokio::task::spawn_blocking(move || -> rusqlite::Result<String> {
            let conn = c.blocking_lock();
            conn.query_row(
                "SELECT blob FROM config_snapshots WHERE version = ?",
                params![version as i64],
                |r| r.get(0),
            )
        })
        .await
        .map_err(|e| ConfigStoreError::Io(format!("join: {e}")))?
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                ConfigStoreError::RollbackTargetMissing(version)
            }
            other => ConfigStoreError::Io(format!("query: {other}")),
        })?;
        serde_json::from_str(&blob)
            .map_err(|e| ConfigStoreError::Validation(format!("snapshot blob parse: {e}")))
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;

    async fn fresh_store() -> ConfigSnapshotStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        ConfigSnapshotStore::new(Arc::new(Mutex::new(c)))
    }

    #[tokio::test]
    async fn save_then_list_returns_one_row() {
        let s = fresh_store().await;
        let mut cfg = Config::default();
        cfg.deployment.display_name = "Hello".into();
        let v = s.save(&cfg, Some("first")).await.unwrap();
        assert!(v.version > 0);
        let list = s.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].version, v.version);
    }

    #[tokio::test]
    async fn load_roundtrip() {
        let s = fresh_store().await;
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        let v = s.save(&cfg, None).await.unwrap();
        let loaded = s.load(v.version).await.unwrap();
        assert_eq!(loaded.deployment.location.lat, 28.5);
    }

    #[tokio::test]
    async fn load_missing_returns_rollback_target_missing() {
        let s = fresh_store().await;
        let err = s.load(9999).await.unwrap_err();
        assert!(matches!(err, ConfigStoreError::RollbackTargetMissing(_)));
    }

    #[tokio::test]
    async fn retention_caps_at_20() {
        let s = fresh_store().await;
        for i in 0..25 {
            let mut cfg = Config::default();
            cfg.deployment.display_name = format!("v{i}");
            s.save(&cfg, None).await.unwrap();
        }
        let list = s.list().await.unwrap();
        assert_eq!(list.len(), 20);
    }
}
