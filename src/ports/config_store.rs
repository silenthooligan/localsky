// ConfigStore port. Backed by /data/localsky.toml today; the trait keeps
// the door open for an external store (Vault, etcd, env-only) without
// touching every call site.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Config;

#[derive(Debug, Error)]
pub enum ConfigStoreError {
    #[error("config not found")]
    NotFound,
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("schema migration failed: {0}")]
    Migration(String),
    #[error("rollback target not found: {0}")]
    RollbackTargetMissing(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigVersion {
    pub version: u32,
    pub applied_at_epoch: i64,
    pub schema_version: u32,
    pub note: Option<String>,
}

#[async_trait]
pub trait ConfigStore: Send + Sync {
    async fn load(&self) -> Result<Config, ConfigStoreError>;
    async fn save(&self, cfg: &Config) -> Result<ConfigVersion, ConfigStoreError>;
    async fn list_snapshots(&self) -> Result<Vec<ConfigVersion>, ConfigStoreError>;
    async fn rollback(&self, version: u32) -> Result<Config, ConfigStoreError>;
}
