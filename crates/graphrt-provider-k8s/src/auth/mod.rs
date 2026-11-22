//! Cluster credentials: in-cluster service account, kubeconfig (token, token
//! file, client certificate, exec plugin) and the hook cloud-specific cluster
//! auth plugs into.
//!
//! Bearer tokens are a [`TokenSource`] from `graphrt-provider-api`: the
//! shared HTTP layer caches them until `exp - 5 min` ([`TokenCache`]),
//! refreshes once on a 401 and never logs them ([`SecretString`]). EKS
//! (presigned STS token), GKE (Google OAuth token) and AKS (Entra token)
//! provide their own `TokenSource` from the `auth-aws` / `auth-gcp` /
//! `auth-azure` modules and set it with [`ClusterConfig::with_token_source`].

#[cfg(feature = "kubeconfig")]
mod exec;
mod in_cluster;
#[cfg(feature = "kubeconfig")]
mod kubeconfig;
#[cfg(feature = "kubeconfig")]
mod time;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use graphrt_provider_api::http::{
    AccessToken, REFRESH_MARGIN, SecretString, StaticToken, TokenCache, TokenFetcher, TokenSource,
};
use graphrt_provider_api::{BoxFuture, ProviderError};

#[cfg(feature = "kubeconfig")]
pub use exec::{ExecConfig, ExecTokenFetcher};
pub use in_cluster::{IN_CLUSTER_DIR, in_cluster_config, in_cluster_config_from};
#[cfg(feature = "kubeconfig")]
pub use kubeconfig::{KubeconfigOptions, kubeconfig_from_env, kubeconfig_from_str};
#[cfg(feature = "kubeconfig")]
pub use time::parse_rfc3339;

/// How often token files are re-read: the kubelet rotates projected service
/// account tokens in place (client-go re-reads every minute too).
pub const TOKEN_FILE_MAX_AGE: Duration = Duration::from_secs(60);

/// Lifetime given to tokens without a known expiry, before they are re-fetched.
#[cfg(feature = "kubeconfig")]
const UNKNOWN_EXPIRY: Duration = Duration::from_secs(24 * 3600);

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

impl From<AuthError> for ProviderError {
    fn from(error: AuthError) -> Self {
        ProviderError::auth(error.to_string())
    }
}

/// A token read from a file, re-read every [`TOKEN_FILE_MAX_AGE`] (in-cluster
/// service account, kubeconfig `tokenFile:`).
#[derive(Debug)]
pub struct TokenFile(pub PathBuf);

impl TokenFetcher for TokenFile {
    fn fetch(&self) -> BoxFuture<'_, Result<AccessToken, ProviderError>> {
        Box::pin(async move {
            let text = tokio::fs::read_to_string(&self.0)
                .await
                .map_err(|error| AuthError::Source(format!("read token file {}: {error}", self.0.display())))?;
            let token = text.trim();
            if token.is_empty() {
                return Err(AuthError::Source(format!("token file {} is empty", self.0.display())).into());
            }
            // TokenCache refreshes at `expires_in - REFRESH_MARGIN`: this re-reads after a minute.
            Ok(AccessToken { value: SecretString::new(token), expires_in: TOKEN_FILE_MAX_AGE + REFRESH_MARGIN })
        })
    }
}

/// Wrap a fetcher in the shared cache (one fetch at a time, refresh before expiry).
pub fn cached<F: TokenFetcher + 'static>(fetcher: F) -> Arc<dyn TokenSource> {
    Arc::new(TokenCache::new(fetcher))
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

    /// Replace the token source: the hook for EKS / GKE / AKS cluster tokens.
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
            .field("ca_pem_bytes", &self.ca_pem.as_ref().map(Vec::len))
            .field("client_identity_pem", &self.client_identity_pem)
            .field("token_source", &self.token_source.as_ref().map(|_| "<token source>"))
            .field("namespace", &self.namespace)
            .finish()
    }
}

/// In-cluster config when running in a pod, else the kubeconfig from
/// `$KUBECONFIG` / `~/.kube/config` (the order client-go uses). Feature `kubeconfig`.
#[cfg(feature = "kubeconfig")]
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

    #[test]
    fn cluster_config_debug_is_redacted() {
        let config = ClusterConfig {
            client_identity_pem: Some(SecretString::new("-----BEGIN PRIVATE KEY-----abc")),
            ..ClusterConfig::new("https://k8s")
        }
        .with_token_source(Arc::new(StaticToken(SecretString::new("super-secret-token"))));
        let printed = format!("{config:?}");
        assert!(!printed.contains("super-secret") && !printed.contains("PRIVATE KEY"), "{printed}");
    }

    #[tokio::test]
    async fn token_files_are_reread_after_a_minute() {
        let path = std::env::temp_dir().join(format!("graphrt-k8s-token-{}", std::process::id()));
        std::fs::write(&path, "first\n").unwrap();
        let token = TokenFile(path.clone()).fetch().await.unwrap();
        assert_eq!(token.value.expose(), "first");
        assert_eq!(token.expires_in.saturating_sub(REFRESH_MARGIN), TOKEN_FILE_MAX_AGE);
        std::fs::write(&path, "").unwrap();
        assert!(TokenFile(path.clone()).fetch().await.is_err());
        std::fs::remove_file(path).unwrap();
    }
}
