//! `--langfuse-scores` against a local fake Langfuse: online and human scores
//! are POSTed to `/api/public/scores` with the run's trace and root span ids,
//! Basic auth and a deterministic id, and transient failures are retried.
#![cfg(all(feature = "langfuse", feature = "online-eval"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use graphrt_cli::langfuse::LangfuseUploader;
use graphrt_cli::serve::{build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

#[derive(Default)]
struct Received {
    requests: Mutex<Vec<(String, Json)>>,
    calls: AtomicUsize,
}

/// A fake Langfuse: the first request gets `503`, every later one `200`.
async fn fake_langfuse() -> (String, Arc<Received>) {
    async fn scores(State(received): State<Arc<Received>>, headers: HeaderMap, body: String) -> StatusCode {
        if received.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return StatusCode::SERVICE_UNAVAILABLE;
        }
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        received.requests.lock().unwrap().push((auth, serde_json::from_str(&body).unwrap()));
        StatusCode::OK
    }
    let received = Arc::new(Received::default());
    let app = axum::Router::new().route("/api/public/scores", axum::routing::post(scores)).with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, received)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Json) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Json::Null))
}

fn post(path: &str, body: &Json) -> Request<Body> {
    Request::post(path).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn online_and_human_scores_reach_langfuse() {
    let (url, received) = fake_langfuse().await;
    let vars: Vec<(String, String)> =
        [("LANGFUSE_BASE_URL", url.as_str()), ("LANGFUSE_PUBLIC_KEY", "pk-lf-1"), ("LANGFUSE_SECRET_KEY", "sk-lf-2")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
    // Loopback is refused by the egress policy unless allowed.
    assert!(LangfuseUploader::start(&vars, false).is_err());
    let uploader = LangfuseUploader::start_with_backoff(&vars, true, Duration::from_millis(5)).unwrap();

    let dir = std::env::temp_dir().join(format!("graphrt-langfuse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("evaluators.yaml");
    std::fs::write(&config, "evaluators: [{name: fast, assert: {type: latency, threshold: 60000}}]").unwrap();
    let hello = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello.yaml");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    let exporter = graphrt_otel::InMemoryExporter::new();
    let telemetry =
        Arc::new(graphrt_cli::otel::Telemetry::with_exporter(Arc::new(exporter), graphrt_otel::CaptureMode::NoContent));
    let state = build_state(&discover(&hello).unwrap(), &options, Limits::default(), None)
        .await
        .unwrap()
        .with_telemetry(Some(telemetry))
        .with_langfuse(Some(uploader))
        .with_evaluators(&config, &options, Limits::default())
        .unwrap();
    let state = Arc::new(state);
    let app = router(state.clone(), 1 << 20);

    let (status, result) = send(&app, post("/v1/workflows/hello/runs", &json!({ "inputs": { "name": "Ada" } }))).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let run_id = result["run_id"].as_str().unwrap().to_owned();
    let feedback = json!({ "name": "thumbs", "value": false, "comment": "wrong name", "user": "qa" });
    assert_eq!(send(&app, post(&format!("/v1/runs/{run_id}/feedback"), &feedback)).await.0, StatusCode::CREATED);

    let mut uploads = Vec::new();
    for _ in 0..400 {
        uploads = received.requests.lock().unwrap().clone();
        if uploads.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(uploads.len(), 2, "{uploads:?}");
    assert!(received.calls.load(Ordering::SeqCst) >= 3, "the 503 was retried");
    let record = state.store().get(&run_id).await.unwrap().unwrap();
    let stored = state.score_store().scores(&run_id).await.unwrap();
    for (auth, body) in &uploads {
        assert_eq!(auth, "Basic cGstbGYtMTpzay1sZi0y");
        assert_eq!(body["traceId"].as_str(), record.trace_id.as_deref());
        assert_eq!(body["observationId"].as_str(), record.span_id.as_deref());
        assert!(stored.iter().any(|s| s.id == body["id"].as_str().unwrap()), "deterministic id: {body}");
    }
    let human = uploads.iter().map(|(_, b)| b).find(|b| b["name"] == "thumbs").unwrap();
    assert_eq!((human["value"].clone(), human["dataType"].clone()), (json!(0), json!("BOOLEAN")));
    assert_eq!((human["comment"].as_str(), human["metadata"]["source"].as_str()), (Some("wrong name"), Some("human")));
    let online = uploads.iter().map(|(_, b)| b).find(|b| b["name"] == "fast").unwrap();
    assert_eq!((online["dataType"].as_str(), online["metadata"]["label"].as_str()), (Some("NUMERIC"), Some("pass")));
    let _ = std::fs::remove_dir_all(&dir);
}
