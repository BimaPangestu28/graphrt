//! `--input-header`: request headers mapped to secret start inputs of the run
//! endpoints (new runs, SSE and resume), never stored or echoed in plaintext.
#![cfg(feature = "serve")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::header_inputs::HeaderInputSpec;
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

/// Five characters, so `max_length: 5` proves the `Bearer ` prefix was stripped.
const TOKEN: &str = "jwt42";
const HEADER: &str = "X-User-Authorization";

/// A temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-header-inputs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `chat` declares `user_jwt` (secret, required) and pauses for a review, so the
/// resume path can be exercised; `plain` declares no mapped input.
fn write_workflows(dir: &TempDir, user_jwt_type: &str) -> PathBuf {
    let flows = dir.path().join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    std::fs::write(
        flows.join("chat.yaml"),
        format!(
            r#"version: "1"
name: chat
graph:
  nodes:
    - id: start
      type: start
      variables:
        - {{ variable: user_jwt, type: {user_jwt_type}, required: true, max_length: 5 }}
        - {{ variable: question, type: string }}
    - id: review
      type: human-input
      prompt: "Answer {{{{#start.question#}}}} for {{{{#start.user_jwt#}}}}?"
      actions:
        - {{ id: approve, title: Approve }}
    - id: done
      type: answer
      answer: "ok {{{{#start.user_jwt#}}}}"
  edges:
    - {{ source: start, target: review }}
    - {{ source: review, target: done, handle: approve }}
"#
        ),
    )
    .unwrap();
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    std::fs::copy(examples.join("hello.yaml"), flows.join("plain.yaml")).unwrap();
    flows
}

async fn state_for(flows: &Path, specs: &[&str]) -> Result<AppState, graphrt_cli::error::CliError> {
    let files = discover(flows).expect("discover");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    let specs: Vec<HeaderInputSpec> = specs.iter().map(|s| s.parse().expect("spec")).collect();
    build_state(&files, &options, Limits::default(), None).await.expect("state").with_header_inputs(&specs)
}

async fn app(dir: &TempDir) -> axum::Router {
    let flows = write_workflows(dir, "secret");
    let state = state_for(&flows, &[&format!("user_jwt={HEADER}")]).await.expect("header inputs");
    router(Arc::new(state), 1 << 20)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn post(uri: &str, body: &Json, auth: Option<&str>) -> Request<Body> {
    let mut req = Request::post(uri).header("content-type", "application/json");
    if let Some(auth) = auth {
        req = req.header(HEADER, auth);
    }
    req.body(Body::from(body.to_string())).expect("request")
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Start `chat` (it pauses at the review) and return its run id.
async fn start_paused(app: &axum::Router) -> String {
    let body = json!({ "inputs": { "question": "balance" } });
    let (status, text) = send(app, post("/v1/workflows/chat/runs", &body, Some(&bearer(TOKEN)))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let json: Json = serde_json::from_str(&text).unwrap();
    assert_eq!(json["status"], "paused", "{json}");
    json["run_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn header_becomes_a_masked_secret_input() {
    let dir = TempDir::new("start");
    let app = app(&dir).await;
    let run_id = start_paused(&app).await;

    let (status, events) =
        send(&app, Request::get(format!("/v1/runs/{run_id}/events?stream=false")).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.contains(r#""user_jwt":"******""#), "{events}");
    assert!(!events.contains(TOKEN), "plaintext token in stored events: {events}");
    let (_, record) = send(&app, Request::get(format!("/v1/runs/{run_id}")).body(Body::empty()).unwrap()).await;
    assert!(!record.contains(TOKEN), "plaintext token in the run record: {record}");
}

#[tokio::test]
async fn bearer_prefix_is_stripped() {
    let dir = TempDir::new("strip");
    let app = app(&dir).await;
    // `max_length: 5` passes only for the bare token, not `Bearer jwt42`.
    start_paused(&app).await;
    let body = json!({ "inputs": {} });
    let (status, text) = send(&app, post("/v1/workflows/chat/runs", &body, Some("jwt42x"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("exceeds max length 5"), "{text}");
}

#[tokio::test]
async fn streaming_runs_take_the_header_too() {
    let dir = TempDir::new("sse");
    let app = app(&dir).await;
    let body = json!({ "stream": true, "inputs": { "question": "balance" } });
    let (status, text) = send(&app, post("/v1/workflows/chat/runs", &body, Some(&bearer(TOKEN)))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("event: run_paused"), "{text}");
    assert!(text.contains(r#""user_jwt":"******""#), "{text}");
    assert!(!text.contains(TOKEN), "plaintext token in SSE: {text}");
}

#[tokio::test]
async fn missing_required_header_is_unauthorized() {
    let dir = TempDir::new("missing");
    let app = app(&dir).await;
    for auth in [None, Some("Bearer "), Some("")] {
        let (status, text) = send(&app, post("/v1/workflows/chat/runs", &json!({}), auth)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{auth:?}: {text}");
        assert!(text.contains("missing `x-user-authorization` header"), "{text}");
    }
    // A refused request leaves no run behind.
    let (_, runs) = send(&app, Request::get("/v1/runs").body(Body::empty()).unwrap()).await;
    assert_eq!(serde_json::from_str::<Json>(&runs).unwrap()["runs"], json!([]), "{runs}");
}

#[tokio::test]
async fn body_cannot_set_a_header_mapped_input() {
    let dir = TempDir::new("body");
    let app = app(&dir).await;
    let body = json!({ "inputs": { "user_jwt": "spoofed" } });
    let (status, text) = send(&app, post("/v1/workflows/chat/runs", &body, Some(&bearer(TOKEN)))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(text.contains("comes from the `x-user-authorization` header"), "{text}");
}

#[tokio::test]
async fn workflows_without_the_input_ignore_the_mapping() {
    let dir = TempDir::new("plain");
    let app = app(&dir).await;
    let body = json!({ "inputs": { "name": "Ada" } });
    let (status, text) = send(&app, post("/v1/workflows/plain/runs", &body, None)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains("succeeded"), "{text}");
}

#[tokio::test]
async fn resume_requires_the_header_again() {
    let dir = TempDir::new("resume");
    let app = app(&dir).await;
    let run_id = start_paused(&app).await;
    let uri = format!("/v1/runs/{run_id}/resume");
    let resume = json!({ "inputs": { "review": { "action": "approve" } } });

    let (status, text) = send(&app, post(&uri, &resume, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    // The refused resume leaves the run paused, so it can be resumed properly.
    let (_, record) = send(&app, Request::get(format!("/v1/runs/{run_id}")).body(Body::empty()).unwrap()).await;
    assert_eq!(serde_json::from_str::<Json>(&record).unwrap()["status"], "paused", "{record}");

    let spoofed = json!({ "inputs": { "review": { "action": "approve" } }, "secrets": { "start.user_jwt": "x" } });
    let (status, text) = send(&app, post(&uri, &spoofed, Some(&bearer(TOKEN)))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");

    let (status, text) = send(&app, post(&uri, &resume, Some(&bearer(TOKEN)))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(text.contains("succeeded") && text.contains("ok ******"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
}

#[tokio::test]
async fn mapped_inputs_must_be_declared_secret() {
    let dir = TempDir::new("typed");
    let flows = write_workflows(&dir, "string");
    let error = state_for(&flows, &[&format!("user_jwt={HEADER}")]).await.expect_err("non-secret input");
    assert!(error.to_string().contains("must be declared `type: secret`"), "{error}");
}
