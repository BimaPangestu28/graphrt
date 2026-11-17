//! HTTP-backed components: OpenAI-compatible embeddings, a generic retriever
//! endpoint and Cohere/Jina/Voyage-compatible rerankers.

use std::fmt;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::error::RagError;
use crate::types::{Embedder, Hit, Metadata, Reranker, RetrievalRequest, Retriever};

/// Maximum bytes read from a response body.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Maximum bytes of an error body included in messages.
const MAX_ERROR_BYTES: usize = 2 * 1024;

/// A string that never appears in `Debug` output.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() { "\"\"" } else { "\"***\"" })
    }
}

/// Shared HTTP settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpSettings {
    /// Bearer token (sent as `Authorization: Bearer ...`) unless `api_key_header` is set.
    #[serde(default)]
    pub api_key: Secret,
    /// Send the key in this header instead (e.g. `api-key` for Azure).
    #[serde(default)]
    pub api_key_header: Option<String>,
    /// Extra headers.
    #[serde(default)]
    pub headers: indexmap::IndexMap<String, String>,
    /// Whole-request timeout in milliseconds (default 60 000).
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    60_000
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            api_key: Secret::default(),
            api_key_header: None,
            headers: indexmap::IndexMap::new(),
            timeout_ms: default_timeout(),
        }
    }
}

/// Build a reqwest client using rustls with the ring crypto provider.
pub(crate) fn build_client(settings: &HttpSettings) -> Result<reqwest::Client, RagError> {
    // reqwest is built without a bundled crypto provider; install ring once per process.
    // An error only means a provider is already installed, which is fine.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        // Credentials may travel in custom headers (`api-key`, `headers`), which
        // reqwest does not strip on cross-host redirects; never follow redirects.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_millis(settings.timeout_ms.max(1)))
        .build()
        .map_err(|e| RagError::Config(format!("cannot build HTTP client: {}", e.without_url())))
}

fn apply_settings(mut req: reqwest::RequestBuilder, settings: &HttpSettings) -> reqwest::RequestBuilder {
    if !settings.api_key.is_empty() {
        req = match &settings.api_key_header {
            Some(header) => req.header(header.as_str(), settings.api_key.expose()),
            None => req.bearer_auth(settings.api_key.expose()),
        };
    }
    for (k, v) in &settings.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req
}

/// Send a JSON POST and decode a JSON response, mapping failures to [`RagError`].
async fn post_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    settings: &HttpSettings,
    body: &serde_json::Value,
) -> Result<T, RagError> {
    let response = apply_settings(client.post(url), settings)
        .json(body)
        .send()
        .await
        .map_err(|e| RagError::Network(e.without_url().to_string()))?;
    let status = response.status().as_u16();
    let retry_after_ms = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|s| s.saturating_mul(1000));
    let bytes = read_capped(response).await?;
    if !(200..300).contains(&status) {
        let snippet =
            String::from_utf8_lossy(bytes.get(..bytes.len().min(MAX_ERROR_BYTES)).unwrap_or_default()).into_owned();
        return Err(match status {
            401 | 403 => RagError::Auth(format!("HTTP {status}: {snippet}")),
            429 => RagError::RateLimit { message: snippet, retry_after_ms },
            _ => RagError::Http { status, message: snippet },
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| RagError::Decode(e.to_string()))
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, RagError> {
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| RagError::Network(e.without_url().to_string()))? {
        if out.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(RagError::Decode(format!("response exceeds {MAX_BODY_BYTES} bytes")));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

// ---------------------------------------------------------------------------
// Embeddings
// ---------------------------------------------------------------------------

/// Configuration of an OpenAI-compatible `/embeddings` endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenAiEmbedderConfig {
    /// API root, e.g. `https://api.openai.com/v1` or `http://localhost:11434/v1`.
    #[serde(default = "default_openai_base")]
    pub base_url: String,
    pub model: String,
    /// Requested output dimensions (models that support it).
    #[serde(default)]
    pub dimensions: Option<usize>,
    /// Texts per request (default 64).
    #[serde(default = "default_batch")]
    pub batch_size: usize,
    /// Prefix added to queries (e.g. `search_query: ` for nomic models).
    #[serde(default)]
    pub query_prefix: String,
    /// Prefix added to documents (e.g. `search_document: `).
    #[serde(default)]
    pub document_prefix: String,
    #[serde(default, flatten)]
    pub http: HttpSettings,
}

fn default_openai_base() -> String {
    "https://api.openai.com/v1".to_owned()
}
fn default_batch() -> usize {
    64
}

impl OpenAiEmbedderConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            dimensions: None,
            batch_size: default_batch(),
            query_prefix: String::new(),
            document_prefix: String::new(),
            http: HttpSettings::default(),
        }
    }

    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.http.api_key = Secret::new(key);
        self
    }
}

/// Embedder for any OpenAI-compatible embeddings API (OpenAI, Azure, Ollama, vLLM, Jina, ...).
#[derive(Clone, Debug)]
pub struct OpenAiEmbedder {
    config: OpenAiEmbedderConfig,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

impl OpenAiEmbedder {
    pub fn new(config: OpenAiEmbedderConfig) -> Result<Self, RagError> {
        if config.model.trim().is_empty() {
            return Err(RagError::Config("embedding model is required".into()));
        }
        let client = build_client(&config.http)?;
        Ok(Self { config, client })
    }

    async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, RagError> {
        let expected = texts.len();
        let mut body = serde_json::json!({ "model": self.config.model, "input": texts, "encoding_format": "float" });
        if let (Some(dims), Some(obj)) = (self.config.dimensions, body.as_object_mut()) {
            obj.insert("dimensions".into(), serde_json::Value::from(dims));
        }
        let url = join_url(&self.config.base_url, "embeddings");
        let mut response: EmbeddingResponse = post_json(&self.client, &url, &self.config.http, &body).await?;
        if response.data.len() != expected {
            return Err(RagError::Decode(format!("expected {expected} embeddings, got {}", response.data.len())));
        }
        response.data.sort_by_key(|item| item.index);
        Ok(response.data.into_iter().map(|item| item.embedding).collect())
    }
}

impl Embedder for OpenAiEmbedder {
    fn model(&self) -> &str {
        &self.config.model
    }

    fn dimensions(&self) -> Option<usize> {
        self.config.dimensions
    }

    fn embed_documents<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>, RagError>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(texts.len());
            for batch in texts.chunks(self.config.batch_size.max(1)) {
                let prefixed = batch.iter().map(|t| format!("{}{t}", self.config.document_prefix)).collect();
                out.extend(self.embed_batch(prefixed).await?);
            }
            Ok(out)
        })
    }

    fn embed_query<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, RagError>> {
        Box::pin(async move {
            let mut vectors = self.embed_batch(vec![format!("{}{text}", self.config.query_prefix)]).await?;
            vectors.pop().ok_or_else(|| RagError::Decode("embedder returned no vector".into()))
        })
    }
}

// ---------------------------------------------------------------------------
// Generic retriever endpoint
// ---------------------------------------------------------------------------

/// Configuration of a remote retrieval endpoint.
///
/// Contract — request (`POST <url>`, JSON):
/// ```json
/// {"query": "...", "top_k": 4, "datasets": ["kb1"], "score_threshold": 0.5,
///  "filter": {"logical_operator": "and", "conditions": [...]}, "rerank": false}
/// ```
/// Response:
/// ```json
/// {"records": [{"content": "...", "score": 0.83, "title": "...",
///               "id": "...", "document_id": "...", "dataset_id": "...", "metadata": {}}]}
/// ```
/// Only `content` and `score` are required per record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpRetrieverConfig {
    pub url: String,
    #[serde(default, flatten)]
    pub http: HttpSettings,
}

/// A [`Retriever`] that delegates to a remote RAG service.
#[derive(Clone, Debug)]
pub struct HttpRetriever {
    config: HttpRetrieverConfig,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct RecordsResponse {
    records: Vec<Record>,
}

#[derive(Deserialize)]
struct Record {
    content: String,
    score: f32,
    #[serde(default)]
    title: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    document_id: String,
    #[serde(default)]
    dataset_id: Option<String>,
    #[serde(default)]
    metadata: Metadata,
}

impl HttpRetriever {
    pub fn new(config: HttpRetrieverConfig) -> Result<Self, RagError> {
        if config.url.trim().is_empty() {
            return Err(RagError::Config("retriever url is required".into()));
        }
        let client = build_client(&config.http)?;
        Ok(Self { config, client })
    }
}

impl Retriever for HttpRetriever {
    fn retrieve<'a>(&'a self, request: &'a RetrievalRequest) -> BoxFuture<'a, Result<Vec<Hit>, RagError>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "query": request.query,
                "top_k": request.top_k,
                "datasets": request.datasets,
                "score_threshold": request.score_threshold,
                "filter": request.filter,
                "rerank": request.rerank,
            });
            let response: RecordsResponse = post_json(&self.client, &self.config.url, &self.config.http, &body).await?;
            let default_dataset = request.datasets.first().cloned().unwrap_or_default();
            let hits = response
                .records
                .into_iter()
                .enumerate()
                .map(|(i, r)| Hit {
                    id: r.id.unwrap_or_else(|| format!("{}#{i}", r.document_id)),
                    dataset_id: r.dataset_id.unwrap_or_else(|| default_dataset.clone()),
                    document_id: r.document_id,
                    title: r.title,
                    content: r.content,
                    score: r.score,
                    metadata: r.metadata,
                })
                .collect();
            Ok(hits)
        })
    }
}

// ---------------------------------------------------------------------------
// Reranker
// ---------------------------------------------------------------------------

/// Configuration of a `/rerank` endpoint (Cohere, Jina, Voyage, vLLM, TEI...).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpRerankerConfig {
    /// API root, e.g. `https://api.cohere.com/v2` or `https://api.jina.ai/v1`.
    pub base_url: String,
    pub model: String,
    #[serde(default, flatten)]
    pub http: HttpSettings,
}

/// Reranker for Cohere/Jina/Voyage-compatible APIs.
#[derive(Clone, Debug)]
pub struct HttpReranker {
    config: HttpRerankerConfig,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct RerankResponse {
    #[serde(alias = "data")]
    results: Vec<RerankItem>,
}

#[derive(Deserialize)]
struct RerankItem {
    index: usize,
    #[serde(alias = "score")]
    relevance_score: f32,
}

impl HttpReranker {
    pub fn new(config: HttpRerankerConfig) -> Result<Self, RagError> {
        let client = build_client(&config.http)?;
        Ok(Self { config, client })
    }
}

impl Reranker for HttpReranker {
    fn rerank<'a>(&'a self, query: &'a str, documents: &'a [String]) -> BoxFuture<'a, Result<Vec<f32>, RagError>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "model": self.config.model,
                "query": query,
                "documents": documents,
                "top_n": documents.len(),
            });
            let url = join_url(&self.config.base_url, "rerank");
            let response: RerankResponse = post_json(&self.client, &url, &self.config.http, &body).await?;
            let mut scores: Vec<Option<f32>> = vec![None; documents.len()];
            for item in response.results {
                let slot = scores
                    .get_mut(item.index)
                    .ok_or_else(|| RagError::Decode(format!("reranker returned out-of-range index {}", item.index)))?;
                *slot = Some(item.relevance_score);
            }
            // `top_n` asks for every document; a partial answer means the response
            // is not what this client understands.
            let scored = scores.iter().filter(|s| s.is_some()).count();
            if scored != documents.len() {
                return Err(RagError::Decode(format!("reranker scored {scored} of {} documents", documents.len())));
            }
            Ok(scores.into_iter().map(|s| s.unwrap_or(f32::MIN)).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_redacted() {
        let cfg = OpenAiEmbedderConfig::new("http://x", "m").api_key("sk-very-secret");
        assert!(!format!("{cfg:?}").contains("sk-very-secret"));
    }

    #[test]
    fn url_join() {
        assert_eq!(join_url("http://a/v1/", "/embeddings"), "http://a/v1/embeddings");
    }
}
