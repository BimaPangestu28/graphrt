//! # graphrt-rag
//!
//! Retrieval building blocks for graphrt workflows and the `knowledge-retrieval` node.
//!
//! - [`Embedder`], [`VectorStore`], [`Reranker`], [`Retriever`]: pluggable traits.
//! - [`MemoryStore`]: in-process cosine + BM25 store (optionally file-backed).
//! - [`OpenAiEmbedder`], [`HttpReranker`], [`HttpRetriever`] (feature `http`) and
//!   [`QdrantStore`] (feature `qdrant`).
//! - [`chunk_text`] and [`Ingestor`] for indexing documents.
//! - [`KnowledgeBase`]: the service the node uses, built in code or from a
//!   [`KnowledgeConfig`] file.
//!
//! ```
//! use std::sync::Arc;
//! use graphrt_core::{Registry, Services};
//! use graphrt_rag::{Document, HashEmbedder, KnowledgeBase, MemoryStore};
//!
//! # futures::executor::block_on(async {
//! let kb = KnowledgeBase::new()
//!     .with_store_dataset("faq", Arc::new(MemoryStore::new()), Arc::new(HashEmbedder::new(256)), None);
//! kb.ingestor("faq").unwrap().ingest(&Document::new("faq", "returns", "Items can be returned within 30 days.")).await.unwrap();
//!
//! let mut registry = Registry::with_builtins();
//! graphrt_rag::register(&mut registry);
//! let services = Services::new().with(kb);
//! # let _ = services;
//! # });
//! ```

mod chunker;
mod error;
mod filter;
mod hash;
#[cfg(feature = "http")]
mod http;
mod ingest;
mod kb;
mod memory;
mod node;
mod pipeline;
#[cfg(feature = "qdrant")]
mod qdrant;
mod types;

#[cfg(feature = "doc")]
mod doc_extractor;
pub use chunker::{ChunkerConfig, TextChunk, chunk_text};
#[cfg(feature = "doc")]
pub use doc_extractor::DocExtractor;
pub use error::RagError;
pub use filter::{Logical, MetadataCondition, MetadataFilter, UnknownOperator};
pub use hash::HashEmbedder;
#[cfg(feature = "http")]
pub use http::{
    HttpReranker, HttpRerankerConfig, HttpRetriever, HttpRetrieverConfig, HttpSettings, OpenAiEmbedder,
    OpenAiEmbedderConfig, Secret,
};
pub use ingest::{Document, IngestReport, Ingestor};
pub use kb::{DatasetConfig, DatasetInfo, EnvLookup, KnowledgeBase, KnowledgeConfig};
pub use memory::{MemoryStore, cosine, dot, tokenize};
pub use node::{KIND, KnowledgeRetrievalNode, descriptor, register};
pub use pipeline::{StoreRetriever, finalize, merge_weighted, rerank};
#[cfg(feature = "qdrant")]
pub use qdrant::{QdrantConfig, QdrantStore};
pub use types::{
    Embedder, Extractor, Hit, HybridWeights, KeywordQuery, Metadata, PlainTextExtractor, Reranker, RetrievalRequest,
    Retriever, StoredChunk, VectorQuery, VectorStore, check_vector, content_hash,
};
