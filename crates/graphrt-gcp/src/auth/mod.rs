//! Google credentials for the gateway and `graphrt deploy`, hand-rolled (no SDK).
//!
//! Sources, chosen by [`GcpAuth::from_env`] the way Google's client libraries do:
//!
//! 1. `GOOGLE_APPLICATION_CREDENTIALS` names a credentials file: a
//!    **service-account key** (an RS256 JWT assertion signed with `ring` is
//!    exchanged at `https://oauth2.googleapis.com/token` for an access token,
//!    or for an ID token when the assertion carries `target_audience`), or a
//!    person's **user credentials** (`authorized_user`: a refresh token,
//!    [`UserCredentials`]).
//! 2. Else the application default credentials file `gcloud auth
//!    application-default login` writes (`$CLOUDSDK_CONFIG` or
//!    `~/.config/gcloud`, `%APPDATA%\gcloud` on Windows).
//! 3. Else the **metadata server** (`GCE_METADATA_HOST`, default
//!    `metadata.google.internal`) of Compute Engine, Cloud Run or GKE (workload
//!    identity) hands out tokens of the attached service account.
//!
//! [`GcpAuth::from_local_env`] is the order of a command run on a person's
//! machine (`graphrt deploy --direct`): an access token in
//! [`ACCESS_TOKEN_ENV`] first (what an app that signed the person in hands
//! over), then 1 and 2, and no metadata server. [`GcpAuthBuilder`] takes each
//! source explicitly, including a [`TokenSource`] of the caller's own.
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
#[cfg(feature = "auth-user")]
mod user;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use graphrt_provider_api::http::{AccessToken, TokenCache, TokenFetcher, TokenSource};
#[cfg(feature = "auth-user")]
use graphrt_provider_api::http::{SecretString, StaticToken};
use graphrt_provider_api::{BoxFuture, ProviderError};

pub use service_account::ServiceAccountKey;
#[cfg(feature = "auth-user")]
pub use user::UserCredentials;

use crate::http_client::{ClientOptions, build_client};
use metadata::MetadataServer;
use service_account::ServiceAccountExchange;
#[cfg(feature = "auth-user")]
use user::UserRefresh;

/// Google's OAuth 2.0 token endpoint.
pub const DEFAULT_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
/// The metadata server reachable from every Google Cloud runtime.
pub const DEFAULT_METADATA_URL: &str = "http://metadata.google.internal";
/// Scope of the access tokens: Cloud Run Admin and Secret Manager both accept it;
/// what the token may do is decided by the service account's IAM roles.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// An OAuth 2.0 access token handed over by the caller (Google's
/// `cloud-platform` scope). It is used as it is: when it expires, the
/// command fails with `unauthorized` and the caller passes a fresh one.
pub const ACCESS_TOKEN_ENV: &str = "GOOGLE_OAUTH_ACCESS_TOKEN";

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

enum Source {
    Metadata(MetadataServer),
    ServiceAccount(ServiceAccountExchange),
    #[cfg(feature = "auth-user")]
    User(UserRefresh),
    /// Tokens of the caller's own (an access token, an app's sign in).
    External(Arc<dyn TokenSource>),
}

impl fmt::Debug for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(server) => server.fmt(formatter),
            Self::ServiceAccount(exchange) => exchange.fmt(formatter),
            #[cfg(feature = "auth-user")]
            Self::User(refresh) => refresh.fmt(formatter),
            Self::External(_) => formatter.write_str("External"),
        }
    }
}

/// What kind of principal the tokens belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// The service account of the metadata server.
    Metadata,
    /// A service-account key.
    ServiceAccount,
    /// A person's Google sign in (refresh token).
    User,
    /// Access tokens from the caller (an app's sign in, `GOOGLE_OAUTH_ACCESS_TOKEN`).
    External,
}

impl CredentialKind {
    /// Whether calls must name the project they bill quota to
    /// (`x-goog-user-project`): a person's tokens belong to an OAuth client in
    /// another project, whose APIs and quota must not be used.
    pub fn bills_quota_to_caller(self) -> bool {
        matches!(self, Self::User | Self::External)
    }
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
                    #[cfg(feature = "auth-user")]
                    Source::User(refresh) => refresh.fetch(&self.grant).await,
                    Source::External(_) => Err(ProviderError::auth(
                        "tokens handed over by the caller are access tokens only: ID tokens need a service account",
                    )),
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
    kind: CredentialKind,
    access: Arc<dyn TokenSource>,
    identity: Mutex<HashMap<String, SharedTokenSource>>,
}

impl fmt::Debug for GcpAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("GcpAuth").field("principal", &self.inner.principal).finish_non_exhaustive()
    }
}

impl GcpAuth {
    /// Chooses the source like Google's client libraries: the credentials file
    /// named by `GOOGLE_APPLICATION_CREDENTIALS`, else the application default
    /// credentials file of `gcloud auth application-default login`, else the
    /// metadata server at `GCE_METADATA_HOST` (default `metadata.google.internal`).
    /// Nothing is fetched yet.
    ///
    /// @throws `ProviderError` (fatal) when a credentials file cannot be read or parsed.
    pub fn from_env() -> Result<Self, ProviderError> {
        if let Some(auth) = Self::from_credentials_files()? {
            return Ok(auth);
        }
        let metadata_url = match std::env::var("GCE_METADATA_HOST") {
            Ok(host) if !host.is_empty() => format!("http://{host}"),
            _ => DEFAULT_METADATA_URL.to_owned(),
        };
        GcpAuthBuilder::metadata_server(metadata_url).build()
    }

    #[cfg(feature = "auth-user")]
    /// The credentials of a command run on a person's machine: an access token
    /// in [`ACCESS_TOKEN_ENV`], else the file named by
    /// `GOOGLE_APPLICATION_CREDENTIALS`, else the application default
    /// credentials file. `Ok(None)` when there are none (no metadata server is
    /// tried: outside Google Cloud it only times out).
    ///
    /// @throws `ProviderError` (fatal) when a credentials file cannot be read or parsed.
    pub fn from_local_env() -> Result<Option<Self>, ProviderError> {
        if let Some(token) = std::env::var(ACCESS_TOKEN_ENV).ok().filter(|token| !token.trim().is_empty()) {
            let source: Arc<dyn TokenSource> = Arc::new(StaticToken(SecretString::new(token.trim())));
            return GcpAuthBuilder::token_source(source, format!("access token from {ACCESS_TOKEN_ENV}"))
                .build()
                .map(Some);
        }
        Self::from_credentials_files()
    }

    fn from_credentials_files() -> Result<Option<Self>, ProviderError> {
        if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").filter(|path| !path.is_empty()) {
            let json = std::fs::read_to_string(&path).map_err(|error| {
                ProviderError::fatal(format!("cannot read GOOGLE_APPLICATION_CREDENTIALS {}: {error}", path.display()))
            })?;
            return GcpAuthBuilder::from_credentials_json(&json)?.build().map(Some);
        }
        Self::from_application_default_credentials()
    }

    /// The file of `gcloud auth application-default login`, when there is one.
    #[cfg(feature = "auth-user")]
    fn from_application_default_credentials() -> Result<Option<Self>, ProviderError> {
        let Some(path) = application_default_credentials_path().filter(|path| path.is_file()) else {
            return Ok(None);
        };
        let json = std::fs::read_to_string(&path)
            .map_err(|error| ProviderError::fatal(format!("cannot read {}: {error}", path.display())))?;
        GcpAuthBuilder::from_credentials_json(&json)?.build().map(Some)
    }

    /// Without feature `auth-user` only service-account keys are read.
    #[cfg(not(feature = "auth-user"))]
    fn from_application_default_credentials() -> Result<Option<Self>, ProviderError> {
        Ok(None)
    }

    /// What kind of principal the tokens belong to.
    pub fn kind(&self) -> CredentialKind {
        self.inner.kind
    }

    /// Who the tokens belong to, safe to log: the key's `client_email`, or `metadata-server`.
    pub fn principal(&self) -> &str {
        &self.inner.principal
    }

    /// Access tokens (scope `cloud-platform` unless the builder set others).
    pub fn access_tokens(&self) -> Arc<dyn TokenSource> {
        self.inner.access.clone()
    }

    /// The project a person's credentials file names for quota, if any.
    pub fn quota_project(&self) -> Option<&str> {
        match self.inner.source.as_ref() {
            #[cfg(feature = "auth-user")]
            Source::User(refresh) => refresh.quota_project_id(),
            _ => None,
        }
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

enum BuilderKind {
    Metadata(String),
    ServiceAccount(Box<ServiceAccountKey>),
    #[cfg(feature = "auth-user")]
    User(Box<UserCredentials>),
    External {
        source: Arc<dyn TokenSource>,
        principal: String,
    },
}

impl fmt::Debug for BuilderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(url) => formatter.debug_tuple("Metadata").field(url).finish(),
            Self::ServiceAccount(key) => formatter.debug_tuple("ServiceAccount").field(key).finish(),
            #[cfg(feature = "auth-user")]
            Self::User(credentials) => formatter.debug_tuple("User").field(credentials).finish(),
            Self::External { principal, .. } => formatter.debug_tuple("External").field(principal).finish(),
        }
    }
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

    /// Tokens from a person's Google sign in (a refresh token).
    #[cfg(feature = "auth-user")]
    pub fn user(credentials: UserCredentials) -> Self {
        Self { kind: BuilderKind::User(Box::new(credentials)), scopes: Vec::new(), token_endpoint: None }
    }

    /// Access tokens from the caller: an app that signed the person in and
    /// refreshes the tokens itself, or a fixed token
    /// ([`StaticToken`](graphrt_provider_api::http::StaticToken)).
    /// Scopes are the caller's business (`cloud-platform` for Cloud Run).
    ///
    /// @param principal - who the tokens belong to, for messages (safe to log)
    pub fn token_source(source: Arc<dyn TokenSource>, principal: impl Into<String>) -> Self {
        Self {
            kind: BuilderKind::External { source, principal: principal.into() },
            scopes: Vec::new(),
            token_endpoint: None,
        }
    }

    /// The source a credentials file describes: `service_account` or `authorized_user`.
    ///
    /// @throws `ProviderError` (fatal) on any other type, with what to use instead.
    pub fn from_credentials_json(json: &str) -> Result<Self, ProviderError> {
        let file: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| ProviderError::fatal(format!("invalid credentials JSON: {error}")))?;
        match file.get("type").and_then(serde_json::Value::as_str) {
            Some("service_account") => Ok(Self::service_account(ServiceAccountKey::from_json(json)?)),
            #[cfg(feature = "auth-user")]
            Some("authorized_user") => Ok(Self::user(UserCredentials::from_value(&file)?)),
            Some(other) => Err(ProviderError::fatal(format!(
                "credential type `{other}` is not supported: use a service-account key, a Google sign in \
                 (gcloud auth application-default login) or an access token in {ACCESS_TOKEN_ENV}"
            ))),
            None => Err(ProviderError::fatal("credentials file has no `type`")),
        }
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
        let (source, principal, kind, scopes) = match self.kind {
            BuilderKind::Metadata(base_url) => {
                let http = build_client(ClientOptions { bypass_proxy: true, ..options })?;
                let server = MetadataServer::new(base_url, http);
                let principal = format!("metadata-server ({})", server.base_url());
                // The metadata server already scopes tokens to `cloud-platform`;
                // only custom scopes are passed on.
                (Source::Metadata(server), principal, CredentialKind::Metadata, self.scopes)
            }
            BuilderKind::ServiceAccount(key) => {
                let http = build_client(options)?;
                let principal = key.client_email().to_owned();
                let endpoint = self.token_endpoint.unwrap_or_else(|| key.token_uri().to_owned());
                let scopes = if self.scopes.is_empty() { vec![CLOUD_PLATFORM_SCOPE.to_owned()] } else { self.scopes };
                let exchange = ServiceAccountExchange::new(Arc::from(key), http, endpoint, Arc::new(AtomicI64::new(0)));
                (Source::ServiceAccount(exchange), principal, CredentialKind::ServiceAccount, scopes)
            }
            #[cfg(feature = "auth-user")]
            BuilderKind::User(credentials) => {
                let http = build_client(options)?;
                let principal = credentials.principal();
                let credentials = match self.token_endpoint {
                    Some(endpoint) => credentials.with_token_uri(endpoint),
                    None => *credentials,
                };
                // A refresh token carries the scopes of its sign in.
                (Source::User(UserRefresh::new(credentials, http)), principal, CredentialKind::User, Vec::new())
            }
            BuilderKind::External { source, principal } => {
                (Source::External(source), principal, CredentialKind::External, Vec::new())
            }
        };
        let source = Arc::new(source);
        let access: Arc<dyn TokenSource> = match source.as_ref() {
            Source::External(tokens) => tokens.clone(),
            _ => Arc::new(TokenCache::new(GrantFetcher { source: source.clone(), grant: Grant::Access { scopes } })),
        };
        let inner = AuthInner { source, principal, kind, access, identity: Mutex::new(HashMap::new()) };
        Ok(GcpAuth { inner: Arc::new(inner) })
    }
}

/// Where `gcloud auth application-default login` writes the credentials.
#[cfg(feature = "auth-user")]
fn application_default_credentials_path() -> Option<std::path::PathBuf> {
    const FILE: &str = "application_default_credentials.json";
    if let Some(config) = std::env::var_os("CLOUDSDK_CONFIG").filter(|dir| !dir.is_empty()) {
        return Some(std::path::PathBuf::from(config).join(FILE));
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|dir| std::path::PathBuf::from(dir).join("gcloud").join(FILE));
    }
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config").join("gcloud").join(FILE))
}
