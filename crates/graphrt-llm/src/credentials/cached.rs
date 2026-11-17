//! Caching wrapper with single-flight refresh, fetch timeouts and shared failures.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::lock::Mutex as AsyncMutex;

use super::{Credential, CredentialSource, unix_now_ms};
use crate::error::LlmError;

/// Credentials are refreshed this long before they expire (60 s).
pub const DEFAULT_REFRESH_SKEW_MS: u64 = 60_000;

/// A fetch from the wrapped source that takes longer than this fails (15 s).
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct SlotState {
    /// Cached credential and the source generation it was fetched under.
    credential: Option<(Credential, u64)>,
    /// Last failed fetch and its fetch sequence number.
    failure: Option<(LlmError, u64)>,
}

/// One provider's cache slot. Holding the async lock while fetching makes
/// concurrent requests wait for that single fetch instead of starting their own.
#[derive(Default)]
struct Slot {
    state: AsyncMutex<SlotState>,
    /// Number of fetches completed in this slot (success or failure).
    completed: AtomicU64,
}

/// Caches credentials per provider until shortly before they expire.
///
/// - Credentials without `expires_at_ms` are cached until [`CredentialSource::invalidate`]
///   or until the wrapped source's [`CredentialSource::generation`] changes (e.g. the
///   host registered a different key source or signed the user out).
/// - Concurrent requests for the same provider share one fetch. If that fetch
///   fails, every request that was waiting behind it gets the same error instead of
///   retrying the failing fetch one after another.
/// - A fetch that does not finish within the fetch timeout (default 15 s) fails
///   with [`LlmError::Auth`]. The timeout needs a Tokio runtime; elsewhere fetches
///   are not time-limited.
/// - `invalidate` drops the cached value and forwards to the inner source.
pub struct CachedCredentials<S> {
    inner: S,
    skew_ms: u64,
    fetch_timeout: Duration,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

impl<S: CredentialSource> CachedCredentials<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            skew_ms: DEFAULT_REFRESH_SKEW_MS,
            fetch_timeout: DEFAULT_FETCH_TIMEOUT,
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Refresh this many milliseconds before expiry.
    pub fn refresh_skew_ms(mut self, skew_ms: u64) -> Self {
        self.skew_ms = skew_ms;
        self
    }

    /// Fail a fetch from the wrapped source after this long.
    pub fn fetch_timeout(mut self, timeout: Duration) -> Self {
        self.fetch_timeout = timeout;
        self
    }

    /// The wrapped source.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Drop every cached credential (all providers).
    pub fn clear(&self) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.clear();
    }

    fn slot(&self, provider: &str) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        slots.entry(provider.to_owned()).or_default().clone()
    }

    async fn fetch(&self, provider: &str) -> Result<Credential, LlmError> {
        let fetch = self.inner.credential(provider);
        #[cfg(feature = "http")]
        {
            if tokio::runtime::Handle::try_current().is_ok() {
                return match tokio::time::timeout(self.fetch_timeout, fetch).await {
                    Ok(result) => result,
                    Err(_) => Err(LlmError::Auth(format!(
                        "credential source for `{provider}` did not answer within {} ms",
                        self.fetch_timeout.as_millis()
                    ))),
                };
            }
        }
        fetch.await
    }
}

impl<S> fmt::Debug for CachedCredentials<S>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedCredentials")
            .field("inner", &self.inner)
            .field("skew_ms", &self.skew_ms)
            .field("fetch_timeout", &self.fetch_timeout)
            .finish()
    }
}

impl<S: CredentialSource> CredentialSource for CachedCredentials<S> {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            let slot = self.slot(provider);
            // Fetches completed before we started waiting; a failure recorded after
            // this point belongs to a fetch we were queued behind.
            let ticket = slot.completed.load(Ordering::SeqCst);
            let mut state = slot.state.lock().await;
            if let Some((error, seq)) = &state.failure {
                if *seq > ticket {
                    return Err(error.clone());
                }
            }
            let generation = self.inner.generation();
            if let Some((credential, cached_generation)) = &state.credential {
                let fresh = *cached_generation == generation && !credential.is_expired_at(unix_now_ms(), self.skew_ms);
                if fresh {
                    return Ok(credential.clone());
                }
            }
            let result = self.fetch(provider).await;
            let seq = slot.completed.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            match result {
                Ok(credential) => {
                    state.credential = Some((credential.clone(), generation));
                    state.failure = None;
                    Ok(credential)
                }
                Err(error) => {
                    state.credential = None;
                    state.failure = Some((error.clone(), seq));
                    Err(error)
                }
            }
        })
    }

    fn invalidate(&self, provider: &str) {
        // Replace the slot: requests already waiting on the old slot finish with
        // whatever it holds; new requests fetch again.
        {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            slots.insert(provider.to_owned(), Arc::new(Slot::default()));
        }
        self.inner.invalidate(provider);
    }

    fn generation(&self) -> u64 {
        self.inner.generation()
    }
}

impl<S: CredentialSource + ?Sized> CredentialSource for Arc<S> {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        (**self).credential(provider)
    }

    fn invalidate(&self, provider: &str) {
        (**self).invalidate(provider);
    }

    fn generation(&self) -> u64 {
        (**self).generation()
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
        generation: AtomicU64,
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

        fn generation(&self) -> u64 {
            self.generation.load(Ordering::SeqCst)
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
    async fn source_generation_change_drops_cached_values() {
        let cache = CachedCredentials::new(Counting::default());
        assert_eq!(cache.credential("p").await.unwrap().secret, "token-1");
        // e.g. the host signed user A out and registered user B's key source.
        cache.inner().generation.fetch_add(1, Ordering::SeqCst);
        assert_eq!(cache.credential("p").await.unwrap().secret, "token-2");
        cache.clear();
        assert_eq!(cache.credential("p").await.unwrap().secret, "token-3");
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

    #[tokio::test]
    async fn waiters_share_one_failure_and_hung_sources_time_out() {
        #[derive(Debug, Default)]
        struct Hangs(AtomicUsize);
        impl CredentialSource for Hangs {
            fn credential<'a>(&'a self, _p: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    futures::future::pending::<()>().await;
                    Ok(Credential::new("never"))
                })
            }
        }
        let cache = Arc::new(CachedCredentials::new(Hangs::default()).fetch_timeout(Duration::from_millis(50)));
        let started = std::time::Instant::now();
        let futs = (0..6).map(|_| {
            let c = cache.clone();
            async move { c.credential("p").await }
        });
        let results = futures::future::join_all(futs).await;
        assert!(results.iter().all(|r| matches!(r, Err(LlmError::Auth(_)))));
        // One fetch timed out and every waiter got its error: not 6 serial timeouts.
        assert_eq!(cache.inner().0.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_millis(250), "{:?}", started.elapsed());
        // A later request tries again.
        assert!(cache.credential("p").await.is_err());
        assert_eq!(cache.inner().0.load(Ordering::SeqCst), 2);
    }
}
