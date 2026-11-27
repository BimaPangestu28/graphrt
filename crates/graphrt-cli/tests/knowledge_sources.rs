//! `/v1/knowledge/sources`: the `sources:` of a knowledge config are listed,
//! synced on request, and their documents are searchable right away.
#![cfg(all(feature = "serve", feature = "connectors"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{AppState, build_state, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

/// A one page web site on a loopback port.
async fn site(body: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let (status, text) =
                    if request.starts_with("GET / ") { ("200 OK", body) } else { ("404 Not Found", "") };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    base
}

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn state(fixture: &Fixture, sources: Value) -> Arc<AppState> {
    let dir = &fixture.0;
    std::fs::create_dir_all(dir.join("flows")).unwrap();
    let hello = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello.yaml");
    std::fs::copy(hello, dir.join("flows/hello.yaml")).unwrap();
    let knowledge = json!({
        "embedders": { "e": { "type": "hash", "dimensions": 64 } },
        "stores": { "local": { "type": "memory" } },
        "datasets": [{ "id": "site", "store": "local", "embedder": "e" }],
        "sources": sources
    });
    std::fs::write(dir.join("knowledge.json"), knowledge.to_string()).unwrap();
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
        knowledge: Some(dir.join("knowledge.json")),
        ..ServiceOptions::default()
    };
    let files = vec![("hello".to_owned(), dir.join("flows/hello.yaml"))];
    Arc::new(build_state(&files, &options, Limits::default(), None).await.expect("state"))
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn sources_are_listed_synced_on_request_and_searchable() {
    let base = site("<html><title>Hours</title><p>The shop opens at eight every morning.</p></html>").await;
    let fixture = Fixture(std::env::temp_dir().join(format!("graphrt-knowledge-sources-{}", std::process::id())));
    let sources = json!([
        { "id": "home", "name": "Home page", "dataset": "site", "type": "web", "urls": [format!("{base}/")],
          "allow_private": true, "schedule": { "every": "6h" } },
        { "id": "drive", "dataset": "site", "type": "google_drive", "folder_id": "root",
          "auth": { "type": "bearer", "token_env": "GRAPHRT_TEST_UNSET_DRIVE_TOKEN" } }
    ]);
    let app = router(state(&fixture, sources).await, 1 << 20);

    let (status, body) = send(&app, Request::get("/v1/knowledge/sources").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let listed = body["sources"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["id"], "home");
    assert_eq!(listed[0]["schedule"], json!({ "every": "6h" }));
    assert_eq!(listed[0]["status"]["outcome"], "never");
    assert!(listed[1]["config_error"].as_str().unwrap().contains("GRAPHRT_TEST_UNSET_DRIVE_TOKEN"), "{body}");

    let sync =
        |id: &str| Request::post(format!("/v1/knowledge/sources/{id}/sync?wait=true")).body(Body::empty()).unwrap();
    let (status, body) = send(&app, sync("home")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"]["outcome"], "succeeded");
    assert_eq!(body["status"]["report"]["indexed"], 1);

    let retrieve = Request::post("/v1/knowledge/retrieve")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": "when does the shop open", "datasets": ["site"] }).to_string()))
        .unwrap();
    let (status, body) = send(&app, retrieve).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.to_string().contains("opens at eight"), "{body}");

    let (status, _) = send(&app, sync("drive")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "a source with a missing secret cannot sync");
    let (status, _) = send(&app, sync("nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(fixture.0.join("knowledge.sync.sqlite").is_file(), "the sync state is kept next to the config");
}
