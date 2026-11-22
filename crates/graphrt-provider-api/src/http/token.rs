//! Credentials seen by the shared HTTP layer: a [`TokenSource`] gives a bearer
//! token (or signs a request in the `auth-*` crates), and [`TokenCache`] keeps
//! it until `exp - 5 min` with one forced refresh on 401 (plan §6, C8).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::error::ProviderError;
use crate::provider::BoxFuture;

/// Refresh margin before expiry.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// A secret string whose `Debug` / `Display` never print the value.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(Arc<str>);

impl SecretString {
    /// Wraps a secret.
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }
    /// The secret value, only for building a header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString(***)")
    }
}

/// A token and how long it stays valid.
#[derive(Debug, Clone)]
pub struct AccessToken {
    /// The bearer value.
    pub value: SecretString,
    /// Lifetime from the moment it was fetched.
    pub expires_in: Duration,
}

/// Fetches fresh tokens (metadata server, STS, Entra, a static deploy token).
pub trait TokenFetcher: Send + Sync {
    /// Fetches a new token.
    fn fetch(&self) -> BoxFuture<'_, Result<AccessToken, ProviderError>>;
}

/// What the HTTP client asks for: a current token, or a forced refresh after a 401.
pub trait TokenSource: Send + Sync {
    /// Returns a valid token; `force_refresh` after the server rejected the cached one.
    fn token(&self, force_refresh: bool) -> BoxFuture<'_, Result<SecretString, ProviderError>>;
}

#[derive(Debug)]
struct CachedToken {
    value: SecretString,
    refresh_at: Instant,
}

/// Caches a [`TokenFetcher`]'s token until `exp - REFRESH_MARGIN`. Concurrent
/// callers share one fetch (the lock is held while fetching).
pub struct TokenCache<F> {
    fetcher: F,
    cached: Mutex<Option<CachedToken>>,
}

impl<F> fmt::Debug for TokenCache<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("TokenCache").finish_non_exhaustive()
    }
}

impl<F: TokenFetcher> TokenCache<F> {
    /// An empty cache over `fetcher`.
    pub fn new(fetcher: F) -> Self {
        Self { fetcher, cached: Mutex::new(None) }
    }
}

impl<F: TokenFetcher> TokenSource for TokenCache<F> {
    fn token(&self, force_refresh: bool) -> BoxFuture<'_, Result<SecretString, ProviderError>> {
        Box::pin(async move {
            let mut cached = self.cached.lock().await;
            if let Some(token) = cached.as_ref().filter(|token| !force_refresh && Instant::now() < token.refresh_at) {
                return Ok(token.value.clone());
            }
            let fresh = self.fetcher.fetch().await?;
            let refresh_at = Instant::now() + fresh.expires_in.saturating_sub(REFRESH_MARGIN);
            *cached = Some(CachedToken { value: fresh.value.clone(), refresh_at });
            Ok(fresh.value)
        })
    }
}

/// A token that never changes (Fly deploy token, tests).
#[derive(Debug, Clone)]
pub struct StaticToken(pub SecretString);

impl TokenSource for StaticToken {
    fn token(&self, _force_refresh: bool) -> BoxFuture<'_, Result<SecretString, ProviderError>> {
        let value = self.0.clone();
        Box::pin(async move { Ok(value) })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct CountingFetcher(AtomicU32);

    impl TokenFetcher for CountingFetcher {
        fn fetch(&self) -> BoxFuture<'_, Result<AccessToken, ProviderError>> {
            Box::pin(async move {
                let number = self.0.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(AccessToken {
                    value: SecretString::new(format!("token-{number}")),
                    expires_in: Duration::from_secs(3600),
                })
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn caches_until_margin_then_refreshes() {
        let cache = TokenCache::new(CountingFetcher(AtomicU32::new(0)));
        assert_eq!(cache.token(false).await.unwrap().expose(), "token-1");
        tokio::time::advance(Duration::from_secs(3600 - 301)).await;
        assert_eq!(cache.token(false).await.unwrap().expose(), "token-1");
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(cache.token(false).await.unwrap().expose(), "token-2");
        assert_eq!(cache.token(true).await.unwrap().expose(), "token-3");
    }

    #[test]
    fn secret_debug_is_redacted() {
        assert_eq!(format!("{:?}", SecretString::new("hunter2")), "SecretString(***)");
    }
}
