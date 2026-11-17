//! A deterministic, dependency-free embedder using feature hashing.
//!
//! Each token (and each pair of adjacent tokens) is hashed into one of `dims`
//! buckets with a ± sign. The result captures lexical overlap only — no
//! semantics — but it is fully offline and reproducible, which makes it useful
//! for tests, demos and keyword-like retrieval without a model server.

use futures::future::BoxFuture;

use crate::error::RagError;
use crate::memory::tokenize;
use crate::types::Embedder;

/// Feature-hashing embedder.
#[derive(Clone, Debug)]
pub struct HashEmbedder {
    dims: usize,
    model: String,
}

impl HashEmbedder {
    /// Create an embedder producing `dims`-dimensional vectors (minimum 8).
    pub fn new(dims: usize) -> Self {
        let dims = dims.max(8);
        Self { dims, model: format!("hash-{dims}") }
    }

    /// Embed one text synchronously.
    pub fn embed_text(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dims];
        let tokens = tokenize(text);
        let mut add = |feature: &str, weight: f32| {
            let h = fnv(feature);
            let bucket = usize::try_from(h % u64::try_from(self.dims).unwrap_or(1)).unwrap_or(0);
            let sign = if (h >> 63) == 0 { 1.0 } else { -1.0 };
            if let Some(slot) = v.get_mut(bucket) {
                *slot += sign * weight;
            }
        };
        for token in &tokens {
            add(token, 1.0);
        }
        for pair in tokens.windows(2) {
            if let [a, b] = pair {
                add(&format!("{a} {b}"), 0.5);
            }
        }
        v
    }
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl Embedder for HashEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> Option<usize> {
        Some(self.dims)
    }

    fn embed_documents<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>, RagError>> {
        let vectors = texts.iter().map(|t| self.embed_text(t)).collect();
        Box::pin(futures::future::ready(Ok(vectors)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::cosine;

    #[test]
    fn similar_texts_score_higher() {
        let e = HashEmbedder::new(256);
        let q = e.embed_text("how do cats purr");
        let near = e.embed_text("cats purr when happy");
        let far = e.embed_text("rocket engines burn fuel");
        assert!(cosine(&q, &near) > cosine(&q, &far));
        assert_eq!(e.embed_text("x"), e.embed_text("x"));
    }
}
