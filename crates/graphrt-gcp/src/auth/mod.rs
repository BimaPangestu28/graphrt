//! Google credentials for the gateway and `graphrt deploy`, hand-rolled (no SDK).
//!
//! Two sources, chosen by [`GcpAuth::from_env`] the way Google's client libraries do:
//!
//! 1. `GOOGLE_APPLICATION_CREDENTIALS` names a **service-account key file**: an
//!    RS256 JWT assertion (signed with `ring`) is exchanged at
//!    `https://oauth2.googleapis.com/token` for an access token, or for an ID
//!    token when the assertion carries `target_audience`.
//! 2. Otherwise the **metadata server** (`GCE_METADATA_HOST`, default
//!    `metadata.google.internal`) of Compute Engine, Cloud Run or GKE (workload
//!    identity) hands out tokens of the attached service account.
//!
//! Caching rules (plan §6, conformance C8):
//! - Every token is cached until `exp - 5 min`, measured on the local monotonic
//!   clock from the moment it was fetched (`expires_in`, or `exp - iat` of an ID
//!   token), so a skewed wall clock never makes a cached token look fresh or stale.
//! - Concurrent callers share one fetch per token (access token, one ID token per audience).
//! - After a 401 the HTTP layer forces one refresh, then gives up.
//! - A skewed local clock (±5 min) rejected by the token endpoint is corrected
//!   once from the endpoint's `Date` header.
//! - Tokens are `SecretString`s: `Debug` prints `***`, and no error message
//!   carries a token, an assertion or key material.

mod http_date;
mod jwt;
mod metadata;
mod service_account;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use graphrt_provider_api::http::{AccessToken, TokenCache, TokenFetcher, TokenSource};
use graphrt_provider_api::{BoxFuture, ProviderError};

pub use service_account::ServiceAccountKey;

use crate::http_client::{ClientOptions, build_client};
use metadata::MetadataServer;
use service_account::ServiceAccountExchange;

/// Google's OAuth 2.0 token endpoint.
pub const DEFAULT_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// The metadata server reachable from every Google Cloud runtime.
pub const DEFAULT_METADATA_URL: &str = "http://metadata.google.internal";
/// Scope of the access tokens: Cloud Run Admin and Secret Manager both accept it;
/// what the token may do is decided by the service account's IAM roles.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Timeout of one token fetch.
const TOKEN_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// What a token is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Grant {
    /// OAuth 2.0 access token with these scopes.
    Access { scopes: Vec<String> },
    /// OpenID Connect ID token for this audience (Cloud Run invoker auth).
    Identity { audience: String },
}

#[derive(Debug)]
enum Source {
    Metadata(MetadataServer),
    ServiceAccount(ServiceAccountExchange),
}

/// One token kind from one source; cached by a [`TokenCache`].
struct GrantFetcher {
    source: Arc<Source>,
    grant: Grant,
}

impl TokenFetcher for GrantFetcher {
    fn fetch(&self) -> BoxFuture<'_, Result<AccessToken, ProviderError>> {
        Box::pin(async move {
            let fetch = async {
                match self.source.as_ref() {
                    Source::Metadata(server) => server.fetch(&self.grant).await,
                    Source::ServiceAccount(exchange) => exchange.fetch(&self.grant).await,
                }
            };
            tokio::time::timeout(TOKEN_FETCH_TIMEOUT, fetch)
                .await
                .map_err(|_| ProviderError::transient("token fetch timed out"))?
        })
    }
}

type SharedTokenSource = Arc<TokenCache<GrantFetcher>>;

/// Google credentials with cached access and ID tokens. Cheap to clone: clones
/// share their caches.
///
/// @example
/// let auth = GcpAuth::from_env()?;
/// let admin_tokens = auth.access_tokens();          // Admin API, Secret Manager
/// let invoker_tokens = auth.id_tokens("https://svc-abc-ew.a.run.app"); // Cloud Run invoke
#[derive(Clone)]
pub struct GcpAuth {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    source: Arc<Source>,
    principal: String,
    access: SharedTokenSource,
    identity: Mutex<HashMap<String, SharedTokenSource>>,
}

impl fmt::Debug for GcpAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("GcpAuth").field("principal", &self.inner.principal).finish_non_exhaustive()
    }
}

impl GcpAuth {
    /// Chooses the source like Google's client libraries: the key file named by
    /// `GOOGLE_APPLICATION_CREDENTIALS`, else the metadata server at
    /// `GCE_METADATA_HOST` (default `metadata.google.internal`). Nothing is fetched yet.
    ///
    /// @throws `ProviderError` (fatal) when the key file cannot be read or parsed.
    pub fn from_env() -> Result<Self, ProviderError> {
        if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").filter(|path| !path.is_empty()) {
            let json = std::fs::read_to_string(&path).map_err(|error| {
                ProviderError::fatal(format!("cannot read GOOGLE_APPLICATION_CREDENTIALS {}: {error}", path.display()))
            })?;
            return GcpAuthBuilder::service_account(ServiceAccountKey::from_json(&json)?).build();
        }
        let metadata_url = match std::env::var("GCE_METADATA_HOST") {
            Ok(host) if !host.is_empty() => format!("http://{host}"),
            _ => DEFAULT_METADATA_URL.to_owned(),
        };
        GcpAuthBuilder::metadata_server(metadata_url).build()
    }

    /// Who the tokens belong to, safe to log: the key's `client_email`, or `metadata-server`.
    pub fn principal(&self) -> &str {
        &self.inner.principal
    }

    /// Access tokens (scope `cloud-platform` unless the builder set others).
    pub fn access_tokens(&self) -> Arc<dyn TokenSource> {
        self.inner.access.clone()
    }

    /// ID tokens whose `aud` is `audience` (a Cloud Run service URL or custom audience).
    /// One cache per audience, shared by every caller.
    pub fn id_tokens(&self, audience: &str) -> Arc<dyn TokenSource> {
        let mut identity = self.inner.identity.lock().unwrap_or_else(PoisonError::into_inner);
        let cache = identity.entry(audience.to_owned()).or_insert_with(|| {
            Arc::new(TokenCache::new(GrantFetcher {
                source: self.inner.source.clone(),
                grant: Grant::Identity { audience: audience.to_owned() },
            }))
        });
        cache.clone()
    }
}

/// Builds a [`GcpAuth`] from an explicit source (operator configuration, tests).
#[derive(Debug)]
pub struct GcpAuthBuilder {
    kind: BuilderKind,
    scopes: Vec<String>,
    token_endpoint: Option<String>,
}

#[derive(Debug)]
enum BuilderKind {
    Metadata(String),
    ServiceAccount(Box<ServiceAccountKey>),
}

impl GcpAuthBuilder {
    /// Tokens from the metadata server at `base_url` (`http://metadata.google.internal`).
    pub fn metadata_server(base_url: impl Into<String>) -> Self {
        Self { kind: BuilderKind::Metadata(base_url.into()), scopes: Vec::new(), token_endpoint: None }
    }

    /// Tokens from a service-account key.
    pub fn service_account(key: ServiceAccountKey) -> Self {
        Self { kind: BuilderKind::ServiceAccount(Box::new(key)), scopes: Vec::new(), token_endpoint: None }
    }

    /// Access-token scopes (default `cloud-platform`).
    pub fn scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Posts assertions to this URL instead of the key's `token_uri` (private
    /// endpoints, tests). Operator configuration only; the assertion's `aud`
    /// stays the key's `token_uri`.
    pub fn token_endpoint(mut self, url: impl Into<String>) -> Self {
        self.token_endpoint = Some(url.into());
        self
    }

    /// Builds the credentials; nothing is fetched until a token is asked for.
    ///
    /// @throws `ProviderError` (fatal) when the HTTP client cannot be built.
    pub fn build(self) -> Result<GcpAuth, ProviderError> {
        let options = ClientOptions { request_timeout: Some(TOKEN_FETCH_TIMEOUT), bypass_proxy: false };
        let (source, principal, scopes) = match self.kind {
            BuilderKind::Metadata(base_url) => {
                let http = build_client(ClientOptions { bypass_proxy: true, ..options })?;
                let server = MetadataServer::new(base_url, http);
                let principal = format!("metadata-server ({})", server.base_url());
                // The metadata server already scopes tokens to `cloud-platform`;
                // only custom scopes are passed on.
                (Source::Metadata(server), principal, self.scopes)
            }
            BuilderKind::ServiceAccount(key) => {
                let http = build_client(options)?;
                let principal = key.client_email().to_owned();
                let endpoint = self.token_endpoint.unwrap_or_else(|| key.token_uri().to_owned());
                let scopes = if self.scopes.is_empty() { vec![CLOUD_PLATFORM_SCOPE.to_owned()] } else { self.scopes };
                let exchange = ServiceAccountExchange::new(Arc::from(key), http, endpoint, Arc::new(AtomicI64::new(0)));
                (Source::ServiceAccount(exchange), principal, scopes)
            }
        };
        let source = Arc::new(source);
        let access =
            Arc::new(TokenCache::new(GrantFetcher { source: source.clone(), grant: Grant::Access { scopes } }));
        Ok(GcpAuth { inner: Arc::new(AuthInner { source, principal, access, identity: Mutex::new(HashMap::new()) }) })
    }
}
