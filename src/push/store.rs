// Persistence layer for push_subscriptions. Same Arc<Mutex<Connection>>
// pattern as history::db so the SSR handlers can share one SQLite file.

use crate::notification_preferences::PushPreferences;
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
    #[serde(default)]
    pub preferences: PushPreferences,
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
            conn.prepare("SELECT endpoint, p256dh, auth, preferences FROM push_subscriptions")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredSubscription {
                    endpoint: row.get(0)?,
                    p256dh: row.get(1)?,
                    auth: row.get(2)?,
                    // Corrupt choices fail closed, never fall back to sending more.
                    preferences: serde_json::from_str::<PushPreferences>(&row.get::<_, String>(3)?)
                        .ok()
                        .filter(|p| p.validate().is_ok())
                        .unwrap_or_else(|| PushPreferences {
                            enabled: false,
                            ..Default::default()
                        }),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
    .context("spawn_blocking join failed")?
}

/// Possession of this subscription's endpoint AND encryption keys is required.
/// Reconnecting updates the keys but deliberately leaves choices and daily claims intact.
pub async fn preferences(
    conn: Arc<Mutex<Connection>>,
    endpoint: String,
    p256dh: String,
    auth: String,
    update: Option<PushPreferences>,
) -> Result<Option<PushPreferences>> {
    if let Some(prefs) = &update {
        prefs.validate().map_err(anyhow::Error::msg)?;
    }
    tokio::task::spawn_blocking(move || -> Result<Option<PushPreferences>> {
        use rusqlite::OptionalExtension;
        let conn = conn.blocking_lock();
        if let Some(prefs) = update {
            let changed = conn.execute(
                "UPDATE push_subscriptions SET preferences=?4 WHERE endpoint=?1 AND p256dh=?2 AND auth=?3",
                params![endpoint, p256dh, auth, serde_json::to_string(&prefs)?],
            )?;
            return Ok((changed == 1).then_some(prefs));
        }
        let raw: Option<String> = conn.query_row(
            "SELECT preferences FROM push_subscriptions WHERE endpoint=?1 AND p256dh=?2 AND auth=?3",
            params![endpoint, p256dh, auth], |row| row.get(0),
        ).optional()?;
        raw.map(|json| {
            let prefs: PushPreferences = serde_json::from_str(&json)?;
            prefs.validate().map_err(anyhow::Error::msg)?;
            Ok(prefs)
        }).transpose()
    }).await.context("subscription preferences task failed")?
}

pub async fn claim_outlook(
    conn: Arc<Mutex<Connection>>,
    endpoint: String,
    today: String,
) -> Result<bool> {
    tokio::task::spawn_blocking(move || -> Result<bool> {
        Ok(conn.blocking_lock().execute(
            "UPDATE push_subscriptions SET last_outlook_day=?2 WHERE endpoint=?1
             AND (last_outlook_day IS NULL OR last_outlook_day < ?2)",
            params![endpoint, today],
        )? == 1)
    })
    .await
    .context("outlook claim task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn device_choices_are_isolated_authenticated_and_survive_reconnects() {
        let mut db = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut db).unwrap();
        let db = Arc::new(Mutex::new(db));
        for endpoint in ["phone", "tablet"] {
            upsert(
                db.clone(),
                StoredSubscription {
                    endpoint: endpoint.into(),
                    p256dh: "key".into(),
                    auth: "secret".into(),
                    preferences: Default::default(),
                },
            )
            .await
            .unwrap();
        }
        let prefs = PushPreferences {
            enabled: false,
            ..Default::default()
        };
        assert!(preferences(
            db.clone(),
            "phone".into(),
            "key".into(),
            "wrong".into(),
            Some(prefs.clone())
        )
        .await
        .unwrap()
        .is_none());
        preferences(
            db.clone(),
            "phone".into(),
            "key".into(),
            "secret".into(),
            Some(prefs),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            claim_outlook(db.clone(), "phone".into(), "2026-10-05".into())
                .await
                .unwrap()
        );
        upsert(
            db.clone(),
            StoredSubscription {
                endpoint: "phone".into(),
                p256dh: "key".into(),
                auth: "secret".into(),
                preferences: Default::default(),
            },
        )
        .await
        .unwrap();
        assert!(
            !claim_outlook(db.clone(), "phone".into(), "2026-10-05".into())
                .await
                .unwrap()
        );
        assert!(
            !claim_outlook(db.clone(), "phone".into(), "2026-10-04".into())
                .await
                .unwrap()
        );
        let rows = list_all(db.clone()).await.unwrap();
        assert!(
            !rows
                .iter()
                .find(|s| s.endpoint == "phone")
                .unwrap()
                .preferences
                .enabled
        );
        assert!(
            rows.iter()
                .find(|s| s.endpoint == "tablet")
                .unwrap()
                .preferences
                .enabled
        );
        assert!(claim_outlook(db, "tablet".into(), "2026-10-05".into())
            .await
            .unwrap());
    }
}
