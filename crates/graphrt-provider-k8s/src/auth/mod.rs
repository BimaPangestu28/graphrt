//! Cluster credentials: in-cluster service account, kubeconfig (token, token
//! file, client certificate, exec plugin) and the [`TokenSource`] hook that
//! cloud-specific auth (EKS presigned STS token, GKE OAuth token, AKS Entra
//! token) plugs into.
//!
//! Tokens are cached until shortly before they expire, refreshed once on a 401
//! and never logged: every credential is held in a [`SecretString`] whose
//! `Debug` output is redacted.

mod exec;
mod in_cluster;
mod kubeconfig;
mod time;

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

pub use exec::{ExecConfig, ExecTokenSource};
pub use in_cluster::{IN_CLUSTER_DIR, in_cluster_config, in_cluster_config_from};
pub use kubeconfig::{KubeconfigOptions, kubeconfig_from_env, kubeconfig_from_str};
pub use time::parse_rfc3339;

/// Refresh a token this long before it expires (plan §6, covers ±5 min skew).
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// A credential string whose `Debug` output never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value, for the one place that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString(<redacted>)")
    }
}

/// A bearer token and when it stops being valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub token: SecretString,
    /// `None` for tokens without a known expiry (static kubeconfig tokens).
    pub expires_at: Option<SystemTime>,
}

/// Errors while resolving cluster credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("kubeconfig: {0}")]
    Kubeconfig(String),
    #[error("not running in a cluster: {0}")]
    NotInCluster(String),
    #[error("exec credential plugin: {0}")]
    Exec(String),
    #[error("token source: {0}")]
    Source(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// Boxed future returned by [`TokenSource::fetch`] (object safe).
pub type TokenFuture<'a> = Pin<Box<dyn Future<Output = Result<AccessToken, AuthError>> + Send + 'a>>;

/// Where bearer tokens come from. Implemented here for static tokens, token
/// files and exec plugins; `gw-eks` / `gw-gke` / `gw-aks` implement it on top of
/// `auth-aws` / `auth-gcp` / `auth-azure`.
pub trait TokenSource: Send + Sync + fmt::Debug {
    /// Fetch a fresh token (the caller caches it).
    fn fetch(&self) -> TokenFuture<'_>;

    /// Upper bound on how long a fetched token is reused even if it does not
    /// expire (token files are re-read every minute because the kubelet rotates
    /// projected service account tokens in place).
    fn max_cache_age(&self) -> Option<Duration> {
        None
    }
}

/// A fixed token (kubeconfig `token:`).
#[derive(Debug)]
pub struct StaticToken(pub SecretString);

impl TokenSource for StaticToken {
    fn fetch(&self) -> TokenFuture<'_> {
        let token = AccessToken { token: self.0.clone(), expires_at: None };
        Box::pin(async move { Ok(token) })
    }
}

/// A token read from a file, re-read every minute (in-cluster service account,
/// kubeconfig `tokenFile:`).
#[derive(Debug)]
pub struct TokenFile(pub PathBuf);

impl TokenSource for TokenFile {
    fn fetch(&self) -> TokenFuture<'_> {
        Box::pin(async move {
            let text = tokio::fs::read_to_string(&self.0)
                .await
                .map_err(|error| AuthError::Source(format!("read token file {}: {error}", self.0.display())))?;
            let token = text.trim();
            if token.is_empty() {
                return Err(AuthError::Source(format!("token file {} is empty", self.0.display())));
            }
            Ok(AccessToken { token: SecretString::new(token), expires_at: None })
        })
    }

    fn max_cache_age(&self) -> Option<Duration> {
        Some(Duration::from_secs(60))
    }
}

#[derive(Debug)]
struct CachedToken {
    token: SecretString,
    refresh_at: SystemTime,
}

/// Caches a [`TokenSource`]: one fetch at a time, reuse until
/// `expires_at - REFRESH_MARGIN` (or half the lifetime for short tokens),
/// invalidate on 401.
#[derive(Debug)]
pub struct CachingTokenSource {
    source: Arc<dyn TokenSource>,
    cached: Mutex<Option<CachedToken>>,
}

impl CachingTokenSource {
    /// Wrap a token source.
    pub fn new(source: Arc<dyn TokenSource>) -> Self {
        Self { source, cached: Mutex::new(None) }
    }

    /// A valid token, fetching one only when the cached one is due.
    pub async fn token(&self) -> Result<SecretString, AuthError> {
        let mut cached = self.cached.lock().await;
        let now = SystemTime::now();
        if let Some(entry) = cached.as_ref().filter(|entry| entry.refresh_at > now) {
            return Ok(entry.token.clone());
        }
        let fresh = self.source.fetch().await?;
        let refresh_at = refresh_deadline(now, fresh.expires_at, self.source.max_cache_age());
        let token = fresh.token.clone();
        *cached = Some(CachedToken { token: fresh.token, refresh_at });
        Ok(token)
    }

    /// Drop the cached token if it is still `rejected` (after a 401), so
    /// concurrent 401s trigger a single refresh.
    pub async fn invalidate(&self, rejected: &SecretString) {
        let mut cached = self.cached.lock().await;
        if cached.as_ref().is_some_and(|entry| &entry.token == rejected) {
            *cached = None;
        }
    }
}

/// When a token fetched at `now` must be refreshed.
fn refresh_deadline(now: SystemTime, expires_at: Option<SystemTime>, max_age: Option<Duration>) -> SystemTime {
    // Static tokens without expiry are kept for a day, then re-fetched (cheap).
    let by_age = now + max_age.unwrap_or(Duration::from_secs(24 * 3600));
    let Some(expires_at) = expires_at else { return by_age };
    let lifetime = expires_at.duration_since(now).unwrap_or_default();
    let by_expiry = if lifetime > REFRESH_MARGIN * 2 { now + (lifetime - REFRESH_MARGIN) } else { now + lifetime / 2 };
    by_expiry.min(by_age)
}

/// Resolved connection settings for one cluster.
#[derive(Clone)]
pub struct ClusterConfig {
    /// API server base URL, e.g. `https://10.0.0.1:443`.
    pub server: String,
    /// PEM bundle trusted for the API server (replaces the web PKI roots).
    pub ca_pem: Option<Vec<u8>>,
    /// Client certificate + private key, PEM, for mTLS auth.
    pub client_identity_pem: Option<SecretString>,
    /// Bearer token source, if the cluster uses tokens.
    pub token_source: Option<Arc<dyn TokenSource>>,
    /// Default namespace (service account namespace or kubeconfig context).
    pub namespace: Option<String>,
}

impl ClusterConfig {
    /// Settings for `server` with no credentials (tests, `kubectl proxy`).
    pub fn new(server: impl Into<String>) -> Self {
        Self { server: server.into(), ca_pem: None, client_identity_pem: None, token_source: None, namespace: None }
    }

    /// Replace the token source; the hook for cloud-specific cluster auth.
    pub fn with_token_source(mut self, source: Arc<dyn TokenSource>) -> Self {
        self.token_source = Some(source);
        self
    }
}

impl fmt::Debug for ClusterConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClusterConfig")
            .field("server", &self.server)
            .field("ca_pem", &self.ca_pem.as_ref().map(|pem| pem.len()))
            .field("client_identity_pem", &self.client_identity_pem.as_ref().map(|_| "<redacted>"))
            .field("token_source", &self.token_source)
            .field("namespace", &self.namespace)
            .finish()
    }
}

/// In-cluster config when running in a pod, else the kubeconfig from
/// `$KUBECONFIG` / `~/.kube/config` (the order client-go uses).
pub fn infer_cluster_config() -> Result<ClusterConfig, AuthError> {
    match in_cluster_config() {
        Ok(config) => Ok(config),
        Err(AuthError::NotInCluster(_)) => kubeconfig_from_env(&KubeconfigOptions::default()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    struct CountingSource {
        fetches: AtomicUsize,
        lifetime: Option<Duration>,
    }

    impl TokenSource for CountingSource {
        fn fetch(&self) -> TokenFuture<'_> {
            let count = self.fetches.fetch_add(1, Ordering::SeqCst) + 1;
            let expires_at = self.lifetime.map(|lifetime| SystemTime::now() + lifetime);
            Box::pin(async move { Ok(AccessToken { token: SecretString::new(format!("token-{count}")), expires_at }) })
        }
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = SecretString::new("super-secret-token");
        assert!(!format!("{secret:?}").contains("super-secret"));
        let config = ClusterConfig {
            client_identity_pem: Some(SecretString::new("-----BEGIN PRIVATE KEY-----abc")),
            ..ClusterConfig::new("https://k8s")
        }
        .with_token_source(Arc::new(StaticToken(SecretString::new("super-secret-token"))));
        let printed = format!("{config:?}");
        assert!(!printed.contains("super-secret") && !printed.contains("PRIVATE KEY"), "{printed}");
    }

    #[test]
    fn refresh_deadline_keeps_margin_before_expiry() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let hour = Duration::from_secs(3600);
        assert_eq!(refresh_deadline(now, Some(now + hour), None), now + hour - REFRESH_MARGIN);
        // Short-lived token: refresh at half its life, never after expiry.
        let short = Duration::from_secs(120);
        assert_eq!(refresh_deadline(now, Some(now + short), None), now + short / 2);
        // Already expired (clock skew): due immediately.
        assert_eq!(refresh_deadline(now, Some(now - hour), None), now);
        // Token files are re-read every minute even without expiry.
        assert_eq!(refresh_deadline(now, None, Some(Duration::from_secs(60))), now + Duration::from_secs(60));
    }

    #[tokio::test]
    async fn caching_fetches_once_and_refreshes_after_invalidate() {
        let source = Arc::new(CountingSource { lifetime: Some(Duration::from_secs(3600)), ..Default::default() });
        let cache = CachingTokenSource::new(source.clone());
        let first = cache.token().await.expect("token");
        assert_eq!(cache.token().await.expect("token"), first);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        // A stale token rejected twice (two concurrent 401s) refreshes once.
        cache.invalidate(&first).await;
        let second = cache.token().await.expect("token");
        cache.invalidate(&first).await;
        assert_eq!(cache.token().await.expect("token"), second);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn caching_refetches_expired_tokens() {
        let source = Arc::new(CountingSource { lifetime: Some(Duration::ZERO), ..Default::default() });
        let cache = CachingTokenSource::new(source.clone());
        let _first = cache.token().await.expect("token");
        let _second = cache.token().await.expect("token");
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
    }
}
