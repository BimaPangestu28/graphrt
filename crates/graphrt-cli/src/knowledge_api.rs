//! `/v1/knowledge`: manage and test the knowledge base served with `--knowledge`.
//!
//! The same [`KnowledgeBase`] backs the workflows' `knowledge-retrieval` nodes,
//! so documents added here are searchable by running workflows immediately.
//!
//! Kept light for small devices: ingestion (text extraction and embedding) runs
//! one request at a time, file extraction runs on the blocking pool, uploads
//! have their own size limit and document listings are paged.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use graphrt_rag::{
    DEFAULT_DOCUMENT_PAGE, Document, HybridWeights, IngestReport, KnowledgeBase, Metadata, MetadataFilter, RagError,
    RetrievalRequest,
};
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::serve::{AppState, error_response};

/// The knowledge base exposed over HTTP, with its ingestion queue.
#[derive(Debug)]
pub struct KnowledgeService {
    kb: KnowledgeBase,
    /// One ingestion at a time: extraction and embedding are the heavy part.
    ingest: Semaphore,
}

impl KnowledgeService {
    pub fn new(kb: KnowledgeBase) -> Self {
        Self { kb, ingest: Semaphore::new(1) }
    }
}

fn not_served() -> Response {
    error_response(StatusCode::NOT_FOUND, "no knowledge base is served (start graphrt serve with --knowledge)")
}

/// Map a retrieval-layer error to an HTTP response.
fn rag_error(error: &RagError) -> Response {
    let status = match error {
        RagError::NotFound(_) => StatusCode::NOT_FOUND,
        RagError::Input(_) | RagError::Config(_) => StatusCode::BAD_REQUEST,
        RagError::Unsupported(_) => StatusCode::CONFLICT,
        RagError::Auth(_)
        | RagError::RateLimit { .. }
        | RagError::Http { .. }
        | RagError::Network(_)
        | RagError::Decode(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = error_response(status, &error.to_string());
    if let Ok(kind) = header::HeaderValue::from_str(error.kind()) {
        response.headers_mut().insert("x-graphrt-error-kind", kind);
    }
    response
}

/// `GET /v1/knowledge/datasets`
pub async fn list_datasets(State(state): State<Arc<AppState>>) -> Response {
    let Some(knowledge) = state.knowledge() else { return not_served() };
    let datasets: Vec<serde_json::Value> = knowledge
        .kb
        .datasets()
        .map(|d| {
            serde_json::json!({
                "id": d.id, "name": d.name, "description": d.description,
                "managed": knowledge.kb.is_managed(&d.id),
            })
        })
        .collect();
    Json(serde_json::json!({ "datasets": datasets })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    limit: Option<usize>,
    after: Option<String>,
}

/// `GET /v1/knowledge/datasets/{dataset}/documents?limit=&after=`
pub async fn list_documents(
    State(state): State<Arc<AppState>>,
    UrlPath(dataset): UrlPath<String>,
    Query(page): Query<PageQuery>,
) -> Response {
    let Some(knowledge) = state.knowledge() else { return not_served() };
    let limit = page.limit.unwrap_or(DEFAULT_DOCUMENT_PAGE);
    match knowledge.kb.list_documents(&dataset, limit, page.after.as_deref()).await {
        Ok(documents) => {
            // A full page may have more after it; an empty `next` means the end.
            let next = (documents.len() >= limit.clamp(1, graphrt_rag::MAX_DOCUMENT_PAGE))
                .then(|| documents.last().map(|d| d.document_id.clone()))
                .flatten();
            Json(serde_json::json!({ "documents": documents, "next": next })).into_response()
        }
        Err(e) => rag_error(&e),
    }
}

/// Body of `POST /v1/knowledge/datasets/{dataset}/documents`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextDocument {
    document_id: String,
    #[serde(default)]
    title: String,
    text: String,
    #[serde(default)]
    metadata: Metadata,
}

/// `POST /v1/knowledge/datasets/{dataset}/documents`: add or replace a document from text.
pub async fn put_text_document(
    State(state): State<Arc<AppState>>,
    UrlPath(dataset): UrlPath<String>,
    Json(body): Json<TextDocument>,
) -> Response {
    let Some(knowledge) = state.knowledge() else { return not_served() };
    if body.document_id.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "`document_id` must not be empty");
    }
    let title = if body.title.is_empty() { body.document_id.clone() } else { body.title };
    let document = Document::new(&dataset, &body.document_id, body.text).title(title).metadata(body.metadata);
    ingest(knowledge, &dataset, document).await
}

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    /// File name used to detect the format (`report.pdf`); defaults to the document id.
    name: Option<String>,
    title: Option<String>,
}

/// `PUT /v1/knowledge/datasets/{dataset}/documents/{document}/file?name=&title=`:
/// add or replace a document from a raw file body (PDF, DOCX, HTML, Markdown, ...).
pub async fn put_file_document(
    State(state): State<Arc<AppState>>,
    UrlPath((dataset, document_id)): UrlPath<(String, String)>,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(knowledge) = state.knowledge() else { return not_served() };
    if body.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "the request body (the file) is empty");
    }
    let name = query.name.unwrap_or_else(|| document_id.clone());
    let mime = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let _turn = match knowledge.ingest.acquire().await {
        Ok(permit) => permit,
        Err(_) => return error_response(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down"),
    };
    // Extraction parses arbitrary binary formats: keep it off the async workers.
    let file_name = name.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        graphrt_rag::Extractor::extract(&graphrt_rag::DocExtractor::new(), &body, mime.as_deref(), &file_name)
    })
    .await;
    let text = match extracted {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => return rag_error(&e),
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "text extraction failed"),
    };
    let title = query.title.unwrap_or(name);
    ingest_locked(knowledge, &dataset, Document::new(&dataset, &document_id, text).title(title)).await
}

/// Ingest after waiting for the ingestion queue.
async fn ingest(knowledge: &KnowledgeService, dataset: &str, document: Document) -> Response {
    let _turn = match knowledge.ingest.acquire().await {
        Ok(permit) => permit,
        Err(_) => return error_response(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down"),
    };
    ingest_locked(knowledge, dataset, document).await
}

/// Ingest while holding the queue permit.
async fn ingest_locked(knowledge: &KnowledgeService, dataset: &str, document: Document) -> Response {
    let Some(ingestor) = knowledge.kb.ingestor(dataset) else {
        let error = if knowledge.kb.contains(dataset) {
            RagError::Unsupported(format!("dataset `{dataset}` is served by an external retriever"))
        } else {
            RagError::NotFound(format!("unknown dataset `{dataset}`"))
        };
        return rag_error(&error);
    };
    let document_id = document.document_id.clone();
    match ingestor.ingest(&document).await {
        Ok(report) => Json(ingest_result(&document_id, report)).into_response(),
        Err(e) => rag_error(&e),
    }
}

fn ingest_result(document_id: &str, report: IngestReport) -> serde_json::Value {
    serde_json::json!({ "document_id": document_id, "report": report })
}

/// `GET /v1/knowledge/datasets/{dataset}/documents/{document}`: the document's chunks.
pub async fn get_document(
    State(state): State<Arc<AppState>>,
    UrlPath((dataset, document_id)): UrlPath<(String, String)>,
) -> Response {
    let Some(knowledge) = state.knowledge() else { return not_served() };
    match knowledge.kb.document_chunks(&dataset, &document_id).await {
        Ok(chunks) if chunks.is_empty() => error_response(StatusCode::NOT_FOUND, "unknown document"),
        Ok(chunks) => {
            // Vectors are internal and large: never sent.
            let chunks: Vec<serde_json::Value> = chunks
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id, "title": c.title, "text": c.text,
                        "characters": c.text.chars().count(), "metadata": c.metadata,
                    })
                })
                .collect();
            Json(serde_json::json!({ "document_id": document_id, "chunks": chunks })).into_response()
        }
        Err(e) => rag_error(&e),
    }
}

/// `DELETE /v1/knowledge/datasets/{dataset}/documents/{document}`
pub async fn delete_document(
    State(state): State<Arc<AppState>>,
    UrlPath((dataset, document_id)): UrlPath<(String, String)>,
) -> Response {
    let Some(knowledge) = state.knowledge() else { return not_served() };
    match knowledge.kb.delete_document(&dataset, &document_id).await {
        Ok(0) => error_response(StatusCode::NOT_FOUND, "unknown document"),
        Ok(removed) => Json(serde_json::json!({ "deleted_chunks": removed })).into_response(),
        Err(e) => rag_error(&e),
    }
}

/// Body of `POST /v1/knowledge/retrieve` (retrieval testing).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrieveBody {
    query: String,
    datasets: Vec<String>,
    #[serde(default)]
    top_k: Option<usize>,
    #[serde(default)]
    score_threshold: Option<f32>,
    #[serde(default)]
    filter: Option<MetadataFilter>,
    #[serde(default)]
    weights: Option<HybridWeights>,
    #[serde(default)]
    rerank: bool,
}

/// `POST /v1/knowledge/retrieve`: run a query exactly as the `knowledge-retrieval` node would.
pub async fn retrieve(State(state): State<Arc<AppState>>, Json(body): Json<RetrieveBody>) -> Response {
    let Some(knowledge) = state.knowledge() else { return not_served() };
    let mut request = RetrievalRequest::new(body.query, body.datasets)
        .threshold(body.score_threshold)
        .filter(body.filter)
        .rerank(body.rerank);
    if let Some(top_k) = body.top_k {
        request = request.top_k(top_k);
    }
    if let Some(weights) = body.weights {
        request = request.weights(weights);
    }
    let started = std::time::Instant::now();
    match knowledge.kb.retrieve(&request).await {
        Ok(hits) => Json(serde_json::json!({
            "hits": hits,
            "elapsed_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }))
        .into_response(),
        Err(e) => rag_error(&e),
    }
}
