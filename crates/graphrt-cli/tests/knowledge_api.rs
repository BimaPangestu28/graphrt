//! In-process tests of `/v1/knowledge` (no sockets, no network).
#![cfg(all(feature = "serve", feature = "rag"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{AppState, build_state, router, router_with_uploads};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A temp directory with one workflow and a knowledge config, removed on drop.
struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-knowledge-api-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("flows")).unwrap();
        let hello = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello.yaml");
        std::fs::copy(hello, dir.join("flows/hello.yaml")).unwrap();
        let knowledge = json!({
            "embedders": { "e": { "type": "hash", "dimensions": 64 } },
            "stores": { "local": { "type": "memory" } },
            "datasets": [
                { "id": "handbook", "name": "Handbook", "store": "local", "embedder": "e" },
                { "id": "remote", "retriever": { "type": "http", "url": "http://127.0.0.1:9/search" } }
            ]
        });
        std::fs::write(dir.join("knowledge.json"), knowledge.to_string()).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn state(fixture: &Fixture, with_knowledge: bool) -> Arc<AppState> {
    let files = vec![("hello".to_owned(), fixture.path().join("flows/hello.yaml"))];
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
        knowledge: with_knowledge.then(|| fixture.path().join("knowledge.json")),
        ..ServiceOptions::default()
    };
    Arc::new(build_state(&files, &options, Limits::default(), None).await.expect("state"))
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn json_request(method: &str, uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn documents_can_be_added_listed_inspected_searched_and_deleted() {
    let fixture = Fixture::new("crud");
    let app = router(state(&fixture, true).await, 1 << 20);

    let (status, body) = send(&app, get("/v1/knowledge/datasets")).await;
    assert_eq!(status, StatusCode::OK);
    let managed: Vec<(String, bool)> = body["datasets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["id"].as_str().unwrap().to_owned(), d["managed"].as_bool().unwrap()))
        .collect();
    assert_eq!(managed, [("handbook".to_owned(), true), ("remote".to_owned(), false)]);
    assert_eq!(body["datasets"][0]["chunking"]["max_chars"], 1000, "managed datasets report their chunking");
    assert_eq!(body["datasets"][0]["store"], "local");
    assert_eq!(body["datasets"][0]["runtime"], false);
    assert_eq!(body["datasets"][1]["chunking"], Value::Null);

    let leave = json!({ "document_id": "leave", "title": "Leave policy",
                        "text": "Every employee gets 24 days of paid annual leave.",
                        "metadata": { "team": "hr" } });
    let (status, body) = send(&app, json_request("POST", "/v1/knowledge/datasets/handbook/documents", &leave)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["report"]["stored"].as_u64().unwrap() >= 1);

    let markdown = "# Expenses\n\nExpenses below 500 USD are approved by your manager.\n";
    let upload = Request::put("/v1/knowledge/datasets/handbook/documents/expenses/file?name=expenses.md")
        .header("content-type", "text/markdown")
        .body(Body::from(markdown))
        .unwrap();
    let (status, body) = send(&app, upload).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["report"]["stored"].as_u64().unwrap() >= 1);

    let (status, body) = send(&app, get("/v1/knowledge/datasets/handbook/documents?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["documents"][0]["document_id"], "expenses");
    assert_eq!(body["next"], "expenses");
    let (_, body) = send(&app, get("/v1/knowledge/datasets/handbook/documents?limit=1&after=expenses")).await;
    assert_eq!(body["documents"][0]["document_id"], "leave");
    assert_eq!(body["documents"][0]["metadata"]["team"], "hr");
    let (_, body) = send(&app, get("/v1/knowledge/datasets/handbook/documents?after=leave")).await;
    assert_eq!(body["documents"], json!([]));
    assert_eq!(body["next"], Value::Null);

    let (status, body) = send(&app, get("/v1/knowledge/datasets/handbook/documents/leave")).await;
    assert_eq!(status, StatusCode::OK);
    let chunk = &body["chunks"][0];
    assert!(chunk["text"].as_str().unwrap().contains("24 days"));
    assert!(chunk.get("vector").is_none(), "vectors are never sent: {chunk}");

    let query = json!({ "query": "annual leave days", "datasets": ["handbook"], "top_k": 1 });
    let (status, body) = send(&app, json_request("POST", "/v1/knowledge/retrieve", &query)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hits"][0]["document_id"], "leave");

    let disable = json!({ "enabled": false });
    let (status, body) =
        send(&app, json_request("PATCH", "/v1/knowledge/datasets/handbook/documents/leave", &disable)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = send(&app, json_request("POST", "/v1/knowledge/retrieve", &query)).await;
    assert!(body["hits"].as_array().unwrap().iter().all(|h| h["document_id"] != "leave"), "{body}");
    let (_, body) = send(&app, get("/v1/knowledge/datasets/handbook/documents?after=expenses")).await;
    assert_eq!(body["documents"][0]["enabled"], false);
    let enable = json!({ "enabled": true });
    send(&app, json_request("PATCH", "/v1/knowledge/datasets/handbook/documents/leave", &enable)).await;
    let (_, body) = send(&app, json_request("POST", "/v1/knowledge/retrieve", &query)).await;
    assert_eq!(body["hits"][0]["document_id"], "leave");
    let (status, _) =
        send(&app, json_request("PATCH", "/v1/knowledge/datasets/handbook/documents/nope", &enable)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let delete = Request::delete("/v1/knowledge/datasets/handbook/documents/leave").body(Body::empty()).unwrap();
    let (status, body) = send(&app, delete).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["deleted_chunks"].as_u64().unwrap() >= 1);
    let (status, _) = send(&app, get("/v1/knowledge/datasets/handbook/documents/leave")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn errors_are_specific() {
    let fixture = Fixture::new("errors");
    let app = router(state(&fixture, true).await, 1 << 20);
    let doc = json!({ "document_id": "d", "text": "x" });

    let (status, body) = send(&app, json_request("POST", "/v1/knowledge/datasets/remote/documents", &doc)).await;
    assert_eq!(status, StatusCode::CONFLICT, "external retriever datasets are read-only: {body}");
    let (status, _) = send(&app, json_request("POST", "/v1/knowledge/datasets/missing/documents", &doc)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, get("/v1/knowledge/datasets/remote/documents")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let empty_id = json!({ "document_id": " ", "text": "x" });
    let (status, _) = send(&app, json_request("POST", "/v1/knowledge/datasets/handbook/documents", &empty_id)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let unknown_field = json!({ "document_id": "d", "text": "x", "txet": "typo" });
    let (status, _) =
        send(&app, json_request("POST", "/v1/knowledge/datasets/handbook/documents", &unknown_field)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn uploads_have_their_own_size_limit() {
    let fixture = Fixture::new("limits");
    let app = router_with_uploads(state(&fixture, true).await, 64, 4096);
    let file = |bytes: usize| {
        Request::put("/v1/knowledge/datasets/handbook/documents/big/file?name=big.txt")
            .header("content-type", "text/plain")
            .body(Body::from("a ".repeat(bytes / 2)))
            .unwrap()
    };
    // Larger than the 64-byte API limit but within the upload limit.
    let (status, body) = send(&app, file(1024)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = send(&app, file(8192)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn without_a_knowledge_base_the_endpoints_say_so() {
    let fixture = Fixture::new("none");
    let app = router(state(&fixture, false).await, 1 << 20);
    let (status, body) = send(&app, get("/v1/knowledge/datasets")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("--knowledge"), "{body}");
}

#[tokio::test]
async fn datasets_can_be_created_and_deleted_and_survive_restarts() {
    let fixture = Fixture::new("datasets");
    let app = router(state(&fixture, true).await, 1 << 20);

    let (status, body) = send(&app, get("/v1/knowledge/resources")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stores"], json!(["local"]));
    assert_eq!(body["embedders"], json!(["e"]));

    let spec = json!({ "id": "notes", "name": "Notes", "store": "local", "embedder": "e",
                       "chunking": { "max_chars": 300, "parent_max_chars": 900 } });
    let (status, body) = send(&app, json_request("POST", "/v1/knowledge/datasets", &spec)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["runtime"], true);
    assert_eq!(body["chunking"]["parent_max_chars"], 900);

    let doc = json!({ "document_id": "n1", "text": "Runtime datasets are searchable right away." });
    let (status, _) = send(&app, json_request("POST", "/v1/knowledge/datasets/notes/documents", &doc)).await;
    assert_eq!(status, StatusCode::OK);
    let query = json!({ "query": "runtime datasets searchable", "datasets": ["notes"] });
    let (_, body) = send(&app, json_request("POST", "/v1/knowledge/retrieve", &query)).await;
    assert_eq!(body["hits"][0]["document_id"], "n1");

    let saved = fixture.path().join("knowledge.datasets.json");
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&saved).unwrap()).unwrap();
    assert_eq!(on_disk[0]["id"], "notes");

    // A restart rebuilds the dataset from the saved list.
    let restarted = router(state(&fixture, true).await, 1 << 20);
    let (_, body) = send(&restarted, get("/v1/knowledge/datasets")).await;
    let ids: Vec<&str> = body["datasets"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["handbook", "remote", "notes"]);

    let (status, body) = send(&app, json_request("POST", "/v1/knowledge/datasets", &spec)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "duplicate id: {body}");
    let bad = json!({ "id": "a/b", "store": "local", "embedder": "e" });
    let (status, _) = send(&app, json_request("POST", "/v1/knowledge/datasets", &bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(&app, Request::delete("/v1/knowledge/datasets/handbook").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT, "config datasets are not deletable over the API");

    let (status, body) = send(&app, Request::delete("/v1/knowledge/datasets/notes").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted_chunks"], 1);
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&saved).unwrap()).unwrap();
    assert_eq!(on_disk, json!([]));
    let (status, _) = send(&app, get("/v1/knowledge/datasets/notes/documents")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
