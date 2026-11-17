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
    let state = build_state(&files, &ServiceOptions::default(), Limits::default(), api_key.map(str::to_owned))
        .await
        .expect("state");
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
