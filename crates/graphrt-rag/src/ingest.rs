//! Ingestion: document -> chunks -> embeddings -> store.

use std::sync::Arc;

use crate::chunker::{ChunkerConfig, chunk_text};
use crate::error::RagError;
use crate::types::{Embedder, Extractor, Metadata, StoredChunk, VectorStore, content_hash};

/// A document to ingest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    pub dataset_id: String,
    pub document_id: String,
    pub title: String,
    pub text: String,
    /// Copied onto every chunk (plus `heading` and `chunk_index`).
    pub metadata: Metadata,
}

impl Document {
    pub fn new(dataset_id: impl Into<String>, document_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            dataset_id: dataset_id.into(),
            document_id: document_id.into(),
            title: String::new(),
            text: text.into(),
            metadata: Metadata::new(),
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Outcome of an ingestion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// Chunks embedded and stored.
    pub stored: usize,
    /// Chunks skipped because identical content already exists in the dataset.
    pub skipped: usize,
}

/// Chunks, embeds (in batches) and stores documents.
#[derive(Clone, Debug)]
pub struct Ingestor {
    store: Arc<dyn VectorStore>,
    embedder: Arc<dyn Embedder>,
    chunker: ChunkerConfig,
    batch_size: usize,
    replace_existing: bool,
}

impl Ingestor {
    pub fn new(store: Arc<dyn VectorStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder, chunker: ChunkerConfig::default(), batch_size: 32, replace_existing: true }
    }

    pub fn chunker(mut self, config: ChunkerConfig) -> Self {
        self.chunker = config;
        self
    }

    /// Texts per embedding request (default 32).
    pub fn batch_size(mut self, size: usize) -> Self {
        self.batch_size = size.max(1);
        self
    }

    /// Delete the document's previous chunks before ingesting (default `true`).
    pub fn replace_existing(mut self, replace: bool) -> Self {
        self.replace_existing = replace;
        self
    }

    /// Ingest one text document.
    pub async fn ingest(&self, doc: &Document) -> Result<IngestReport, RagError> {
        if doc.dataset_id.is_empty() || doc.document_id.is_empty() {
            return Err(RagError::Input("document needs a dataset_id and a document_id".into()));
        }
        if self.replace_existing {
            self.store.delete_document(&doc.dataset_id, &doc.document_id).await?;
        }

        let mut pending: Vec<StoredChunk> = Vec::new();
        let mut report = IngestReport::default();
        for chunk in chunk_text(&doc.text, &self.chunker) {
            let hash = content_hash(&doc.dataset_id, &chunk.text);
            let duplicate = pending.iter().any(|c| c.content_hash == hash)
                || self.store.contains_hash(&doc.dataset_id, &hash).await?;
            if duplicate {
                report.skipped += 1;
                continue;
            }
            let mut metadata = doc.metadata.clone();
            metadata.insert("chunk_index".into(), serde_json::Value::from(chunk.index));
            if let Some(h) = &chunk.heading {
                metadata.insert("heading".into(), serde_json::Value::String(h.clone()));
            }
            pending.push(StoredChunk {
                id: format!("{}:{}:{}", doc.dataset_id, doc.document_id, chunk.index),
                dataset_id: doc.dataset_id.clone(),
                document_id: doc.document_id.clone(),
                title: doc.title.clone(),
                text: chunk.text,
                metadata,
                content_hash: hash,
                vector: Vec::new(),
            });
        }

        for batch in pending.chunks_mut(self.batch_size) {
            let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
            let vectors = self.embedder.embed_documents(&texts).await?;
            if vectors.len() != batch.len() {
                return Err(RagError::Decode(format!(
                    "embedder returned {} vectors for {} texts",
                    vectors.len(),
                    batch.len()
                )));
            }
            for (chunk, vector) in batch.iter_mut().zip(vectors) {
                chunk.vector = vector;
            }
        }
        report.stored = pending.len();
        if !pending.is_empty() {
            self.store.upsert(pending).await?;
        }
        Ok(report)
    }

    /// Extract text from raw bytes with `extractor`, then ingest it.
    pub async fn ingest_bytes(
        &self,
        doc: Document,
        bytes: &[u8],
        mime_type: Option<&str>,
        file_name: &str,
        extractor: &dyn Extractor,
    ) -> Result<IngestReport, RagError> {
        let text = extractor.extract(bytes, mime_type, file_name)?;
        self.ingest(&Document { text, ..doc }).await
    }
}
