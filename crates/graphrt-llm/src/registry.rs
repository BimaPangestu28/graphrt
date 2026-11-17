//! Named model providers, registered as a host service.

use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::error::LlmError;
use crate::types::ChatModel;

/// Maps provider names (as used in node configs: `model.provider`) to models.
///
/// Register it with `graphrt_core::Services::insert(registry)`.
///
/// Name resolution (see [`ModelRegistry::get`]) is exact, with two explicit
/// exceptions: aliases added with [`ModelRegistry::alias`], and plugin-style
/// identifiers `<org>/<provider>/<provider>` (for example
/// `acme/openai/openai`), which resolve to `<provider>`. Other names, such as
/// `acme/openai`, are not guessed.
#[derive(Clone, Default)]
pub struct ModelRegistry {
    providers: IndexMap<String, Arc<dyn ChatModel>>,
    aliases: IndexMap<String, String>,
}

impl fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("aliases", &self.aliases)
            .finish()
    }
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider, replacing any provider already registered under
    /// `name`. Use [`ModelRegistry::try_add`] to reject duplicates instead.
    pub fn add(&mut self, name: impl Into<String>, model: Arc<dyn ChatModel>) -> &mut Self {
        self.providers.insert(name.into(), model);
        self
    }

    /// Register a provider, failing if `name` is already registered.
    pub fn try_add(&mut self, name: impl Into<String>, model: Arc<dyn ChatModel>) -> Result<&mut Self, LlmError> {
        let name = name.into();
        if self.providers.contains_key(&name) {
            return Err(LlmError::Config(format!("model provider `{name}` is already registered")));
        }
        self.providers.insert(name, model);
        Ok(self)
    }

    /// Make `alias` resolve to the provider registered as `target`.
    pub fn alias(&mut self, alias: impl Into<String>, target: impl Into<String>) -> &mut Self {
        self.aliases.insert(alias.into(), target.into());
        self
    }

    /// Builder-style [`ModelRegistry::add`].
    pub fn with(mut self, name: impl Into<String>, model: Arc<dyn ChatModel>) -> Self {
        self.add(name, model);
        self
    }

    /// Remove a provider (and any alias with that name). Returns whether something was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let provider = self.providers.shift_remove(name).is_some();
        let alias = self.aliases.shift_remove(name).is_some();
        provider || alias
    }

    /// Decompose into providers and aliases.
    pub fn into_parts(self) -> (IndexMap<String, Arc<dyn ChatModel>>, IndexMap<String, String>) {
        (self.providers, self.aliases)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Resolve a provider name: an exact name, then an alias, then a
    /// plugin-style `<org>/<provider>/<provider>` identifier.
    pub fn get(&self, name: &str) -> Result<Arc<dyn ChatModel>, LlmError> {
        if let Some(m) = self.providers.get(name) {
            return Ok(Arc::clone(m));
        }
        if let Some(m) = self.aliases.get(name).and_then(|target| self.providers.get(target)) {
            return Ok(Arc::clone(m));
        }
        if let Some(m) = plugin_provider(name).and_then(|short| self.providers.get(short)) {
            return Ok(Arc::clone(m));
        }
        let known: Vec<&str> = self.names().collect();
        Err(LlmError::Config(format!("unknown model provider `{name}` (configured: {known:?})")))
    }

    /// Build a registry from environment variables:
    ///
    /// | Variables | Provider name |
    /// |---|---|
    /// | `OPENAI_API_KEY`, optional `OPENAI_BASE_URL` | `openai` |
    /// | `ANTHROPIC_API_KEY`, optional `ANTHROPIC_BASE_URL` | `anthropic` |
    /// | `OLLAMA_HOST` (default `http://localhost:11434`; always registered) | `ollama` |
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
                registry.try_add("openai", Arc::new(OpenAiModel::new(cfg)?))?;
            }
            {
                // `ollama` is always available: it defaults to the standard local port, and
                // only connects when a node actually uses it.
                let host = get("OLLAMA_HOST").unwrap_or_else(|| "http://localhost:11434".to_owned());
                let host = if host.starts_with("http") { host } else { format!("http://{host}") };
                let cfg = OpenAiConfig::new(format!("{}/v1", host.trim_end_matches('/')), None);
                registry.try_add("ollama", Arc::new(OpenAiModel::new(cfg)?))?;
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
                // A duplicate (e.g. `..._OPENAI_BASE_URL` next to `OPENAI_API_KEY`) is an error, not an override.
                registry.try_add(provider.to_ascii_lowercase(), Arc::new(OpenAiModel::new(cfg)?))?;
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
                registry.try_add("anthropic", Arc::new(AnthropicModel::new(cfg)?))?;
            }
        }

        let _ = &get;
        Ok(registry)
    }
}

/// `<org>/<provider>/<provider>` -> `<provider>`.
fn plugin_provider(name: &str) -> Option<&str> {
    let mut parts = name.split('/');
    let (org, plugin, provider) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || org.is_empty() || provider.is_empty() || plugin != provider {
        return None;
    }
    Some(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockModel;

    #[test]
    fn resolves_names_explicitly() {
        let mut reg = ModelRegistry::new().with("openai", Arc::new(MockModel::new(vec![])));
        assert!(reg.get("openai").is_ok());
        assert!(reg.get("vendor/openai/openai").is_ok());
        assert!(matches!(reg.get("acme/openai"), Err(LlmError::Config(_))), "no guessing");
        assert!(matches!(reg.get("a/b/openai"), Err(LlmError::Config(_))));
        assert!(matches!(reg.get("nope"), Err(LlmError::Config(_))));
        reg.alias("acme/openai", "openai");
        assert!(reg.get("acme/openai").is_ok());
    }

    #[test]
    fn try_add_rejects_duplicates() {
        let mut reg = ModelRegistry::new();
        assert!(reg.try_add("m", Arc::new(MockModel::new(vec![]))).is_ok());
        assert!(matches!(reg.try_add("m", Arc::new(MockModel::new(vec![]))), Err(LlmError::Config(_))));
    }

    #[cfg(feature = "openai")]
    #[test]
    fn duplicate_env_providers_are_rejected() {
        let vars = vec![
            ("OPENAI_API_KEY".to_owned(), "sk-abc".to_owned()),
            ("GRAPHRT_OPENAI_COMPAT_OPENAI_BASE_URL".to_owned(), "https://other/v1".to_owned()),
        ];
        assert!(matches!(ModelRegistry::from_vars(&vars), Err(LlmError::Config(_))));
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
