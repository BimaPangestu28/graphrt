//! End-to-end retrieval tests: the node through the real engine, and the HTTP
//! components against a local HTTP/1.1 responder.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;

use futures::StreamExt;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_core::{Value, Workflow};
use graphrt_rag::{
    Document, HashEmbedder, KnowledgeBase, MemoryStore, MetadataFilter, Reranker, RetrievalRequest, VectorStore,
};
use serde_json::json;

fn workflow(retrieval: serde_json::Value) -> Arc<Graph> {
    let mut node = json!({"id": "kr", "type": "knowledge-retrieval", "query_variable_selector": ["start", "q"]});
    if let (Some(obj), Some(extra)) = (node.as_object_mut(), retrieval.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "q", "type": "string", "required": true}]},
            node,
            {"id": "end", "type": "end", "outputs": [{"variable": "result", "value_selector": ["kr", "result"]}]}
        ],
        "edges": [{"source": "start", "target": "kr"}, {"source": "kr", "target": "end"}]
    }});
    let mut registry = Registry::with_builtins();
    graphrt_rag::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

async fn knowledge_base() -> KnowledgeBase {
    let embedder = Arc::new(HashEmbedder::new(256));
    let kb = KnowledgeBase::new()
        .with_store_dataset("handbook", Arc::new(MemoryStore::new()), embedder.clone(), None)
        .with_store_dataset("blog", Arc::new(MemoryStore::new()), embedder, None);
    let docs = [
        ("handbook", "vacation", "Employees get 25 vacation days per year.", "hr"),
        ("handbook", "laptop", "Every engineer receives a laptop on the first day.", "it"),
        ("handbook", "expenses", "Expenses above 500 need manager approval.", "finance"),
        ("blog", "launch", "We launched our new product in March.", "news"),
    ];
    for (ds, id, text, team) in docs {
        let mut meta = serde_json::Map::new();
        meta.insert("team".into(), team.into());
        kb.ingestor(ds).unwrap().ingest(&Document::new(ds, id, text).title(id).metadata(meta)).await.unwrap();
    }
    kb
}

async fn run(graph: Arc<Graph>, kb: KnowledgeBase, query: &str) -> Event {
    let engine = Engine::new(graph).with_services(Services::new().with(kb));
    let events: Vec<Event> = engine.run(RunInput::new().input("q", query)).collect().await;
    events.last().cloned().unwrap()
}

fn result_of(event: &Event) -> Vec<Value> {
    match event {
        Event::RunSucceeded { outputs, .. } => outputs["result"].as_array().unwrap().to_vec(),
        other => panic!("expected success, got {other:?}"),
    }
}

#[tokio::test]
async fn node_retrieves_through_engine() {
    let graph = workflow(json!({"dataset_ids": ["handbook", "blog"], "multiple_retrieval_config": {"top_k": 2}}));
    let result = result_of(&run(graph, knowledge_base().await, "how many vacation days do employees get").await);
    assert_eq!(result.len(), 2);
    let first = result[0].as_object().unwrap();
    assert_eq!(first["document_id"], Value::from("vacation"));
    assert!(first["content"].as_str().unwrap().contains("25 vacation days"));
    assert_eq!(first["metadata"].get_path(&["team"]), Some(Value::from("hr")));
}

#[tokio::test]
async fn hybrid_weights_and_metadata_filter() {
    let graph = workflow(json!({
        "dataset_ids": ["handbook"],
        "multiple_retrieval_config": {
            "top_k": 3,
            "reranking_enable": true,
            "reranking_mode": "weighted_score",
            "weights": {"vector_setting": {"vector_weight": 0.3}, "keyword_setting": {"keyword_weight": 0.7}}
        },
        "metadata_filtering_mode": "manual",
        "metadata_filtering_conditions": {"logical_operator": "and", "conditions": [
            {"name": "team", "comparison_operator": "is not", "value": "hr"}
        ]}
    }));
    let result = result_of(&run(graph, knowledge_base().await, "laptop for engineers").await);
    assert!(!result.is_empty());
    assert_eq!(result[0].as_object().unwrap()["document_id"], Value::from("laptop"));
    assert!(result.iter().all(|r| r.as_object().unwrap()["metadata"].get_path(&["team"]) != Some(Value::from("hr"))));
}

#[tokio::test]
async fn unknown_dataset_and_missing_service_fail_clearly() {
    let graph = workflow(json!({"dataset_ids": ["nope"]}));
    match run(graph.clone(), knowledge_base().await, "x").await {
        Event::RunFailed { error, .. } => assert!(error.contains("unknown dataset"), "{error}"),
        other => panic!("{other:?}"),
    }
    let engine = Engine::new(graph);
    let events: Vec<Event> = engine.run(RunInput::new().input("q", "x")).collect().await;
    match events.last().unwrap() {
        Event::RunFailed { error, .. } => assert!(error.contains("knowledge base"), "{error}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn invalid_config_rejected_at_build() {
    let mut registry = Registry::with_builtins();
    graphrt_rag::register(&mut registry);
    let bad = [
        json!({"query_variable_selector": ["sys", "query"], "dataset_ids": []}),
        json!({"query_variable_selector": ["sys", "query"], "dataset_ids": ["a"],
               "metadata_filtering_conditions": {"conditions": [{"name": "x", "comparison_operator": "resembles"}]}}),
    ];
    for cfg in bad {
        let doc = graphrt_core::dsl::NodeDoc::new("kr", "knowledge-retrieval", cfg);
        assert!(registry.build(&doc).is_err());
    }
    let descriptor = registry.descriptor("knowledge-retrieval").unwrap();
    assert_eq!(descriptor.source, "graphrt-rag");
}

#[derive(Debug)]
struct ReverseReranker;

impl Reranker for ReverseReranker {
    fn rerank<'a>(
        &'a self,
        _q: &'a str,
        docs: &'a [String],
    ) -> futures::future::BoxFuture<'a, Result<Vec<f32>, graphrt_rag::RagError>> {
        // Prefer shorter documents.
        let scores = docs.iter().map(|d| 1.0 / (d.len() as f32)).collect();
        Box::pin(futures::future::ready(Ok(scores)))
    }
}

#[tokio::test]
async fn reranker_reorders_and_threshold_applies() {
    let store: Arc<dyn VectorStore> = Arc::new(MemoryStore::new());
    let embedder = Arc::new(HashEmbedder::new(64));
    let kb = KnowledgeBase::new().with_store_dataset("d", store, embedder, Some(Arc::new(ReverseReranker)));
    let ing = kb.ingestor("d").unwrap();
    ing.ingest(&Document::new("d", "long", "apple apple apple apple pie recipe with many many words")).await.unwrap();
    ing.ingest(&Document::new("d", "short", "apple tart")).await.unwrap();
    let req = RetrievalRequest::new("apple", vec!["d".into()]).top_k(2).rerank(true);
    let hits = kb.retrieve(&req).await.unwrap();
    assert_eq!(hits[0].document_id, "short");
    let req = req.threshold(Some(0.05));
    let hits = kb.retrieve(&req).await.unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn ingest_dedupes_identical_chunks() {
    let kb = KnowledgeBase::new().with_store_dataset(
        "d",
        Arc::new(MemoryStore::new()),
        Arc::new(HashEmbedder::new(32)),
        None,
    );
    let ing = kb.ingestor("d").unwrap();
    let first = ing.ingest(&Document::new("d", "a", "same text")).await.unwrap();
    let second = ing.ingest(&Document::new("d", "b", "same text")).await.unwrap();
    assert_eq!((first.stored, second.stored, second.skipped), (1, 0, 1));
    let _ = MetadataFilter::default();
}

#[cfg(feature = "http")]
mod http {
    use super::*;
    use graphrt_rag::{
        Embedder, Hit, HttpReranker, HttpRerankerConfig, HttpRetriever, HttpRetrieverConfig, HttpSettings,
        OpenAiEmbedder, OpenAiEmbedderConfig, RagError, Secret,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    /// Serve canned `(status, body)` replies, one per connection; forward raw requests.
    async fn serve(replies: Vec<(&'static str, String)>) -> (String, mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for (status, body) in replies {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let raw = read_request(&mut socket).await;
                let _ = tx.send(raw);
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(idx) = text.find("\r\n\r\n") {
                let len = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if buf.len() >= idx + 4 + len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn body_json(raw: &str) -> serde_json::Value {
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default();
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn openai_embedder_batches_and_orders() {
        let (url, mut requests) = serve(vec![
            (
                "200 OK",
                json!({"data": [{"index": 1, "embedding": [0.0, 1.0]}, {"index": 0, "embedding": [1.0, 0.0]}]})
                    .to_string(),
            ),
            ("200 OK", json!({"data": [{"index": 0, "embedding": [0.5, 0.5]}]}).to_string()),
        ])
        .await;
        let mut cfg = OpenAiEmbedderConfig::new(format!("{url}/v1"), "nomic-embed-text").api_key("sk-test");
        cfg.batch_size = 2;
        cfg.document_prefix = "search_document: ".into();
        let embedder = OpenAiEmbedder::new(cfg).unwrap();
        let texts = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let vectors = embedder.embed_documents(&texts).await.unwrap();
        assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![0.5, 0.5]]);
        let first = requests.recv().await.unwrap();
        assert!(first.starts_with("POST /v1/embeddings"));
        assert!(first.to_ascii_lowercase().contains("authorization: bearer sk-test"));
        let body = body_json(&first);
        assert_eq!(body["model"], "nomic-embed-text");
        assert_eq!(body["input"], json!(["search_document: a", "search_document: b"]));
    }

    #[tokio::test]
    async fn embedder_maps_errors() {
        let (url, _rx) = serve(vec![
            ("401 Unauthorized", "{\"error\":\"bad key\"}".into()),
            ("429 Too Many Requests", "{}".into()),
            ("503 Service Unavailable", "{}".into()),
        ])
        .await;
        let embedder = OpenAiEmbedder::new(OpenAiEmbedderConfig::new(url, "m")).unwrap();
        assert!(matches!(embedder.embed_query("x").await, Err(RagError::Auth(_))));
        let e = embedder.embed_query("x").await.unwrap_err();
        assert!(matches!(e, RagError::RateLimit { .. }) && e.is_retryable());
        let e = embedder.embed_query("x").await.unwrap_err();
        assert!(e.is_retryable(), "{e:?}");
    }

    #[tokio::test]
    async fn http_retriever_contract() {
        let (url, mut requests) = serve(vec![(
            "200 OK",
            json!({"records": [
                {"content": "Refunds take 5 days.", "score": 0.91, "title": "Refunds", "metadata": {"lang": "en"}},
                {"content": "Shipping is free.", "score": 0.4}
            ]})
            .to_string(),
        )])
        .await;
        let http = HttpSettings { api_key: Secret::new("tok"), ..HttpSettings::default() };
        let retriever = HttpRetriever::new(HttpRetrieverConfig { url: format!("{url}/search"), http }).unwrap();
        let kb = KnowledgeBase::new().with_retriever("crm", Arc::new(retriever));
        let hits: Vec<Hit> = kb
            .retrieve(&RetrievalRequest::new("refund time", vec!["crm".into()]).top_k(5).threshold(Some(0.5)))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Refunds");
        assert_eq!(hits[0].dataset_id, "crm");
        let raw = requests.recv().await.unwrap();
        let body = body_json(&raw);
        assert_eq!(body["query"], "refund time");
        assert_eq!(body["datasets"], json!(["crm"]));
    }

    #[tokio::test]
    async fn http_reranker_cohere_and_voyage_shapes() {
        let (url, _rx) = serve(vec![
            (
                "200 OK",
                json!({"results": [{"index": 1, "relevance_score": 0.9}, {"index": 0, "relevance_score": 0.2}]})
                    .to_string(),
            ),
            (
                "200 OK",
                json!({"data": [{"index": 0, "relevance_score": 0.7}, {"index": 1, "relevance_score": 0.1}]})
                    .to_string(),
            ),
        ])
        .await;
        let reranker = HttpReranker::new(HttpRerankerConfig {
            base_url: url,
            model: "rerank".into(),
            http: HttpSettings::default(),
        })
        .unwrap();
        let docs = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(reranker.rerank("q", &docs).await.unwrap(), vec![0.2, 0.9]);
        assert_eq!(reranker.rerank("q", &docs).await.unwrap(), vec![0.7, 0.1]);
    }

    /// Live check against a local Ollama: `GRAPHRT_LIVE_OLLAMA=1 cargo test -p graphrt-rag live_ollama`.
    #[tokio::test]
    async fn live_ollama_embeddings() {
        if std::env::var("GRAPHRT_LIVE_OLLAMA").ok().as_deref() != Some("1") {
            return;
        }
        let mut cfg = OpenAiEmbedderConfig::new("http://localhost:11434/v1", "nomic-embed-text");
        cfg.query_prefix = "search_query: ".into();
        cfg.document_prefix = "search_document: ".into();
        let embedder: Arc<dyn Embedder> = Arc::new(OpenAiEmbedder::new(cfg).unwrap());
        let kb = KnowledgeBase::new().with_store_dataset("live", Arc::new(MemoryStore::new()), embedder, None);
        let ing = kb.ingestor("live").unwrap();
        for (id, text) in [
            ("cats", "Cats are independent pets that purr when content."),
            ("rust", "Rust is a systems programming language focused on memory safety."),
            ("paris", "Paris is the capital city of France."),
        ] {
            ing.ingest(&Document::new("live", id, text)).await.unwrap();
        }
        let hits = kb
            .retrieve(&RetrievalRequest::new("Which language prevents memory bugs?", vec!["live".into()]).top_k(1))
            .await
            .unwrap();
        assert_eq!(hits[0].document_id, "rust", "{hits:?}");
        let hits = kb
            .retrieve(&RetrievalRequest::new("What is France's capital?", vec!["live".into()]).top_k(1))
            .await
            .unwrap();
        assert_eq!(hits[0].document_id, "paris", "{hits:?}");
    }
}
