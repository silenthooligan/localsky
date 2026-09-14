// Cloud-session plumbing every token-authenticated adapter needs and
// used to copy: a cached token that a 401 invalidates, and one retry
// after re-authenticating. Netatmo, Tuya, YoLink, B-hyve, Hydrawise and
// Rachio each had their own `Mutex<Option<String>>` and their own
// "on 401, log in again and try once more" loop.

use std::future::Future;

use tokio::sync::Mutex;

/// A token that is fetched on first use, reused until something
/// invalidates it, and refreshed on demand.
#[derive(Debug, Default)]
pub struct TokenCache {
    token: Mutex<Option<String>>,
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached token, or the one `fetch` produces (which is then cached).
    pub async fn get_or_fetch<E, F, Fut>(&self, fetch: F) -> Result<String, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String, E>>,
    {
        if let Some(t) = self.token.lock().await.clone() {
            return Ok(t);
        }
        let t = fetch().await?;
        *self.token.lock().await = Some(t.clone());
        Ok(t)
    }

    /// Forget the token so the next `get_or_fetch` re-authenticates.
    pub async fn invalidate(&self) {
        *self.token.lock().await = None;
    }

    pub async fn set(&self, token: String) {
        *self.token.lock().await = Some(token);
    }

    pub async fn peek(&self) -> Option<String> {
        self.token.lock().await.clone()
    }
}

/// Run `call` with the cached token; when it reports the token was
/// rejected, invalidate, re-authenticate and run it once more. `rejected`
/// decides which errors mean "the token is bad" (a 401, usually) rather
/// than "the upstream is down".
pub async fn with_reauth<T, E, Auth, AuthFut, Call, CallFut>(
    cache: &TokenCache,
    auth: Auth,
    rejected: impl Fn(&E) -> bool,
    mut call: Call,
) -> Result<T, E>
where
    Auth: Fn() -> AuthFut,
    AuthFut: Future<Output = Result<String, E>>,
    Call: FnMut(String) -> CallFut,
    CallFut: Future<Output = Result<T, E>>,
{
    let token = cache.get_or_fetch(&auth).await?;
    match call(token).await {
        Err(e) if rejected(&e) => {
            cache.invalidate().await;
            let token = cache.get_or_fetch(&auth).await?;
            call(token).await
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn the_cache_fetches_once_until_invalidated() {
        let cache = TokenCache::new();
        let logins = AtomicUsize::new(0);
        let login = || async {
            let n = logins.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(format!("t{n}"))
        };
        assert_eq!(cache.get_or_fetch(login).await.unwrap(), "t0");
        assert_eq!(cache.get_or_fetch(login).await.unwrap(), "t0");
        cache.invalidate().await;
        assert_eq!(cache.get_or_fetch(login).await.unwrap(), "t1");
        assert_eq!(logins.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_rejected_token_is_replaced_and_the_call_retried_once() {
        let cache = TokenCache::new();
        cache.set("stale".into()).await;
        let logins = AtomicUsize::new(0);
        let calls = AtomicUsize::new(0);
        let calls_ref = &calls;
        let out: Result<&str, &str> = with_reauth(
            &cache,
            || async {
                logins.fetch_add(1, Ordering::SeqCst);
                Ok("fresh".to_string())
            },
            |e| *e == "401",
            |token| async move {
                calls_ref.fetch_add(1, Ordering::SeqCst);
                if token == "stale" {
                    Err("401")
                } else {
                    Ok("data")
                }
            },
        )
        .await;
        assert_eq!(out, Ok("data"));
        assert_eq!(logins.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.peek().await.as_deref(), Some("fresh"));
    }

    #[tokio::test]
    async fn an_outage_is_not_a_bad_token() {
        let cache = TokenCache::new();
        cache.set("good".into()).await;
        let logins = AtomicUsize::new(0);
        let out: Result<(), &str> = with_reauth(
            &cache,
            || async {
                logins.fetch_add(1, Ordering::SeqCst);
                Ok("x".to_string())
            },
            |e| *e == "401",
            |_| async { Err("timeout") },
        )
        .await;
        assert_eq!(out, Err("timeout"));
        assert_eq!(logins.load(Ordering::SeqCst), 0, "no re-login on an outage");
        assert_eq!(cache.peek().await.as_deref(), Some("good"));
    }
}
