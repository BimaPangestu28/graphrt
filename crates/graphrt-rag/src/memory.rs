//! In-process vector store with a BM25 keyword index.
//!
//! Search is brute force: exact cosine similarity over every chunk of the
//! requested datasets. That is fast enough for tens of thousands of chunks
//! (one dot product per chunk, auto-vectorized) and keeps the store free of
//! dependencies. With the `fs` feature the store can persist to a single JSON
//! file, rewritten atomically after every change.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::future::BoxFuture;

use crate::error::RagError;
use crate::filter::MetadataFilter;
use crate::types::{Hit, KeywordQuery, StoredChunk, VectorQuery, VectorStore};

/// BM25 parameters.
const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

#[derive(Debug)]
struct Entry {
    chunk: StoredChunk,
    norm: f32,
    /// Term -> frequency in this chunk.
    terms: HashMap<String, u32>,
    length: u32,
}

#[derive(Debug, Default)]
struct Dataset {
    entries: Vec<Entry>,
    /// Chunk id -> index in `entries`.
    by_id: HashMap<String, usize>,
    /// Term -> number of chunks containing it.
    doc_freq: HashMap<String, u32>,
    total_length: u64,
}

impl Dataset {
    fn insert(&mut self, chunk: StoredChunk) {
        if let Some(&ix) = self.by_id.get(&chunk.id) {
            self.remove_at(ix);
        }
        let terms = term_frequencies(&chunk.text);
        let length = terms.values().sum();
        for term in terms.keys() {
            *self.doc_freq.entry(term.clone()).or_insert(0) += 1;
        }
        self.total_length += u64::from(length);
        let norm = norm(&chunk.vector);
        self.by_id.insert(chunk.id.clone(), self.entries.len());
        self.entries.push(Entry { chunk, norm, terms, length });
    }

    /// Remove the entry at `ix` (swap-remove, fixing up the moved entry's index).
    fn remove_at(&mut self, ix: usize) {
        if ix >= self.entries.len() {
            return;
        }
        let removed = self.entries.swap_remove(ix);
        self.by_id.remove(&removed.chunk.id);
        for term in removed.terms.keys() {
            if let Some(df) = self.doc_freq.get_mut(term) {
                *df = df.saturating_sub(1);
                if *df == 0 {
                    self.doc_freq.remove(term);
                }
            }
        }
        self.total_length = self.total_length.saturating_sub(u64::from(removed.length));
        if let Some(moved) = self.entries.get(ix) {
            self.by_id.insert(moved.chunk.id.clone(), ix);
        }
    }

    fn remove_where(&mut self, predicate: impl Fn(&StoredChunk) -> bool) -> usize {
        let mut removed = 0;
        let mut ix = 0;
        while ix < self.entries.len() {
            let hit = self.entries.get(ix).is_some_and(|e| predicate(&e.chunk));
            if hit {
                self.remove_at(ix);
                removed += 1;
            } else {
                ix += 1;
            }
        }
        removed
    }
}

/// In-memory [`VectorStore`] with cosine similarity and BM25 keyword search.
#[derive(Debug, Default, Clone)]
pub struct MemoryStore {
    inner: Arc<RwLock<HashMap<String, Dataset>>>,
    #[cfg(feature = "fs")]
    path: Option<std::path::PathBuf>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of chunks across all datasets.
    pub fn len(&self) -> usize {
        self.read(|d| d.values().map(|ds| ds.entries.len()).sum()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// All stored chunks (for export / inspection).
    pub fn chunks(&self) -> Vec<StoredChunk> {
        self.read(|d| d.values().flat_map(|ds| ds.entries.iter().map(|e| e.chunk.clone())).collect())
            .unwrap_or_default()
    }

    fn read<T>(&self, f: impl FnOnce(&HashMap<String, Dataset>) -> T) -> Result<T, RagError> {
        let guard = self.inner.read().map_err(|_| RagError::Store("memory store lock poisoned".into()))?;
        Ok(f(&guard))
    }

    fn write<T>(&self, f: impl FnOnce(&mut HashMap<String, Dataset>) -> T) -> Result<T, RagError> {
        let result = {
            let mut guard = self.inner.write().map_err(|_| RagError::Store("memory store lock poisoned".into()))?;
            f(&mut guard)
        };
        self.persist()?;
        Ok(result)
    }

    fn insert_all(&self, chunks: Vec<StoredChunk>) -> Result<(), RagError> {
        self.write(|data| {
            for chunk in chunks {
                data.entry(chunk.dataset_id.clone()).or_default().insert(chunk);
            }
        })
    }

    #[cfg(not(feature = "fs"))]
    fn persist(&self) -> Result<(), RagError> {
        Ok(())
    }
}

#[cfg(feature = "fs")]
mod persistence {
    use super::*;
    use std::path::{Path, PathBuf};

    #[derive(serde::Serialize, serde::Deserialize)]
    struct FileFormat {
        version: u32,
        chunks: Vec<StoredChunk>,
    }

    impl MemoryStore {
        /// Open a store backed by `path`: existing content is loaded, and every
        /// change is written back atomically (temp file + rename).
        pub fn open(path: impl Into<PathBuf>) -> Result<Self, RagError> {
            let path = path.into();
            let store = Self { inner: Arc::default(), path: None };
            if path.exists() {
                let text =
                    std::fs::read_to_string(&path).map_err(|e| RagError::Io(format!("{}: {e}", path.display())))?;
                let file: FileFormat =
                    serde_json::from_str(&text).map_err(|e| RagError::Decode(format!("{}: {e}", path.display())))?;
                if file.version != 1 {
                    return Err(RagError::Decode(format!("unsupported store file version {}", file.version)));
                }
                store.insert_all(file.chunks)?;
            }
            Ok(Self { path: Some(path), ..store })
        }

        /// Write the whole store to `path` atomically.
        pub fn save_to(&self, path: &Path) -> Result<(), RagError> {
            let chunks = self.chunks();
            let text = serde_json::to_string(&FileFormat { version: 1, chunks })
                .map_err(|e| RagError::Store(format!("cannot encode store: {e}")))?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text).map_err(|e| RagError::Io(format!("{}: {e}", tmp.display())))?;
            std::fs::rename(&tmp, path).map_err(|e| RagError::Io(format!("{}: {e}", path.display())))
        }

        pub(super) fn persist(&self) -> Result<(), RagError> {
            match &self.path {
                Some(path) => self.save_to(path),
                None => Ok(()),
            }
        }
    }
}

impl VectorStore for MemoryStore {
    fn upsert(&self, chunks: Vec<StoredChunk>) -> BoxFuture<'_, Result<(), RagError>> {
        Box::pin(futures::future::ready(self.insert_all(chunks)))
    }

    fn delete_document<'a>(
        &'a self,
        dataset_id: &'a str,
        document_id: &'a str,
    ) -> BoxFuture<'a, Result<usize, RagError>> {
        let result = self.write(|data| match data.get_mut(dataset_id) {
            Some(ds) => ds.remove_where(|c| c.document_id == document_id),
            None => 0,
        });
        Box::pin(futures::future::ready(result))
    }

    fn delete_dataset<'a>(&'a self, dataset_id: &'a str) -> BoxFuture<'a, Result<usize, RagError>> {
        let result = self.write(|data| data.remove(dataset_id).map_or(0, |ds| ds.entries.len()));
        Box::pin(futures::future::ready(result))
    }

    fn search<'a>(&'a self, query: VectorQuery<'a>) -> BoxFuture<'a, Result<Vec<Hit>, RagError>> {
        let result = self.read(|data| {
            let q_norm = norm(query.vector);
            let mut scored: Vec<(f32, &Entry)> = Vec::new();
            for ds in query.datasets.iter().filter_map(|id| data.get(id)) {
                for entry in &ds.entries {
                    if entry.chunk.vector.len() != query.vector.len() || !passes(query.filter, &entry.chunk) {
                        continue;
                    }
                    let denom = q_norm * entry.norm;
                    let score = if denom > 0.0 { dot(query.vector, &entry.chunk.vector) / denom } else { 0.0 };
                    scored.push((score, entry));
                }
            }
            top_hits(scored, query.top_k)
        });
        Box::pin(futures::future::ready(result))
    }

    fn supports_keyword(&self) -> bool {
        true
    }

    fn keyword_search<'a>(&'a self, query: KeywordQuery<'a>) -> BoxFuture<'a, Result<Vec<Hit>, RagError>> {
        let terms: Vec<String> = tokenize(query.text);
        let result = self.read(|data| {
            let mut scored: Vec<(f32, &Entry)> = Vec::new();
            for ds in query.datasets.iter().filter_map(|id| data.get(id)) {
                let n = ds.entries.len();
                if n == 0 {
                    continue;
                }
                #[allow(clippy::cast_precision_loss)]
                let (n_f, avgdl) = (n as f32, (ds.total_length as f32 / n as f32).max(1.0));
                for entry in &ds.entries {
                    if !passes(query.filter, &entry.chunk) {
                        continue;
                    }
                    let mut score = 0.0f32;
                    #[allow(clippy::cast_precision_loss)]
                    let dl = entry.length as f32;
                    for term in &terms {
                        let Some(&tf) = entry.terms.get(term) else { continue };
                        #[allow(clippy::cast_precision_loss)]
                        let df = ds.doc_freq.get(term).copied().unwrap_or(0) as f32;
                        let idf = ((n_f - df + 0.5) / (df + 0.5) + 1.0).ln();
                        #[allow(clippy::cast_precision_loss)]
                        let tf = tf as f32;
                        score += idf * (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avgdl));
                    }
                    if score > 0.0 {
                        scored.push((score, entry));
                    }
                }
            }
            top_hits(scored, query.top_k)
        });
        Box::pin(futures::future::ready(result))
    }

    fn contains_hash<'a>(
        &'a self,
        dataset_id: &'a str,
        content_hash: &'a str,
    ) -> BoxFuture<'a, Result<bool, RagError>> {
        let result = self.read(|data| {
            data.get(dataset_id).is_some_and(|ds| ds.entries.iter().any(|e| e.chunk.content_hash == content_hash))
        });
        Box::pin(futures::future::ready(result))
    }
}

fn passes(filter: Option<&MetadataFilter>, chunk: &StoredChunk) -> bool {
    filter.is_none_or(|f| f.matches(&chunk.metadata))
}

fn top_hits(mut scored: Vec<(f32, &Entry)>, top_k: usize) -> Vec<Hit> {
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().take(top_k).map(|(score, e)| Hit::from_chunk(&e.chunk, score)).collect()
}

/// Dot product written as 8 independent accumulators so the compiler can vectorize it.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let chunks_a = a.chunks_exact(8);
    let chunks_b = b.chunks_exact(8);
    let (rest_a, rest_b) = (chunks_a.remainder(), chunks_b.remainder());
    for (x, y) in chunks_a.zip(chunks_b) {
        for ((slot, xi), yi) in acc.iter_mut().zip(x).zip(y) {
            *slot += xi * yi;
        }
    }
    let tail: f32 = rest_a.iter().zip(rest_b).map(|(x, y)| x * y).sum();
    acc.iter().sum::<f32>() + tail
}

/// Euclidean norm.
pub fn norm(v: &[f32]) -> f32 {
    dot(v, v).sqrt()
}

/// Cosine similarity (0 when either vector is zero or lengths differ).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let denom = norm(a) * norm(b);
    if denom > 0.0 { dot(a, b) / denom } else { 0.0 }
}

/// Lower-cased alphanumeric tokens. CJK characters become one token each so
/// languages without spaces still get a usable keyword index.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if is_cjk(c) {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(c.to_string());
        } else if c.is_alphanumeric() {
            current.extend(c.to_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_cjk(c: char) -> bool {
    matches!(u32::from(c), 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

fn term_frequencies(text: &str) -> HashMap<String, u32> {
    let mut map = HashMap::new();
    for t in tokenize(text) {
        *map.entry(t).or_insert(0) += 1;
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, ds: &str, text: &str, vector: Vec<f32>) -> StoredChunk {
        StoredChunk {
            id: id.into(),
            dataset_id: ds.into(),
            document_id: format!("doc-{id}"),
            title: String::new(),
            text: text.into(),
            metadata: serde_json::Map::new(),
            content_hash: crate::types::content_hash(ds, text),
            vector,
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    #[test]
    fn cosine_ranking_and_dataset_scoping() {
        let store = MemoryStore::new();
        block_on(store.upsert(vec![
            chunk("a", "d1", "a", vec![1.0, 0.0, 0.0]),
            chunk("b", "d1", "b", vec![0.7, 0.7, 0.0]),
            chunk("c", "d2", "c", vec![1.0, 0.0, 0.0]),
        ]))
        .unwrap();
        let ds = vec!["d1".to_owned()];
        let hits =
            block_on(store.search(VectorQuery { datasets: &ds, vector: &[1.0, 0.1, 0.0], top_k: 5, filter: None }))
                .unwrap();
        assert_eq!(hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn bm25_prefers_rare_terms() {
        let store = MemoryStore::new();
        block_on(store.upsert(vec![
            chunk("1", "d", "the cat sat on the mat", vec![1.0]),
            chunk("2", "d", "the dog chased the cat", vec![1.0]),
            chunk("3", "d", "quantum entanglement explained", vec![1.0]),
        ]))
        .unwrap();
        let ds = vec!["d".to_owned()];
        let hits = block_on(store.keyword_search(KeywordQuery { datasets: &ds, text: "dog", top_k: 5, filter: None }))
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "2");
        let hits =
            block_on(store.keyword_search(KeywordQuery { datasets: &ds, text: "cat quantum", top_k: 5, filter: None }))
                .unwrap();
        assert_eq!(hits[0].id, "3", "rare term outweighs common one");
    }

    #[test]
    fn upsert_replaces_and_delete_updates_index() {
        let store = MemoryStore::new();
        block_on(store.upsert(vec![chunk("x", "d", "apple banana", vec![1.0]), chunk("y", "d", "apple", vec![1.0])]))
            .unwrap();
        block_on(store.upsert(vec![chunk("x", "d", "cherry", vec![1.0])])).unwrap();
        assert_eq!(store.len(), 2);
        let ds = vec!["d".to_owned()];
        let hits =
            block_on(store.keyword_search(KeywordQuery { datasets: &ds, text: "banana", top_k: 5, filter: None }))
                .unwrap();
        assert!(hits.is_empty());
        assert_eq!(block_on(store.delete_document("d", "doc-y")).unwrap(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(block_on(store.delete_dataset("d")).unwrap(), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn dot_matches_naive() {
        let a: Vec<f32> = (0..37).map(|i| i as f32 * 0.5).collect();
        let b: Vec<f32> = (0..37).map(|i| (37 - i) as f32).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - naive).abs() < 1e-3);
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
    }

    #[test]
    fn tokenizer_handles_cjk() {
        assert_eq!(tokenize("Hello, World! 日本"), vec!["hello", "world", "日", "本"]);
    }

    #[cfg(feature = "fs")]
    #[test]
    fn persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!("graphrt-rag-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kb.json");
        let _ = std::fs::remove_file(&path);
        {
            let store = MemoryStore::open(&path).unwrap();
            block_on(store.upsert(vec![chunk("a", "d", "persist me", vec![0.5, 0.5])])).unwrap();
        }
        let reopened = MemoryStore::open(&path).unwrap();
        assert_eq!(reopened.len(), 1);
        let ds = vec!["d".to_owned()];
        let hits =
            block_on(reopened.keyword_search(KeywordQuery { datasets: &ds, text: "persist", top_k: 1, filter: None }))
                .unwrap();
        assert_eq!(hits[0].content, "persist me");
        block_on(reopened.delete_dataset("d")).unwrap();
        assert!(MemoryStore::open(&path).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
