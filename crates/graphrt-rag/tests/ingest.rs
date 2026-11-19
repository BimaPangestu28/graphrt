//! Regression tests for ingestion consistency and store integrity.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use graphrt_rag::{
    ChunkerConfig, Document, Embedder, HashEmbedder, Ingestor, KeywordQuery, MemoryStore, RagError, StoredChunk,
    VectorQuery, VectorStore,
};

fn small_chunks() -> ChunkerConfig {
    ChunkerConfig { max_chars: 40, overlap: 0, markdown: false, parent_max_chars: None }
}

fn ingestor(store: &Arc<MemoryStore>, embedder: Arc<dyn Embedder>) -> Ingestor {
    Ingestor::new(store.clone(), embedder).chunker(small_chunks())
}

async fn texts(store: &MemoryStore, doc: &str) -> Vec<String> {
    let mut chunks: Vec<StoredChunk> = store.document_chunks("d", doc).await.unwrap();
    chunks.sort_by_key(|c| c.metadata.get("chunk_index").and_then(serde_json::Value::as_u64));
    chunks.into_iter().map(|c| c.text.trim().to_owned()).collect()
}

const A: &str = "Alpha paragraph about apples.";
const B: &str = "Bravo paragraph about bananas.";
const X: &str = "Xray paragraph inserted first.";

#[tokio::test]
async fn reingest_with_inserted_paragraph_keeps_every_chunk() {
    let store = Arc::new(MemoryStore::new());
    let ing = ingestor(&store, Arc::new(HashEmbedder::new(32)));
    let v1 = ing.ingest(&Document::new("d", "doc", format!("{A}\n\n{B}"))).await.unwrap();
    assert_eq!(v1.stored, 2);

    let v2 = ing.ingest(&Document::new("d", "doc", format!("{X}\n\n{A}\n\n{B}"))).await.unwrap();
    assert_eq!((v2.stored, v2.skipped, v2.removed), (1, 2, 0));
    assert_eq!(texts(&store, "doc").await, vec![X, A, B]);

    // A shrinking document drops the chunks it no longer contains.
    let v3 = ing.ingest(&Document::new("d", "doc", B)).await.unwrap();
    assert_eq!((v3.stored, v3.skipped, v3.removed), (0, 1, 2));
    assert_eq!(texts(&store, "doc").await, vec![B]);
}

#[tokio::test]
async fn append_mode_keeps_previous_chunks() {
    let store = Arc::new(MemoryStore::new());
    let ing = ingestor(&store, Arc::new(HashEmbedder::new(32))).replace_existing(false);
    ing.ingest(&Document::new("d", "doc", A)).await.unwrap();
    let report = ing.ingest(&Document::new("d", "doc", B)).await.unwrap();
    assert_eq!(report.removed, 0);
    assert_eq!(store.document_chunks("d", "doc").await.unwrap().len(), 2);
}

/// Embedder that fails every call after the first `ok_calls`.
#[derive(Debug)]
struct FlakyEmbedder {
    inner: HashEmbedder,
    calls: AtomicUsize,
    ok_calls: usize,
}

impl Embedder for FlakyEmbedder {
    fn model(&self) -> &str {
        "flaky"
    }

    fn embed_documents<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>, RagError>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n >= self.ok_calls {
            return Box::pin(futures::future::ready(Err(RagError::Network("provider down".into()))));
        }
        self.inner.embed_documents(texts)
    }
}

#[tokio::test]
async fn embedder_failure_keeps_the_previous_version() {
    let store = Arc::new(MemoryStore::new());
    let flaky = Arc::new(FlakyEmbedder { inner: HashEmbedder::new(32), calls: AtomicUsize::new(0), ok_calls: 1 });
    let ing = ingestor(&store, flaky);
    ing.ingest(&Document::new("d", "doc", format!("{A}\n\n{B}"))).await.unwrap();

    let err = ing.ingest(&Document::new("d", "doc", X)).await.unwrap_err();
    assert!(matches!(err, RagError::Network(_)), "{err:?}");
    assert_eq!(texts(&store, "doc").await, vec![A, B], "old version must survive a failed re-ingest");
}

#[tokio::test]
async fn identical_text_in_two_documents_is_independent() {
    let store = Arc::new(MemoryStore::new());
    let ing = ingestor(&store, Arc::new(HashEmbedder::new(32)));
    ing.ingest(&Document::new("d", "a", "shared license text").title("A")).await.unwrap();
    let b = ing.ingest(&Document::new("d", "b", "shared license text").title("B")).await.unwrap();
    assert_eq!(b.stored, 1);

    store.delete_document("d", "a").await.unwrap();
    let ds = vec!["d".to_owned()];
    let hits =
        store.keyword_search(KeywordQuery { datasets: &ds, text: "license", top_k: 5, filter: None }).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!((hits[0].document_id.as_str(), hits[0].title.as_str()), ("b", "B"));
}

#[tokio::test]
async fn embedding_dimension_mismatch_is_an_error() {
    let store = Arc::new(MemoryStore::new());
    ingestor(&store, Arc::new(HashEmbedder::new(32))).ingest(&Document::new("d", "a", A)).await.unwrap();

    // A different embedder cannot silently mix vector sizes into the dataset.
    let err = ingestor(&store, Arc::new(HashEmbedder::new(64))).ingest(&Document::new("d", "b", B)).await.unwrap_err();
    assert!(matches!(err, RagError::Input(ref m) if m.contains("32-dimensional")), "{err:?}");

    // Searching with the wrong dimension fails instead of returning nothing.
    let ds = vec!["d".to_owned()];
    let query = vec![0.5f32; 64];
    let err = store.search(VectorQuery { datasets: &ds, vector: &query, top_k: 3, filter: None }).await.unwrap_err();
    assert!(matches!(err, RagError::Input(_)), "{err:?}");

    // Non-finite vectors are rejected.
    let mut chunk = store.document_chunks("d", "a").await.unwrap().remove(0);
    chunk.id = "nan".into();
    chunk.vector = vec![f32::NAN; 32];
    assert!(store.upsert(vec![chunk]).await.is_err());
}

#[cfg(feature = "fs")]
#[test]
fn persistence_survives_concurrent_writers() {
    let dir = std::env::temp_dir().join(format!("graphrt-rag-concurrent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kb.json");
    let _ = std::fs::remove_file(&path);
    let store = MemoryStore::open(&path).unwrap();

    let threads: Vec<_> = (0..8)
        .map(|t| {
            let store = store.clone();
            std::thread::spawn(move || {
                for i in 0..10 {
                    let chunk = StoredChunk {
                        id: format!("{t}-{i}"),
                        dataset_id: "d".into(),
                        document_id: format!("doc-{t}"),
                        title: String::new(),
                        text: format!("thread {t} item {i}"),
                        metadata: serde_json::Map::new(),
                        content_hash: format!("{t}-{i}"),
                        vector: vec![1.0, 0.5],
                    };
                    futures::executor::block_on(store.upsert(vec![chunk])).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let reopened = MemoryStore::open(&path).unwrap();
    assert_eq!(reopened.len(), 80);
    // No temporary files are left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "fs")]
#[tokio::test]
async fn bulk_load_with_autosave_off_writes_once_on_flush() {
    let dir = std::env::temp_dir().join(format!("graphrt-rag-bulk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("kb.json");
    let _ = std::fs::remove_file(&path);
    let store = Arc::new(MemoryStore::open(&path).unwrap());
    store.set_autosave(false);
    ingestor(&store, Arc::new(HashEmbedder::new(16))).ingest(&Document::new("d", "a", A)).await.unwrap();
    assert!(!path.exists(), "nothing written before flush");
    store.flush().unwrap();
    assert_eq!(MemoryStore::open(&path).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
