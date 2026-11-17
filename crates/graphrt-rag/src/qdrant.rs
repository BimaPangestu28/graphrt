//! Qdrant vector store over its REST API.
//!
//! All datasets share one collection; every point carries `dataset_id`,
//! `document_id`, `title`, `text`, `content_hash` and `metadata` in its payload.
//! Dataset scoping is pushed down as a Qdrant filter, and so are metadata
//! equality conditions whose result cannot differ from the in-process filter
//! (non-numeric, non-boolean string values). Every other condition is applied
//! after the search by the pipeline, which over-fetches candidates when a filter
//! is present. Point ids are UUIDs derived from SHA-256 of the chunk id, so ids
//! chosen by one tenant cannot be made to collide with another tenant's chunks.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::error::RagError;
use crate::filter::{Logical, MetadataFilter};
use crate::http::{HttpSettings, build_client};
use crate::types::{Hit, Metadata, StoredChunk, VectorQuery, VectorStore};

/// Qdrant connection settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QdrantConfig {
    /// e.g. `http://localhost:6333`.
    pub url: String,
    pub collection: String,
    /// Create the collection (cosine distance) on first upsert when missing.
    #[serde(default = "yes")]
    pub create_collection: bool,
    /// API key, sent as the `api-key` header.
    #[serde(default, flatten)]
    pub http: HttpSettings,
}

fn yes() -> bool {
    true
}

/// [`VectorStore`] backed by Qdrant.
#[derive(Clone, Debug)]
pub struct QdrantStore {
    config: Arc<QdrantConfig>,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    result: Vec<ScoredPoint>,
}

#[derive(Deserialize)]
struct ScoredPoint {
    score: f32,
    #[serde(default)]
    payload: Payload,
}

#[derive(Default, Deserialize, Serialize)]
struct Payload {
    #[serde(default)]
    chunk_id: String,
    #[serde(default)]
    dataset_id: String,
    #[serde(default)]
    document_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    content_hash: String,
    #[serde(default)]
    metadata: Metadata,
}

#[derive(Deserialize)]
struct CountResponse {
    result: CountResult,
}

#[derive(Deserialize)]
struct CountResult {
    count: usize,
}

#[derive(Deserialize)]
struct ScrollResponse {
    result: ScrollResult,
}

#[derive(Deserialize)]
struct ScrollResult {
    #[serde(default)]
    points: Vec<ScrolledPoint>,
    #[serde(default)]
    next_page_offset: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ScrolledPoint {
    #[serde(default)]
    payload: Payload,
    #[serde(default)]
    vector: Option<serde_json::Value>,
}

impl QdrantStore {
    pub fn new(mut config: QdrantConfig) -> Result<Self, RagError> {
        if config.url.trim().is_empty() || config.collection.trim().is_empty() {
            return Err(RagError::Config("qdrant needs `url` and `collection`".into()));
        }
        if config.http.api_key_header.is_none() {
            config.http.api_key_header = Some("api-key".into());
        }
        let client = build_client(&config.http)?;
        Ok(Self { config: Arc::new(config), client })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/collections/{}{path}", self.config.url.trim_end_matches('/'), self.config.collection)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, Vec<u8>), RagError> {
        let mut req = self.client.request(method, url);
        if !self.config.http.api_key.is_empty() {
            let header = self.config.http.api_key_header.as_deref().unwrap_or("api-key");
            req = req.header(header, self.config.http.api_key.expose());
        }
        for (k, v) in &self.config.http.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().await.map_err(|e| RagError::Network(e.without_url().to_string()))?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(|e| RagError::Network(e.without_url().to_string()))?.to_vec();
        Ok((status, bytes))
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Vec<u8>, RagError> {
        let (status, bytes) = self.send(method, self.url(path), body).await?;
        if (200..300).contains(&status) {
            return Ok(bytes);
        }
        let message = String::from_utf8_lossy(bytes.get(..bytes.len().min(2048)).unwrap_or_default()).into_owned();
        Err(match status {
            401 | 403 => RagError::Auth(message),
            404 => RagError::NotFound(format!("qdrant collection `{}`: {message}", self.config.collection)),
            _ => RagError::Http { status, message },
        })
    }

    async fn ensure_collection(&self, dims: usize) -> Result<(), RagError> {
        let (status, _) = self.send(reqwest::Method::GET, self.url(""), None).await?;
        if status == 200 {
            return Ok(());
        }
        if status != 404 || !self.config.create_collection {
            return Err(RagError::Store(format!(
                "qdrant collection `{}` unavailable (HTTP {status})",
                self.config.collection
            )));
        }
        let body = serde_json::json!({ "vectors": { "size": dims, "distance": "Cosine" } });
        self.call(reqwest::Method::PUT, "", Some(body)).await?;
        // Index the fields every filter uses, so dataset/document lookups are not full scans.
        for field in ["dataset_id", "document_id", "content_hash"] {
            let index = serde_json::json!({ "field_name": field, "field_schema": "keyword" });
            self.call(reqwest::Method::PUT, "/index?wait=true", Some(index)).await?;
        }
        Ok(())
    }

    async fn count(&self, filter: serde_json::Value) -> Result<usize, RagError> {
        let bytes = self
            .call(reqwest::Method::POST, "/points/count", Some(serde_json::json!({"filter": filter, "exact": true})))
            .await?;
        let parsed: CountResponse = serde_json::from_slice(&bytes).map_err(|e| RagError::Decode(e.to_string()))?;
        Ok(parsed.result.count)
    }

    async fn delete_matching(&self, filter: serde_json::Value) -> Result<usize, RagError> {
        let count = match self.count(filter.clone()).await {
            Ok(n) => n,
            Err(RagError::NotFound(_)) => return Ok(0),
            Err(e) => return Err(e),
        };
        if count > 0 {
            self.call(reqwest::Method::POST, "/points/delete?wait=true", Some(serde_json::json!({"filter": filter})))
                .await?;
        }
        Ok(count)
    }
}

/// Qdrant point ids must be unsigned integers or UUIDs; derive a stable UUID from
/// SHA-256 of the chunk id (collision-resistant, unlike a 64-bit hash).
fn point_id(chunk_id: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(chunk_id.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let part = |range: std::ops::Range<usize>| hex.get(range).unwrap_or_default().to_owned();
    format!("{}-{}-{}-{}-{}", part(0..8), part(8..12), part(12..16), part(16..20), part(20..32))
}

fn key_match(key: &str, value: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "key": key, "match": { "value": value } })
}

/// Build the Qdrant filter: dataset scope plus pushed-down equality conditions.
fn qdrant_filter(datasets: &[String], filter: Option<&MetadataFilter>) -> serde_json::Value {
    let mut must = vec![serde_json::json!({ "key": "dataset_id", "match": { "any": datasets } })];
    if let Some(f) = filter {
        // Only AND filters can be pushed down safely; OR filters are applied afterwards.
        if f.logical_operator == Logical::And {
            for c in &f.conditions {
                let is_eq = matches!(c.comparison_operator.trim(), "is" | "=" | "==" | "eq");
                if is_eq && pushdown_safe(&c.value) {
                    must.push(key_match(&format!("metadata.{}", c.name), c.value.clone()));
                }
            }
        }
    }
    serde_json::json!({ "must": must })
}

/// Whether Qdrant's type-exact `match` gives the same result as the in-process
/// `equals`, which also treats `2024 == "2024"` and `true == "true"` as equal.
/// That holds only for strings that are not numbers, booleans, null or JSON.
fn pushdown_safe(value: &serde_json::Value) -> bool {
    let serde_json::Value::String(s) = value else { return false };
    let t = s.trim();
    !(t.is_empty()
        || t.parse::<f64>().is_ok()
        || matches!(t, "true" | "false" | "null")
        || t.starts_with(['{', '['])
        || t.len() != s.len())
}

fn document_filter(dataset_id: &str, document_id: &str) -> serde_json::Value {
    serde_json::json!({ "must": [
        key_match("dataset_id", dataset_id.into()),
        key_match("document_id", document_id.into()),
    ]})
}

/// Decode a point vector (unnamed vectors only).
fn parse_vector(raw: Option<serde_json::Value>) -> Result<Vec<f32>, RagError> {
    match raw {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(v) => serde_json::from_value(v).map_err(|e| {
            RagError::Decode(format!("unexpected qdrant vector format (named vectors are not supported): {e}"))
        }),
    }
}

impl VectorStore for QdrantStore {
    fn upsert(&self, chunks: Vec<StoredChunk>) -> BoxFuture<'_, Result<(), RagError>> {
        Box::pin(async move {
            let Some(dims) = chunks.first().map(|c| c.vector.len()) else {
                return Ok(());
            };
            for chunk in &chunks {
                crate::types::check_vector(&chunk.vector, &format!("chunk `{}`", chunk.id))?;
                if chunk.vector.len() != dims {
                    return Err(RagError::Input(format!(
                        "chunks mix {dims}- and {}-dimensional vectors",
                        chunk.vector.len()
                    )));
                }
            }
            self.ensure_collection(dims).await?;
            let points: Vec<serde_json::Value> = chunks
                .into_iter()
                .map(|c| {
                    let payload = Payload {
                        chunk_id: c.id.clone(),
                        dataset_id: c.dataset_id,
                        document_id: c.document_id,
                        title: c.title,
                        text: c.text,
                        content_hash: c.content_hash,
                        metadata: c.metadata,
                    };
                    serde_json::json!({ "id": point_id(&c.id), "vector": c.vector, "payload": payload })
                })
                .collect();
            for batch in points.chunks(256) {
                self.call(reqwest::Method::PUT, "/points?wait=true", Some(serde_json::json!({ "points": batch })))
                    .await?;
            }
            Ok(())
        })
    }

    fn delete_document<'a>(
        &'a self,
        dataset_id: &'a str,
        document_id: &'a str,
    ) -> BoxFuture<'a, Result<usize, RagError>> {
        Box::pin(async move {
            let filter = serde_json::json!({ "must": [
                key_match("dataset_id", dataset_id.into()),
                key_match("document_id", document_id.into()),
            ]});
            self.delete_matching(filter).await
        })
    }

    fn delete_dataset<'a>(&'a self, dataset_id: &'a str) -> BoxFuture<'a, Result<usize, RagError>> {
        Box::pin(async move {
            let filter = serde_json::json!({ "must": [key_match("dataset_id", dataset_id.into())] });
            self.delete_matching(filter).await
        })
    }

    fn document_chunks<'a>(
        &'a self,
        dataset_id: &'a str,
        document_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<StoredChunk>, RagError>> {
        Box::pin(async move {
            let mut chunks = Vec::new();
            let mut offset: Option<serde_json::Value> = None;
            loop {
                let mut body = serde_json::json!({
                    "filter": document_filter(dataset_id, document_id),
                    "limit": 256,
                    "with_payload": true,
                    "with_vector": true,
                });
                if let (Some(o), Some(obj)) = (offset.take(), body.as_object_mut()) {
                    obj.insert("offset".into(), o);
                }
                let bytes = match self.call(reqwest::Method::POST, "/points/scroll", Some(body)).await {
                    Ok(b) => b,
                    // No collection yet: nothing stored.
                    Err(RagError::NotFound(_)) => return Ok(chunks),
                    Err(e) => return Err(e),
                };
                let page: ScrollResponse =
                    serde_json::from_slice(&bytes).map_err(|e| RagError::Decode(e.to_string()))?;
                for point in page.result.points {
                    let vector = parse_vector(point.vector)?;
                    let p = point.payload;
                    chunks.push(StoredChunk {
                        id: p.chunk_id,
                        dataset_id: p.dataset_id,
                        document_id: p.document_id,
                        title: p.title,
                        text: p.text,
                        metadata: p.metadata,
                        content_hash: p.content_hash,
                        vector,
                    });
                }
                match page.result.next_page_offset {
                    Some(next) if !next.is_null() => offset = Some(next),
                    _ => return Ok(chunks),
                }
            }
        })
    }

    fn delete_chunks<'a>(&'a self, dataset_id: &'a str, ids: &'a [String]) -> BoxFuture<'a, Result<usize, RagError>> {
        Box::pin(async move {
            let _ = dataset_id;
            for batch in ids.chunks(256) {
                let points: Vec<String> = batch.iter().map(|id| point_id(id)).collect();
                self.call(
                    reqwest::Method::POST,
                    "/points/delete?wait=true",
                    Some(serde_json::json!({ "points": points })),
                )
                .await?;
            }
            Ok(ids.len())
        })
    }

    fn search<'a>(&'a self, query: VectorQuery<'a>) -> BoxFuture<'a, Result<Vec<Hit>, RagError>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "vector": query.vector,
                "limit": query.top_k,
                "with_payload": true,
                "filter": qdrant_filter(query.datasets, query.filter),
            });
            let bytes = self.call(reqwest::Method::POST, "/points/search", Some(body)).await?;
            let parsed: SearchResponse = serde_json::from_slice(&bytes).map_err(|e| RagError::Decode(e.to_string()))?;
            Ok(parsed
                .result
                .into_iter()
                .map(|p| Hit {
                    id: p.payload.chunk_id,
                    dataset_id: p.payload.dataset_id,
                    document_id: p.payload.document_id,
                    title: p.payload.title,
                    content: p.payload.text,
                    score: p.score,
                    metadata: p.payload.metadata,
                })
                .collect())
        })
    }

    fn contains_hash<'a>(
        &'a self,
        dataset_id: &'a str,
        content_hash: &'a str,
    ) -> BoxFuture<'a, Result<bool, RagError>> {
        Box::pin(async move {
            let filter = serde_json::json!({ "must": [
                key_match("dataset_id", dataset_id.into()),
                key_match("content_hash", content_hash.into()),
            ]});
            match self.count(filter).await {
                Ok(n) => Ok(n > 0),
                Err(RagError::NotFound(_)) => Ok(false),
                Err(e) => Err(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::MetadataCondition;

    #[test]
    fn filter_pushdown_only_equality_with_and() {
        let f = MetadataFilter {
            logical_operator: Logical::And,
            conditions: vec![
                MetadataCondition { name: "lang".into(), comparison_operator: "is".into(), value: "en".into() },
                MetadataCondition { name: "year".into(), comparison_operator: ">".into(), value: 2000.into() },
            ],
        };
        let q = qdrant_filter(&["d".to_owned()], Some(&f));
        let must = q["must"].as_array().unwrap();
        assert_eq!(must.len(), 2);
        assert_eq!(must[1]["key"], "metadata.lang");
        // Values whose local equality is type-lenient are never pushed down.
        for value in
            [serde_json::json!("2024"), serde_json::json!(2024), serde_json::json!(9.5), serde_json::json!("true")]
        {
            let cond = MetadataCondition { name: "x".into(), comparison_operator: "is".into(), value };
            let f = MetadataFilter { logical_operator: Logical::And, conditions: vec![cond] };
            assert_eq!(qdrant_filter(&["d".to_owned()], Some(&f))["must"].as_array().unwrap().len(), 1);
        }
        let or = MetadataFilter { logical_operator: Logical::Or, ..f };
        assert_eq!(qdrant_filter(&["d".to_owned()], Some(&or))["must"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn point_ids_are_stable_uuids() {
        assert_eq!(point_id("a:b:0"), point_id("a:b:0"));
        assert_ne!(point_id("a:b:0"), point_id("a:b:1"));
        let id = point_id("a:b:0");
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
    }
}
