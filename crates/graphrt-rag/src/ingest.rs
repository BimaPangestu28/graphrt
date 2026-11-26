//! Ingestion: document -> chunks -> embeddings -> store.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::chunker::{ChunkerConfig, chunk_text};
use crate::error::RagError;
use crate::pages::{PAGE_KEY, PAGES_KEY, PageTracker};
use crate::types::{Embedder, Extractor, Metadata, StoredChunk, VectorStore, check_vector, content_hash};

/// A document to ingest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Document {
    pub dataset_id: String,
    pub document_id: String,
    pub title: String,
    pub text: String,
    /// Copied onto every chunk (plus `heading`, `chunk_index`, and `page` /
    /// `pages` when the text has page markers, see [`crate::PAGE_MARKER_START`]).
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
    /// New or changed chunks that were embedded and stored.
    pub stored: usize,
    /// Chunks already present with the same content (their vectors are reused,
    /// metadata is refreshed), plus exact duplicates within the document.
    pub skipped: usize,
    /// Chunks of an earlier version of the document that were removed.
    pub removed: usize,
}

/// Chunks, embeds (in batches) and stores documents.
///
/// Chunk ids are content-addressed per document (`dataset:document:hash`), so
/// re-ingesting a changed document embeds only new text, keeps unchanged chunks
/// (refreshing their position and metadata), and — with
/// [`Ingestor::replace_existing`] — removes chunks the new version no longer
/// contains. The swap is ordered so a failure never loses the previous version:
/// everything is embedded first, then upserted, and only then are stale chunks
/// deleted. Identical text in two different documents is stored once per
/// document, so deleting one document never removes content from another.
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

    /// Remove chunks of the previous version that the new version no longer
    /// contains (default `true`). With `false`, ingestion only adds chunks.
    pub fn replace_existing(mut self, replace: bool) -> Self {
        self.replace_existing = replace;
        self
    }

    /// Ingest one text document.
    pub async fn ingest(&self, doc: &Document) -> Result<IngestReport, RagError> {
        if doc.dataset_id.is_empty() || doc.document_id.is_empty() {
            return Err(RagError::Input("document needs a dataset_id and a document_id".into()));
        }
        let existing: HashMap<String, StoredChunk> = self
            .store
            .document_chunks(&doc.dataset_id, &doc.document_id)
            .await?
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect();

        let mut report = IngestReport::default();
        let mut seen: HashSet<String> = HashSet::new();
        let mut fresh: Vec<StoredChunk> = Vec::new();
        let mut refreshed: Vec<StoredChunk> = Vec::new();
        let mut page_tracker = PageTracker::default();
        for mut chunk in chunk_text(&doc.text, &self.chunker) {
            let (stripped, pages) = page_tracker.take(&chunk.text);
            if let Some(text) = stripped {
                // Markers removed: a chunk that was only a marker has nothing to store.
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                chunk.text = trimmed.to_owned();
            }
            let hash = content_hash(&doc.dataset_id, &chunk.text);
            let id = format!("{}:{}:{hash}", doc.dataset_id, doc.document_id);
            if !seen.insert(id.clone()) {
                report.skipped += 1;
                continue;
            }
            let mut metadata = doc.metadata.clone();
            metadata.insert("chunk_index".into(), serde_json::Value::from(chunk.index));
            if let Some(h) = &chunk.heading {
                metadata.insert("heading".into(), serde_json::Value::String(h.clone()));
            }
            if let Some(first_page) = pages.first() {
                metadata.insert(PAGE_KEY.into(), serde_json::Value::from(*first_page));
                if pages.len() > 1 {
                    metadata.insert(PAGES_KEY.into(), serde_json::Value::from(pages.clone()));
                }
            }
            let mut stored = StoredChunk {
                id,
                dataset_id: doc.dataset_id.clone(),
                document_id: doc.document_id.clone(),
                title: doc.title.clone(),
                text: chunk.text,
                metadata,
                content_hash: hash,
                vector: Vec::new(),
            };
            match existing.get(&stored.id).filter(|old| !old.vector.is_empty() && old.text == stored.text) {
                Some(old) => {
                    report.skipped += 1;
                    if old.title != stored.title || old.metadata != stored.metadata {
                        stored.vector.clone_from(&old.vector);
                        refreshed.push(stored);
                    }
                }
                None => fresh.push(stored),
            }
        }

        // 1. Embed everything new. A failure here changes nothing in the store.
        for batch in fresh.chunks_mut(self.batch_size) {
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
                check_vector(&vector, &format!("embedding of chunk `{}`", chunk.id))?;
                chunk.vector = vector;
            }
        }
        report.stored = fresh.len();

        // 2. Store new and refreshed chunks next to the old version.
        fresh.append(&mut refreshed);
        if !fresh.is_empty() {
            self.store.upsert(fresh).await?;
        }

        // 3. Only now drop what the new version no longer contains.
        if self.replace_existing {
            let stale: Vec<String> = existing.keys().filter(|id| !seen.contains(*id)).cloned().collect();
            if !stale.is_empty() {
                report.removed = self.store.delete_chunks(&doc.dataset_id, &stale).await?;
            }
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
