//! Qdrant vector store over its REST API.
//!
//! All datasets share one collection; every point carries `dataset_id`,
//! `document_id`, `title`, `text`, `content_hash` and `metadata` in its payload.
//! Dataset scoping and equality metadata conditions are pushed down as Qdrant
//! filters; other operators are applied after the search by the pipeline.

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
        self.call(reqwest::Method::PUT, "", Some(body)).await.map(|_| ())
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

/// Qdrant point ids must be unsigned integers or UUIDs; derive a stable u64.
fn point_id(chunk_id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in chunk_id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h >> 1
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
                if is_eq && (c.value.is_string() || c.value.is_number() || c.value.is_boolean()) {
                    must.push(key_match(&format!("metadata.{}", c.name), c.value.clone()));
                }
            }
        }
    }
    serde_json::json!({ "must": must })
}

impl VectorStore for QdrantStore {
    fn upsert(&self, chunks: Vec<StoredChunk>) -> BoxFuture<'_, Result<(), RagError>> {
        Box::pin(async move {
            let Some(dims) = chunks.first().map(|c| c.vector.len()) else {
                return Ok(());
            };
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
        let or = MetadataFilter { logical_operator: Logical::Or, ..f };
        assert_eq!(qdrant_filter(&["d".to_owned()], Some(&or))["must"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn point_ids_are_stable() {
        assert_eq!(point_id("a:b:0"), point_id("a:b:0"));
        assert_ne!(point_id("a:b:0"), point_id("a:b:1"));
    }
}
