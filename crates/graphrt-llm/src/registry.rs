//! Named model providers, registered as a host service.

use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::error::LlmError;
use crate::types::ChatModel;

/// Maps provider names (as used in node configs: `model.provider`) to models.
///
/// Register it with `graphrt_core::Services::insert(registry)`.
#[derive(Clone, Default)]
pub struct ModelRegistry {
    providers: IndexMap<String, Arc<dyn ChatModel>>,
}

impl fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRegistry").field("providers", &self.providers.keys().collect::<Vec<_>>()).finish()
    }
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a provider.
    pub fn add(&mut self, name: impl Into<String>, model: Arc<dyn ChatModel>) -> &mut Self {
        self.providers.insert(name.into(), model);
        self
    }

    /// Builder-style [`ModelRegistry::add`].
    pub fn with(mut self, name: impl Into<String>, model: Arc<dyn ChatModel>) -> Self {
        self.add(name, model);
        self
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Resolve a provider name. Exact names win; otherwise the last `/`-separated
    /// segment is tried (so `vendor/openai/openai` resolves to `openai`).
    pub fn get(&self, name: &str) -> Result<Arc<dyn ChatModel>, LlmError> {
        if let Some(m) = self.providers.get(name) {
            return Ok(Arc::clone(m));
        }
        let short = name.rsplit('/').next().unwrap_or(name);
        self.providers.get(short).cloned().ok_or_else(|| {
            let known: Vec<&str> = self.names().collect();
            LlmError::Config(format!("unknown model provider `{name}` (configured: {known:?})"))
        })
    }

    /// Build a registry from environment variables:
    ///
    /// | Variables | Provider name |
    /// |---|---|
    /// | `OPENAI_API_KEY`, optional `OPENAI_BASE_URL` | `openai` |
    /// | `ANTHROPIC_API_KEY`, optional `ANTHROPIC_BASE_URL` | `anthropic` |
    /// | `OLLAMA_HOST` (e.g. `http://localhost:11434`) | `ollama` |
    /// | `GRAPHRT_OPENAI_COMPAT_<NAME>_BASE_URL`, optional `..._API_KEY` | `<name>` (lowercase) |
    #[cfg(feature = "http")]
    pub fn from_env() -> Result<Self, LlmError> {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        Self::from_vars(&vars)
    }

    /// Like [`ModelRegistry::from_env`] but reads from the given variables.
    #[cfg(feature = "http")]
    pub fn from_vars(vars: &[(String, String)]) -> Result<Self, LlmError> {
        let get = |k: &str| vars.iter().find(|(name, v)| name == k && !v.is_empty()).map(|(_, v)| v.clone());
        #[allow(unused_mut)]
        let mut registry = Self::new();

        #[cfg(feature = "openai")]
        {
            use crate::openai::{OpenAiConfig, OpenAiModel};
            let key = get("OPENAI_API_KEY");
            let base = get("OPENAI_BASE_URL");
            if key.is_some() || base.is_some() {
                let mut cfg = OpenAiConfig::new(base.unwrap_or_else(|| OpenAiConfig::default().base_url), key);
                cfg.use_max_completion_tokens = cfg.base_url.contains("api.openai.com");
                registry.add("openai", Arc::new(OpenAiModel::new(cfg)?));
            }
            if let Some(host) = get("OLLAMA_HOST") {
                let host = if host.starts_with("http") { host } else { format!("http://{host}") };
                let cfg = OpenAiConfig::new(format!("{}/v1", host.trim_end_matches('/')), None);
                registry.add("ollama", Arc::new(OpenAiModel::new(cfg)?));
            }
            const PREFIX: &str = "GRAPHRT_OPENAI_COMPAT_";
            for (name, value) in vars {
                let Some(rest) = name.strip_prefix(PREFIX) else { continue };
                let Some(provider) = rest.strip_suffix("_BASE_URL") else { continue };
                if value.is_empty() || provider.is_empty() {
                    continue;
                }
                let key = get(&format!("{PREFIX}{provider}_API_KEY"));
                let cfg = OpenAiConfig::new(value.clone(), key);
                registry.add(provider.to_ascii_lowercase(), Arc::new(OpenAiModel::new(cfg)?));
            }
        }

        #[cfg(feature = "anthropic")]
        {
            use crate::anthropic::{AnthropicConfig, AnthropicModel};
            if let Some(key) = get("ANTHROPIC_API_KEY") {
                let mut cfg = AnthropicConfig::new(key);
                if let Some(base) = get("ANTHROPIC_BASE_URL") {
                    cfg.base_url = base;
                }
                registry.add("anthropic", Arc::new(AnthropicModel::new(cfg)?));
            }
        }

        let _ = &get;
        Ok(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockModel;

    #[test]
    fn resolves_short_names() {
        let reg = ModelRegistry::new().with("openai", Arc::new(MockModel::new(vec![])));
        assert!(reg.get("openai").is_ok());
        assert!(reg.get("vendor/openai/openai").is_ok());
        assert!(matches!(reg.get("nope"), Err(LlmError::Config(_))));
    }

    #[cfg(all(feature = "openai", feature = "anthropic"))]
    #[test]
    fn from_vars_builds_providers_without_leaking_keys() {
        let vars = vec![
            ("OPENAI_API_KEY".to_owned(), "sk-abc".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), "ak-xyz".to_owned()),
            ("OLLAMA_HOST".to_owned(), "localhost:11434".to_owned()),
            ("GRAPHRT_OPENAI_COMPAT_GROQ_BASE_URL".to_owned(), "https://api.groq.com/openai/v1".to_owned()),
        ];
        let reg = ModelRegistry::from_vars(&vars).unwrap();
        let names: Vec<&str> = reg.names().collect();
        assert_eq!(names, vec!["openai", "ollama", "groq", "anthropic"]);
        let debug = format!("{:?}", reg.get("openai").unwrap());
        assert!(!debug.contains("sk-abc"));
        let debug = format!("{:?}", reg.get("anthropic").unwrap());
        assert!(!debug.contains("ak-xyz"));
    }
}
