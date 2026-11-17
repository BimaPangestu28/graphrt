//! Simple credential sources: async closures and environment variables.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;

use super::{Credential, CredentialSource};
use crate::error::LlmError;

type CredentialFn = dyn Fn(String) -> BoxFuture<'static, Result<Credential, LlmError>> + Send + Sync;
type InvalidateFn = dyn Fn(&str) + Send + Sync;

/// A [`CredentialSource`] backed by an async closure, e.g. a keychain lookup.
///
/// ```
/// use graphrt_llm::credentials::{Credential, FnCredentials};
/// let source = FnCredentials::new(|provider: String| async move {
///     // Read the user's own key for `provider` from secure storage here.
///     Ok(Credential::new(format!("key-for-{provider}")))
/// });
/// # let _ = source;
/// ```
#[derive(Clone)]
pub struct FnCredentials {
    fetch: Arc<CredentialFn>,
    on_invalidate: Option<Arc<InvalidateFn>>,
}

impl FnCredentials {
    pub fn new<F, Fut>(fetch: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Credential, LlmError>> + Send + 'static,
    {
        Self { fetch: Arc::new(move |provider| Box::pin(fetch(provider))), on_invalidate: None }
    }

    /// Called when a provider rejects a credential (e.g. to clear a stored key).
    pub fn on_invalidate<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.on_invalidate = Some(Arc::new(f));
        self
    }
}

impl fmt::Debug for FnCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FnCredentials")
    }
}

impl CredentialSource for FnCredentials {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        (self.fetch)(provider.to_owned())
    }

    fn invalidate(&self, provider: &str) {
        if let Some(f) = &self.on_invalidate {
            f(provider);
        }
    }
}

/// Reads keys from environment variables at request time (servers), so rotating
/// the variable in a supervisor takes effect without rebuilding the registry.
#[derive(Clone, Debug, Default)]
pub struct EnvCredentials {
    vars: HashMap<String, String>,
}

impl EnvCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use environment variable `var` for `provider` (`*` matches any provider).
    pub fn var(mut self, provider: impl Into<String>, var: impl Into<String>) -> Self {
        self.vars.insert(provider.into(), var.into());
        self
    }
}

impl CredentialSource for EnvCredentials {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        let result = match self.vars.get(provider).or_else(|| self.vars.get("*")) {
            None => Err(LlmError::Config(format!("no environment variable configured for provider `{provider}`"))),
            Some(var) => match std::env::var(var) {
                Ok(value) if !value.is_empty() => Ok(Credential::new(value)),
                _ => Err(LlmError::Auth(format!("environment variable `{var}` is not set"))),
            },
        };
        Box::pin(futures::future::ready(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fn_credentials_pass_provider_name() {
        let source = FnCredentials::new(|p: String| async move { Ok(Credential::new(format!("k-{p}"))) });
        let c = source.credential("openai").await.unwrap();
        assert_eq!(c.secret, "k-openai");
    }

    #[tokio::test]
    async fn env_credentials_report_missing_variable_by_name() {
        let source = EnvCredentials::new().var("openai", "GRAPHRT_TEST_DEFINITELY_UNSET_VAR");
        let err = source.credential("openai").await.unwrap_err();
        assert!(err.to_string().contains("GRAPHRT_TEST_DEFINITELY_UNSET_VAR"));
        let err = source.credential("other").await.unwrap_err();
        assert!(matches!(err, LlmError::Config(_)));
    }
}
