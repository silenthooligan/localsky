// AuthStore: every SQLite touch for users/sessions/api_tokens, all via
// spawn_blocking like the rest of persistence. Cheap to clone.

use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use tokio::sync::Mutex;

use super::hash;

#[derive(Clone)]
pub struct AuthStore {
    db: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiTokenRow {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl AuthStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let conn = db.blocking_lock();
            f(&conn).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("join: {e}"))?
    }

    /// Number of non-disabled accounts. 0 = setup incomplete.
    pub async fn user_count(&self) -> Result<i64, String> {
        self.with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM users WHERE disabled = 0", [], |r| {
                r.get(0)
            })
        })
        .await
    }

    /// Create the account. Fails on duplicate username.
    pub async fn create_user(&self, username: &str, password: &str) -> Result<i64, String> {
        let username = username.trim().to_lowercase();
        if username.is_empty() {
            return Err("username is required".into());
        }
        if password.len() < 8 {
            return Err("password must be at least 8 characters".into());
        }
        let phc = hash::hash_password(password)?;
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO users (username, password_hash, created_at) VALUES (?1, ?2, ?3)",
                params![username, phc, now()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
        .map_err(|e| {
            if e.contains("UNIQUE") {
                "username already exists".into()
            } else {
                e
            }
        })
    }

    /// Verify credentials. Returns the user id on success. Constant-shape
    /// failure (no user / bad password are indistinguishable to callers).
    pub async fn verify_login(&self, username: &str, password: &str) -> Result<i64, String> {
        let username = username.trim().to_lowercase();
        let row: Option<(i64, String)> = self
            .with_conn(move |c| {
                c.query_row(
                    "SELECT id, password_hash FROM users WHERE username = ?1 AND disabled = 0",
                    params![username],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
            })
            .await?;
        // Verify against a dummy hash when the user is unknown so timing
        // doesn't leak username existence.
        const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        match row {
            Some((id, phc)) if hash::verify_password(password, &phc) => Ok(id),
            Some(_) => Err("invalid credentials".into()),
            None => {
                let _ = hash::verify_password(password, DUMMY);
                Err("invalid credentials".into())
            }
        }
    }

    /// Create a browser session. Returns the plaintext cookie value.
    pub async fn create_session(
        &self,
        user_id: i64,
        ttl_days: u32,
        user_agent: Option<String>,
    ) -> Result<String, String> {
        let (plaintext, token_hash) = hash::generate_token(super::SESSION_PREFIX);
        let expires = now() + i64::from(ttl_days) * 86_400;
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO auth_sessions (token_hash, user_id, created_at, last_seen_at, expires_at, user_agent)
                 VALUES (?1, ?2, ?3, ?3, ?4, ?5)",
                params![token_hash, user_id, now(), expires, user_agent],
            )?;
            // Opportunistic GC of expired sessions.
            c.execute(
                "DELETE FROM auth_sessions WHERE expires_at < ?1",
                params![now()],
            )?;
            Ok(())
        })
        .await?;
        Ok(plaintext)
    }

    /// Validate a session cookie value. Rolling expiry: when the session
    /// is past 1 day old, bump expires_at forward by ttl_days.
    pub async fn validate_session(
        &self,
        token: &str,
        ttl_days: u32,
    ) -> Result<Option<i64>, String> {
        let token_hash = hash::sha256_hex(token);
        self.with_conn(move |c| {
            let row: Option<(i64, i64, i64)> = c
                .query_row(
                    "SELECT id, user_id, last_seen_at FROM auth_sessions
                     WHERE token_hash = ?1 AND expires_at > ?2",
                    params![token_hash, now()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((id, user_id, last_seen)) = row {
                if now() - last_seen > 86_400 {
                    c.execute(
                        "UPDATE auth_sessions SET last_seen_at = ?1, expires_at = ?2 WHERE id = ?3",
                        params![now(), now() + i64::from(ttl_days) * 86_400, id],
                    )?;
                }
                Ok(Some(user_id))
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// Delete the session for a cookie value (logout).
    pub async fn delete_session(&self, token: &str) -> Result<(), String> {
        let token_hash = hash::sha256_hex(token);
        self.with_conn(move |c| {
            c.execute(
                "DELETE FROM auth_sessions WHERE token_hash = ?1",
                params![token_hash],
            )?;
            Ok(())
        })
        .await
    }

    /// Create a named API token for integrations. Returns the plaintext
    /// (shown exactly once).
    pub async fn create_api_token(&self, user_id: i64, name: &str) -> Result<String, String> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("token name is required".into());
        }
        let (plaintext, token_hash) = hash::generate_token(super::API_TOKEN_PREFIX);
        self.with_conn(move |c| {
            c.execute(
                "INSERT INTO api_tokens (token_hash, name, user_id, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![token_hash, name, user_id, now()],
            )?;
            Ok(())
        })
        .await?;
        Ok(plaintext)
    }

    /// Validate a Bearer/query API token. Updates last_used_at (coarse:
    /// at most once a minute to keep writes off the hot path).
    pub async fn validate_api_token(&self, token: &str) -> Result<Option<i64>, String> {
        let token_hash = hash::sha256_hex(token);
        self.with_conn(move |c| {
            let row: Option<(i64, i64, Option<i64>)> = c
                .query_row(
                    "SELECT id, user_id, last_used_at FROM api_tokens
                     WHERE token_hash = ?1 AND revoked = 0",
                    params![token_hash],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((id, user_id, last_used)) = row {
                if last_used.map(|t| now() - t > 60).unwrap_or(true) {
                    c.execute(
                        "UPDATE api_tokens SET last_used_at = ?1 WHERE id = ?2",
                        params![now(), id],
                    )?;
                }
                Ok(Some(user_id))
            } else {
                Ok(None)
            }
        })
        .await
    }

    pub async fn list_api_tokens(&self) -> Result<Vec<ApiTokenRow>, String> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, created_at, last_used_at FROM api_tokens
                 WHERE revoked = 0 ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(ApiTokenRow {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        created_at: r.get(2)?,
                        last_used_at: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn revoke_api_token(&self, id: i64) -> Result<(), String> {
        self.with_conn(move |c| {
            c.execute(
                "UPDATE api_tokens SET revoked = 1 WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_user(&self, id: i64) -> Result<Option<UserRow>, String> {
        self.with_conn(move |c| {
            c.query_row(
                "SELECT id, username, role, created_at FROM users WHERE id = ?1",
                params![id],
                |r| {
                    Ok(UserRow {
                        id: r.get(0)?,
                        username: r.get(1)?,
                        role: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> AuthStore {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
        AuthStore::new(Arc::new(Mutex::new(conn)))
    }

    #[tokio::test]
    async fn user_lifecycle() {
        let s = store();
        assert_eq!(s.user_count().await.unwrap(), 0);
        let uid = s.create_user("Erik", "hunter22valid").await.unwrap();
        assert_eq!(s.user_count().await.unwrap(), 1);
        // Username normalized to lowercase; duplicate rejected.
        assert!(s.create_user("erik", "another-pass").await.is_err());
        // Login works case-insensitively on username, exact on password.
        assert_eq!(s.verify_login("ERIK", "hunter22valid").await.unwrap(), uid);
        assert!(s.verify_login("erik", "wrong").await.is_err());
        assert!(s.verify_login("ghost", "hunter22valid").await.is_err());
    }

    #[tokio::test]
    async fn weak_password_rejected() {
        let s = store();
        assert!(s.create_user("erik", "short").await.is_err());
    }

    #[tokio::test]
    async fn session_lifecycle() {
        let s = store();
        let uid = s.create_user("erik", "hunter22valid").await.unwrap();
        let cookie = s.create_session(uid, 30, None).await.unwrap();
        assert!(cookie.starts_with("lss_"));
        assert_eq!(s.validate_session(&cookie, 30).await.unwrap(), Some(uid));
        s.delete_session(&cookie).await.unwrap();
        assert_eq!(s.validate_session(&cookie, 30).await.unwrap(), None);
    }

    #[tokio::test]
    async fn api_token_lifecycle() {
        let s = store();
        let uid = s.create_user("erik", "hunter22valid").await.unwrap();
        let tok = s.create_api_token(uid, "hacs").await.unwrap();
        assert!(tok.starts_with("lsk_"));
        assert_eq!(s.validate_api_token(&tok).await.unwrap(), Some(uid));
        let listed = s.list_api_tokens().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "hacs");
        s.revoke_api_token(listed[0].id).await.unwrap();
        assert_eq!(s.validate_api_token(&tok).await.unwrap(), None);
    }
}
