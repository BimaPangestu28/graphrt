//! In-process tests of the HTTP API (no sockets).
#![cfg(feature = "serve")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use tower::ServiceExt;

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

async fn state(api_key: Option<&str>) -> Arc<AppState> {
    let files = discover(&examples()).expect("discover");
    // Ignore the developer's own installed plugins.
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
        },
        ..ServiceOptions::default()
    };
    let state = build_state(&files, &options, Limits::default(), api_key.map(str::to_owned)).await.expect("state");
    Arc::new(state)
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let res = app.oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::post(uri).header("content-type", "application/json").body(Body::from(body.to_string())).expect("request")
}

#[tokio::test]
async fn health_and_listing() {
    let app = router(state(None).await, 1 << 20);
    let (status, body) = send(app.clone(), Request::get("/healthz").body(Body::empty()).expect("req")).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "ok"));

    let (status, body) = send(app, Request::get("/v1/workflows").body(Body::empty()).expect("req")).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let names: Vec<&str> =
        json["workflows"].as_array().expect("array").iter().filter_map(|w| w["name"].as_str()).collect();
    assert!(names.contains(&"hello") && names.contains(&"branching"), "{names:?}");
    assert!(!names.contains(&"mcp"), "mcp.json is not a workflow");
}

#[tokio::test]
async fn node_catalog_and_validation_endpoints() {
    let app = router(state(None).await, 1 << 20);
    let (status, body) = send(app.clone(), Request::get("/v1/nodes").body(Body::empty()).expect("req")).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let kinds: Vec<&str> = json["nodes"].as_array().expect("array").iter().filter_map(|n| n["kind"].as_str()).collect();
    assert!(kinds.contains(&"if-else") && kinds.contains(&"start"), "{kinds:?}");

    let valid = serde_json::json!({"graph": {"nodes": [{"id": "s", "type": "start"}, {"id": "e", "type": "end"}],
        "edges": [{"source": "s", "target": "e"}]}});
    let (status, body) = send(app.clone(), post("/v1/validate", valid)).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["valid"], true, "{json}");

    let yaml = "graph:\n  nodes:\n    - {id: s, type: start}\n    - {id: x, type: no-such-node}\n  edges: [{source: s, target: x}]\n";
    let (status, body) = send(app, post("/v1/validate", serde_json::json!({ "document": yaml }))).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["valid"], false);
    assert!(json["errors"][0].as_str().unwrap_or_default().contains("no-such-node"), "{json}");
}

#[tokio::test]
async fn blocking_run_returns_outputs() {
    let app = router(state(None).await, 1 << 20);
    let (status, body) =
        send(app, post("/v1/workflows/hello/runs", serde_json::json!({"inputs": {"name": "Ada"}}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["status"], "succeeded");
    assert_eq!(json["outputs"]["greeting"], "Hello, Ada.");
}

#[tokio::test]
async fn streaming_run_emits_sse() {
    let app = router(state(None).await, 1 << 20);
    let body = serde_json::json!({"inputs": {"priority": 9, "subject": "Outage"}, "stream": true});
    let (status, text) = send(app, post("/v1/workflows/branching/runs", body)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("event: run_started"), "{text}");
    assert!(text.contains("event: response"), "{text}");
    assert!(text.contains("URGENT: paging on-call"), "{text}");
    assert!(text.contains("event: run_succeeded"), "{text}");
}

#[tokio::test]
async fn unknown_workflow_is_404() {
    let app = router(state(None).await, 1 << 20);
    let (status, _) = send(app, post("/v1/workflows/nope/runs", serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn auth_is_enforced_except_health() {
    let app = router(state(Some("s3cret")).await, 1 << 20);
    let (status, _) = send(app.clone(), Request::get("/healthz").body(Body::empty()).expect("req")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send(app.clone(), Request::get("/v1/workflows").body(Body::empty()).expect("req")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let req = Request::get("/v1/workflows").header("authorization", "Bearer wrong").body(Body::empty()).expect("req");
    assert_eq!(send(app.clone(), req).await.0, StatusCode::UNAUTHORIZED);

    let req = Request::get("/v1/workflows").header("authorization", "Bearer s3cret").body(Body::empty()).expect("req");
    assert_eq!(send(app, req).await.0, StatusCode::OK);
}

/// Every `/v1` route answers `401` without the key before it looks at the
/// body (none here has a JSON content type), so a route added outside the
/// auth layer cannot go unnoticed.
#[tokio::test]
async fn every_api_route_needs_the_key() {
    let app = router(state(Some("s3cret")).await, 1 << 20);
    let mut routes = vec![
        ("GET", "/v1/workflows"),
        ("POST", "/v1/workflows/hello/runs"),
        ("GET", "/v1/runs"),
        ("GET", "/v1/runs/run-1"),
        ("DELETE", "/v1/runs/run-1"),
        ("GET", "/v1/runs/run-1/events"),
        ("POST", "/v1/runs/run-1/abort"),
        ("POST", "/v1/runs/run-1/resume"),
        ("GET", "/v1/nodes"),
        ("POST", "/v1/validate"),
        ("GET", "/v1/hooks"),
    ];
    if cfg!(feature = "scores") {
        routes.extend([("GET", "/v1/runs/run-1/scores"), ("POST", "/v1/runs/run-1/feedback")]);
    }
    if cfg!(feature = "rag") {
        routes.extend([
            ("GET", "/v1/knowledge/datasets"),
            ("POST", "/v1/knowledge/datasets"),
            ("DELETE", "/v1/knowledge/datasets/d"),
            ("GET", "/v1/knowledge/resources"),
            ("GET", "/v1/knowledge/datasets/d/documents"),
            ("POST", "/v1/knowledge/datasets/d/documents"),
            ("GET", "/v1/knowledge/datasets/d/documents/doc"),
            ("PATCH", "/v1/knowledge/datasets/d/documents/doc"),
            ("DELETE", "/v1/knowledge/datasets/d/documents/doc"),
            ("PUT", "/v1/knowledge/datasets/d/documents/doc/file"),
            ("POST", "/v1/knowledge/retrieve"),
        ]);
    }
    for (method, path) in routes {
        let req = Request::builder().method(method).uri(path).body(Body::from("{")).expect("request");
        assert_eq!(send(app.clone(), req).await.0, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
}

#[tokio::test]
async fn body_limit_is_enforced() {
    let app = router(state(None).await, 64);
    let big = serde_json::json!({"inputs": {"name": "x".repeat(1000)}});
    let (status, _) = send(app, post("/v1/workflows/hello/runs", big)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn pause_returns_snapshot_and_resume_checks_run_id() {
    let app = router(state(None).await, 1 << 20);
    let body = serde_json::json!({"inputs": {"amount": 10, "reason": "Taxi"}});
    let (status, text) = send(app.clone(), post("/v1/workflows/approval/runs", body)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(json["status"], "paused");
    let run_id = json["run_id"].as_str().expect("run id").to_owned();
    let snapshot = json["snapshot"].clone();
    assert_eq!(snapshot["run_id"], run_id);

    // Resuming under another run id is rejected.
    let req = post("/v1/runs/other/resume", serde_json::json!({"snapshot": snapshot, "inputs": {}}));
    assert_eq!(send(app.clone(), req).await.0, StatusCode::BAD_REQUEST);

    // Aborting a run that is not active is a 404.
    let (status, _) = send(app, post(&format!("/v1/runs/{run_id}/abort"), serde_json::json!({}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[cfg(all(feature = "plugins", feature = "wasm"))]
#[tokio::test]
async fn plugin_kinds_are_served_and_validated() {
    let plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: vec![plugins.clone()],
            no_default: true,
            ..Default::default()
        },
        ..ServiceOptions::default()
    };
    let files = discover(&plugins.join("demo.yaml")).expect("discover");
    let state = Arc::new(build_state(&files, &options, Limits::default(), None).await.expect("state"));
    let app = router(state, 1 << 20);

    let (status, body) = send(app.clone(), Request::get("/v1/nodes").body(Body::empty()).expect("req")).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let kinds: Vec<&str> = json["nodes"].as_array().expect("nodes").iter().filter_map(|n| n["kind"].as_str()).collect();
    assert!(kinds.contains(&"sentiment.analyze") && kinds.contains(&"slugify"), "{kinds:?}");

    let doc = std::fs::read_to_string(plugins.join("demo.yaml")).expect("demo");
    let (_, body) = send(app.clone(), post("/v1/validate", serde_json::json!({ "document": doc }))).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["valid"], true, "{json}");

    let unknown = serde_json::json!({
        "document": "graph:\n  nodes:\n    - { id: s, type: start }\n    - { id: x, type: acme.missing }\n  edges:\n    - { source: s, target: x }\n"
    });
    let (_, body) = send(app, post("/v1/validate", unknown)).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["valid"], false);
    assert!(json["errors"][0].as_str().unwrap_or_default().contains("graphrt plugin install"), "{json}");
}
