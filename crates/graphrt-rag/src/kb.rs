//! [`KnowledgeBase`]: the service the `knowledge-retrieval` node uses.
//!
//! A knowledge base maps dataset ids to retrievers. A dataset is either backed
//! by the built-in pipeline (a [`VectorStore`] + [`Embedder`], optionally a
//! [`Reranker`]) or by any external [`Retriever`]. Register it in
//! `graphrt_core::Services` as a plain `KnowledgeBase` value:
//!
//! ```
//! # use std::sync::Arc;
//! # use graphrt_core::Services;
//! # use graphrt_rag::{KnowledgeBase, MemoryStore, HashEmbedder};
//! let kb = KnowledgeBase::new()
//!     .with_store_dataset("docs", Arc::new(MemoryStore::new()), Arc::new(HashEmbedder::new(64)), None);
//! let services = Services::new().with(kb);
//! ```

use std::sync::Arc;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::RagError;
use crate::ingest::Ingestor;
use crate::pipeline::{StoreRetriever, finalize, rerank};
use crate::types::{Embedder, Hit, Reranker, RetrievalRequest, Retriever, VectorStore};

/// Public description of a dataset.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug)]
struct DatasetEntry {
    info: DatasetInfo,
    retriever: Arc<dyn Retriever>,
    store: Option<(Arc<dyn VectorStore>, Arc<dyn Embedder>)>,
}

/// Dataset id -> retriever registry.
#[derive(Clone, Debug, Default)]
pub struct KnowledgeBase {
    datasets: IndexMap<String, DatasetEntry>,
    /// Reranker applied across datasets in multi-dataset retrieval.
    reranker: Option<Arc<dyn Reranker>>,
}

impl KnowledgeBase {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a dataset backed by the built-in store pipeline.
    pub fn with_store_dataset(
        mut self,
        id: impl Into<String>,
        store: Arc<dyn VectorStore>,
        embedder: Arc<dyn Embedder>,
        reranker: Option<Arc<dyn Reranker>>,
    ) -> Self {
        let id = id.into();
        self.add_store_dataset(DatasetInfo { id, ..DatasetInfo::default() }, store, embedder, reranker);
        self
    }

    /// Add (or replace) a store-backed dataset with descriptive info.
    pub fn add_store_dataset(
        &mut self,
        info: DatasetInfo,
        store: Arc<dyn VectorStore>,
        embedder: Arc<dyn Embedder>,
        reranker: Option<Arc<dyn Reranker>>,
    ) -> &mut Self {
        let retriever = StoreRetriever::new(store.clone(), embedder.clone()).with_reranker(reranker);
        self.datasets.insert(
            info.id.clone(),
            DatasetEntry { info, retriever: Arc::new(retriever), store: Some((store, embedder)) },
        );
        self
    }

    /// Add a dataset served by an external retriever.
    pub fn with_retriever(mut self, id: impl Into<String>, retriever: Arc<dyn Retriever>) -> Self {
        let id = id.into();
        self.add_retriever(DatasetInfo { id, ..DatasetInfo::default() }, retriever);
        self
    }

    /// Add (or replace) a dataset served by an external retriever.
    pub fn add_retriever(&mut self, info: DatasetInfo, retriever: Arc<dyn Retriever>) -> &mut Self {
        self.datasets.insert(info.id.clone(), DatasetEntry { info, retriever, store: None });
        self
    }

    /// Reranker applied to the merged results of multi-dataset retrieval.
    pub fn with_reranker(mut self, reranker: Option<Arc<dyn Reranker>>) -> Self {
        self.reranker = reranker;
        self
    }

    pub fn datasets(&self) -> impl Iterator<Item = &DatasetInfo> {
        self.datasets.values().map(|d| &d.info)
    }

    pub fn contains(&self, dataset_id: &str) -> bool {
        self.datasets.contains_key(dataset_id)
    }

    /// An ingestor for a store-backed dataset (`None` for external retrievers or unknown ids).
    pub fn ingestor(&self, dataset_id: &str) -> Option<Ingestor> {
        let entry = self.datasets.get(dataset_id)?;
        let (store, embedder) = entry.store.as_ref()?;
        Some(Ingestor::new(store.clone(), embedder.clone()))
    }

    fn entry(&self, id: &str) -> Result<&DatasetEntry, RagError> {
        self.datasets.get(id).ok_or_else(|| RagError::NotFound(format!("unknown dataset `{id}`")))
    }

    /// Query every dataset concurrently. Returns `(dataset id, hits)` in request order.
    async fn query_each(
        &self,
        request: &RetrievalRequest,
        rerank_each: bool,
    ) -> Result<Vec<(String, Vec<Hit>)>, RagError> {
        let mut jobs = Vec::with_capacity(request.datasets.len());
        for id in &request.datasets {
            let entry = self.entry(id)?;
            let sub = RetrievalRequest {
                datasets: vec![id.clone()],
                // Threshold and top-k are applied after merging.
                score_threshold: None,
                rerank: rerank_each,
                ..request.clone()
            };
            jobs.push(async move { entry.retriever.retrieve(&sub).await.map(|hits| (id.clone(), hits)) });
        }
        futures::future::try_join_all(jobs).await
    }

    /// Retrieve across all requested datasets and merge by score.
    ///
    /// When `request.rerank` is set and the knowledge base has a cross-dataset
    /// reranker, the merged list is reranked as a whole (making scores from
    /// different datasets comparable). Otherwise each dataset's own reranker, if
    /// any, is applied before merging.
    pub async fn retrieve(&self, request: &RetrievalRequest) -> Result<Vec<Hit>, RagError> {
        validate_request(request)?;
        let global_rerank = request.rerank && self.reranker.is_some() && request.datasets.len() > 1;
        let per_dataset = self.query_each(request, request.rerank && !global_rerank).await?;
        let mut merged: Vec<Hit> = per_dataset.into_iter().flat_map(|(_, hits)| hits).collect();
        if global_rerank {
            if let Some(reranker) = &self.reranker {
                merged = rerank(reranker.as_ref(), &request.query, merged).await?;
            }
        }
        Ok(finalize(merged, request.score_threshold, request.top_k))
    }

    /// Retrieve from the single best-matching dataset: every dataset is queried,
    /// and only the hits of the dataset whose top hit scores highest are kept.
    pub async fn retrieve_single(&self, request: &RetrievalRequest) -> Result<Vec<Hit>, RagError> {
        validate_request(request)?;
        let per_dataset = self.query_each(request, request.rerank).await?;
        let best = per_dataset
            .into_iter()
            .filter(|(_, hits)| !hits.is_empty())
            .max_by(|a, b| top_score(&a.1).total_cmp(&top_score(&b.1)));
        Ok(best.map(|(_, hits)| finalize(hits, request.score_threshold, request.top_k)).unwrap_or_default())
    }
}

/// Reject requests that would otherwise silently match nothing.
fn validate_request(request: &RetrievalRequest) -> Result<(), RagError> {
    if request.datasets.is_empty() {
        return Err(RagError::Input("no dataset ids given".into()));
    }
    if let Some(filter) = &request.filter {
        filter.validate().map_err(|e| RagError::Input(e.to_string()))?;
    }
    Ok(())
}

fn top_score(hits: &[Hit]) -> f32 {
    hits.iter().map(|h| h.score).fold(f32::MIN, f32::max)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Declarative knowledge-base configuration (`knowledge.yaml` / `.json`).
///
/// ```yaml
/// embedders:
///   default: { type: openai, base_url: "http://localhost:11434/v1", model: nomic-embed-text }
///   inproc:  { type: local, model: sentence-transformers/all-MiniLM-L6-v2 }   # feature `local-embed`
///   cloud:   { type: openai, model: text-embedding-3-small, api_key_env: OPENAI_API_KEY }
/// rerankers:
///   cohere: { type: http, base_url: "https://api.cohere.com/v2", model: rerank-v3.5, api_key_env: COHERE_API_KEY }
/// stores:
///   local:  { type: sqlite, path: ./kb.db }
///   json:   { type: memory, path: ./kb.json }
///   vector: { type: qdrant, url: "http://localhost:6333", collection: kb, api_key_env: QDRANT_API_KEY }
/// datasets:
///   - { id: handbook, name: Handbook, store: local, embedder: default, reranker: cohere }
///   - { id: crm, retriever: { type: http, url: "https://rag.internal/search", api_key_env: RAG_TOKEN } }
/// reranker: cohere   # optional, used across datasets
/// ```
///
/// Secrets are never written inline: every provider takes `api_key_env`, the
/// name of an environment variable resolved when the configuration is built.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeConfig {
    #[serde(default)]
    pub embedders: IndexMap<String, serde_json::Value>,
    #[serde(default)]
    pub rerankers: IndexMap<String, serde_json::Value>,
    #[serde(default)]
    pub stores: IndexMap<String, serde_json::Value>,
    #[serde(default)]
    pub datasets: Vec<DatasetConfig>,
    /// Cross-dataset reranker (a key of `rerankers`).
    #[serde(default)]
    pub reranker: Option<String>,
}

/// One dataset entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Key of `stores` (store-backed datasets).
    #[serde(default)]
    pub store: Option<String>,
    /// Key of `embedders` (store-backed datasets).
    #[serde(default)]
    pub embedder: Option<String>,
    /// Key of `rerankers`.
    #[serde(default)]
    pub reranker: Option<String>,
    /// External retriever definition (instead of store + embedder).
    #[serde(default)]
    pub retriever: Option<serde_json::Value>,
}

/// Resolves environment variable names to values (`std::env::var` in production).
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

impl KnowledgeConfig {
    pub fn from_json(text: &str) -> Result<Self, RagError> {
        serde_json::from_str(text).map_err(|e| RagError::Config(format!("invalid knowledge config: {e}")))
    }

    #[cfg(feature = "yaml")]
    pub fn from_yaml(text: &str) -> Result<Self, RagError> {
        let options = serde_saphyr::options! { strict_booleans: true };
        serde_saphyr::from_str_with_options(text, options)
            .map_err(|e| RagError::Config(format!("invalid knowledge config: {e}")))
    }

    /// Build the knowledge base, resolving secrets with `std::env::var`.
    pub fn build(&self) -> Result<KnowledgeBase, RagError> {
        self.build_with_env(&|name| std::env::var(name).ok())
    }

    /// Build the knowledge base with a custom environment lookup.
    pub fn build_with_env(&self, env: EnvLookup<'_>) -> Result<KnowledgeBase, RagError> {
        let mut embedders: IndexMap<&str, Arc<dyn Embedder>> = IndexMap::new();
        for (name, raw) in &self.embedders {
            embedders.insert(name, build_embedder(name, raw, env)?);
        }
        let mut rerankers: IndexMap<&str, Arc<dyn Reranker>> = IndexMap::new();
        for (name, raw) in &self.rerankers {
            rerankers.insert(name, build_reranker(name, raw, env)?);
        }
        let mut stores: IndexMap<&str, Arc<dyn VectorStore>> = IndexMap::new();
        for (name, raw) in &self.stores {
            stores.insert(name, build_store(name, raw, env)?);
        }

        let lookup_reranker = |key: &Option<String>| -> Result<Option<Arc<dyn Reranker>>, RagError> {
            match key {
                None => Ok(None),
                Some(k) => rerankers
                    .get(k.as_str())
                    .cloned()
                    .map(Some)
                    .ok_or_else(|| RagError::Config(format!("unknown reranker `{k}`"))),
            }
        };

        let mut kb = KnowledgeBase::new().with_reranker(lookup_reranker(&self.reranker)?);
        for ds in &self.datasets {
            if ds.id.trim().is_empty() {
                return Err(RagError::Config("dataset id must not be empty".into()));
            }
            if kb.contains(&ds.id) {
                return Err(RagError::Config(format!("duplicate dataset `{}`", ds.id)));
            }
            let info = DatasetInfo { id: ds.id.clone(), name: ds.name.clone(), description: ds.description.clone() };
            if let Some(raw) = &ds.retriever {
                kb.add_retriever(info, build_retriever(&ds.id, raw, env)?);
                continue;
            }
            let store_key = ds.store.as_deref().ok_or_else(|| {
                RagError::Config(format!("dataset `{}` needs either `store` + `embedder` or `retriever`", ds.id))
            })?;
            let store = stores
                .get(store_key)
                .cloned()
                .ok_or_else(|| RagError::Config(format!("unknown store `{store_key}`")))?;
            let embedder_key = match ds.embedder.as_deref() {
                Some(k) => k,
                None if embedders.len() == 1 => embedders.keys().next().copied().unwrap_or_default(),
                None => return Err(RagError::Config(format!("dataset `{}` needs `embedder`", ds.id))),
            };
            let embedder = embedders
                .get(embedder_key)
                .cloned()
                .ok_or_else(|| RagError::Config(format!("unknown embedder `{embedder_key}`")))?;
            kb.add_store_dataset(info, store, embedder, lookup_reranker(&ds.reranker)?);
        }
        Ok(kb)
    }
}

/// Keys accepted by every HTTP-backed provider entry.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
const HTTP_KEYS: &[&str] = &["type", "api_key", "api_key_env", "api_key_header", "headers", "timeout_ms"];

/// Reject keys a provider entry does not understand (typos would otherwise be
/// ignored silently, e.g. `file:` instead of `path:` turning a persistent store
/// into an in-memory one).
fn check_keys(name: &str, raw: &serde_json::Value, allowed: &[&str], with_http: bool) -> Result<(), RagError> {
    let obj = raw.as_object().ok_or_else(|| RagError::Config(format!("`{name}` must be an object")))?;
    for key in obj.keys() {
        let known = allowed.contains(&key.as_str()) || (with_http && HTTP_KEYS.contains(&key.as_str()));
        if !known && key != "type" {
            return Err(RagError::Config(format!("`{name}`: unknown key `{key}`")));
        }
    }
    Ok(())
}

fn provider_type(name: &str, raw: &serde_json::Value) -> Result<String, RagError> {
    raw.get("type")
        .and_then(|t| t.as_str())
        .map(str::to_owned)
        .ok_or_else(|| RagError::Config(format!("`{name}` needs a `type`")))
}

/// Strip `type` / `api_key_env`, reject inline secrets, and inject the resolved key.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn with_secret(name: &str, raw: &serde_json::Value, env: EnvLookup<'_>) -> Result<serde_json::Value, RagError> {
    let mut obj = raw.as_object().cloned().ok_or_else(|| RagError::Config(format!("`{name}` must be an object")))?;
    obj.remove("type");
    if obj.contains_key("api_key") {
        return Err(RagError::Config(format!(
            "`{name}`: inline `api_key` is not allowed; use `api_key_env` with an environment variable name"
        )));
    }
    if let Some(var) = obj.remove("api_key_env") {
        let var = var.as_str().ok_or_else(|| RagError::Config(format!("`{name}`: `api_key_env` must be a string")))?;
        let value =
            env(var).ok_or_else(|| RagError::Config(format!("`{name}`: environment variable `{var}` is not set")))?;
        obj.insert("api_key".into(), serde_json::Value::String(value));
    }
    Ok(serde_json::Value::Object(obj))
}

#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn parse<T: serde::de::DeserializeOwned>(name: &str, value: serde_json::Value) -> Result<T, RagError> {
    serde_json::from_value(value).map_err(|e| RagError::Config(format!("`{name}`: {e}")))
}

fn build_embedder(name: &str, raw: &serde_json::Value, env: EnvLookup<'_>) -> Result<Arc<dyn Embedder>, RagError> {
    let kind = provider_type(name, raw)?;
    match kind.as_str() {
        #[cfg(feature = "http")]
        "openai" | "openai-compatible" | "ollama" => {
            check_keys(
                name,
                raw,
                &["base_url", "model", "dimensions", "batch_size", "query_prefix", "document_prefix"],
                true,
            )?;
            let mut value = with_secret(name, raw, env)?;
            if kind == "ollama" {
                if let Some(obj) = value.as_object_mut() {
                    obj.entry("base_url").or_insert_with(|| "http://localhost:11434/v1".into());
                }
            }
            let cfg: crate::http::OpenAiEmbedderConfig = parse(name, value)?;
            Ok(Arc::new(crate::http::OpenAiEmbedder::new(cfg)?))
        }
        #[cfg(feature = "local-embed")]
        "local" => {
            let mut value = raw.as_object().cloned().unwrap_or_default();
            value.remove("type");
            let config: crate::local_embed::LocalEmbedderConfig = parse(name, serde_json::Value::Object(value))?;
            Ok(Arc::new(crate::local_embed::LocalEmbedder::new(config)?))
        }
        "hash" => {
            check_keys(name, raw, &["dimensions"], false)?;
            let dims = match raw.get("dimensions") {
                None => 256,
                Some(v) => v
                    .as_u64()
                    .and_then(|d| usize::try_from(d).ok())
                    .filter(|d| *d > 0)
                    .ok_or_else(|| RagError::Config(format!("`{name}`: `dimensions` must be a positive integer")))?,
            };
            let _ = env;
            Ok(Arc::new(crate::hash::HashEmbedder::new(dims)))
        }
        other => Err(RagError::Config(format!("embedder `{name}`: unsupported type `{other}` in this build"))),
    }
}

fn build_reranker(name: &str, raw: &serde_json::Value, env: EnvLookup<'_>) -> Result<Arc<dyn Reranker>, RagError> {
    let kind = provider_type(name, raw)?;
    match kind.as_str() {
        #[cfg(feature = "http")]
        "http" | "cohere" | "jina" | "voyage" => {
            check_keys(name, raw, &["base_url", "model"], true)?;
            let cfg: crate::http::HttpRerankerConfig = parse(name, with_secret(name, raw, env)?)?;
            Ok(Arc::new(crate::http::HttpReranker::new(cfg)?))
        }
        other => {
            let _ = env;
            Err(RagError::Config(format!("reranker `{name}`: unsupported type `{other}` in this build")))
        }
    }
}

fn build_store(name: &str, raw: &serde_json::Value, env: EnvLookup<'_>) -> Result<Arc<dyn VectorStore>, RagError> {
    let kind = provider_type(name, raw)?;
    match kind.as_str() {
        "memory" => {
            check_keys(name, raw, &["path"], false)?;
            let path = match raw.get("path") {
                None => None,
                Some(serde_json::Value::String(p)) => Some(p.as_str()),
                Some(_) => return Err(RagError::Config(format!("store `{name}`: `path` must be a string"))),
            };
            match path {
                #[cfg(feature = "fs")]
                Some(path) => Ok(Arc::new(crate::memory::MemoryStore::open(path)?)),
                #[cfg(not(feature = "fs"))]
                Some(_) => Err(RagError::Config(format!("store `{name}`: `path` requires the `fs` feature"))),
                None => Ok(Arc::new(crate::memory::MemoryStore::new())),
            }
        }
        #[cfg(feature = "sqlite")]
        "sqlite" => {
            check_keys(name, raw, &["path"], false)?;
            let path =
                raw.get("path").and_then(serde_json::Value::as_str).filter(|p| !p.trim().is_empty()).ok_or_else(
                    || RagError::Config(format!("store `{name}`: `path` (the database file) is required")),
                )?;
            Ok(Arc::new(crate::sqlite::SqliteStore::open(path)?))
        }
        #[cfg(feature = "qdrant")]
        "qdrant" => {
            check_keys(name, raw, &["url", "collection", "create_collection"], true)?;
            let mut value = with_secret(name, raw, env)?;
            if let Some(obj) = value.as_object_mut() {
                obj.entry("api_key_header").or_insert_with(|| "api-key".into());
            }
            let cfg: crate::qdrant::QdrantConfig = parse(name, value)?;
            Ok(Arc::new(crate::qdrant::QdrantStore::new(cfg)?))
        }
        other => {
            let _ = env;
            Err(RagError::Config(format!("store `{name}`: unsupported type `{other}` in this build")))
        }
    }
}

fn build_retriever(name: &str, raw: &serde_json::Value, env: EnvLookup<'_>) -> Result<Arc<dyn Retriever>, RagError> {
    let kind = provider_type(name, raw)?;
    match kind.as_str() {
        #[cfg(feature = "http")]
        "http" => {
            check_keys(name, raw, &["url"], true)?;
            let cfg: crate::http::HttpRetrieverConfig = parse(name, with_secret(name, raw, env)?)?;
            Ok(Arc::new(crate::http::HttpRetriever::new(cfg)?))
        }
        other => {
            let _ = env;
            Err(RagError::Config(format!("retriever for `{name}`: unsupported type `{other}` in this build")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::HashEmbedder;
    use crate::ingest::Document;
    use crate::memory::MemoryStore;

    fn kb_with_two_datasets() -> KnowledgeBase {
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(128));
        KnowledgeBase::new()
            .with_store_dataset("pets", Arc::new(MemoryStore::new()), embedder.clone(), None)
            .with_store_dataset("space", Arc::new(MemoryStore::new()), embedder, None)
    }

    async fn ingest(kb: &KnowledgeBase, ds: &str, id: &str, text: &str) {
        kb.ingestor(ds).unwrap().ingest(&Document::new(ds, id, text)).await.unwrap();
    }

    #[tokio::test]
    async fn multi_dataset_merge_and_single_mode() {
        let kb = kb_with_two_datasets();
        ingest(&kb, "pets", "cats", "Cats are small furry pets that purr.").await;
        ingest(&kb, "pets", "dogs", "Dogs are loyal pets that bark.").await;
        ingest(&kb, "space", "mars", "Mars is the red planet in our solar system.").await;

        let req = RetrievalRequest::new("which pets purr", vec!["pets".into(), "space".into()]).top_k(2);
        let hits = kb.retrieve(&req).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].document_id, "cats");

        let single = kb.retrieve_single(&req).await.unwrap();
        assert!(single.iter().all(|h| h.dataset_id == "pets"));

        let unknown = RetrievalRequest::new("x", vec!["nope".into()]);
        assert!(matches!(kb.retrieve(&unknown).await, Err(RagError::NotFound(_))));
    }

    #[test]
    fn config_rejects_inline_secrets_and_resolves_env() {
        let cfg = KnowledgeConfig::from_json(
            r#"{"embedders":{"e":{"type":"hash","dimensions":16}},
                "stores":{"m":{"type":"memory"}},
                "datasets":[{"id":"a","store":"m"}]}"#,
        )
        .unwrap();
        let kb = cfg.build_with_env(&|_| None).unwrap();
        assert!(kb.contains("a"));

        let bad = KnowledgeConfig::from_json(
            r#"{"rerankers":{"r":{"type":"cohere","base_url":"http://x","model":"m","api_key":"inline"}}}"#,
        )
        .unwrap();
        let err = bad.build_with_env(&|_| None).unwrap_err();
        assert!(err.to_string().contains("inline") || err.to_string().contains("unsupported"), "{err}");

        let missing = KnowledgeConfig::from_json(r#"{"datasets":[{"id":"a","store":"nope"}]}"#).unwrap();
        assert!(missing.build_with_env(&|_| None).is_err());
    }

    #[cfg(feature = "http")]
    #[test]
    fn config_env_secret_lookup() {
        let cfg = KnowledgeConfig::from_json(
            r#"{"embedders":{"e":{"type":"openai","base_url":"http://127.0.0.1:9","model":"m","api_key_env":"MY_KEY"}}}"#,
        )
        .unwrap();
        assert!(cfg.build_with_env(&|_| None).unwrap_err().to_string().contains("MY_KEY"));
        assert!(cfg.build_with_env(&|n| (n == "MY_KEY").then(|| "secret".to_owned())).is_ok());
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn yaml_config() {
        let cfg = KnowledgeConfig::from_yaml(
            "embedders:\n  e: { type: hash }\nstores:\n  m: { type: memory }\ndatasets:\n  - { id: kb, store: m, embedder: e }\n",
        )
        .unwrap();
        assert_eq!(cfg.datasets.len(), 1);
        assert!(cfg.build_with_env(&|_| None).is_ok());
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_store_from_config_persists_between_builds() {
        let dir = std::env::temp_dir().join(format!("graphrt-rag-kb-sqlite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kb.db");
        let config = KnowledgeConfig::from_json(
            &serde_json::json!({
                "embedders": { "e": { "type": "hash", "dimensions": 64 } },
                "stores": { "local": { "type": "sqlite", "path": path } },
                "datasets": [{ "id": "faq", "store": "local", "embedder": "e" }]
            })
            .to_string(),
        )
        .unwrap();

        let first = config.build_with_env(&|_| None).unwrap();
        ingest(&first, "faq", "returns", "Items can be returned within 30 days.").await;
        drop(first);

        let second = config.build_with_env(&|_| None).unwrap();
        let hits = second.retrieve(&RetrievalRequest::new("returned within days", vec!["faq".into()])).await.unwrap();
        assert_eq!(hits.first().map(|h| h.document_id.as_str()), Some("returns"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "local-embed")]
    #[test]
    fn local_embedder_from_config_is_lazy_and_strict() {
        let build = |embedder: serde_json::Value| {
            KnowledgeConfig::from_json(&serde_json::json!({ "embedders": { "e": embedder } }).to_string())
                .unwrap()
                .build_with_env(&|_| None)
        };
        // Nothing is downloaded or loaded while building the knowledge base.
        assert!(build(serde_json::json!({ "type": "local" })).is_ok());
        assert!(build(serde_json::json!({ "type": "local", "path": "/not/loaded/yet" })).is_ok());
        let typo = build(serde_json::json!({ "type": "local", "modle": "x/y" })).unwrap_err();
        assert!(typo.to_string().contains("modle"), "{typo}");
        let traversal = build(serde_json::json!({ "type": "local", "model": "../../etc" })).unwrap_err();
        assert!(traversal.to_string().contains("`model`"), "{traversal}");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_store_requires_a_path() {
        let config = KnowledgeConfig::from_json(r#"{"stores":{"local":{"type":"sqlite"}}}"#).unwrap();
        let error = config.build_with_env(&|_| None).unwrap_err();
        assert!(error.to_string().contains("`path` (the database file) is required"), "{error}");
    }
}
