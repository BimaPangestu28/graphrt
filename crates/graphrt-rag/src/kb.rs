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

use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::chunker::ChunkerConfig;
use crate::error::RagError;
use crate::ingest::Ingestor;
use crate::pipeline::{StoreRetriever, finalize, rerank};
use crate::types::{
    DocumentSummary, Embedder, Hit, MAX_DOCUMENT_PAGE, Reranker, RetrievalRequest, Retriever, StoredChunk, VectorStore,
};

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
    /// How documents ingested into this dataset are chunked.
    chunking: ChunkerConfig,
    /// Names of the config's store and embedder backing this dataset, when known.
    names: Option<(String, String)>,
    /// Set for datasets created at runtime with [`KnowledgeBase::create_dataset`].
    runtime: Option<DatasetSpec>,
}

/// A dataset created at runtime from the stores, embedders and rerankers a
/// knowledge config declares (see [`KnowledgeBase::create_dataset`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetSpec {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Name of a store in the knowledge config.
    pub store: String,
    /// Name of an embedder in the knowledge config.
    pub embedder: String,
    /// Name of a reranker in the knowledge config.
    #[serde(default)]
    pub reranker: Option<String>,
    #[serde(default)]
    pub chunking: Option<ChunkerConfig>,
}

/// A dataset as listed for management UIs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DatasetDetails {
    #[serde(flatten)]
    pub info: DatasetInfo,
    /// Documents can be added, listed and removed (store-backed datasets).
    pub managed: bool,
    /// Created at runtime (and removable); `false` for datasets from the config.
    pub runtime: bool,
    pub store: Option<String>,
    pub embedder: Option<String>,
    pub chunking: Option<ChunkerConfig>,
}

/// Names of the stores, embedders and rerankers runtime datasets can use.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ResourceNames {
    pub stores: Vec<String>,
    pub embedders: Vec<String>,
    pub rerankers: Vec<String>,
}

/// Named building blocks declared by a knowledge config.
#[derive(Clone, Debug, Default)]
struct Resources {
    stores: IndexMap<String, Arc<dyn VectorStore>>,
    embedders: IndexMap<String, Arc<dyn Embedder>>,
    rerankers: IndexMap<String, Arc<dyn Reranker>>,
}

type Registry = IndexMap<String, DatasetEntry>;

/// Dataset id -> retriever registry.
///
/// Clones share their datasets: a dataset added to or removed from one clone
/// (for example by the management API) is seen by every other clone (for
/// example the one the workflows' `knowledge-retrieval` nodes hold).
#[derive(Clone, Debug, Default)]
pub struct KnowledgeBase {
    datasets: Arc<RwLock<Registry>>,
    resources: Arc<Resources>,
    /// Reranker applied across datasets in multi-dataset retrieval.
    reranker: Option<Arc<dyn Reranker>>,
}

impl KnowledgeBase {
    pub fn new() -> Self {
        Self::default()
    }

    // Every mutation replaces whole entries, so data behind a poisoned lock is still consistent.
    fn read(&self) -> RwLockReadGuard<'_, Registry> {
        self.datasets.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Registry> {
        self.datasets.write().unwrap_or_else(PoisonError::into_inner)
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
        self.insert_store_dataset(info, store, embedder, reranker, ChunkerConfig::default(), None, None);
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_store_dataset(
        &self,
        info: DatasetInfo,
        store: Arc<dyn VectorStore>,
        embedder: Arc<dyn Embedder>,
        reranker: Option<Arc<dyn Reranker>>,
        chunking: ChunkerConfig,
        names: Option<(String, String)>,
        runtime: Option<DatasetSpec>,
    ) {
        let retriever = StoreRetriever::new(store.clone(), embedder.clone()).with_reranker(reranker);
        let entry = DatasetEntry {
            info,
            retriever: Arc::new(retriever),
            store: Some((store, embedder)),
            chunking,
            names,
            runtime,
        };
        self.write().insert(entry.info.id.clone(), entry);
    }

    /// Add a dataset served by an external retriever.
    pub fn with_retriever(mut self, id: impl Into<String>, retriever: Arc<dyn Retriever>) -> Self {
        let id = id.into();
        self.add_retriever(DatasetInfo { id, ..DatasetInfo::default() }, retriever);
        self
    }

    /// Add (or replace) a dataset served by an external retriever.
    pub fn add_retriever(&mut self, info: DatasetInfo, retriever: Arc<dyn Retriever>) -> &mut Self {
        let entry = DatasetEntry {
            info,
            retriever,
            store: None,
            chunking: ChunkerConfig::default(),
            names: None,
            runtime: None,
        };
        self.write().insert(entry.info.id.clone(), entry);
        self
    }

    /// Reranker applied to the merged results of multi-dataset retrieval.
    pub fn with_reranker(mut self, reranker: Option<Arc<dyn Reranker>>) -> Self {
        self.reranker = reranker;
        self
    }

    /// Every dataset's id, name and description, in registration order.
    pub fn datasets(&self) -> Vec<DatasetInfo> {
        self.read().values().map(|d| d.info.clone()).collect()
    }

    /// Every dataset with what a management UI shows about it.
    pub fn dataset_details(&self) -> Vec<DatasetDetails> {
        self.read()
            .values()
            .map(|d| DatasetDetails {
                info: d.info.clone(),
                managed: d.store.is_some(),
                runtime: d.runtime.is_some(),
                store: d.names.as_ref().map(|(store, _)| store.clone()),
                embedder: d.names.as_ref().map(|(_, embedder)| embedder.clone()),
                chunking: d.store.as_ref().map(|_| d.chunking.clone()),
            })
            .collect()
    }

    /// The stores, embedders and rerankers [`KnowledgeBase::create_dataset`] can use.
    pub fn resource_names(&self) -> ResourceNames {
        ResourceNames {
            stores: self.resources.stores.keys().cloned().collect(),
            embedders: self.resources.embedders.keys().cloned().collect(),
            rerankers: self.resources.rerankers.keys().cloned().collect(),
        }
    }

    /// Create a store-backed dataset at runtime from the named resources of the
    /// knowledge config. Every clone of this knowledge base sees it at once.
    ///
    /// @throws `RagError::Input` for an invalid or taken id, an unknown store,
    /// embedder or reranker name, or invalid chunking.
    pub fn create_dataset(&self, spec: DatasetSpec) -> Result<DatasetDetails, RagError> {
        validate_dataset_id(&spec.id)?;
        let lookup = |kind: &str, name: &str, found: bool| {
            if found {
                Ok(())
            } else {
                Err(RagError::Input(format!("unknown {kind} `{name}` in the knowledge config")))
            }
        };
        let store = self.resources.stores.get(&spec.store).cloned();
        lookup("store", &spec.store, store.is_some())?;
        let embedder = self.resources.embedders.get(&spec.embedder).cloned();
        lookup("embedder", &spec.embedder, embedder.is_some())?;
        let reranker = match &spec.reranker {
            Some(name) => {
                let reranker = self.resources.rerankers.get(name).cloned();
                lookup("reranker", name, reranker.is_some())?;
                reranker
            }
            None => None,
        };
        let chunking = spec.chunking.clone().unwrap_or_default();
        validate_chunking(&spec.id, &chunking).map_err(|e| RagError::Input(e.to_string()))?;
        let (Some(store), Some(embedder)) = (store, embedder) else {
            return Err(RagError::Input("store and embedder are required".into()));
        };
        let info = DatasetInfo { id: spec.id.clone(), name: spec.name.clone(), description: spec.description.clone() };
        {
            // Check and insert under one lock so two requests cannot create the same id.
            let mut registry = self.write();
            if registry.contains_key(&spec.id) {
                return Err(RagError::Input(format!("dataset `{}` already exists", spec.id)));
            }
            let retriever = StoreRetriever::new(store.clone(), embedder.clone()).with_reranker(reranker);
            registry.insert(
                spec.id.clone(),
                DatasetEntry {
                    info,
                    retriever: Arc::new(retriever),
                    store: Some((store, embedder)),
                    chunking,
                    names: Some((spec.store.clone(), spec.embedder.clone())),
                    runtime: Some(spec.clone()),
                },
            );
        }
        self.dataset_details()
            .into_iter()
            .find(|d| d.info.id == spec.id)
            .ok_or_else(|| RagError::Store(format!("dataset `{}` vanished after creation", spec.id)))
    }

    /// Delete a dataset created at runtime together with its documents.
    /// Returns the number of chunks removed.
    ///
    /// @throws `RagError::NotFound` for an unknown dataset, `RagError::Unsupported`
    /// for one declared in the knowledge config (edit the config instead).
    pub async fn remove_dataset(&self, dataset_id: &str) -> Result<usize, RagError> {
        let entry = self.entry(dataset_id)?;
        if entry.runtime.is_none() {
            return Err(RagError::Unsupported(format!(
                "dataset `{dataset_id}` is declared in the knowledge config; remove it there"
            )));
        }
        // Delete the data first: if that fails the dataset stays, instead of leaving orphaned chunks.
        let removed = match &entry.store {
            Some((store, _)) => store.delete_dataset(dataset_id).await?,
            None => 0,
        };
        self.write().shift_remove(dataset_id);
        Ok(removed)
    }

    /// Specs of the datasets created at runtime, for persisting them.
    pub fn runtime_datasets(&self) -> Vec<DatasetSpec> {
        self.read().values().filter_map(|d| d.runtime.clone()).collect()
    }

    pub fn contains(&self, dataset_id: &str) -> bool {
        self.read().contains_key(dataset_id)
    }

    /// An ingestor for a store-backed dataset (`None` for external retrievers or unknown ids),
    /// using the dataset's chunking settings.
    pub fn ingestor(&self, dataset_id: &str) -> Option<Ingestor> {
        let registry = self.read();
        let entry = registry.get(dataset_id)?;
        let (store, embedder) = entry.store.as_ref()?;
        Some(Ingestor::new(store.clone(), embedder.clone()).chunker(entry.chunking.clone()))
    }

    /// Set how documents ingested into `dataset_id` are chunked (e.g. parent-child).
    ///
    /// @throws `RagError::NotFound` for an unknown dataset, `RagError::Config` when
    /// `parent_max_chars` is not larger than `max_chars`.
    pub fn set_chunking(&mut self, dataset_id: &str, chunking: ChunkerConfig) -> Result<&mut Self, RagError> {
        validate_chunking(dataset_id, &chunking)?;
        {
            let mut registry = self.write();
            let entry = registry
                .get_mut(dataset_id)
                .ok_or_else(|| RagError::NotFound(format!("unknown dataset `{dataset_id}`")))?;
            entry.chunking = chunking;
        }
        Ok(self)
    }

    /// The chunking settings of a dataset.
    pub fn chunking(&self, dataset_id: &str) -> Option<ChunkerConfig> {
        self.read().get(dataset_id).map(|d| d.chunking.clone())
    }

    /// A copy of a dataset's entry (cheap: shared handles), so no lock is held across awaits.
    fn entry(&self, id: &str) -> Result<DatasetEntry, RagError> {
        self.read().get(id).cloned().ok_or_else(|| RagError::NotFound(format!("unknown dataset `{id}`")))
    }

    /// Whether documents can be added to, listed in and removed from this dataset
    /// (store-backed datasets; `false` for external retrievers and unknown ids).
    pub fn is_managed(&self, dataset_id: &str) -> bool {
        self.read().get(dataset_id).is_some_and(|d| d.store.is_some())
    }

    /// The store of a store-backed dataset.
    ///
    /// @throws `RagError::NotFound` for an unknown dataset, `RagError::Unsupported`
    /// for one served by an external retriever.
    fn managed_store(&self, dataset_id: &str) -> Result<Arc<dyn VectorStore>, RagError> {
        let entry = self.entry(dataset_id)?;
        entry
            .store
            .map(|(store, _)| store)
            .ok_or_else(|| RagError::Unsupported(format!("dataset `{dataset_id}` is served by an external retriever")))
    }

    /// Up to `limit` documents of a dataset (capped at [`MAX_DOCUMENT_PAGE`]),
    /// ordered by id and starting after the `after` id.
    pub async fn list_documents(
        &self,
        dataset_id: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<DocumentSummary>, RagError> {
        let limit = limit.clamp(1, MAX_DOCUMENT_PAGE);
        self.managed_store(dataset_id)?.list_documents(dataset_id, limit, after).await
    }

    /// Chunks of one document in chunk order (vectors included).
    pub async fn document_chunks(&self, dataset_id: &str, document_id: &str) -> Result<Vec<StoredChunk>, RagError> {
        let mut chunks = self.managed_store(dataset_id)?.document_chunks(dataset_id, document_id).await?;
        chunks.sort_by_key(|c| c.metadata.get("chunk_index").and_then(serde_json::Value::as_u64).unwrap_or(u64::MAX));
        Ok(chunks)
    }

    /// Enable or disable a document: a disabled document keeps its chunks (and
    /// stays disabled across re-ingestion) but no search returns it. Returns the
    /// number of chunks of the document.
    ///
    /// @throws `RagError::NotFound` when the document does not exist.
    pub async fn set_document_enabled(
        &self,
        dataset_id: &str,
        document_id: &str,
        enabled: bool,
    ) -> Result<usize, RagError> {
        let store = self.managed_store(dataset_id)?;
        let mut chunks = store.document_chunks(dataset_id, document_id).await?;
        if chunks.is_empty() {
            return Err(RagError::NotFound(format!("unknown document `{document_id}` in dataset `{dataset_id}`")));
        }
        let count = chunks.len();
        chunks.retain(|c| crate::types::is_disabled(&c.metadata) == enabled);
        for chunk in &mut chunks {
            if enabled {
                chunk.metadata.remove(crate::types::DISABLED_KEY);
            } else {
                chunk.metadata.insert(crate::types::DISABLED_KEY.into(), serde_json::Value::Bool(true));
            }
        }
        if !chunks.is_empty() {
            store.upsert(chunks).await?;
        }
        Ok(count)
    }

    /// Remove a document. Returns the number of chunks removed (0 when it did not exist).
    pub async fn delete_document(&self, dataset_id: &str, document_id: &str) -> Result<usize, RagError> {
        self.managed_store(dataset_id)?.delete_document(dataset_id, document_id).await
    }

    /// Query every dataset concurrently. Returns `(dataset id, hits)` in request order.
    async fn query_each(
        &self,
        request: &RetrievalRequest,
        rerank_each: bool,
    ) -> Result<Vec<(String, Vec<Hit>)>, RagError> {
        let mut jobs = Vec::with_capacity(request.datasets.len());
        for id in &request.datasets {
            let retriever = self.entry(id)?.retriever;
            let sub = RetrievalRequest {
                datasets: vec![id.clone()],
                // Threshold and top-k are applied after merging.
                score_threshold: None,
                rerank: rerank_each,
                ..request.clone()
            };
            jobs.push(async move { retriever.retrieve(&sub).await.map(|hits| (id.clone(), hits)) });
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

/// Runtime dataset ids appear in URLs and file names: 1-64 ASCII letters,
/// digits, `-`, `_` or `.`, not starting with `.`.
fn validate_dataset_id(id: &str) -> Result<(), RagError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(RagError::Input(format!(
            "invalid dataset id `{id}`: use 1-64 ASCII letters, digits, `-`, `_` or `.`, not starting with `.`"
        )))
    }
}

fn validate_chunking(dataset_id: &str, chunking: &ChunkerConfig) -> Result<(), RagError> {
    if chunking.max_chars == 0 {
        return Err(RagError::Config(format!("dataset `{dataset_id}`: `chunking.max_chars` must be positive")));
    }
    if chunking.parent_max_chars.is_some_and(|p| p <= chunking.max_chars) {
        return Err(RagError::Config(format!(
            "dataset `{dataset_id}`: `chunking.parent_max_chars` must be larger than `max_chars`"
        )));
    }
    Ok(())
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
    /// Chunking of ingested documents, e.g. `{ max_chars: 400, parent_max_chars: 1500 }`
    /// for parent-child retrieval (store-backed datasets).
    #[serde(default)]
    pub chunking: Option<ChunkerConfig>,
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
            let chunking = ds.chunking.clone().unwrap_or_default();
            validate_chunking(&ds.id, &chunking)?;
            let names = Some((store_key.to_owned(), embedder_key.to_owned()));
            kb.insert_store_dataset(info, store, embedder, lookup_reranker(&ds.reranker)?, chunking, names, None);
        }
        kb.resources = Arc::new(Resources {
            stores: stores.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
            embedders: embedders.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
            rerankers: rerankers.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        });
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
    async fn documents_can_be_listed_paged_inspected_and_deleted() {
        let kb = kb_with_two_datasets();
        let ingestor = kb.ingestor("pets").unwrap();
        let mut meta = crate::types::Metadata::new();
        meta.insert("lang".into(), serde_json::json!("en"));
        let long = "Cats purr. ".repeat(200);
        ingestor.ingest(&Document::new("pets", "cats", long).title("Cats").metadata(meta)).await.unwrap();
        ingest(&kb, "pets", "dogs", "Dogs bark.").await;
        ingest(&kb, "pets", "birds", "Birds sing.").await;

        let first = kb.list_documents("pets", 2, None).await.unwrap();
        let ids: Vec<&str> = first.iter().map(|d| d.document_id.as_str()).collect();
        assert_eq!(ids, ["birds", "cats"]);
        let cats = &first[1];
        assert_eq!(cats.title, "Cats");
        assert!(cats.chunks > 1, "long document is split");
        assert_eq!(cats.metadata.get("lang"), Some(&serde_json::json!("en")));
        assert!(!cats.metadata.contains_key("chunk_index"), "per-chunk keys are not document metadata");
        let next = kb.list_documents("pets", 2, Some("cats")).await.unwrap();
        assert_eq!(next.iter().map(|d| d.document_id.as_str()).collect::<Vec<_>>(), ["dogs"]);

        let chunks = kb.document_chunks("pets", "cats").await.unwrap();
        assert_eq!(chunks.len(), cats.chunks);
        let order: Vec<u64> = chunks.iter().map(|c| c.metadata["chunk_index"].as_u64().unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "chunks come in document order: {order:?}");

        assert_eq!(kb.delete_document("pets", "cats").await.unwrap(), cats.chunks);
        assert_eq!(kb.delete_document("pets", "cats").await.unwrap(), 0);
        assert_eq!(kb.list_documents("pets", 50, None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn disabled_documents_are_kept_but_never_retrieved() {
        let kb = kb_with_two_datasets();
        ingest(&kb, "pets", "cats", "Cats are small furry pets that purr.").await;
        ingest(&kb, "pets", "dogs", "Dogs are loyal pets that bark.").await;
        let query = RetrievalRequest::new("pets that purr", vec!["pets".into()]).top_k(5);
        let found = |hits: &[Hit]| hits.iter().any(|h| h.document_id == "cats");
        assert!(found(&kb.retrieve(&query).await.unwrap()));

        assert!(kb.set_document_enabled("pets", "cats", false).await.unwrap() >= 1);
        assert!(!found(&kb.retrieve(&query).await.unwrap()), "disabled document retrieved");
        let docs = kb.list_documents("pets", 10, None).await.unwrap();
        let cats = docs.iter().find(|d| d.document_id == "cats").unwrap();
        assert!(!cats.enabled);
        assert!(!cats.metadata.contains_key(crate::types::DISABLED_KEY), "flag is not document metadata");

        // A new version of a disabled document stays disabled.
        ingest(&kb, "pets", "cats", "Cats are small furry pets that purr and nap.").await;
        assert!(!found(&kb.retrieve(&query).await.unwrap()), "re-ingestion re-enabled the document");

        kb.set_document_enabled("pets", "cats", true).await.unwrap();
        assert!(found(&kb.retrieve(&query).await.unwrap()));
        assert_eq!(kb.set_document_enabled("pets", "nope", false).await.unwrap_err().kind(), "not_found");
    }

    #[tokio::test]
    async fn parent_child_datasets_return_whole_parents_once() {
        let mut kb = KnowledgeBase::new().with_store_dataset(
            "docs",
            Arc::new(MemoryStore::new()),
            Arc::new(HashEmbedder::new(256)),
            None,
        );
        let chunking = ChunkerConfig { max_chars: 60, overlap: 0, markdown: false, parent_max_chars: Some(220) };
        kb.set_chunking("docs", chunking).unwrap();
        let text = "Leave policy. Every employee gets 24 days of paid annual leave. Unused leave carries over for six months. Sick leave is separate.\n\n\
                    Expense policy. Expenses below 500 USD are approved by your manager. Larger ones need finance approval.";
        ingest(&kb, "docs", "handbook", text).await;
        let children = kb.document_chunks("docs", "handbook").await.unwrap();
        assert!(children.len() > 2, "the document is split into several children");

        let request = RetrievalRequest::new("annual leave days carries over", vec!["docs".into()]).top_k(5);
        let hits = kb.retrieve(&request).await.unwrap();
        let leave: Vec<&Hit> = hits.iter().filter(|h| h.content.contains("24 days")).collect();
        assert_eq!(leave.len(), 1, "children of one parent collapse into one hit: {hits:?}");
        let parent = &leave[0].content;
        assert!(parent.contains("Leave policy") && parent.contains("Sick leave"), "whole parent returned: {parent}");
        assert!(!parent.contains("Expense"), "parents stay separate: {parent}");
    }

    #[test]
    fn chunking_config_is_validated() {
        let build = |chunking: serde_json::Value| {
            KnowledgeConfig::from_json(
                &serde_json::json!({
                    "embedders": { "e": { "type": "hash" } },
                    "stores": { "m": { "type": "memory" } },
                    "datasets": [{ "id": "d", "store": "m", "embedder": "e", "chunking": chunking }]
                })
                .to_string(),
            )
            .unwrap()
            .build_with_env(&|_| None)
        };
        let kb = build(serde_json::json!({ "max_chars": 300, "parent_max_chars": 1200 })).unwrap();
        assert_eq!(kb.chunking("d").unwrap().parent_max_chars, Some(1200));
        let error = build(serde_json::json!({ "max_chars": 300, "parent_max_chars": 200 })).unwrap_err();
        assert!(error.to_string().contains("parent_max_chars"), "{error}");
    }

    fn config_kb() -> KnowledgeBase {
        KnowledgeConfig::from_json(
            &serde_json::json!({
                "embedders": { "e": { "type": "hash", "dimensions": 64 } },
                "stores": { "shared": { "type": "memory" } },
                "datasets": [{ "id": "static", "store": "shared", "embedder": "e" }]
            })
            .to_string(),
        )
        .unwrap()
        .build_with_env(&|_| None)
        .unwrap()
    }

    fn spec(id: &str) -> DatasetSpec {
        DatasetSpec {
            id: id.into(),
            name: "Runtime".into(),
            description: String::new(),
            store: "shared".into(),
            embedder: "e".into(),
            reranker: None,
            chunking: None,
        }
    }

    #[tokio::test]
    async fn runtime_datasets_are_shared_by_clones_and_removable() {
        let api = config_kb();
        let workflows = api.clone();
        assert_eq!(api.resource_names().stores, ["shared"]);

        let created = api.create_dataset(spec("notes")).unwrap();
        assert!(created.runtime && created.managed);
        assert_eq!(created.store.as_deref(), Some("shared"));
        assert!(workflows.contains("notes"), "clones see runtime datasets");

        ingest(&api, "notes", "n1", "Runtime datasets work like any other dataset.").await;
        ingest(&api, "static", "s1", "The static dataset shares the store.").await;
        let hits = workflows.retrieve(&RetrievalRequest::new("runtime datasets", vec!["notes".into()])).await.unwrap();
        assert_eq!(hits.first().map(|h| h.document_id.as_str()), Some("n1"));

        assert_eq!(api.runtime_datasets(), vec![spec("notes")]);
        assert_eq!(api.remove_dataset("notes").await.unwrap(), 1);
        assert!(!workflows.contains("notes"));
        assert_eq!(api.list_documents("static", 10, None).await.unwrap().len(), 1, "other datasets keep their data");
        assert_eq!(api.remove_dataset("static").await.unwrap_err().kind(), "unsupported");
        assert_eq!(api.remove_dataset("missing").await.unwrap_err().kind(), "not_found");
    }

    #[test]
    fn runtime_dataset_specs_are_validated() {
        let kb = config_kb();
        let error = |spec: DatasetSpec| kb.create_dataset(spec).unwrap_err().to_string();
        assert!(error(spec("static")).contains("already exists"));
        assert!(error(spec("bad/id")).contains("invalid dataset id"));
        assert!(error(spec("")).contains("invalid dataset id"));
        assert!(error(DatasetSpec { store: "nope".into(), ..spec("a") }).contains("unknown store `nope`"));
        assert!(error(DatasetSpec { embedder: "nope".into(), ..spec("b") }).contains("unknown embedder `nope`"));
        assert!(error(DatasetSpec { reranker: Some("nope".into()), ..spec("c") }).contains("unknown reranker"));
        let bad_chunking = ChunkerConfig { max_chars: 100, overlap: 0, markdown: true, parent_max_chars: Some(50) };
        assert!(error(DatasetSpec { chunking: Some(bad_chunking), ..spec("d") }).contains("parent_max_chars"));
        assert_eq!(kb.datasets().len(), 1, "failed creations leave nothing behind");
    }

    #[tokio::test]
    async fn external_and_unknown_datasets_are_not_managed() {
        let store_kb = kb_with_two_datasets();
        let retriever = store_kb.entry("pets").unwrap().retriever;
        let kb = KnowledgeBase::new().with_retriever("remote", retriever);
        assert!(!kb.is_managed("remote"));
        assert_eq!(kb.list_documents("remote", 10, None).await.unwrap_err().kind(), "unsupported");
        assert_eq!(kb.delete_document("remote", "x").await.unwrap_err().kind(), "unsupported");
        assert_eq!(kb.list_documents("missing", 10, None).await.unwrap_err().kind(), "not_found");
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
