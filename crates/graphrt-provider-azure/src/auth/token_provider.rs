//! Per-scope token cache with single-flight refresh.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{AccessToken, AuthError, AzureCredential};
use graphrt_provider_api::http::{SecretString, TokenSource};
use graphrt_provider_api::{BoxFuture, ProviderError};

/// Unix seconds; injectable so tests can skew the clock.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Default refresh margin: a token is replaced `5 min` before it expires.
const DEFAULT_REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// Hands out bearer tokens for scopes, fetching each at most once per lifetime.
///
/// Hot path: a cached, unexpired token is one `RwLock` read. Refreshes are
/// serialised by an async mutex, so 20 concurrent callers on a cold cache
/// cause one token request.
pub struct TokenProvider {
    credential: AzureCredential,
    client: reqwest::Client,
    cache: RwLock<HashMap<String, AccessToken>>,
    refresh_lock: tokio::sync::Mutex<()>,
    refresh_margin: Duration,
    clock: Clock,
}

impl fmt::Debug for TokenProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenProvider")
            .field("credential", &self.credential.kind())
            .field("refresh_margin", &self.refresh_margin)
            .finish_non_exhaustive()
    }
}

impl TokenProvider {
    /// @throws `AuthError::Config` when the HTTP client cannot be built.
    pub fn new(credential: AzureCredential) -> Result<Self, AuthError> {
        let client = crate::http::build_client(Duration::from_secs(15)).map_err(AuthError::Config)?;
        Ok(Self::with_client(credential, client))
    }

    pub fn with_client(credential: AzureCredential, client: reqwest::Client) -> Self {
        Self {
            credential,
            client,
            cache: RwLock::new(HashMap::new()),
            refresh_lock: tokio::sync::Mutex::new(()),
            refresh_margin: DEFAULT_REFRESH_MARGIN,
            clock: Arc::new(system_now),
        }
    }

    /// Replace the clock (tests: clock skew).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_refresh_margin(mut self, margin: Duration) -> Self {
        self.refresh_margin = margin;
        self
    }

    pub fn credential(&self) -> &AzureCredential {
        &self.credential
    }

    /// A token for `scope` valid for at least the refresh margin.
    ///
    /// @throws `AuthError` from the credential when a refresh fails.
    pub async fn token(&self, scope: &str) -> Result<SecretString, AuthError> {
        if let Some(token) = self.cached(scope) {
            return Ok(token);
        }
        let _refreshing = self.refresh_lock.lock().await;
        // Another caller may have refreshed while this one waited.
        if let Some(token) = self.cached(scope) {
            return Ok(token);
        }
        let fresh = self.credential.fetch(&self.client, scope, (self.clock)()).await?;
        let token = fresh.token.clone();
        self.write_cache().insert(scope.to_string(), fresh);
        Ok(token)
    }

    /// Drop `rejected` if it is still the cached token for `scope` (the API
    /// answered 401), so the next `token` call fetches a new one. A token
    /// already replaced by a concurrent refresh is kept.
    pub fn invalidate(&self, scope: &str, rejected: &SecretString) {
        let mut cache = self.write_cache();
        if cache.get(scope).is_some_and(|cached| &cached.token == rejected) {
            cache.remove(scope);
        }
    }

    /// Drop whatever token is cached for `scope`.
    pub fn forget(&self, scope: &str) {
        self.write_cache().remove(scope);
    }

    fn cached(&self, scope: &str) -> Option<SecretString> {
        let cache = match self.cache.read() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = cache.get(scope)?;
        let refresh_at = entry.expires_at.saturating_sub(self.refresh_margin.as_secs());
        ((self.clock)() < refresh_at).then(|| entry.token.clone())
    }

    fn write_cache(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, AccessToken>> {
        match self.cache.write() {
            Ok(cache) => cache,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// One scope of a [`TokenProvider`] as the shared HTTP layer's
/// [`TokenSource`]: `force_refresh` (after a 401) drops the cached token and
/// fetches a new one.
#[derive(Debug, Clone)]
pub struct ScopedToken {
    provider: Arc<TokenProvider>,
    scope: &'static str,
}

impl ScopedToken {
    pub fn new(provider: Arc<TokenProvider>, scope: &'static str) -> Self {
        Self { provider, scope }
    }

    pub fn provider(&self) -> &Arc<TokenProvider> {
        &self.provider
    }
}

impl TokenSource for ScopedToken {
    fn token(&self, force_refresh: bool) -> BoxFuture<'_, Result<SecretString, ProviderError>> {
        Box::pin(async move {
            if force_refresh {
                self.provider.forget(self.scope);
            }
            Ok(self.provider.token(self.scope).await?)
        })
    }
}

fn system_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}
