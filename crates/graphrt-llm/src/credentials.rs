//! Dynamic credentials for model providers.
//!
//! A static API key is fine on a server, but an app running on a user's device
//! must not ship a provider key inside its binary. Providers therefore accept a
//! [`CredentialSource`], asked for a credential right before each request:
//!
//! - bring-your-own-key: the host app reads the user's key from the platform
//!   keychain / keystore;
//! - token broker: a backend issues short-lived, per-user tokens;
//! - static keys for servers and tests ([`StaticCredential`]).
//!
//! Sources are consulted per request; caching and refresh are the source's job.

use std::fmt;

use futures::future::BoxFuture;

use crate::error::LlmError;

/// A credential to authenticate one request.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// The secret sent to the provider (API key or bearer token).
    pub secret: String,
    /// Absolute expiry in Unix milliseconds, when known.
    pub expires_at_ms: Option<u64>,
    /// Extra headers to attach (e.g. a tenant or session header issued with a token).
    pub headers: Vec<(String, String)>,
}

impl Credential {
    pub fn new(secret: impl Into<String>) -> Self {
        Self { secret: secret.into(), expires_at_ms: None, headers: Vec::new() }
    }

    pub fn expires_at(mut self, unix_ms: u64) -> Self {
        self.expires_at_ms = Some(unix_ms);
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("secret", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("headers", &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish()
    }
}

/// Supplies credentials on demand.
pub trait CredentialSource: Send + Sync + fmt::Debug {
    /// Credential for `provider` (the registry name, e.g. `openai`).
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>>;

    /// Called when the provider rejected the last credential (HTTP 401/403), so
    /// cached values can be dropped before one retry.
    fn invalidate(&self, provider: &str) {
        let _ = provider;
    }
}

/// A fixed secret.
#[derive(Clone)]
pub struct StaticCredential(Credential);

impl StaticCredential {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(Credential::new(secret))
    }
}

impl fmt::Debug for StaticCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticCredential(<redacted>)")
    }
}

impl CredentialSource for StaticCredential {
    fn credential<'a>(&'a self, _provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(futures::future::ready(Ok(self.0.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_secrets() {
        let c = Credential::new("sk-secret").header("x-tenant", "t-secret");
        let text = format!("{c:?} {:?}", StaticCredential::new("sk-secret"));
        assert!(!text.contains("sk-secret"));
        assert!(!text.contains("t-secret"));
    }
}
