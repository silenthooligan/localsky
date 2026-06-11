// Tiny TTL cache for advisor responses. Keys are short strings
// derived from the snapshot inputs that would change the answer;
// values are owned T's. Single Mutex around a HashMap is plenty
// reads are infrequent (at most a few per minute under any realistic
// dashboard load), so contention is irrelevant.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Cached<T> {
    pub value: T,
    pub expires_at_epoch: i64,
}

pub struct TtlCache<T> {
    inner: Mutex<HashMap<String, Cached<T>>>,
}

impl<T: Clone> Default for TtlCache<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl<T: Clone> TtlCache<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a cached value if it's still fresh. None otherwise.
    /// Doesn't evict on miss, eviction happens on the next put().
    pub fn get(&self, key: &str) -> Option<T> {
        let now = chrono::Utc::now().timestamp();
        let guard = self.inner.lock().ok()?;
        let entry = guard.get(key)?;
        if entry.expires_at_epoch > now {
            Some(entry.value.clone())
        } else {
            None
        }
    }

    /// Store a value with the given TTL in seconds.
    pub fn put(&self, key: String, value: T, ttl_seconds: i64) {
        let now = chrono::Utc::now().timestamp();
        if let Ok(mut guard) = self.inner.lock() {
            // Opportunistic eviction: drop expired entries on every
            // write so the map doesn't grow unbounded.
            guard.retain(|_, v| v.expires_at_epoch > now);
            guard.insert(
                key,
                Cached {
                    value,
                    expires_at_epoch: now + ttl_seconds,
                },
            );
        }
    }
}
