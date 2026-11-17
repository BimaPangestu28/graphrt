//! Declarative provider credentials: build providers whose keys come from a
//! [`CredentialSource`] instead of a static API key.
//!
//! Used by embedding hosts (mobile apps via `graphrt-ffi`) and by providers
//! files. One entry per provider name:
//!
//! ```json
//! {
//!   "openai":    { "source": "external" },
//!   "anthropic": { "source": "token_broker", "url": "https://api.example.com/llm-token",
//!                  "app_auth": "external" },
//!   "proxy":     { "source": "external", "kind": "openai",
//!                  "base_url": "https://llm-proxy.example.com/v1" },
//!   "server":    { "source": "env", "var": "OPENAI_API_KEY" }
//! }
//! ```
//!
//! | `source` | Credential comes from |
//! | --- | --- |
//! | `external` | The host, registered at run time in [`HostCredentials`] (keychain, user input...). |
//! | `token_broker` | `POST url` returning a short-lived token (see [`crate::credentials::TokenBroker`]). |
//! | `env` | Environment variable `var`, read per request (servers). |
//!
//! `kind` (`openai` or `anthropic`) defaults to `anthropic` for the provider
//! named `anthropic` and `openai` otherwise; `base_url` defaults to the
//! vendor's public endpoint. Point `base_url` at your proxy for the thin-proxy
//! pattern. Every source is wrapped in [`CachedCredentials`].

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::Deserialize;

use crate::credentials::{CachedCredentials, Credential, CredentialSource, EnvCredentials};
use crate::error::LlmError;
use crate::registry::ModelRegistry;
use crate::types::ChatModel;

/// API dialect of a credentialed provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderKind {
    /// OpenAI-compatible chat completions (OpenAI, Azure, Groq, OpenRouter, proxies...).
    Openai,
    /// Anthropic Messages API.
    Anthropic,
}

/// Where a provider's credential comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SourceKind {
    External,
    TokenBroker,
    Env,
}

/// How the app authenticates to a token broker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AppAuth {
    /// No `Authorization` header (static `broker_headers` only).
    #[default]
    None,
    /// Bearer token asked from [`HostCredentials`] with provider name `token_broker`.
    External,
}

/// One provider entry.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCredentialConfig {
    pub source: SourceKind,
    #[serde(default)]
    pub kind: Option<ProviderKind>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Extra headers on every model request.
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    /// Header carrying the key for OpenAI-compatible providers (default
    /// `Authorization: Bearer`), e.g. `api-key` for Azure.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// `env` source: variable name.
    #[serde(default)]
    pub var: Option<String>,
    /// `token_broker` source: broker URL.
    #[serde(default)]
    pub url: Option<String>,
    /// `token_broker` source: extra JSON fields sent to the broker.
    #[serde(default)]
    pub claims: serde_json::Map<String, serde_json::Value>,
    /// `token_broker` source: extra headers sent to the broker.
    #[serde(default)]
    pub broker_headers: IndexMap<String, String>,
    /// `token_broker` source: app authentication.
    #[serde(default)]
    pub app_auth: AppAuth,
    /// Refresh this long before expiry (default 60 000 ms).
    #[serde(default)]
    pub refresh_skew_ms: Option<u64>,
}

/// Credential sources registered by the host at run time, per provider name
/// (`*` is the fallback for every provider). Cheap to clone; clones share state.
#[derive(Clone, Default)]
pub struct HostCredentials {
    sources: Arc<RwLock<HashMap<String, Arc<dyn CredentialSource>>>>,
}

impl fmt::Debug for HostCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sources = self.sources.read().unwrap_or_else(PoisonError::into_inner);
        let mut names: Vec<&String> = sources.keys().collect();
        names.sort();
        f.debug_struct("HostCredentials").field("providers", &names).finish()
    }
}

impl HostCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `source` for `provider` (or `*` for every provider), replacing any previous one.
    pub fn set(&self, provider: impl Into<String>, source: Arc<dyn CredentialSource>) {
        let mut sources = self.sources.write().unwrap_or_else(PoisonError::into_inner);
        sources.insert(provider.into(), source);
    }

    /// Remove the source registered for `provider`.
    pub fn remove(&self, provider: &str) {
        let mut sources = self.sources.write().unwrap_or_else(PoisonError::into_inner);
        sources.remove(provider);
    }

    fn lookup(&self, provider: &str) -> Option<Arc<dyn CredentialSource>> {
        let sources = self.sources.read().unwrap_or_else(PoisonError::into_inner);
        sources.get(provider).or_else(|| sources.get("*")).cloned()
    }
}

impl CredentialSource for HostCredentials {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            match self.lookup(provider) {
                Some(source) => source.credential(provider).await,
                None => Err(LlmError::Auth(format!("no credential provider registered for `{provider}`"))),
            }
        })
    }

    fn invalidate(&self, provider: &str) {
        if let Some(source) = self.lookup(provider) {
            source.invalidate(provider);
        }
    }
}

/// Build the credential source for one provider entry.
pub fn build_source(
    name: &str,
    config: &ProviderCredentialConfig,
    host: &HostCredentials,
) -> Result<Arc<dyn CredentialSource>, LlmError> {
    let bad = |msg: &str| LlmError::Config(format!("credentials for `{name}`: {msg}"));
    let cached = |source: Arc<dyn CredentialSource>| -> Arc<dyn CredentialSource> {
        let cache = CachedCredentials::new(source);
        match config.refresh_skew_ms {
            Some(skew) => Arc::new(cache.refresh_skew_ms(skew)),
            None => Arc::new(cache),
        }
    };
    match config.source {
        SourceKind::External => Ok(cached(Arc::new(host.clone()))),
        SourceKind::Env => {
            let var = config.var.as_deref().filter(|v| !v.is_empty()).ok_or_else(|| bad("`env` needs `var`"))?;
            Ok(Arc::new(EnvCredentials::new().var("*", var)))
        }
        SourceKind::TokenBroker => build_broker(name, config, host, &bad).map(cached),
    }
}

#[cfg(feature = "http")]
fn build_broker(
    _name: &str,
    config: &ProviderCredentialConfig,
    host: &HostCredentials,
    bad: &dyn Fn(&str) -> LlmError,
) -> Result<Arc<dyn CredentialSource>, LlmError> {
    use crate::credentials::{BrokerAuth, TokenBroker, TokenBrokerConfig};
    let url = config.url.as_deref().filter(|u| !u.is_empty()).ok_or_else(|| bad("`token_broker` needs `url`"))?;
    let mut broker = TokenBrokerConfig::new(url);
    for (k, v) in &config.broker_headers {
        broker = broker.header(k, v);
    }
    for (k, v) in &config.claims {
        broker = broker.claim(k, v.clone());
    }
    if config.app_auth == AppAuth::External {
        broker = broker.auth(BrokerAuth::Bearer(Arc::new(host.clone())));
    }
    Ok(Arc::new(TokenBroker::new(broker)?))
}

#[cfg(not(feature = "http"))]
fn build_broker(
    _name: &str,
    _config: &ProviderCredentialConfig,
    _host: &HostCredentials,
    bad: &dyn Fn(&str) -> LlmError,
) -> Result<Arc<dyn CredentialSource>, LlmError> {
    Err(bad("`token_broker` requires the `http` feature"))
}

/// Build a chat model for one provider entry.
pub fn build_model(
    name: &str,
    config: &ProviderCredentialConfig,
    host: &HostCredentials,
) -> Result<Arc<dyn ChatModel>, LlmError> {
    let source = build_source(name, config, host)?;
    let kind = config.kind.unwrap_or(if name == "anthropic" { ProviderKind::Anthropic } else { ProviderKind::Openai });
    match kind {
        #[cfg(feature = "openai")]
        ProviderKind::Openai => {
            use crate::openai::{AuthStyle, OpenAiConfig, OpenAiModel};
            let mut cfg = OpenAiConfig::default().with_credentials(source).credential_name(name);
            if let Some(url) = &config.base_url {
                cfg.base_url.clone_from(url);
            }
            if let Some(header) = config.auth_header.as_deref().filter(|h| !h.eq_ignore_ascii_case("authorization")) {
                cfg.auth = AuthStyle::Header(header.to_owned());
            }
            cfg.headers = config.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            cfg.use_max_completion_tokens = cfg.base_url.contains("api.openai.com");
            Ok(Arc::new(OpenAiModel::new(cfg)?))
        }
        #[cfg(feature = "anthropic")]
        ProviderKind::Anthropic => {
            use crate::anthropic::{AnthropicConfig, AnthropicModel};
            let mut cfg = AnthropicConfig::default().with_credentials(source).credential_name(name);
            if let Some(url) = &config.base_url {
                cfg.base_url.clone_from(url);
            }
            cfg.headers = config.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            Ok(Arc::new(AnthropicModel::new(cfg)?))
        }
        #[allow(unreachable_patterns)]
        other => {
            drop(source);
            Err(LlmError::Config(format!("provider `{name}`: kind {other:?} is not enabled in this build")))
        }
    }
}

/// Build every entry and add it to `registry`, replacing providers with the same name
/// (so a credentialed `openai` entry overrides one built from environment variables).
pub fn apply(
    registry: &mut ModelRegistry,
    entries: &IndexMap<String, ProviderCredentialConfig>,
    host: &HostCredentials,
) -> Result<(), LlmError> {
    for (name, config) in entries {
        let model = build_model(name, config, host)?;
        registry.remove(name);
        registry.add(name.clone(), model);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::StaticCredential;

    #[tokio::test]
    async fn host_credentials_dispatch_with_fallback() {
        let host = HostCredentials::new();
        assert!(matches!(host.credential("openai").await, Err(LlmError::Auth(_))));
        host.set("*", Arc::new(StaticCredential::new("sk-fallback")));
        host.set("anthropic", Arc::new(StaticCredential::new("sk-ant-secret")));
        assert_eq!(host.credential("openai").await.unwrap().secret, "sk-fallback");
        assert_eq!(host.credential("anthropic").await.unwrap().secret, "sk-ant-secret");
        let text = format!("{host:?}");
        assert!(!text.contains("sk-fallback") && !text.contains("sk-ant-secret"));
    }

    #[cfg(feature = "openai")]
    #[test]
    fn config_validation() {
        let entries: IndexMap<String, ProviderCredentialConfig> = serde_json::from_value(serde_json::json!({
            "openai": {"source": "external"},
            "proxy": {"source": "external", "kind": "openai", "base_url": "https://proxy.example.com/v1"},
            "server": {"source": "env", "var": "OPENAI_API_KEY"}
        }))
        .unwrap();
        let mut registry = ModelRegistry::new();
        apply(&mut registry, &entries, &HostCredentials::new()).unwrap();
        assert!(registry.get("proxy").is_ok());
        assert!(registry.get("server").is_ok());

        let bad: Result<IndexMap<String, ProviderCredentialConfig>, _> =
            serde_json::from_value(serde_json::json!({"x": {"source": "external", "typo": 1}}));
        assert!(bad.is_err());
        let missing: IndexMap<String, ProviderCredentialConfig> =
            serde_json::from_value(serde_json::json!({"x": {"source": "env"}})).unwrap();
        assert!(apply(&mut ModelRegistry::new(), &missing, &HostCredentials::new()).is_err());
    }
}
