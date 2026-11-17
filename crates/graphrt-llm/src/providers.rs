//! Config-driven model providers.
//!
//! A providers file declares any number of OpenAI-compatible, Anthropic,
//! Gemini or Bedrock endpoints without code:
//!
//! ```yaml
//! providers:
//!   - name: openrouter
//!     kind: openai                       # openai | anthropic
//!     base_url: https://openrouter.ai/api/v1
//!     api_key_env: OPENROUTER_API_KEY    # read from the environment, never stored in the file
//!     headers: { HTTP-Referer: "${SITE_URL}" }
//!     aliases: [or]                      # extra provider names
//!     models: { fast: meta-llama/llama-3.1-8b-instruct }   # model-name aliases
//!     defaults: { temperature: 0.2, max_tokens: 1024 }     # applied when a node sets none
//!   - name: azure
//!     kind: openai
//!     base_url: https://my.openai.azure.com/openai/deployments/gpt4o
//!     api_key_env: AZURE_OPENAI_KEY
//!     auth_header: api-key
//!     query: { api-version: "2024-06-01" }
//!   - name: gemini
//!     kind: gemini
//!     api_key_env: GEMINI_API_KEY
//!   - name: vertex
//!     kind: gemini
//!     base_url: https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google
//!     api_key_env: VERTEX_ACCESS_TOKEN
//!     auth: bearer                       # gemini: api-key (default) | bearer
//!   - name: bedrock
//!     kind: bedrock
//!     region: us-east-1
//!     access_key_id_env: AWS_ACCESS_KEY_ID          # SigV4 ...
//!     secret_access_key_env: AWS_SECRET_ACCESS_KEY
//!     session_token_env: AWS_SESSION_TOKEN          # optional
//!     # ... or a Bedrock API key: api_key_env: AWS_BEARER_TOKEN_BEDROCK
//! ```
//!
//! `${VAR}` in `base_url`, `headers` and `query` values is replaced with the
//! environment variable `VAR`. Missing variables are errors that name the
//! variable but never print values.

#[cfg(feature = "http")]
use std::sync::Arc;

#[cfg(feature = "http")]
use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::Deserialize;

use crate::error::LlmError;
use crate::registry::ModelRegistry;
#[cfg(feature = "http")]
use crate::types::{ChatModel, ChatRequest, ChatStream};

/// Provider protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ProviderKind {
    /// OpenAI-compatible chat completions.
    #[serde(alias = "openai-compatible", alias = "openai_compatible")]
    Openai,
    /// Anthropic Messages API.
    Anthropic,
    /// Google Gemini (Gemini API or Vertex AI).
    Gemini,
    /// AWS Bedrock Converse API.
    Bedrock,
}

/// Default request parameters applied when the node does not set them.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultParams {
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Vec<String>,
}

/// One provider entry.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    /// Endpoint base URL (provider default when omitted).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Environment variable holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// When true, a missing `api_key_env` variable is not an error (e.g. local servers).
    #[serde(default)]
    pub api_key_optional: bool,
    /// OpenAI-compatible only: send the key in this header instead of `Authorization: Bearer`.
    #[serde(default)]
    pub auth_header: Option<String>,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    /// OpenAI-compatible only: query parameters added to every request.
    #[serde(default)]
    pub query: IndexMap<String, String>,
    /// Additional provider names resolving to this provider.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Model-name aliases: `alias -> real model id`.
    #[serde(default)]
    pub models: IndexMap<String, String>,
    #[serde(default)]
    pub defaults: DefaultParams,
    /// Gemini only: how the key is sent, `api-key` (default) or `bearer`.
    #[serde(default)]
    pub auth: Option<String>,
    /// Bedrock only: AWS region (`${VAR}` allowed).
    #[serde(default)]
    pub region: Option<String>,
    /// Bedrock only: environment variable holding the access key id (SigV4).
    #[serde(default)]
    pub access_key_id_env: Option<String>,
    /// Bedrock only: environment variable holding the secret access key (SigV4).
    #[serde(default)]
    pub secret_access_key_env: Option<String>,
    /// Bedrock only: environment variable holding a session token (optional).
    #[serde(default)]
    pub session_token_env: Option<String>,
}

/// A providers file: either `{providers: [...]}` or a bare list.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(from = "ProvidersRepr")]
pub struct ProvidersConfig {
    pub providers: Vec<ProviderConfig>,
    /// Providers authenticated by a dynamic credential source (token broker, env var
    /// read per request, or host-supplied), keyed by provider name. Entries replace
    /// same-named providers. See `docs/credentials.md`.
    pub credentials: IndexMap<String, crate::credential_config::ProviderCredentialConfig>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProvidersRepr {
    Wrapped {
        #[serde(default)]
        providers: Vec<ProviderConfig>,
        #[serde(default)]
        credentials: IndexMap<String, crate::credential_config::ProviderCredentialConfig>,
    },
    List(Vec<ProviderConfig>),
}

impl From<ProvidersRepr> for ProvidersConfig {
    fn from(repr: ProvidersRepr) -> Self {
        match repr {
            ProvidersRepr::Wrapped { providers, credentials } => Self { providers, credentials },
            ProvidersRepr::List(providers) => Self { providers, credentials: IndexMap::new() },
        }
    }
}

impl ProvidersConfig {
    /// Decode from a JSON value (hosts decode YAML into JSON first).
    pub fn from_value(value: serde_json::Value) -> Result<Self, LlmError> {
        // Decode through a strict wrapper first so typos produce a precise message
        // instead of the generic "did not match any variant" of the untagged enum.
        let mut credentials = IndexMap::new();
        let entries = match value {
            serde_json::Value::Object(mut map) => {
                let providers = map.remove("providers").unwrap_or(serde_json::Value::Array(Vec::new()));
                if let Some(raw) = map.remove("credentials") {
                    credentials = serde_json::from_value(raw)
                        .map_err(|e| LlmError::Config(format!("providers config `credentials`: {e}")))?;
                }
                if let Some(key) = map.keys().next() {
                    return Err(LlmError::Config(format!("unknown key `{key}` in providers config")));
                }
                providers
            }
            other => other,
        };
        let serde_json::Value::Array(items) = entries else {
            return Err(LlmError::Config("providers config must be a list of providers".into()));
        };
        let mut providers = Vec::with_capacity(items.len());
        for (i, item) in items.into_iter().enumerate() {
            let label = item.get("name").and_then(serde_json::Value::as_str).map(str::to_owned);
            let cfg: ProviderConfig = serde_json::from_value(item).map_err(|e| {
                LlmError::Config(format!("provider #{} ({}): {e}", i + 1, label.as_deref().unwrap_or("unnamed")))
            })?;
            providers.push(cfg);
        }
        Ok(Self { providers, credentials })
    }

    /// Decode from JSON text.
    pub fn from_json(text: &str) -> Result<Self, LlmError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| LlmError::Config(format!("invalid providers JSON: {e}")))?;
        Self::from_value(value)
    }
}

impl ModelRegistry {
    /// Build a registry from a providers config, reading secrets and `${VAR}`
    /// references from the process environment.
    #[cfg(feature = "http")]
    pub fn from_config(config: &ProvidersConfig) -> Result<Self, LlmError> {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        Self::from_config_with_vars(config, &vars)
    }

    /// Like [`ModelRegistry::from_config`] with explicit variables.
    ///
    /// A `typesafe` entry in the `credentials` section is not a chat model; use
    /// [`ProvidersConfig::load`] to get its System One client as well.
    #[cfg(feature = "http")]
    pub fn from_config_with_vars(config: &ProvidersConfig, vars: &[(String, String)]) -> Result<Self, LlmError> {
        config.load(vars).map(|loaded| loaded.models)
    }
}

/// Everything a providers file configures.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct LoadedProviders {
    pub models: ModelRegistry,
    /// The TypeSafe System One client from a `typesafe` credentials entry.
    #[cfg(feature = "typesafe")]
    pub system_one: Option<crate::typesafe::SystemOneClient>,
}

impl ProvidersConfig {
    /// Build the chat models (and the System One client of a `typesafe`
    /// credentials entry), reading secrets and `${VAR}` references from `vars`.
    ///
    /// Providers files have no host app, so `credentials` entries cannot use
    /// `source: external` or `app_auth: external`, and a credentials entry may not
    /// share its name with a `providers` entry (it would silently replace its
    /// aliases, model map and defaults).
    #[cfg(feature = "http")]
    pub fn load(&self, vars: &[(String, String)]) -> Result<LoadedProviders, LlmError> {
        let config = self;
        for (name, entry) in &config.credentials {
            if entry.needs_host() {
                return Err(LlmError::Config(format!(
                    "credentials for `{name}`: `source: external` / `app_auth: external` need a host app that \
                     registers credentials at run time (e.g. graphrt-ffi); in a providers file use `env` or \
                     `token_broker`"
                )));
            }
            if config.providers.iter().any(|p| &p.name == name || p.aliases.iter().any(|a| a == name)) {
                return Err(LlmError::Config(format!(
                    "credentials entry `{name}` has the same name as a `providers` entry; rename one of them"
                )));
            }
        }
        // An alias must not shadow a provider of the same file, and must name exactly
        // one provider: either mistake would silently route requests elsewhere.
        let names: std::collections::HashSet<&str> = config.providers.iter().map(|p| p.name.as_str()).collect();
        let mut aliases: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for provider in &config.providers {
            for alias in &provider.aliases {
                if names.contains(alias.as_str()) {
                    return Err(LlmError::Config(format!(
                        "provider `{}`: alias `{alias}` is also the name of a provider",
                        provider.name
                    )));
                }
                if let Some(other) = aliases.insert(alias.as_str(), provider.name.as_str()) {
                    return Err(LlmError::Config(format!(
                        "alias `{alias}` is declared by both `{other}` and `{}`",
                        provider.name
                    )));
                }
            }
        }
        let mut registry = ModelRegistry::new();
        for provider in &config.providers {
            let model = build_provider(provider, vars)?;
            registry.try_add(provider.name.clone(), Arc::clone(&model))?;
            for alias in &provider.aliases {
                registry.alias(alias.clone(), provider.name.clone());
            }
        }
        let host = crate::credential_config::HostCredentials::new();
        #[allow(unused_variables)]
        let applied = crate::credential_config::apply(&mut registry, &config.credentials, &host)?;
        Ok(LoadedProviders {
            models: registry,
            #[cfg(feature = "typesafe")]
            system_one: applied.system_one,
        })
    }
}

impl ModelRegistry {
    /// Add every provider and alias of `other`. `other` wins every conflict: a
    /// same-named provider is replaced, and an alias of `other` shadows a provider
    /// of `self` with that name (so a providers file can redirect `openai` to a
    /// local server). Used to layer a providers file over [`ModelRegistry::from_env`].
    pub fn merge(&mut self, other: ModelRegistry) -> &mut Self {
        let (providers, aliases) = other.into_parts();
        for (name, model) in providers {
            self.remove(&name);
            self.add(name, model);
        }
        for (alias, target) in aliases {
            self.remove(&alias);
            self.alias(alias, target);
        }
        self
    }
}

#[cfg(feature = "http")]
fn build_provider(p: &ProviderConfig, vars: &[(String, String)]) -> Result<Arc<dyn ChatModel>, LlmError> {
    let ctx = |what: &str| format!("provider `{}`: {what}", p.name);
    if p.name.trim().is_empty() {
        return Err(LlmError::Config("provider `name` must not be empty".into()));
    }
    let api_key = match &p.api_key_env {
        Some(var) => match lookup(vars, var) {
            Some(v) => Some(v),
            None if p.api_key_optional => None,
            None => return Err(LlmError::Config(ctx(&format!("environment variable `{var}` is not set")))),
        },
        None => None,
    };
    let base_url = p.base_url.as_deref().map(|u| interpolate(u, vars)).transpose().map_err(|v| missing(&ctx, &v))?;
    let mut headers = Vec::with_capacity(p.headers.len());
    for (k, v) in &p.headers {
        headers.push((k.clone(), interpolate(v, vars).map_err(|var| missing(&ctx, &var))?));
    }

    let protocol_only_fields = [
        (p.auth.is_some() && p.kind != ProviderKind::Gemini, "auth"),
        (p.region.is_some() && p.kind != ProviderKind::Bedrock, "region"),
        (p.access_key_id_env.is_some() && p.kind != ProviderKind::Bedrock, "access_key_id_env"),
        (p.secret_access_key_env.is_some() && p.kind != ProviderKind::Bedrock, "secret_access_key_env"),
        (p.session_token_env.is_some() && p.kind != ProviderKind::Bedrock, "session_token_env"),
    ];
    if let Some((_, field)) = protocol_only_fields.iter().find(|(bad, _)| *bad) {
        return Err(LlmError::Config(ctx(&format!("`{field}` is not supported for this provider kind"))));
    }
    let inner: Arc<dyn ChatModel> = match p.kind {
        ProviderKind::Openai => openai_provider(p, base_url, api_key, headers, vars)?,
        ProviderKind::Anthropic => anthropic_provider(p, base_url, api_key, headers)?,
        ProviderKind::Gemini => gemini_provider(p, base_url, api_key, headers)?,
        ProviderKind::Bedrock => bedrock_provider(p, base_url, api_key, headers, vars)?,
    };
    if p.models.is_empty() && p.defaults == DefaultParams::default() {
        return Ok(inner);
    }
    Ok(Arc::new(Configured { inner, models: p.models.clone(), defaults: p.defaults.clone() }))
}

#[cfg(all(feature = "http", feature = "openai"))]
fn openai_provider(
    p: &ProviderConfig,
    base_url: Option<String>,
    api_key: Option<String>,
    headers: Vec<(String, String)>,
    vars: &[(String, String)],
) -> Result<Arc<dyn ChatModel>, LlmError> {
    use crate::openai::{AuthStyle, OpenAiConfig, OpenAiModel};
    let mut cfg = OpenAiConfig::new(base_url.unwrap_or_else(|| OpenAiConfig::default().base_url), api_key);
    cfg.use_max_completion_tokens = cfg.base_url.contains("api.openai.com");
    if let Some(header) = &p.auth_header {
        cfg.auth = AuthStyle::Header(header.clone());
    }
    cfg.headers = headers;
    for (k, v) in &p.query {
        let value =
            interpolate(v, vars).map_err(|var| missing(&|w: &str| format!("provider `{}`: {w}", p.name), &var))?;
        cfg.query.push((k.clone(), value));
    }
    Ok(Arc::new(OpenAiModel::new(cfg)?))
}

#[cfg(all(feature = "http", not(feature = "openai")))]
fn openai_provider(
    p: &ProviderConfig,
    _base_url: Option<String>,
    _api_key: Option<String>,
    _headers: Vec<(String, String)>,
    _vars: &[(String, String)],
) -> Result<Arc<dyn ChatModel>, LlmError> {
    Err(LlmError::Config(format!("provider `{}`: the `openai` feature is disabled in this build", p.name)))
}

#[cfg(all(feature = "http", feature = "anthropic"))]
fn anthropic_provider(
    p: &ProviderConfig,
    base_url: Option<String>,
    api_key: Option<String>,
    headers: Vec<(String, String)>,
) -> Result<Arc<dyn ChatModel>, LlmError> {
    use crate::anthropic::{AnthropicConfig, AnthropicModel};
    if !p.query.is_empty() || p.auth_header.is_some() {
        return Err(LlmError::Config(format!(
            "provider `{}`: `query` and `auth_header` are only supported for kind `openai`",
            p.name
        )));
    }
    let mut cfg = AnthropicConfig { api_key, ..AnthropicConfig::default() };
    if let Some(base) = base_url {
        cfg.base_url = base;
    }
    cfg.headers = headers;
    Ok(Arc::new(AnthropicModel::new(cfg)?))
}

#[cfg(all(feature = "http", not(feature = "anthropic")))]
fn anthropic_provider(
    p: &ProviderConfig,
    _base_url: Option<String>,
    _api_key: Option<String>,
    _headers: Vec<(String, String)>,
) -> Result<Arc<dyn ChatModel>, LlmError> {
    Err(LlmError::Config(format!("provider `{}`: the `anthropic` feature is disabled in this build", p.name)))
}

#[cfg(all(feature = "http", feature = "gemini"))]
fn gemini_provider(
    p: &ProviderConfig,
    base_url: Option<String>,
    api_key: Option<String>,
    headers: Vec<(String, String)>,
) -> Result<Arc<dyn ChatModel>, LlmError> {
    use crate::gemini::{GeminiAuth, GeminiConfig, GeminiModel};
    if !p.query.is_empty() || p.auth_header.is_some() {
        return Err(LlmError::Config(format!(
            "provider `{}`: `query` and `auth_header` are only supported for kind `openai`",
            p.name
        )));
    }
    let auth = match p.auth.as_deref() {
        None | Some("api-key") => GeminiAuth::ApiKey,
        Some("bearer") => GeminiAuth::Bearer,
        Some(other) => {
            return Err(LlmError::Config(format!(
                "provider `{}`: unknown auth `{other}` (expected `api-key` or `bearer`)",
                p.name
            )));
        }
    };
    let mut cfg = GeminiConfig { api_key, auth, headers, provider_name: p.name.clone(), ..GeminiConfig::default() };
    if let Some(base) = base_url {
        cfg.base_url = base;
    }
    Ok(Arc::new(GeminiModel::new(cfg)?))
}

#[cfg(all(feature = "http", not(feature = "gemini")))]
fn gemini_provider(
    p: &ProviderConfig,
    _base_url: Option<String>,
    _api_key: Option<String>,
    _headers: Vec<(String, String)>,
) -> Result<Arc<dyn ChatModel>, LlmError> {
    Err(LlmError::Config(format!("provider `{}`: the `gemini` feature is disabled in this build", p.name)))
}

#[cfg(all(feature = "http", feature = "bedrock"))]
fn bedrock_provider(
    p: &ProviderConfig,
    base_url: Option<String>,
    api_key: Option<String>,
    headers: Vec<(String, String)>,
    vars: &[(String, String)],
) -> Result<Arc<dyn ChatModel>, LlmError> {
    use crate::bedrock::{AwsCredentials, BedrockAuth, BedrockConfig, BedrockModel};
    let ctx = |w: &str| format!("provider `{}`: {w}", p.name);
    if !p.query.is_empty() || p.auth_header.is_some() {
        return Err(LlmError::Config(ctx("`query` and `auth_header` are only supported for kind `openai`")));
    }
    let region = match &p.region {
        Some(r) => interpolate(r, vars).map_err(|v| missing(&ctx, &v))?,
        None => lookup(vars, "AWS_REGION")
            .or_else(|| lookup(vars, "AWS_DEFAULT_REGION"))
            .ok_or_else(|| LlmError::Config(ctx("`region` is required (or set AWS_REGION)")))?,
    };
    let required = |var: &Option<String>, field: &str| -> Result<Option<String>, LlmError> {
        match var {
            Some(name) => lookup(vars, name)
                .map(Some)
                .ok_or_else(|| LlmError::Config(ctx(&format!("environment variable `{name}` ({field}) is not set")))),
            None => Ok(None),
        }
    };
    let access = required(&p.access_key_id_env, "access_key_id_env")?;
    let secret = required(&p.secret_access_key_env, "secret_access_key_env")?;
    let session_token = required(&p.session_token_env, "session_token_env")?;
    if api_key.is_some() && session_token.is_some() {
        return Err(LlmError::Config(ctx("`session_token_env` only applies to AWS access keys, not `api_key_env`")));
    }
    let auth = match (api_key, access, secret) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(LlmError::Config(ctx("use either `api_key_env` or AWS access keys, not both")));
        }
        (Some(key), None, None) => BedrockAuth::ApiKey(key),
        (None, Some(access_key_id), Some(secret_access_key)) => {
            BedrockAuth::SigV4(AwsCredentials { access_key_id, secret_access_key, session_token })
        }
        _ => {
            return Err(LlmError::Config(ctx(
                "needs `api_key_env` (Bedrock API key) or both `access_key_id_env` and `secret_access_key_env`",
            )));
        }
    };
    let mut cfg = BedrockConfig::new(region, auth);
    cfg.endpoint = base_url;
    cfg.headers = headers;
    cfg.provider_name.clone_from(&p.name);
    Ok(Arc::new(BedrockModel::new(cfg)?))
}

#[cfg(all(feature = "http", not(feature = "bedrock")))]
fn bedrock_provider(
    p: &ProviderConfig,
    _base_url: Option<String>,
    _api_key: Option<String>,
    _headers: Vec<(String, String)>,
    _vars: &[(String, String)],
) -> Result<Arc<dyn ChatModel>, LlmError> {
    Err(LlmError::Config(format!("provider `{}`: the `bedrock` feature is disabled in this build", p.name)))
}

#[cfg(feature = "http")]
fn missing(ctx: &dyn Fn(&str) -> String, var: &str) -> LlmError {
    if var.ends_with("(unterminated `${`)") {
        return LlmError::Config(ctx(&format!("invalid `${{...}}` reference: {var}")));
    }
    LlmError::Config(ctx(&format!("environment variable `{var}` referenced as ${{{var}}} is not set")))
}

fn lookup(vars: &[(String, String)], name: &str) -> Option<String> {
    vars.iter().find(|(k, v)| k == name && !v.is_empty()).map(|(_, v)| v.clone())
}

/// Replace `${VAR}` references. Returns the name of the first missing variable on error.
/// `$$` is a literal `$`.
pub fn interpolate(text: &str, vars: &[(String, String)]) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('$') {
        let (before, after) = rest.split_at(pos);
        out.push_str(before);
        let after = &after[1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$');
            rest = stripped;
        } else if let Some(body) = after.strip_prefix('{') {
            match body.find('}') {
                Some(end) => {
                    let name = &body[..end];
                    let value = lookup(vars, name).ok_or_else(|| name.to_owned())?;
                    out.push_str(&value);
                    rest = &body[end + 1..];
                }
                None => {
                    // An unterminated `${` would otherwise end up literally in a URL or header.
                    return Err(format!("{} (unterminated `${{`)", body.chars().take(40).collect::<String>()));
                }
            }
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Applies model-name aliases and default parameters before delegating.
#[cfg(feature = "http")]
#[derive(Debug)]
struct Configured {
    inner: Arc<dyn ChatModel>,
    models: IndexMap<String, String>,
    defaults: DefaultParams,
}

#[cfg(feature = "http")]
impl ChatModel for Configured {
    fn chat(&self, mut request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        if let Some(real) = self.models.get(&request.model) {
            request.model.clone_from(real);
        }
        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }
        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }
        if request.max_tokens.is_none() {
            request.max_tokens = self.defaults.max_tokens;
        }
        if request.stop.is_empty() {
            request.stop.clone_from(&self.defaults.stop);
        }
        self.inner.chat(request)
    }
}

#[cfg(all(test, feature = "openai"))]
mod credentials_section_tests {
    use super::*;

    #[test]
    fn credentials_section_registers_dynamic_providers() {
        let config = ProvidersConfig::from_value(serde_json::json!({
            "providers": [],
            "credentials": {
                "proxy": {"source": "env", "kind": "openai", "var": "PROXY_TOKEN", "base_url": "https://proxy.example.com/v1"}
            }
        }))
        .unwrap();
        assert_eq!(config.credentials.len(), 1);
        let registry = ModelRegistry::from_config_with_vars(&config, &[]).unwrap();
        assert!(registry.get("proxy").is_ok());

        let typo = ProvidersConfig::from_value(serde_json::json!({"credentials": {"p": {"source": "env", "vr": "X"}}}));
        assert!(typo.is_err());
    }

    #[test]
    fn host_sources_and_name_clashes_are_rejected_in_files() {
        let load = |v: serde_json::Value| ProvidersConfig::from_value(v).unwrap().load(&[]);
        let external = load(serde_json::json!({"credentials": {"openai": {"source": "external"}}}));
        assert!(external.unwrap_err().to_string().contains("host app"));
        let clash = load(serde_json::json!({
            "providers": [{"name": "router", "kind": "openai", "api_key_env": "K", "api_key_optional": true}],
            "credentials": {"router": {"source": "env", "var": "X", "kind": "openai"}}
        }));
        assert!(clash.unwrap_err().to_string().contains("same name"));
    }

    #[cfg(feature = "typesafe")]
    #[test]
    fn typesafe_credentials_build_the_system_one_client() {
        let config = ProvidersConfig::from_value(
            serde_json::json!({"credentials": {"typesafe": {"source": "env", "var": "TYPESAFE_API_KEY"}}}),
        )
        .unwrap();
        let loaded = config.load(&[]).unwrap();
        assert!(loaded.system_one.is_some());
        assert!(loaded.models.get("typesafe").is_err());
    }

    #[test]
    fn unterminated_interpolation_is_an_error() {
        assert!(interpolate("https://x/${HOST", &[]).is_err());
        let config = ProvidersConfig::from_value(serde_json::json!({
            "providers": [{"name": "a", "kind": "openai", "base_url": "https://${HOST/v1", "api_key_optional": true}]
        }))
        .unwrap();
        let err = ModelRegistry::from_config_with_vars(&config, &[]).unwrap_err().to_string();
        assert!(err.contains("unterminated"), "{err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "mock")]
    use crate::types::ChatModel;
    #[cfg(feature = "mock")]
    use std::sync::Arc;

    fn vars() -> Vec<(String, String)> {
        vec![("OR_KEY".into(), "sk-or-secret".into()), ("SITE".into(), "https://example.com".into())]
    }

    #[test]
    fn interpolation() {
        let v = vars();
        assert_eq!(interpolate("${SITE}/x", &v).unwrap(), "https://example.com/x");
        assert_eq!(interpolate("a$$b $x", &v).unwrap(), "a$b $x");
        assert!(interpolate("a ${", &v).is_err(), "an unterminated `${{` is an error, not literal text");
        assert_eq!(interpolate("${NOPE}", &v).unwrap_err(), "NOPE");
    }

    #[test]
    fn parses_wrapped_and_list_forms() {
        let wrapped = ProvidersConfig::from_json(r#"{"providers":[{"name":"a","kind":"openai"}]}"#).unwrap();
        let list = ProvidersConfig::from_json(r#"[{"name":"a","kind":"openai-compatible"}]"#).unwrap();
        assert_eq!(wrapped.providers.len(), 1);
        assert_eq!(list.providers[0].kind, ProviderKind::Openai);
    }

    #[test]
    fn typos_are_reported() {
        let err = ProvidersConfig::from_json(r#"[{"name":"a","kind":"openai","api_key_evn":"X"}]"#).unwrap_err();
        assert!(err.to_string().contains("api_key_evn"), "{err}");
        let err = ProvidersConfig::from_json(r#"{"provider":[]}"#).unwrap_err();
        assert!(err.to_string().contains("provider"), "{err}");
    }

    #[cfg(all(feature = "openai", feature = "anthropic"))]
    #[test]
    fn builds_registry_with_aliases_and_no_secret_leaks() {
        let cfg = ProvidersConfig::from_json(
            r#"{"providers":[
                {"name":"openrouter","kind":"openai","base_url":"https://openrouter.ai/api/v1","api_key_env":"OR_KEY",
                 "headers":{"HTTP-Referer":"${SITE}"},"aliases":["or"],"models":{"fast":"x/y"},"defaults":{"temperature":0.2}},
                {"name":"claude","kind":"anthropic","api_key_env":"OR_KEY"},
                {"name":"local","kind":"openai","base_url":"http://localhost:11434/v1","api_key_env":"NONE","api_key_optional":true}
            ]}"#,
        )
        .unwrap();
        let registry = ModelRegistry::from_config_with_vars(&cfg, &vars()).unwrap();
        assert!(registry.get("openrouter").is_ok());
        assert!(registry.get("or").is_ok());
        assert!(registry.get("claude").is_ok());
        assert!(registry.get("local").is_ok());
        let debug = format!("{registry:?} {:?}", registry.get("openrouter").unwrap());
        assert!(!debug.contains("sk-or-secret"), "{debug}");
    }

    #[cfg(feature = "openai")]
    #[test]
    fn conflicting_aliases_are_rejected() {
        let shadow = ProvidersConfig::from_json(
            r#"[{"name":"a","kind":"openai","base_url":"http://x/v1","aliases":["b"]},
                {"name":"b","kind":"openai","base_url":"http://y/v1"}]"#,
        )
        .unwrap();
        let err = ModelRegistry::from_config_with_vars(&shadow, &[]).unwrap_err().to_string();
        assert!(err.contains("alias `b`"), "{err}");
        let twice = ProvidersConfig::from_json(
            r#"[{"name":"a","kind":"openai","base_url":"http://x/v1","aliases":["fast"]},
                {"name":"c","kind":"openai","base_url":"http://y/v1","aliases":["fast"]}]"#,
        )
        .unwrap();
        let err = ModelRegistry::from_config_with_vars(&twice, &[]).unwrap_err().to_string();
        assert!(err.contains("`fast`") && err.contains("both"), "{err}");
    }

    #[cfg(feature = "openai")]
    #[test]
    fn missing_key_variable_names_the_variable() {
        let cfg = ProvidersConfig::from_json(r#"[{"name":"a","kind":"openai","api_key_env":"MISSING_KEY"}]"#).unwrap();
        let err = ModelRegistry::from_config_with_vars(&cfg, &[]).unwrap_err().to_string();
        assert!(err.contains("MISSING_KEY") && err.contains("`a`"), "{err}");
        let cfg = ProvidersConfig::from_json(r#"[{"name":"a","kind":"openai","headers":{"X":"${GONE}"}}]"#).unwrap();
        let err = ModelRegistry::from_config_with_vars(&cfg, &[]).unwrap_err().to_string();
        assert!(err.contains("GONE"), "{err}");
    }

    #[cfg(all(feature = "mock", feature = "http"))]
    #[test]
    fn model_aliases_and_defaults_are_applied() {
        use crate::mock::MockModel;
        use crate::types::Message;

        let mock = Arc::new(MockModel::new(vec![MockModel::text("ok")]));
        let configured = Configured {
            inner: Arc::clone(&mock) as Arc<dyn ChatModel>,
            models: IndexMap::from([("fast".to_owned(), "real-model".to_owned())]),
            defaults: DefaultParams { temperature: Some(0.3), max_tokens: Some(50), ..DefaultParams::default() },
        };
        let request = ChatRequest::new("fast", vec![Message::user("hi")]).with_temperature(0.9);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _stream = rt.block_on(configured.chat(request)).unwrap();
        let seen = mock.requests();
        let req = seen.first().unwrap();
        assert_eq!(req.model, "real-model");
        assert_eq!(req.temperature, Some(0.9));
        assert_eq!(req.max_tokens, Some(50));
    }

    #[cfg(all(feature = "mock", feature = "openai"))]
    #[test]
    fn provider_alias_redirects_a_model_to_another_provider() {
        use crate::mock::MockModel;
        // A providers file mapping `openai/gpt-4o-mini` to a local `ollama/qwen2.5:14b`.
        let cfg = ProvidersConfig::from_json(
            r#"[{"name":"ollama","kind":"openai","base_url":"http://localhost:11434/v1",
                 "aliases":["openai"],"models":{"gpt-4o-mini":"qwen2.5:14b"}}]"#,
        )
        .unwrap();
        let overlay = ModelRegistry::from_config_with_vars(&cfg, &[]).unwrap();
        let env_openai: Arc<dyn ChatModel> = Arc::new(MockModel::new(vec![MockModel::text("from env")]));
        let mut registry = ModelRegistry::new().with("openai", Arc::clone(&env_openai));
        registry.merge(overlay);
        let resolved = registry.get("openai").unwrap();
        assert!(!Arc::ptr_eq(&resolved, &env_openai), "the providers file alias must win");
        assert!(format!("{resolved:?}").contains("qwen2.5:14b"), "{resolved:?}");
    }

    #[cfg(all(feature = "gemini", feature = "bedrock"))]
    #[test]
    fn gemini_and_bedrock_entries() {
        let vars: Vec<(String, String)> = [
            ("GKEY", "AIza-secret"),
            ("REGION", "ap-southeast-1"),
            ("AK", "AKID"),
            ("SK", "aws-secret"),
            ("BKEY", "bedrock-key"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let cfg = ProvidersConfig::from_json(
            r#"[{"name":"g","kind":"gemini","api_key_env":"GKEY"},
                {"name":"vertex","kind":"gemini","api_key_env":"GKEY","auth":"bearer","base_url":"https://x/v1/projects/p/locations/l/publishers/google"},
                {"name":"b","kind":"bedrock","region":"${REGION}","access_key_id_env":"AK","secret_access_key_env":"SK"},
                {"name":"bk","kind":"bedrock","region":"us-east-1","api_key_env":"BKEY"}]"#,
        )
        .unwrap();
        let reg = ModelRegistry::from_config_with_vars(&cfg, &vars).unwrap();
        let debug = format!("{:?} {:?} {:?}", reg.get("g").unwrap(), reg.get("vertex").unwrap(), reg.get("b").unwrap());
        assert!(debug.contains("ap-southeast-1") && debug.contains("Bearer"), "{debug}");
        assert!(!debug.contains("AIza-secret") && !debug.contains("aws-secret"), "{debug}");
        assert!(reg.get("bk").is_ok());

        let bad = |json: &str| {
            let cfg = ProvidersConfig::from_json(json).unwrap();
            ModelRegistry::from_config_with_vars(&cfg, &vars).unwrap_err().to_string()
        };
        assert!(bad(r#"[{"name":"x","kind":"bedrock","region":"us-east-1"}]"#).contains("needs"));
        assert!(
            bad(
                r#"[{"name":"x","kind":"bedrock","region":"us-east-1","api_key_env":"BKEY","access_key_id_env":"AK"}]"#
            )
            .contains("either")
        );
        assert!(bad(r#"[{"name":"x","kind":"gemini","api_key_env":"GKEY","auth":"oauth"}]"#).contains("auth"));
        assert!(bad(r#"[{"name":"x","kind":"openai","base_url":"http://x","region":"us-east-1"}]"#).contains("region"));
        assert!(bad(r#"[{"name":"x","kind":"bedrock","region":"us-east-1","access_key_id_env":"NOPE","secret_access_key_env":"SK"}]"#)
            .contains("NOPE"));
    }

    #[cfg(feature = "mock")]
    #[test]
    fn merge_replaces_same_named_providers() {
        use crate::mock::MockModel;
        let a: Arc<dyn ChatModel> = Arc::new(MockModel::new(vec![MockModel::text("a")]));
        let b: Arc<dyn ChatModel> = Arc::new(MockModel::new(vec![MockModel::text("b")]));
        let mut base = ModelRegistry::new().with("openai", Arc::clone(&a));
        let mut overlay = ModelRegistry::new().with("openai", Arc::clone(&b));
        overlay.alias("oai", "openai");
        base.merge(overlay);
        assert!(Arc::ptr_eq(&base.get("openai").unwrap(), &b));
        assert!(base.get("oai").is_ok());
    }
}
