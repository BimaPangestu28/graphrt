//! In-process sentence embeddings: [`LocalEmbedder`].
//!
//! Runs a BERT-family sentence-embedding model on the CPU with candle, so a
//! knowledge base needs no embedding server, API key or network once the model
//! is on disk. Models come from a local directory (`path`) or, with the `http`
//! feature, are downloaded once from the Hugging Face Hub into a cache.
//!
//! A model directory holds `config.json`, `tokenizer.json` and
//! `model.safetensors` (the layout of a Hub repository). The model is loaded on
//! first use; every batch runs on its own thread so the async executor driving
//! a workflow is never blocked by inference. With `path` any executor works;
//! downloading uses reqwest and, like the other HTTP providers, needs a Tokio
//! runtime.
//!
//! ```yaml
//! embedders:
//!   local:
//!     type: local
//!     model: sentence-transformers/all-MiniLM-L6-v2   # default, English, 384 dims
//! # Multilingual (e.g. Indonesian): needs the e5 prefixes.
//! #   model: intfloat/multilingual-e5-small
//! #   query_prefix: "query: "
//! #   document_prefix: "passage: "
//! ```

mod model;

#[cfg(feature = "http")]
mod download;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::error::RagError;
use crate::types::Embedder;
use model::{ModelOptions, SentenceModel};

/// Files every model directory must contain.
pub(crate) const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// Default model: small, fast, English.
pub const DEFAULT_LOCAL_MODEL: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// How token states become one sentence vector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pooling {
    /// Average of the token states, ignoring padding (sentence-transformers, e5).
    #[default]
    Mean,
    /// State of the first (`[CLS]`) token (BGE models).
    Cls,
}

/// Configuration of a [`LocalEmbedder`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalEmbedderConfig {
    /// Hugging Face repository (`owner/name`). Also the embedder's model id.
    #[serde(default = "default_model")]
    pub model: String,
    /// Branch, tag or commit to download (default `main`; pin a commit for reproducible indexes).
    #[serde(default = "default_revision")]
    pub revision: String,
    /// Load the model from this directory instead of downloading it.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Download cache (default `$GRAPHRT_MODEL_DIR`, else `$XDG_CACHE_HOME/graphrt/models`,
    /// else `~/.cache/graphrt/models`).
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    /// Hub endpoint (default `https://huggingface.co`; mirrors must speak the same URL scheme).
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub pooling: Pooling,
    /// Scale vectors to unit length (default `true`).
    #[serde(default = "default_true")]
    pub normalize: bool,
    /// Tokens per text before truncation (default 512, capped by the model).
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    /// Texts per forward pass (default 16).
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// Prefix added to queries (e.g. `query: ` for e5 models).
    #[serde(default)]
    pub query_prefix: String,
    /// Prefix added to documents (e.g. `passage: ` for e5 models).
    #[serde(default)]
    pub document_prefix: String,
}

fn default_model() -> String {
    DEFAULT_LOCAL_MODEL.to_owned()
}
fn default_revision() -> String {
    "main".to_owned()
}
fn default_endpoint() -> String {
    "https://huggingface.co".to_owned()
}
fn default_true() -> bool {
    true
}
fn default_max_tokens() -> usize {
    512
}
fn default_batch_size() -> usize {
    16
}

impl Default for LocalEmbedderConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            revision: default_revision(),
            path: None,
            cache_dir: None,
            endpoint: default_endpoint(),
            pooling: Pooling::default(),
            normalize: true,
            max_tokens: default_max_tokens(),
            batch_size: default_batch_size(),
            query_prefix: String::new(),
            document_prefix: String::new(),
        }
    }
}

impl LocalEmbedderConfig {
    /// A config for the Hub repository `model` with every other setting at its default.
    pub fn new(model: impl Into<String>) -> Self {
        Self { model: model.into(), ..Self::default() }
    }

    /// A config that loads the model from `dir` (no download).
    pub fn from_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self { model: dir.display().to_string(), path: Some(dir), ..Self::default() }
    }

    fn validate(&self) -> Result<(), RagError> {
        if self.path.is_none() {
            check_hub_name("model", &self.model, true)?;
            check_hub_name("revision", &self.revision, false)?;
            check_endpoint(&self.endpoint)?;
        }
        if self.max_tokens == 0 || self.batch_size == 0 {
            return Err(RagError::Config("local embedder: `max_tokens` and `batch_size` must be positive".into()));
        }
        Ok(())
    }

    fn options(&self) -> ModelOptions {
        ModelOptions { pooling: self.pooling, normalize: self.normalize, max_tokens: self.max_tokens }
    }
}

/// Repository names and revisions become cache path segments: allow only
/// `[A-Za-z0-9._-]` (plus one `/` in a repository name) and never `..`.
fn check_hub_name(field: &str, value: &str, with_owner: bool) -> Result<(), RagError> {
    let segment_ok = |s: &str| {
        !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    };
    let valid = if with_owner {
        matches!(value.split_once('/'), Some((owner, name)) if segment_ok(owner) && segment_ok(name))
    } else {
        segment_ok(value)
    };
    if valid {
        Ok(())
    } else {
        let expected = if with_owner { "`owner/name`" } else { "a branch, tag or commit" };
        Err(RagError::Config(format!("local embedder: `{field}` must be {expected}, got `{value}`")))
    }
}

fn check_endpoint(endpoint: &str) -> Result<(), RagError> {
    let loopback = ["http://localhost", "http://127.0.0.1", "http://[::1]"];
    if endpoint.starts_with("https://") || loopback.iter().any(|p| endpoint.starts_with(p)) {
        Ok(())
    } else {
        Err(RagError::Config(format!("local embedder: `endpoint` must use https, got `{endpoint}`")))
    }
}

/// Default download cache directory.
fn default_cache_dir() -> Result<PathBuf, RagError> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("GRAPHRT_MODEL_DIR")
        .or_else(|| var("XDG_CACHE_HOME").map(|d| d.join("graphrt").join("models")))
        .or_else(|| var("HOME").map(|d| d.join(".cache").join("graphrt").join("models")))
        .ok_or_else(|| RagError::Config("local embedder: set `cache_dir` (no HOME to derive a cache from)".into()))
}

/// [`Embedder`] that runs a sentence-embedding model in this process.
#[derive(Debug)]
pub struct LocalEmbedder {
    config: LocalEmbedderConfig,
    /// Loaded on first use; the async lock makes concurrent first calls load once.
    model: futures::lock::Mutex<Option<Arc<SentenceModel>>>,
    dimensions: OnceLock<usize>,
}

impl LocalEmbedder {
    /// Validate `config`. Nothing is downloaded or loaded until the first embedding.
    ///
    /// @throws `RagError::Config` for an invalid model name, revision, endpoint or limit.
    pub fn new(config: LocalEmbedderConfig) -> Result<Self, RagError> {
        config.validate()?;
        Ok(Self { config, model: futures::lock::Mutex::new(None), dimensions: OnceLock::new() })
    }

    /// Download (if needed) and load the model now instead of on the first embedding.
    pub async fn preload(&self) -> Result<(), RagError> {
        self.loaded().await.map(drop)
    }

    async fn loaded(&self) -> Result<Arc<SentenceModel>, RagError> {
        let mut slot = self.model.lock().await;
        if let Some(model) = slot.as_ref() {
            return Ok(Arc::clone(model));
        }
        let dir = self.model_dir().await?;
        let options = self.config.options();
        let model = Arc::new(run_on_thread(move || SentenceModel::load(&dir, options)).await?);
        let _first_load = self.dimensions.set(model.dimensions());
        *slot = Some(Arc::clone(&model));
        Ok(model)
    }

    async fn model_dir(&self) -> Result<PathBuf, RagError> {
        if let Some(path) = &self.config.path {
            return Ok(path.clone());
        }
        let cache = match &self.config.cache_dir {
            Some(dir) => dir.clone(),
            None => default_cache_dir()?,
        };
        self.fetch(&cache).await
    }

    #[cfg(feature = "http")]
    async fn fetch(&self, cache: &std::path::Path) -> Result<PathBuf, RagError> {
        download::ensure_model(&self.config.endpoint, &self.config.model, &self.config.revision, cache).await
    }

    #[cfg(not(feature = "http"))]
    async fn fetch(&self, _cache: &std::path::Path) -> Result<PathBuf, RagError> {
        Err(RagError::Config(format!(
            "local embedder: downloading `{}` needs the `http` feature; set `path` to a model directory",
            self.config.model
        )))
    }

    async fn embed_all(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, RagError> {
        let model = self.loaded().await?;
        let batch_size = self.config.batch_size;
        run_on_thread(move || {
            let mut vectors = Vec::with_capacity(texts.len());
            for batch in texts.chunks(batch_size) {
                vectors.extend(model.embed(batch)?);
            }
            Ok(vectors)
        })
        .await
    }
}

/// Run blocking work (file loading, inference) on a fresh thread and await it,
/// so any executor — or none — can drive the returned future.
async fn run_on_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, RagError> + Send + 'static,
) -> Result<T, RagError> {
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("graphrt-embed".into())
        .spawn(move || {
            let _receiver_gone = sender.send(work());
        })
        .map_err(|e| RagError::Model(format!("cannot start embedding thread: {e}")))?;
    receiver.await.map_err(|_| RagError::Model("embedding thread panicked".into()))?
}

impl Embedder for LocalEmbedder {
    fn model(&self) -> &str {
        &self.config.model
    }

    fn dimensions(&self) -> Option<usize> {
        self.dimensions.get().copied()
    }

    fn embed_documents<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>, RagError>> {
        let prefixed = texts.iter().map(|t| format!("{}{t}", self.config.document_prefix)).collect();
        Box::pin(self.embed_all(prefixed))
    }

    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, RagError>> {
        let prefixed = vec![format!("{}{text}", self.config.query_prefix)];
        Box::pin(async move {
            let mut vectors = self.embed_all(prefixed).await?;
            vectors.pop().ok_or_else(|| RagError::Model("model returned no vector".into()))
        })
    }
}

#[cfg(test)]
mod tests;
