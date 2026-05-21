// Persistence layer for push_subscriptions. Same Arc<Mutex<Connection>>
// pattern as history::db so the SSR handlers can share one SQLite file.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

pub async fn upsert(conn: Arc<Mutex<Connection>>, sub: StoredSubscription) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = conn.blocking_lock();
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO push_subscriptions (endpoint, p256dh, auth, created_at, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?4) \
             ON CONFLICT(endpoint) DO UPDATE SET \
                p256dh = excluded.p256dh, \
                auth = excluded.auth, \
                last_seen = excluded.last_seen",
            params![sub.endpoint, sub.p256dh, sub.auth, now],
        )?;
        Ok(())
    })
    .await
    .context("spawn_blocking join failed")?
}

pub async fn delete_endpoint(conn: Arc<Mutex<Connection>>, endpoint: String) -> Result<usize> {
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let conn = conn.blocking_lock();
        let n = conn.execute(
            "DELETE FROM push_subscriptions WHERE endpoint = ?1",
            params![endpoint],
        )?;
        Ok(n)
    })
    .await
    .context("spawn_blocking join failed")?
}

pub async fn list_all(conn: Arc<Mutex<Connection>>) -> Result<Vec<StoredSubscription>> {
    tokio::task::spawn_blocking(move || -> Result<Vec<StoredSubscription>> {
        let conn = conn.blocking_lock();
        let mut stmt =
            conn.prepare("SELECT endpoint, p256dh, auth FROM push_subscriptions")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredSubscription {
                    endpoint: row.get(0)?,
                    p256dh: row.get(1)?,
                    auth: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
    .context("spawn_blocking join failed")?
}
