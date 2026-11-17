//! The built-in retrieval pipeline: vector + keyword search, weighted merge,
//! optional reranking, threshold and top-k.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::error::RagError;
use crate::types::{Embedder, Hit, KeywordQuery, Reranker, RetrievalRequest, Retriever, VectorQuery, VectorStore};

/// How many candidates each search returns before merging (multiple of `top_k`).
const CANDIDATE_FACTOR: usize = 4;
const MIN_CANDIDATES: usize = 20;
/// With a metadata filter, stores may apply only part of it before ranking; fetch
/// more candidates so the post-filter still has enough to choose from.
const FILTERED_CANDIDATE_FACTOR: usize = 20;
const MAX_CANDIDATES: usize = 1_000;

/// A [`Retriever`] over a [`VectorStore`] + [`Embedder`] (+ optional [`Reranker`]).
#[derive(Clone, Debug)]
pub struct StoreRetriever {
    store: Arc<dyn VectorStore>,
    embedder: Arc<dyn Embedder>,
    reranker: Option<Arc<dyn Reranker>>,
}

impl StoreRetriever {
    pub fn new(store: Arc<dyn VectorStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder, reranker: None }
    }

    pub fn with_reranker(mut self, reranker: Option<Arc<dyn Reranker>>) -> Self {
        self.reranker = reranker;
        self
    }

    pub fn store(&self) -> &Arc<dyn VectorStore> {
        &self.store
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }
}

impl Retriever for StoreRetriever {
    fn retrieve<'a>(&'a self, request: &'a RetrievalRequest) -> BoxFuture<'a, Result<Vec<Hit>, RagError>> {
        Box::pin(async move {
            let query = request.query.trim();
            if query.is_empty() || request.top_k == 0 {
                return Ok(Vec::new());
            }
            let factor = if request.filter.is_some() { FILTERED_CANDIDATE_FACTOR } else { CANDIDATE_FACTOR };
            let candidates =
                request.top_k.saturating_mul(factor).max(MIN_CANDIDATES).min(MAX_CANDIDATES.max(request.top_k));
            let weights = request.weights;
            if weights.keyword > 0.0 && !self.store.supports_keyword() {
                return Err(RagError::Config(
                    "keyword (hybrid) weight requested, but this dataset's store has no keyword index".into(),
                ));
            }
            if request.rerank && self.reranker.is_none() {
                return Err(RagError::Config(
                    "reranking requested, but this dataset has no reranker configured".into(),
                ));
            }
            let use_keyword = weights.keyword > 0.0;
            let use_vector = weights.vector > 0.0 || !use_keyword;

            let vector_hits = if use_vector {
                let vector = self.embedder.embed_query(query).await?;
                self.store
                    .search(VectorQuery {
                        datasets: &request.datasets,
                        vector: &vector,
                        top_k: candidates,
                        filter: request.filter.as_ref(),
                    })
                    .await?
            } else {
                Vec::new()
            };
            let keyword_hits = if use_keyword {
                self.store
                    .keyword_search(KeywordQuery {
                        datasets: &request.datasets,
                        text: query,
                        top_k: candidates,
                        filter: request.filter.as_ref(),
                    })
                    .await?
            } else {
                Vec::new()
            };

            let mut hits = if use_keyword {
                let vector_weight = if use_vector { weights.vector } else { 0.0 };
                merge_weighted(vector_hits, keyword_hits, vector_weight, weights.keyword)
            } else {
                vector_hits
            };
            // Stores may push filters down partially; enforce them fully here.
            if let Some(filter) = &request.filter {
                hits.retain(|h| filter.matches(&h.metadata));
            }
            if let (true, Some(reranker)) = (request.rerank, &self.reranker) {
                hits = rerank(reranker.as_ref(), query, hits).await?;
            }
            Ok(finalize(hits, request.score_threshold, request.top_k))
        })
    }
}

/// Blend vector (cosine, clamped to `[0, 1]`) and keyword (BM25 divided by the
/// best BM25 score) scores by id: `score = wv * vector + wk * keyword`, with weights
/// normalized to sum to 1. Missing components count as 0.
pub fn merge_weighted(vector: Vec<Hit>, keyword: Vec<Hit>, vector_weight: f32, keyword_weight: f32) -> Vec<Hit> {
    let total = (vector_weight + keyword_weight).max(f32::EPSILON);
    let (wv, wk) = (vector_weight / total, keyword_weight / total);
    let max_kw = keyword.iter().map(|h| h.score).fold(0.0f32, f32::max);

    let mut merged: Vec<Hit> = Vec::with_capacity(vector.len() + keyword.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    for mut hit in vector {
        hit.score = wv * hit.score.clamp(0.0, 1.0);
        index.insert(hit.id.clone(), merged.len());
        merged.push(hit);
    }
    for mut hit in keyword {
        let normalized = if max_kw > 0.0 { hit.score / max_kw } else { 0.0 };
        match index.get(&hit.id).and_then(|&ix| merged.get_mut(ix)) {
            Some(existing) => existing.score += wk * normalized,
            None => {
                hit.score = wk * normalized;
                index.insert(hit.id.clone(), merged.len());
                merged.push(hit);
            }
        }
    }
    merged.sort_by(|a, b| b.score.total_cmp(&a.score));
    merged
}

/// Replace scores with reranker scores and re-sort.
pub async fn rerank(reranker: &dyn Reranker, query: &str, mut hits: Vec<Hit>) -> Result<Vec<Hit>, RagError> {
    if hits.is_empty() {
        return Ok(hits);
    }
    let docs: Vec<String> = hits.iter().map(|h| h.content.clone()).collect();
    let scores = reranker.rerank(query, &docs).await?;
    if scores.len() != hits.len() {
        return Err(RagError::Decode(format!(
            "reranker returned {} scores for {} documents",
            scores.len(),
            hits.len()
        )));
    }
    for (hit, score) in hits.iter_mut().zip(scores) {
        if !score.is_finite() {
            return Err(RagError::Decode("reranker returned a non-finite score".into()));
        }
        hit.score = score;
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(hits)
}

/// Apply the score threshold, then keep the best `top_k`.
pub fn finalize(mut hits: Vec<Hit>, threshold: Option<f32>, top_k: usize) -> Vec<Hit> {
    // A NaN would sort first with `total_cmp`; such scores carry no ranking information.
    hits.retain(|h| h.score.is_finite());
    if let Some(t) = threshold {
        hits.retain(|h| h.score >= t);
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(top_k);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, score: f32) -> Hit {
        Hit {
            id: id.into(),
            dataset_id: "d".into(),
            document_id: String::new(),
            title: String::new(),
            content: id.into(),
            score,
            metadata: serde_json::Map::new(),
        }
    }

    #[test]
    fn weighted_merge_combines_by_id() {
        let vector = vec![hit("a", 0.9), hit("b", 0.5)];
        let keyword = vec![hit("b", 10.0), hit("c", 5.0)];
        let merged = merge_weighted(vector, keyword, 0.5, 0.5);
        let ids: Vec<&str> = merged.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a", "c"]);
        assert!((merged[0].score - 0.75).abs() < 1e-6);
        assert!((merged[1].score - 0.45).abs() < 1e-6);
        assert!((merged[2].score - 0.25).abs() < 1e-6);
    }

    #[test]
    fn finalize_thresholds_and_truncates() {
        let out = finalize(vec![hit("a", 0.2), hit("b", 0.8), hit("c", 0.5)], Some(0.4), 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "b");
    }
}
