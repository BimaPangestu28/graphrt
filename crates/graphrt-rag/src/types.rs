//! Core retrieval types and the pluggable traits.

use std::fmt;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::error::RagError;
use crate::filter::MetadataFilter;

/// Free-form chunk / document metadata.
pub type Metadata = serde_json::Map<String, serde_json::Value>;

/// A chunk as stored in a [`VectorStore`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredChunk {
    /// Unique id within the store.
    pub id: String,
    pub dataset_id: String,
    pub document_id: String,
    #[serde(default)]
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub metadata: Metadata,
    /// Stable hash of `dataset_id` + `text`, used for de-duplication.
    #[serde(default)]
    pub content_hash: String,
    pub vector: Vec<f32>,
}

/// One retrieved chunk with its relevance score (higher is better).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub id: String,
    pub dataset_id: String,
    #[serde(default)]
    pub document_id: String,
    #[serde(default)]
    pub title: String,
    pub content: String,
    pub score: f32,
    #[serde(default)]
    pub metadata: Metadata,
}

impl Hit {
    /// Build a hit from a stored chunk.
    pub fn from_chunk(chunk: &StoredChunk, score: f32) -> Self {
        Self {
            id: chunk.id.clone(),
            dataset_id: chunk.dataset_id.clone(),
            document_id: chunk.document_id.clone(),
            title: chunk.title.clone(),
            content: chunk.text.clone(),
            score,
            metadata: chunk.metadata.clone(),
        }
    }
}

/// One document of a dataset, as listed by [`VectorStore::list_documents`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocumentSummary {
    pub document_id: String,
    #[serde(default)]
    pub title: String,
    /// Number of stored chunks.
    pub chunks: usize,
    /// Total characters across the chunks.
    pub characters: usize,
    /// `false` when the document is disabled (kept, but skipped by searches).
    pub enabled: bool,
    /// Document-level metadata (the per-chunk `chunk_index` and `heading` keys are left out).
    #[serde(default)]
    pub metadata: Metadata,
}

/// Reserved chunk metadata key marking a disabled document: its chunks stay
/// stored but every search skips them.
pub const DISABLED_KEY: &str = "_disabled";

/// Whether chunk metadata marks its document as disabled.
pub fn is_disabled(metadata: &Metadata) -> bool {
    metadata.get(DISABLED_KEY).and_then(serde_json::Value::as_bool).unwrap_or(false)
}

/// Chunk metadata keys managed per chunk by graphrt, not part of a document's metadata.
pub(crate) const PER_CHUNK_KEYS: [&str; 3] = ["chunk_index", "heading", DISABLED_KEY];

/// Default and largest page size for document listings.
pub const DEFAULT_DOCUMENT_PAGE: usize = 50;
pub const MAX_DOCUMENT_PAGE: usize = 500;

/// Vector similarity query.
#[derive(Clone, Debug)]
pub struct VectorQuery<'a> {
    pub datasets: &'a [String],
    pub vector: &'a [f32],
    pub top_k: usize,
    pub filter: Option<&'a MetadataFilter>,
}

/// Keyword (BM25) query.
#[derive(Clone, Debug)]
pub struct KeywordQuery<'a> {
    pub datasets: &'a [String],
    pub text: &'a str,
    pub top_k: usize,
    pub filter: Option<&'a MetadataFilter>,
}

/// Turns text into embedding vectors.
pub trait Embedder: Send + Sync + fmt::Debug {
    /// Model identifier (used to detect mixing incompatible embeddings).
    fn model(&self) -> &str;

    /// Vector dimensionality, when known up front.
    fn dimensions(&self) -> Option<usize> {
        None
    }

    /// Embed documents for indexing. Returns one vector per input, in order.
    fn embed_documents<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>, RagError>>;

    /// Embed a search query. Defaults to [`Embedder::embed_documents`] on one text;
    /// override for models that need a query-specific prefix or endpoint.
    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, RagError>> {
        Box::pin(async move {
            let texts = [text.to_owned()];
            let mut vectors = self.embed_documents(&texts).await?;
            vectors.pop().ok_or_else(|| RagError::Decode("embedder returned no vector".into()))
        })
    }
}

/// Stores chunks with vectors and answers similarity queries.
pub trait VectorStore: Send + Sync + fmt::Debug {
    /// Insert or replace chunks (by `id`).
    fn upsert(&self, chunks: Vec<StoredChunk>) -> BoxFuture<'_, Result<(), RagError>>;

    /// Remove every chunk of a document. Returns the number removed when known.
    fn delete_document<'a>(
        &'a self,
        dataset_id: &'a str,
        document_id: &'a str,
    ) -> BoxFuture<'a, Result<usize, RagError>>;

    /// Remove every chunk of a dataset.
    fn delete_dataset<'a>(&'a self, dataset_id: &'a str) -> BoxFuture<'a, Result<usize, RagError>>;

    /// Every chunk of one document, vectors included. The ingestor uses this to
    /// re-ingest a document without re-embedding unchanged chunks and to find the
    /// chunks a new version no longer contains.
    fn document_chunks<'a>(
        &'a self,
        dataset_id: &'a str,
        document_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<StoredChunk>, RagError>>;

    /// Remove chunks by id. Returns the number removed when known.
    fn delete_chunks<'a>(&'a self, dataset_id: &'a str, ids: &'a [String]) -> BoxFuture<'a, Result<usize, RagError>>;

    /// Up to `limit` documents of a dataset ordered by document id, starting after
    /// `after` (a document id from the previous page). Stores that cannot list
    /// return [`RagError::Unsupported`].
    fn list_documents<'a>(
        &'a self,
        dataset_id: &'a str,
        limit: usize,
        after: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<DocumentSummary>, RagError>> {
        let _ = (dataset_id, limit, after);
        Box::pin(futures::future::ready(Err(RagError::Unsupported("this store cannot list documents".into()))))
    }

    /// Nearest neighbours by cosine similarity, best first.
    fn search<'a>(&'a self, query: VectorQuery<'a>) -> BoxFuture<'a, Result<Vec<Hit>, RagError>>;

    /// Whether [`VectorStore::keyword_search`] is implemented.
    fn supports_keyword(&self) -> bool {
        false
    }

    /// Keyword (BM25) search, best first. Stores without a keyword index return nothing.
    fn keyword_search<'a>(&'a self, query: KeywordQuery<'a>) -> BoxFuture<'a, Result<Vec<Hit>, RagError>> {
        let _ = query;
        Box::pin(futures::future::ready(Ok(Vec::new())))
    }

    /// Whether a chunk with this content hash already exists in the dataset.
    /// Stores that cannot answer cheaply return `false`. The built-in [`crate::Ingestor`]
    /// does not use this (it de-duplicates per document via [`VectorStore::document_chunks`]).
    fn contains_hash<'a>(
        &'a self,
        dataset_id: &'a str,
        content_hash: &'a str,
    ) -> BoxFuture<'a, Result<bool, RagError>> {
        let _ = (dataset_id, content_hash);
        Box::pin(futures::future::ready(Ok(false)))
    }
}

/// Re-scores documents against a query (cross-encoder style).
pub trait Reranker: Send + Sync + fmt::Debug {
    /// Return one relevance score per document, in input order.
    fn rerank<'a>(&'a self, query: &'a str, documents: &'a [String]) -> BoxFuture<'a, Result<Vec<f32>, RagError>>;
}

/// How vector and keyword scores are blended.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HybridWeights {
    pub vector: f32,
    pub keyword: f32,
}

impl Default for HybridWeights {
    fn default() -> Self {
        Self { vector: 1.0, keyword: 0.0 }
    }
}

/// A high-level retrieval request.
#[derive(Clone, Debug, PartialEq)]
pub struct RetrievalRequest {
    pub query: String,
    pub datasets: Vec<String>,
    pub top_k: usize,
    /// Hits scoring below this are dropped (after reranking, when enabled).
    pub score_threshold: Option<f32>,
    pub filter: Option<MetadataFilter>,
    pub weights: HybridWeights,
    /// Apply the dataset's reranker, when one is configured.
    pub rerank: bool,
}

impl RetrievalRequest {
    pub fn new(query: impl Into<String>, datasets: Vec<String>) -> Self {
        Self {
            query: query.into(),
            datasets,
            top_k: 4,
            score_threshold: None,
            filter: None,
            weights: HybridWeights::default(),
            rerank: false,
        }
    }

    pub fn top_k(mut self, top_k: usize) -> Self {
        self.top_k = top_k;
        self
    }

    pub fn threshold(mut self, threshold: Option<f32>) -> Self {
        self.score_threshold = threshold;
        self
    }

    pub fn filter(mut self, filter: Option<MetadataFilter>) -> Self {
        self.filter = filter;
        self
    }

    pub fn weights(mut self, weights: HybridWeights) -> Self {
        self.weights = weights;
        self
    }

    pub fn rerank(mut self, rerank: bool) -> Self {
        self.rerank = rerank;
        self
    }
}

/// End-to-end retrieval: query in, ranked hits out. Implement this to plug an
/// external RAG service in place of the built-in store pipeline.
pub trait Retriever: Send + Sync + fmt::Debug {
    fn retrieve<'a>(&'a self, request: &'a RetrievalRequest) -> BoxFuture<'a, Result<Vec<Hit>, RagError>>;
}

/// Turns raw document bytes into text for ingestion (PDF, DOCX, ... extractors plug in here).
pub trait Extractor: Send + Sync + fmt::Debug {
    fn extract(&self, bytes: &[u8], mime_type: Option<&str>, file_name: &str) -> Result<String, RagError>;
}

/// Extractor for UTF-8 text formats (plain text, Markdown, JSON, CSV, HTML as-is).
#[derive(Clone, Copy, Debug, Default)]
pub struct PlainTextExtractor;

impl Extractor for PlainTextExtractor {
    fn extract(&self, bytes: &[u8], mime_type: Option<&str>, file_name: &str) -> Result<String, RagError> {
        let textual = mime_type.is_none_or(|m| {
            m.starts_with("text/")
                || m.contains("json")
                || m.contains("xml")
                || m.contains("markdown")
                || m.contains("csv")
        });
        if !textual {
            return Err(RagError::Input(format!(
                "`{file_name}` ({}) is not a text format; register a document extractor",
                mime_type.unwrap_or("unknown")
            )));
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| RagError::Input(format!("`{file_name}` is not valid UTF-8")))
    }
}

/// Check that a vector can be stored or searched: non-empty and finite.
pub fn check_vector(vector: &[f32], what: &str) -> Result<(), RagError> {
    if vector.is_empty() {
        return Err(RagError::Input(format!("{what} has an empty embedding vector")));
    }
    if vector.iter().any(|x| !x.is_finite()) {
        return Err(RagError::Input(format!("{what} has a non-finite (NaN or infinite) embedding value")));
    }
    Ok(())
}

/// Stable 64-bit FNV-1a hash rendered as hex.
pub fn content_hash(dataset_id: &str, text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in dataset_id.bytes().chain(std::iter::once(0)).chain(text.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}
