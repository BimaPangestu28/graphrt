//! Config-driven model providers.
//!
//! A providers file declares any number of OpenAI-compatible or Anthropic
//! endpoints without code:
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
}

/// A providers file: either `{providers: [...]}` or a bare list.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(from = "ProvidersRepr")]
pub struct ProvidersConfig {
    pub providers: Vec<ProviderConfig>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProvidersRepr {
    Wrapped { providers: Vec<ProviderConfig> },
    List(Vec<ProviderConfig>),
}

impl From<ProvidersRepr> for ProvidersConfig {
    fn from(repr: ProvidersRepr) -> Self {
        match repr {
            ProvidersRepr::Wrapped { providers } | ProvidersRepr::List(providers) => Self { providers },
        }
    }
}

impl ProvidersConfig {
    /// Decode from a JSON value (hosts decode YAML into JSON first).
    pub fn from_value(value: serde_json::Value) -> Result<Self, LlmError> {
        // Decode through a strict wrapper first so typos produce a precise message
        // instead of the generic "did not match any variant" of the untagged enum.
        let entries = match value {
            serde_json::Value::Object(mut map) => {
                let providers = map.remove("providers").unwrap_or(serde_json::Value::Array(Vec::new()));
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
        Ok(Self { providers })
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
    #[cfg(feature = "http")]
    pub fn from_config_with_vars(config: &ProvidersConfig, vars: &[(String, String)]) -> Result<Self, LlmError> {
        let mut registry = Self::new();
        for provider in &config.providers {
            let model = build_provider(provider, vars)?;
            registry.try_add(provider.name.clone(), Arc::clone(&model))?;
            for alias in &provider.aliases {
                registry.alias(alias.clone(), provider.name.clone());
            }
        }
        Ok(registry)
    }

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

    let inner: Arc<dyn ChatModel> = match p.kind {
        ProviderKind::Openai => openai_provider(p, base_url, api_key, headers, vars)?,
        ProviderKind::Anthropic => anthropic_provider(p, base_url, api_key, headers)?,
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

#[cfg(feature = "http")]
fn missing(ctx: &dyn Fn(&str) -> String, var: &str) -> LlmError {
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
                    out.push('$');
                    rest = after;
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
        assert_eq!(interpolate("a$$b $x ${", &v).unwrap(), "a$b $x ${");
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
