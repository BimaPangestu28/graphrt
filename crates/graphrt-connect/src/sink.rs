//! [`KnowledgeSink`]: synced documents go into a graphrt-rag knowledge base,
//! through the same extraction, chunking and embedding as uploaded files
//! (graphrt-doc text extraction, then the dataset's [`graphrt_rag::Ingestor`]
//! with its chunking settings). There is no second indexing pipeline.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_rag::{DocExtractor, Document, Extractor, KnowledgeBase, Metadata, RagError};
use tokio::sync::Semaphore;

use crate::config::SourceConfig;
use crate::error::{ConnectError, Result};
use crate::sync::{DocumentSink, SyncDocument};
use crate::types::ItemContent;

/// Metadata keys written on every synced chunk.
pub const METADATA_SOURCE: &str = "source_id";
pub const METADATA_ITEM: &str = "source_item";
pub const METADATA_URL: &str = "url";

/// Indexes synced documents into the dataset of their source.
#[derive(Clone, Debug)]
pub struct KnowledgeSink {
    kb: KnowledgeBase,
    extractor: Arc<dyn Extractor>,
    /// Shared with other ingestion (uploads), so embedding runs one at a time.
    gate: Option<Arc<Semaphore>>,
}

fn rag_error(error: RagError) -> ConnectError {
    match error {
        RagError::Auth(message) => ConnectError::Auth(format!("the embedding service refused the key: {message}")),
        RagError::RateLimit { .. } => ConnectError::RateLimited("the embedding service asked to slow down".into()),
        RagError::Input(message) => ConnectError::Decode(message),
        other => ConnectError::Store(other.to_string()),
    }
}

impl KnowledgeSink {
    pub fn new(kb: KnowledgeBase) -> Self {
        Self { kb, extractor: Arc::new(DocExtractor::new()), gate: None }
    }

    /// Share an ingestion queue with other writers of the knowledge base.
    pub fn with_gate(mut self, gate: Arc<Semaphore>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Use another text extractor.
    pub fn with_extractor(mut self, extractor: Arc<dyn Extractor>) -> Self {
        self.extractor = extractor;
        self
    }

    async fn text(&self, content: ItemContent) -> Result<String> {
        let text = match content {
            ItemContent::Text(text) => text,
            ItemContent::File { bytes, mime_type, file_name } => {
                let extractor = Arc::clone(&self.extractor);
                // Extraction parses arbitrary formats: keep it off the async workers.
                tokio::task::spawn_blocking(move || extractor.extract(&bytes, mime_type.as_deref(), &file_name))
                    .await
                    .map_err(|_| ConnectError::Decode("text extraction stopped unexpectedly".into()))?
                    .map_err(rag_error)?
            }
        };
        if text.trim().is_empty() {
            return Err(ConnectError::Decode("no text found in it (scanned pages need OCR first)".into()));
        }
        Ok(text)
    }
}

impl DocumentSink for KnowledgeSink {
    fn upsert<'a>(&'a self, document: SyncDocument<'a>) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let dataset = &document.source.dataset;
            let text = self.text(document.content).await?;
            let ingestor = self.kb.ingestor(dataset).ok_or_else(|| {
                ConnectError::Config(format!(
                    "source `{}`: dataset `{dataset}` is unknown or cannot store documents",
                    document.source.id
                ))
            })?;
            let mut metadata = Metadata::new();
            metadata.insert(METADATA_SOURCE.into(), serde_json::Value::String(document.source.id.clone()));
            metadata.insert(METADATA_ITEM.into(), serde_json::Value::String(document.item.id.clone()));
            if let Some(url) = &document.item.url {
                metadata.insert(METADATA_URL.into(), serde_json::Value::String(url.clone()));
            }
            let doc = Document::new(dataset.as_str(), document.document_id.as_str(), text)
                .title(document.item.title.as_str())
                .metadata(metadata);
            let _turn = match &self.gate {
                Some(gate) => Some(
                    gate.acquire()
                        .await
                        .map_err(|_| ConnectError::Store("the knowledge base is shutting down".into()))?,
                ),
                None => None,
            };
            let report = ingestor.ingest(&doc).await.map_err(rag_error)?;
            Ok(u64::try_from(report.stored + report.skipped).unwrap_or(u64::MAX))
        })
    }

    fn remove<'a>(&'a self, source: &'a SourceConfig, document_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.kb.delete_document(&source.dataset, document_id).await {
                Ok(_) | Err(RagError::NotFound(_)) => Ok(()),
                Err(error) => Err(rag_error(error)),
            }
        })
    }
}
