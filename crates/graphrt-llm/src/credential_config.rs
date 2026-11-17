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
//!   "typesafe":  { "source": "token_broker", "url": "https://api.example.com/llm-token" },
//!   "server":    { "source": "env", "var": "OPENAI_API_KEY", "kind": "openai" }
//! }
//! ```
//!
//! | `source` | Credential comes from |
//! | --- | --- |
//! | `external` | The host, registered at run time in [`HostCredentials`] (keychain, user input...). Not available in providers files (no host). |
//! | `token_broker` | `POST url` returning a short-lived token (see [`crate::credentials::TokenBroker`]). |
//! | `env` | Environment variable `var`, read per request (servers). |
//!
//! `kind` (`openai`, `anthropic`, `gemini`, `bedrock` or `typesafe`) is inferred
//! only from those exact provider names; any other name must set `kind`, so a
//! credential is never sent to a vendor the entry was not written for. `typesafe`
//! entries configure the System One decision client, never a chat model.
//! `base_url` defaults to the vendor's public endpoint and must be `https://`
//! (plain http only for localhost). Point it at your proxy for the thin-proxy
//! pattern. `external` and `token_broker` sources are cached ([`CachedCredentials`]);
//! `env` sources are read on every request.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Google Gemini API (or Vertex AI with `auth_header: authorization`).
    Gemini,
    /// AWS Bedrock Converse API with bearer API keys (needs `region`).
    Bedrock,
    /// TypeSafe System One decision API (not a chat model).
    Typesafe,
}

impl ProviderKind {
    /// The kind implied by a well-known provider name.
    pub fn from_known_name(name: &str) -> Option<Self> {
        Some(match name {
            "openai" => ProviderKind::Openai,
            "anthropic" => ProviderKind::Anthropic,
            "gemini" => ProviderKind::Gemini,
            "bedrock" => ProviderKind::Bedrock,
            "typesafe" => ProviderKind::Typesafe,
            _ => return None,
        })
    }
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
    /// Bearer token asked from [`HostCredentials`] under the exact provider name
    /// `token_broker` (the `*` fallback is never used for app tokens).
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
    /// Extra headers on every model request (not sent when a credential moves the
    /// request to a different origin).
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    /// OpenAI-compatible: header carrying the key instead of `Authorization: Bearer`,
    /// e.g. `api-key` for Azure. Gemini: `authorization` sends a bearer token (Vertex AI).
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Bedrock: AWS region.
    #[serde(default)]
    pub region: Option<String>,
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
    /// Cached sources: refresh this long before expiry (default 60 000 ms).
    #[serde(default)]
    pub refresh_skew_ms: Option<u64>,
    /// Cached sources: fail a credential fetch after this long (default 15 000 ms).
    #[serde(default)]
    pub fetch_timeout_ms: Option<u64>,
}

impl ProviderCredentialConfig {
    /// The provider kind: explicit, or implied by a well-known name.
    pub fn kind_for(&self, name: &str) -> Result<ProviderKind, LlmError> {
        self.kind.or_else(|| ProviderKind::from_known_name(name)).ok_or_else(|| {
            LlmError::Config(format!(
                "credentials for `{name}`: set `kind` (openai, anthropic, gemini, bedrock or typesafe); \
                 it is only inferred for providers named exactly like a kind"
            ))
        })
    }

    /// Reject fields that the chosen source or kind would silently ignore.
    pub fn validate(&self, name: &str) -> Result<ProviderKind, LlmError> {
        let kind = self.kind_for(name)?;
        let bad = |field: &str, why: &str| {
            Err(LlmError::Config(format!("credentials for `{name}`: `{field}` is not used {why}")))
        };
        let broker_only = self.url.is_some()
            || !self.claims.is_empty()
            || !self.broker_headers.is_empty()
            || self.app_auth != AppAuth::None;
        match self.source {
            SourceKind::Env => {
                if broker_only {
                    return bad("url/claims/broker_headers/app_auth", "with `source: env`");
                }
                if self.refresh_skew_ms.is_some() || self.fetch_timeout_ms.is_some() {
                    return bad("refresh_skew_ms/fetch_timeout_ms", "with `source: env` (read on every request)");
                }
            }
            SourceKind::External => {
                if broker_only {
                    return bad("url/claims/broker_headers/app_auth", "with `source: external`");
                }
                if self.var.is_some() {
                    return bad("var", "with `source: external`");
                }
            }
            SourceKind::TokenBroker => {
                if self.var.is_some() {
                    return bad("var", "with `source: token_broker`");
                }
            }
        }
        if self.auth_header.is_some() && !matches!(kind, ProviderKind::Openai | ProviderKind::Gemini) {
            return bad("auth_header", "for this provider kind");
        }
        if self.region.is_some() && kind != ProviderKind::Bedrock {
            return bad("region", "for this provider kind");
        }
        if kind == ProviderKind::Bedrock && self.region.as_deref().is_none_or(str::is_empty) && self.base_url.is_none()
        {
            return Err(LlmError::Config(format!("credentials for `{name}`: bedrock needs `region` (or `base_url`)")));
        }
        if let Some(url) = &self.base_url {
            crate::credentials::require_secure_endpoint(url, &format!("credentials for `{name}`: `base_url`"))?;
        }
        Ok(kind)
    }

    /// Whether this entry needs a host app to supply credentials at run time.
    pub fn needs_host(&self) -> bool {
        self.source == SourceKind::External || self.app_auth == AppAuth::External
    }
}

/// Credential sources registered by the host at run time, per provider name
/// (`*` is the fallback for every provider). Cheap to clone; clones share state.
///
/// Registering, removing or clearing a source changes [`CredentialSource::generation`],
/// so credentials cached from the previous source are dropped (sign-out takes
/// effect on the next request).
#[derive(Clone, Default)]
pub struct HostCredentials {
    sources: Arc<RwLock<HashMap<String, Arc<dyn CredentialSource>>>>,
    generation: Arc<AtomicU64>,
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
        {
            let mut sources = self.sources.write().unwrap_or_else(PoisonError::into_inner);
            sources.insert(provider.into(), source);
        }
        self.clear_cache();
    }

    /// Remove the source registered for `provider`.
    pub fn remove(&self, provider: &str) {
        {
            let mut sources = self.sources.write().unwrap_or_else(PoisonError::into_inner);
            sources.remove(provider);
        }
        self.clear_cache();
    }

    /// Drop every credential cached from host sources (e.g. after the user signed
    /// out of the app without the sources changing).
    pub fn clear_cache(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn lookup(&self, provider: &str) -> Option<Arc<dyn CredentialSource>> {
        let sources = self.sources.read().unwrap_or_else(PoisonError::into_inner);
        sources.get(provider).or_else(|| sources.get("*")).cloned()
    }

    fn lookup_exact(&self, provider: &str) -> Option<Arc<dyn CredentialSource>> {
        let sources = self.sources.read().unwrap_or_else(PoisonError::into_inner);
        sources.get(provider).cloned()
    }

    /// A source answering only from the source registered under exactly `name`
    /// (no `*` fallback). Used for app session tokens sent to a token broker, so a
    /// user's own provider key registered under `*` never reaches the broker.
    pub fn exact(&self, name: impl Into<String>) -> Arc<dyn CredentialSource> {
        Arc::new(ExactHostSource { host: self.clone(), name: name.into() })
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

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

/// See [`HostCredentials::exact`].
#[derive(Debug)]
struct ExactHostSource {
    host: HostCredentials,
    name: String,
}

impl CredentialSource for ExactHostSource {
    fn credential<'a>(&'a self, _provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            match self.host.lookup_exact(&self.name) {
                Some(source) => source.credential(&self.name).await,
                None => Err(LlmError::Auth(format!(
                    "no credential provider registered under `{}` (app session for the token broker)",
                    self.name
                ))),
            }
        })
    }

    fn invalidate(&self, _provider: &str) {
        if let Some(source) = self.host.lookup_exact(&self.name) {
            source.invalidate(&self.name);
        }
    }

    fn generation(&self) -> u64 {
        self.host.generation()
    }
}

/// Build the credential source for one provider entry.
pub fn build_source(
    name: &str,
    config: &ProviderCredentialConfig,
    host: &HostCredentials,
) -> Result<Arc<dyn CredentialSource>, LlmError> {
    config.validate(name)?;
    let bad = |msg: &str| LlmError::Config(format!("credentials for `{name}`: {msg}"));
    let cached = |source: Arc<dyn CredentialSource>| -> Arc<dyn CredentialSource> {
        let mut cache = CachedCredentials::new(source);
        if let Some(skew) = config.refresh_skew_ms {
            cache = cache.refresh_skew_ms(skew);
        }
        if let Some(ms) = config.fetch_timeout_ms {
            cache = cache.fetch_timeout(std::time::Duration::from_millis(ms));
        }
        Arc::new(cache)
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
        broker = broker.auth(BrokerAuth::Bearer(host.exact("token_broker")));
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

#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn static_headers(config: &ProviderCredentialConfig) -> Vec<(String, String)> {
    config.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Build a chat model for one provider entry. `typesafe` entries are not chat
/// models and are rejected here (see [`build_system_one`]).
pub fn build_model(
    name: &str,
    config: &ProviderCredentialConfig,
    host: &HostCredentials,
) -> Result<Arc<dyn ChatModel>, LlmError> {
    let kind = config.validate(name)?;
    let source = build_source(name, config, host)?;
    build_model_with_source(name, kind, config, source)
}

#[cfg_attr(not(feature = "http"), allow(unused_variables))]
fn build_model_with_source(
    name: &str,
    kind: ProviderKind,
    config: &ProviderCredentialConfig,
    source: Arc<dyn CredentialSource>,
) -> Result<Arc<dyn ChatModel>, LlmError> {
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
            cfg.headers = static_headers(config);
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
            cfg.headers = static_headers(config);
            Ok(Arc::new(AnthropicModel::new(cfg)?))
        }
        #[cfg(feature = "gemini")]
        ProviderKind::Gemini => {
            use crate::gemini::{GeminiAuth, GeminiConfig, GeminiModel};
            let mut cfg =
                GeminiConfig { provider_name: name.to_owned(), ..GeminiConfig::default() }.with_credentials(source);
            if let Some(url) = &config.base_url {
                cfg.base_url.clone_from(url);
            }
            match config.auth_header.as_deref() {
                None => {}
                Some(h) if h.eq_ignore_ascii_case("authorization") => cfg.auth = GeminiAuth::Bearer,
                Some(h) if h.eq_ignore_ascii_case("x-goog-api-key") => cfg.auth = GeminiAuth::ApiKey,
                Some(h) => {
                    return Err(LlmError::Config(format!(
                        "credentials for `{name}`: gemini `auth_header` must be `x-goog-api-key` or \
                         `authorization`, got `{h}`"
                    )));
                }
            }
            cfg.headers = static_headers(config);
            Ok(Arc::new(GeminiModel::new(cfg)?))
        }
        #[cfg(feature = "bedrock")]
        ProviderKind::Bedrock => {
            use crate::bedrock::{BedrockConfig, BedrockModel};
            let region = config.region.clone().unwrap_or_else(|| "us-east-1".to_owned());
            let mut cfg = BedrockConfig {
                region,
                endpoint: config.base_url.clone(),
                auth: None,
                credentials: Some(source),
                provider_name: name.to_owned(),
                headers: static_headers(config),
                connect_timeout: std::time::Duration::from_secs(10),
            };
            cfg.provider_name = name.to_owned();
            Ok(Arc::new(BedrockModel::new(cfg)?))
        }
        ProviderKind::Typesafe => Err(LlmError::Config(format!(
            "credentials for `{name}`: a `typesafe` entry configures the System One decision client, \
             not a chat model"
        ))),
        #[allow(unreachable_patterns)]
        other => {
            drop(source);
            Err(LlmError::Config(format!("provider `{name}`: kind {other:?} is not enabled in this build")))
        }
    }
}

/// Build the TypeSafe System One client for a `typesafe`-kind entry.
#[cfg(feature = "typesafe")]
pub fn build_system_one(
    name: &str,
    config: &ProviderCredentialConfig,
    host: &HostCredentials,
) -> Result<crate::typesafe::SystemOneClient, LlmError> {
    let kind = config.validate(name)?;
    if kind != ProviderKind::Typesafe {
        return Err(LlmError::Config(format!("credentials for `{name}`: not a `typesafe` entry")));
    }
    let source = build_source(name, config, host)?;
    system_one_with_source(config, source)
}

#[cfg(feature = "typesafe")]
fn system_one_with_source(
    config: &ProviderCredentialConfig,
    source: Arc<dyn CredentialSource>,
) -> Result<crate::typesafe::SystemOneClient, LlmError> {
    let mut ts = crate::typesafe::SystemOneConfig { credentials: Some(source), ..Default::default() };
    if let Some(url) = &config.base_url {
        ts.base_url.clone_from(url);
    }
    ts.headers = static_headers(config);
    crate::typesafe::SystemOneClient::new(ts)
}

/// What [`apply`] built besides chat models.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Applied {
    /// The System One client from the `typesafe`-kind entry, if any.
    #[cfg(feature = "typesafe")]
    pub system_one: Option<crate::typesafe::SystemOneClient>,
}

/// Build every entry. Chat entries are added to `registry`, replacing providers
/// with the same name (so a credentialed `openai` entry overrides one built from
/// environment variables). A `typesafe`-kind entry is never registered as a chat
/// model; it is returned as [`Applied::system_one`], built from the same cached
/// source.
pub fn apply(
    registry: &mut ModelRegistry,
    entries: &IndexMap<String, ProviderCredentialConfig>,
    host: &HostCredentials,
) -> Result<Applied, LlmError> {
    #[allow(unused_mut)]
    let mut applied = Applied::default();
    for (name, config) in entries {
        let kind = config.validate(name)?;
        if kind == ProviderKind::Typesafe {
            #[cfg(feature = "typesafe")]
            {
                if applied.system_one.is_some() {
                    return Err(LlmError::Config("only one `typesafe` credentials entry is supported".into()));
                }
                let source = build_source(name, config, host)?;
                applied.system_one = Some(system_one_with_source(config, source)?);
                continue;
            }
            #[cfg(not(feature = "typesafe"))]
            return Err(LlmError::Config(format!(
                "credentials for `{name}`: the `typesafe` kind is not enabled in this build"
            )));
        }
        let source = build_source(name, config, host)?;
        let model = build_model_with_source(name, kind, config, source)?;
        registry.remove(name);
        registry.add(name.clone(), model);
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::StaticCredential;

    #[allow(dead_code)] // Unused when no provider feature is enabled.
    fn entries(v: serde_json::Value) -> IndexMap<String, ProviderCredentialConfig> {
        serde_json::from_value(v).unwrap()
    }

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

    #[tokio::test]
    async fn sign_out_takes_effect_through_the_cache() {
        let host = HostCredentials::new();
        host.set("openai", Arc::new(StaticCredential::new("user-a")));
        let cfg: ProviderCredentialConfig = serde_json::from_value(serde_json::json!({"source": "external"})).unwrap();
        let source = build_source("openai", &cfg, &host).unwrap();
        assert_eq!(source.credential("openai").await.unwrap().secret, "user-a");
        // User A signs out.
        host.remove("openai");
        assert!(source.credential("openai").await.is_err());
        // User B signs in.
        host.set("openai", Arc::new(StaticCredential::new("user-b")));
        assert_eq!(source.credential("openai").await.unwrap().secret, "user-b");
    }

    #[tokio::test]
    async fn broker_app_token_never_uses_the_wildcard_source() {
        let host = HostCredentials::new();
        host.set("*", Arc::new(StaticCredential::new("sk-users-own-key")));
        let app = host.exact("token_broker");
        assert!(matches!(app.credential("token_broker").await, Err(LlmError::Auth(_))));
        host.set("token_broker", Arc::new(StaticCredential::new("session")));
        assert_eq!(app.credential("token_broker").await.unwrap().secret, "session");
    }

    #[cfg(all(feature = "openai", feature = "typesafe"))]
    #[test]
    fn config_validation() {
        let mut registry = ModelRegistry::new();
        let applied = apply(
            &mut registry,
            &entries(serde_json::json!({
                "openai": {"source": "external"},
                "proxy": {"source": "external", "kind": "openai", "base_url": "https://proxy.example.com/v1"},
                "server": {"source": "env", "var": "OPENAI_API_KEY", "kind": "openai"},
                "typesafe": {"source": "env", "var": "TYPESAFE_API_KEY"}
            })),
            &HostCredentials::new(),
        )
        .unwrap();
        assert!(registry.get("proxy").is_ok());
        assert!(registry.get("server").is_ok());
        // TypeSafe is never a chat model.
        assert!(registry.get("typesafe").is_err());
        assert!(applied.system_one.is_some());

        let bad: Result<IndexMap<String, ProviderCredentialConfig>, _> =
            serde_json::from_value(serde_json::json!({"x": {"source": "external", "typo": 1}}));
        assert!(bad.is_err());
        let host = HostCredentials::new();
        let fails = |v: serde_json::Value| apply(&mut ModelRegistry::new(), &entries(v), &host).is_err();
        // Missing `var`, unknown name without `kind`, fields of another source / kind,
        // insecure base URL.
        assert!(fails(serde_json::json!({"x": {"source": "env", "kind": "openai"}})));
        assert!(fails(serde_json::json!({"mistral": {"source": "external"}})));
        assert!(fails(serde_json::json!({"openai": {"source": "token_broker", "url": "https://b", "var": "X"}})));
        assert!(fails(serde_json::json!({"openai": {"source": "env", "var": "X", "url": "https://b"}})));
        assert!(fails(serde_json::json!({"anthropic": {"source": "external", "auth_header": "x"}})));
        assert!(fails(serde_json::json!({"openai": {"source": "external", "base_url": "http://proxy.example.com"}})));
        assert!(fails(serde_json::json!({"openai": {"source": "token_broker", "url": "http://broker.example.com"}})));
        assert!(fails(serde_json::json!({"bedrock": {"source": "external"}})));
    }

    #[cfg(all(feature = "gemini", feature = "bedrock"))]
    #[test]
    fn gemini_and_bedrock_entries_build_their_own_clients() {
        let mut registry = ModelRegistry::new();
        apply(
            &mut registry,
            &entries(serde_json::json!({
                "gemini": {"source": "external"},
                "bedrock": {"source": "external", "region": "eu-west-1"}
            })),
            &HostCredentials::new(),
        )
        .unwrap();
        let gemini = format!("{:?}", registry.get("gemini").unwrap());
        let bedrock = format!("{:?}", registry.get("bedrock").unwrap());
        assert!(gemini.contains("Gemini"), "{gemini}");
        assert!(bedrock.contains("Bedrock"), "{bedrock}");
    }
}
