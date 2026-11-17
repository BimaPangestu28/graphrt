//! Host-supplied model credentials (Keychain / Keystore, token brokers).
//!
//! Apps must not ship provider API keys. Instead a provider is declared in the
//! runtime config with `"source": "external"` and the app registers a
//! [`CredentialProvider`] callback that returns the key (bring-your-own-key
//! from secure storage) or a short-lived token for a proxy.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_llm::LlmError;
use graphrt_llm::credentials::{Credential, CredentialSource};

use crate::GraphrtError;

/// A credential returned by the host.
#[derive(Clone, uniffi::Record)]
pub struct CredentialRecord {
    /// API key or bearer token for the provider (or the app's token for a proxy).
    pub secret: String,
    /// Absolute expiry in Unix milliseconds; `None` caches until a 401/403.
    pub expires_at_ms: Option<u64>,
    /// Extra request headers as a JSON object (`{"x-tenant": "acme"}`), or `None`.
    pub headers_json: Option<String>,
    /// Endpoint override for this credential (e.g. a regional proxy), or `None`.
    pub base_url: Option<String>,
}

impl std::fmt::Debug for CredentialRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialRecord")
            .field("secret", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// Supplies model credentials. Implemented in the host language.
///
/// `get_credential` may block (Keychain, IPC, a network call to your backend);
/// it is invoked on a background thread. Results are cached until
/// `expires_at_ms` (minus a refresh margin) or until the provider rejects the
/// credential, in which case `invalidate` is called and `get_credential` is
/// asked once more. The provider name `token_broker` is used for the app's own
/// session token when a token broker entry sets `"app_auth": "external"`.
#[uniffi::export(with_foreign)]
pub trait CredentialProvider: Send + Sync {
    /// Credential for `provider` (the provider name from the runtime config).
    fn get_credential(&self, provider: String) -> Result<CredentialRecord, GraphrtError>;
    /// The last credential for `provider` was rejected; drop any cached copy.
    fn invalidate(&self, provider: String);
}

/// Adapts a foreign [`CredentialProvider`] to [`CredentialSource`].
pub(crate) struct ForeignCredentials {
    callback: Arc<dyn CredentialProvider>,
}

impl ForeignCredentials {
    pub(crate) fn new(callback: Arc<dyn CredentialProvider>) -> Self {
        Self { callback }
    }
}

impl std::fmt::Debug for ForeignCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ForeignCredentials")
    }
}

fn call(callback: &Arc<dyn CredentialProvider>, provider: &str) -> Result<Credential, LlmError> {
    let outcome = catch_unwind(AssertUnwindSafe(|| callback.get_credential(provider.to_owned())));
    let record = match outcome {
        Ok(Ok(record)) => record,
        Ok(Err(e)) => return Err(LlmError::Auth(format!("credential provider failed for `{provider}`: {e}"))),
        Err(_) => return Err(LlmError::Auth(format!("credential provider panicked for `{provider}`"))),
    };
    to_credential(provider, record)
}

fn to_credential(provider: &str, record: CredentialRecord) -> Result<Credential, LlmError> {
    if record.secret.is_empty() {
        return Err(LlmError::Auth(format!("credential provider returned an empty secret for `{provider}`")));
    }
    let mut credential = Credential::new(record.secret);
    if let Some(exp) = record.expires_at_ms {
        credential = credential.expires_at(exp);
    }
    if let Some(raw) = record.headers_json.as_deref().filter(|s| !s.trim().is_empty()) {
        let headers: indexmap::IndexMap<String, String> = serde_json::from_str(raw).map_err(|e| {
            LlmError::Config(format!("credential headers for `{provider}` must be a JSON object of strings: {e}"))
        })?;
        for (k, v) in headers {
            credential = credential.header(k, v);
        }
    }
    if let Some(url) = record.base_url.filter(|u| !u.is_empty()) {
        if !graphrt_llm::credentials::is_secure_endpoint(&url) {
            return Err(LlmError::Config(format!(
                "credential base_url for `{provider}` must be https:// (http only for localhost / 10.0.2.2)"
            )));
        }
        credential = credential.base_url(url);
    }
    Ok(credential)
}

impl CredentialSource for ForeignCredentials {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        let callback = self.callback.clone();
        let provider = provider.to_owned();
        Box::pin(async move {
            // Foreign code may block (Keychain, IPC); keep it off the async workers.
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => handle
                    .spawn_blocking(move || call(&callback, &provider))
                    .await
                    .unwrap_or_else(|_| Err(LlmError::Auth("credential provider task failed".into()))),
                Err(_) => call(&callback, &provider),
            }
        })
    }

    fn invalidate(&self, provider: &str) {
        let callback = self.callback.clone();
        let provider = provider.to_owned();
        let run = move || {
            let _ = catch_unwind(AssertUnwindSafe(move || callback.invalidate(provider)));
        };
        // A Keychain delete may block: never on an async worker thread.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(run)),
            Err(_) => run(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_validated() {
        let ok = CredentialRecord {
            secret: "sk-very-secret".into(),
            expires_at_ms: Some(5),
            headers_json: Some(r#"{"x-a":"1"}"#.into()),
            base_url: Some("https://p.example.com/v1".into()),
        };
        let c = to_credential("p", ok.clone()).unwrap();
        assert_eq!(c.headers, vec![("x-a".to_owned(), "1".to_owned())]);
        assert!(to_credential("p", CredentialRecord { secret: String::new(), ..ok.clone() }).is_err());
        assert!(to_credential("p", CredentialRecord { headers_json: Some("[1]".into()), ..ok.clone() }).is_err());
        assert!(to_credential("p", CredentialRecord { base_url: Some("file:///x".into()), ..ok.clone() }).is_err());
        let plain_http = CredentialRecord { base_url: Some("http://proxy.example.com/v1".into()), ..ok.clone() };
        assert!(to_credential("p", plain_http).is_err());
        assert!(!format!("{ok:?}").contains("sk-very-secret"));
    }
}
