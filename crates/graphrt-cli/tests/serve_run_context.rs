//! Run requests carry caller context (metadata, session, user, tags) onto the run
//! record and every span; records keep trace ids and, opt-in, masked inputs.
#![cfg(all(feature = "serve", feature = "otel"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::otel::Telemetry;
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use graphrt_otel::{AttrValue, CaptureMode, InMemoryExporter, Span};
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const TOKEN: &str = "sk-context-test-0123456789";

fn workflows_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-run-context-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("greet.yaml"),
        r#"version: "1"
name: greet
graph:
  nodes:
    - id: start
      type: start
      variables: [{ variable: name, type: string }, { variable: token, type: secret }]
    - { id: hello, type: answer, answer: "Hello, {{#start.name#}}." }
  edges:
    - { source: start, target: hello }
"#,
    )
    .unwrap();
    dir
}

async fn app(tag: &str, store_inputs: bool, exporter: &InMemoryExporter) -> axum::Router {
    let files = discover(&workflows_dir(tag)).expect("discover");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
        capture_content: true,
        ..ServiceOptions::default()
    };
    let telemetry = Arc::new(Telemetry::with_exporter(Arc::new(exporter.clone()), CaptureMode::SpanOnly));
    let state: AppState = build_state(&files, &options, Limits::default(), None)
        .await
        .expect("state")
        .with_telemetry(Some(telemetry))
        .with_store_inputs(store_inputs);
    router(Arc::new(state), 1 << 20)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Json) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Json::Null))
}

fn post(body: Json) -> Request<Body> {
    Request::post("/v1/workflows/greet/runs")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn str_attr<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
    match span.attr(key) {
        Some(AttrValue::Str(s)) => Some(s),
        _ => None,
    }
}

#[tokio::test]
async fn context_reaches_record_and_spans_with_inputs_masked() {
    let exporter = InMemoryExporter::new();
    let app = app("on", true, &exporter).await;
    let request = json!({
        "inputs": { "name": "Ada", "token": TOKEN },
        "metadata": { "tenant": "acme" },
        "session_id": "sess-9",
        "user_id": "user-3",
        "tags": ["eval"],
    });
    let (status, body) = send(&app, post(request)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let run_id = body["run_id"].as_str().unwrap();
    let get = Request::get(format!("/v1/runs/{run_id}")).body(Body::empty()).unwrap();
    let (_, record) = send(&app, get).await;
    assert_eq!(record["inputs"], json!({ "name": "Ada", "token": "******" }));
    assert!(!record.to_string().contains(TOKEN), "secret stored: {record}");
    assert_eq!(record["metadata"], json!({ "tenant": "acme" }));
    assert_eq!(record["session_id"], "sess-9");
    assert_eq!(record["user_id"], "user-3");
    assert_eq!(record["tags"], json!(["eval"]));
    let trace_id = record["trace_id"].as_str().unwrap();
    let span_id = record["span_id"].as_str().unwrap();
    assert_eq!((trace_id.len(), span_id.len()), (32, 16));

    let spans = exporter.take();
    let root = spans.iter().find(|s| s.name == "invoke_workflow greet").unwrap();
    assert_eq!(root.span_id.to_hex(), span_id);
    assert_eq!(root.trace_id.to_hex(), trace_id);
    let input = str_attr(root, "langfuse.observation.input").unwrap();
    assert!(input.contains("Ada") && !input.contains(TOKEN), "{input}");
    assert_eq!(str_attr(root, "graphrt.run.metadata.tenant"), Some("acme"));
    for span in &spans {
        assert_eq!(str_attr(span, "session.id"), Some("sess-9"), "{}", span.name);
        assert_eq!(str_attr(span, "langfuse.user.id"), Some("user-3"), "{}", span.name);
    }
}

#[tokio::test]
async fn inputs_stay_out_of_records_by_default_and_context_is_validated() {
    let exporter = InMemoryExporter::new();
    let app = app("off", false, &exporter).await;
    let (status, body) = send(&app, post(json!({ "inputs": { "name": "Ada", "token": TOKEN } }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let get = Request::get(format!("/v1/runs/{}", body["run_id"].as_str().unwrap())).body(Body::empty()).unwrap();
    let (_, record) = send(&app, get).await;
    assert!(record.get("inputs").is_none(), "{record}");
    assert!(record.get("metadata").is_none() && record.get("session_id").is_none(), "{record}");

    let tags: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
    let (status, body) = send(&app, post(json!({ "tags": tags }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = send(&app, post(json!({ "session_id": "" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(&app, post(json!({ "metadata": { "k": "v".repeat(5000) } }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
