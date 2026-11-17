//! Caching wrapper with single-flight refresh.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use futures::lock::Mutex as AsyncMutex;

use super::{Credential, CredentialSource, unix_now_ms};
use crate::error::LlmError;

/// Credentials are refreshed this long before they expire (60 s).
pub const DEFAULT_REFRESH_SKEW_MS: u64 = 60_000;

/// One provider's cache slot. Holding the async lock while fetching makes
/// concurrent requests wait for that single fetch instead of starting their own.
type Slot = Arc<AsyncMutex<Option<Credential>>>;

/// Caches credentials per provider until shortly before they expire.
///
/// - Credentials without `expires_at_ms` are cached until [`CredentialSource::invalidate`].
/// - Concurrent requests for the same provider share one fetch.
/// - `invalidate` drops the cached value and forwards to the inner source.
pub struct CachedCredentials<S> {
    inner: S,
    skew_ms: u64,
    slots: Mutex<HashMap<String, Slot>>,
}

impl<S: CredentialSource> CachedCredentials<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, skew_ms: DEFAULT_REFRESH_SKEW_MS, slots: Mutex::new(HashMap::new()) }
    }

    /// Refresh this many milliseconds before expiry.
    pub fn refresh_skew_ms(mut self, skew_ms: u64) -> Self {
        self.skew_ms = skew_ms;
        self
    }

    /// The wrapped source.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    fn slot(&self, provider: &str) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.entry(provider.to_owned()).or_default().clone()
    }
}

impl<S> fmt::Debug for CachedCredentials<S>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedCredentials").field("inner", &self.inner).field("skew_ms", &self.skew_ms).finish()
    }
}

impl<S: CredentialSource> CredentialSource for CachedCredentials<S> {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            let slot = self.slot(provider);
            let mut cached = slot.lock().await;
            if let Some(credential) = cached.as_ref() {
                if !credential.is_expired_at(unix_now_ms(), self.skew_ms) {
                    return Ok(credential.clone());
                }
            }
            let fresh = self.inner.credential(provider).await?;
            *cached = Some(fresh.clone());
            Ok(fresh)
        })
    }

    fn invalidate(&self, provider: &str) {
        // Replace the slot: requests already waiting on the old slot finish with
        // whatever it holds; new requests fetch again.
        {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            slots.insert(provider.to_owned(), Slot::default());
        }
        self.inner.invalidate(provider);
    }
}

impl<S: CredentialSource + ?Sized> CredentialSource for Arc<S> {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        (**self).credential(provider)
    }

    fn invalidate(&self, provider: &str) {
        (**self).invalidate(provider);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Debug, Default)]
    struct Counting {
        calls: AtomicUsize,
        ttl_ms: Option<u64>,
    }

    impl CredentialSource for Counting {
        fn credential<'a>(&'a self, _provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let mut c = Credential::new(format!("token-{n}"));
            if let Some(ttl) = self.ttl_ms {
                c = c.expires_at(unix_now_ms() + ttl);
            }
            Box::pin(async move { Ok(c) })
        }
    }

    #[tokio::test]
    async fn caches_until_invalidated() {
        let cache = CachedCredentials::new(Counting::default());
        let a = cache.credential("p").await.unwrap();
        let b = cache.credential("p").await.unwrap();
        assert_eq!(a.secret, "token-1");
        assert_eq!(b.secret, "token-1");
        cache.invalidate("p");
        let c = cache.credential("p").await.unwrap();
        assert_eq!(c.secret, "token-2");
        // Providers are cached independently.
        let other = cache.credential("q").await.unwrap();
        assert_eq!(other.secret, "token-3");
    }

    #[tokio::test]
    async fn refreshes_near_expiry() {
        // Expires in 30 s, refresh skew 60 s: always considered stale.
        let cache = CachedCredentials::new(Counting { ttl_ms: Some(30_000), ..Counting::default() });
        let a = cache.credential("p").await.unwrap();
        let b = cache.credential("p").await.unwrap();
        assert_ne!(a.secret, b.secret);
        // With a smaller skew the same credential is reused.
        let cache =
            CachedCredentials::new(Counting { ttl_ms: Some(30_000), ..Counting::default() }).refresh_skew_ms(1_000);
        let a = cache.credential("p").await.unwrap();
        let b = cache.credential("p").await.unwrap();
        assert_eq!(a.secret, b.secret);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_fetch() {
        #[derive(Debug, Default)]
        struct Slow(AtomicUsize);
        impl CredentialSource for Slow {
            fn credential<'a>(&'a self, _p: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    Ok(Credential::new("t"))
                })
            }
        }
        let cache = Arc::new(CachedCredentials::new(Slow::default()));
        let futs = (0..8).map(|_| {
            let c = cache.clone();
            async move { c.credential("p").await }
        });
        let results = futures::future::join_all(futs).await;
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(cache.inner().0.load(Ordering::SeqCst), 1);
    }
}
