//! Online evaluation in `graphrt serve` (`--evaluators`): sampled finished runs
//! are scored in the background with assertions and judge workflows, the scores
//! are stored with the run and become `evaluate` spans.
#![cfg(feature = "online-eval")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn options() -> ServiceOptions {
    ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    }
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Json) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Json::Null))
}

async fn start(app: &axum::Router, workflow: &str, body: Json) -> String {
    let req = Request::post(format!("/v1/workflows/{workflow}/runs"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, result) = send(app, req).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    result["run_id"].as_str().unwrap().to_owned()
}

/// The run's scores once `count` arrived (evaluation is asynchronous).
async fn scores(app: &axum::Router, run_id: &str, count: usize) -> Vec<Json> {
    let mut last = Vec::new();
    for _ in 0..400 {
        let req = Request::get(format!("/v1/runs/{run_id}/scores")).body(Body::empty()).unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        last = body["scores"].as_array().cloned().unwrap_or_default();
        if last.len() >= count {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {count} score(s) for {run_id}, got {last:?}");
}

fn by_name<'a>(scores: &'a [Json], name: &str) -> &'a Json {
    scores.iter().find(|s| s["name"] == name).unwrap_or_else(|| panic!("no score `{name}` in {scores:?}"))
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// A judge with no model: passes when the output says hello.
const HELLO_JUDGE: &str = r#"version: "1"
name: says-hello
graph:
  nodes:
    - id: start
      type: start
      variables: [{ variable: output, type: paragraph, required: true }]
    - id: decide
      type: template-transform
      template: "{% if 'Hello' in output %}pass{% else %}fail{% endif %}"
      variables: [{ variable: output, value_selector: [start, output] }]
    - id: end
      type: end
      outputs: [{ variable: verdict, value_selector: [decide, output] }]
  edges:
    - { source: start, target: decide }
    - { source: decide, target: end }
"#;

const EVALUATORS: &str = r#"
concurrency: 2
queue: 16
evaluators:
  - name: says-hello
    workflows: [hello]
    assert: { type: judge, workflow: judges/says-hello.yaml }
  - name: greets-by-name
    assert: { type: icontains, value: "{{name}}" }
  - name: prod-only
    tags: [prod]
    assert: { type: latency, threshold: 60000 }
  - name: gold-only
    metadata: { tier: gold }
    assert: { type: latency, threshold: 60000 }
  - name: never
    sample: 0
    assert: { type: latency, threshold: 1 }
  - name: failures-only
    status: [failed]
    assert: { type: latency, threshold: 1 }
"#;

async fn hello_with_evaluators(dir: &Path) -> (Arc<AppState>, axum::Router, graphrt_otel::InMemoryExporter) {
    std::fs::create_dir_all(dir.join("judges")).unwrap();
    write(&dir.join("judges"), "says-hello.yaml", HELLO_JUDGE);
    let config = write(dir, "evaluators.yaml", EVALUATORS);
    let files = discover(&repo_root().join("examples/hello.yaml")).unwrap();
    let exporter = graphrt_otel::InMemoryExporter::new();
    let telemetry = Arc::new(graphrt_cli::otel::Telemetry::with_exporter(
        Arc::new(exporter.clone()),
        graphrt_otel::CaptureMode::NoContent,
    ));
    let state = build_state(&files, &options(), Limits::default(), None)
        .await
        .unwrap()
        .with_telemetry(Some(telemetry))
        .with_evaluators(&config, &options(), Limits::default())
        .unwrap();
    let state = Arc::new(state);
    (state.clone(), router(state, 1 << 20), exporter)
}

#[tokio::test]
async fn sampled_runs_are_scored_with_filters_and_spans() {
    let dir = std::env::temp_dir().join(format!("graphrt-online-eval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (state, app, exporter) = hello_with_evaluators(&dir).await;

    let plain = start(&app, "hello", json!({ "inputs": { "name": "Ada" } })).await;
    let scored = scores(&app, &plain, 2).await;
    assert_eq!(scored.len(), 2, "only the unfiltered evaluators: {scored:?}");
    let judge = by_name(&scored, "says-hello");
    assert_eq!((judge["source"].as_str(), judge["label"].as_str()), (Some("online"), Some("pass")));
    assert_eq!(judge["type"], "judge");
    assert_eq!(judge["judge"]["verdict"], "pass");
    assert_eq!(judge["judge"]["fingerprint"].as_str().map(str::len), Some(16));
    let named = by_name(&scored, "greets-by-name");
    assert_eq!((named["value"].as_f64(), named["label"].as_str()), (Some(1.0), Some("pass")), "{{name}} rendered");

    let tagged =
        start(&app, "hello", json!({ "inputs": { "name": "Bo" }, "tags": ["prod"], "metadata": { "tier": "gold" } }))
            .await;
    let scored = scores(&app, &tagged, 4).await;
    let names: Vec<&str> = scored.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"prod-only") && names.contains(&"gold-only"), "{names:?}");
    assert!(!names.contains(&"never") && !names.contains(&"failures-only"), "{names:?}");

    let online = state.online_eval().unwrap();
    assert_eq!((online.queued(), online.dropped()), (2, 0));
    // Every score is an `evaluate` span in its run's trace.
    let record = state.store().get(&plain).await.unwrap().unwrap();
    let spans = exporter.spans();
    let span = spans
        .iter()
        .find(|s| s.name == "evaluate says-hello" && Some(s.trace_id.to_hex()) == record.trace_id)
        .expect("evaluate span");
    assert_eq!(span.parent_span_id.map(|id| id.to_hex()), record.span_id);
    assert!(span.end_unix_nano >= span.start_unix_nano);
    let event = &span.events[0];
    assert_eq!(event.name, "gen_ai.evaluation.result");
    assert!(
        event
            .attributes
            .iter()
            .any(|(k, v)| k == "gen_ai.evaluation.score.label" && *v == graphrt_otel::AttrValue::from("pass"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn offline_only_assertions_fail_at_startup() {
    let dir = std::env::temp_dir().join(format!("graphrt-online-eval-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let files = discover(&repo_root().join("examples/hello.yaml")).unwrap();
    let state = || async { build_state(&files, &options(), Limits::default(), None).await.unwrap() };
    let equals = write(&dir, "equals.yaml", "evaluators: [{name: e, assert: {type: equals, value: x}}]");
    let err = state().await.with_evaluators(&equals, &options(), Limits::default()).unwrap_err();
    assert!(err.message.contains("cannot run online"), "{}", err.message);
    let missing = write(&dir, "judge.yaml", "evaluators: [{name: j, assert: {type: judge, workflow: nope.yaml}}]");
    let err = state().await.with_evaluators(&missing, &options(), Limits::default()).unwrap_err();
    assert!(err.message.contains("nope.yaml"), "{}", err.message);
    let _ = std::fs::remove_dir_all(&dir);
}

/// examples/evals/online: the RAG workflow scored by replayed LLM judges.
#[cfg(feature = "rag")]
#[tokio::test]
async fn the_online_example_replays_offline() {
    let root = repo_root();
    // The example's knowledge store path is relative to the repository root.
    std::env::set_current_dir(&root).unwrap();
    let mut options = options();
    options.providers = Some(root.join("examples/evals/judged/providers.replay.yaml"));
    options.knowledge = Some(root.join("examples/evals/retrieval/knowledge.yaml"));
    let files = discover(&root.join("examples/evals/judged/rag-answer.yaml")).unwrap();
    let config = root.join("examples/evals/online/evaluators.yaml");
    let state = build_state(&files, &options, Limits::default(), None)
        .await
        .unwrap()
        .with_evaluators(&config, &options, Limits::default())
        .unwrap();
    let app = router(Arc::new(state), 1 << 20);
    let question = "How many days do I have to request a refund?";
    let run_id = start(&app, "rag-answer", json!({ "inputs": { "question": question } })).await;
    let scored = scores(&app, &run_id, 4).await;
    for name in ["faithfulness", "relevance", "token-budget", "no-ai-disclaimer"] {
        assert_eq!(by_name(&scored, name)["label"], "pass", "{name}: {scored:?}");
    }
    let judge = by_name(&scored, "faithfulness")["judge"]["workflow"].as_str().unwrap();
    assert!(judge.ends_with("examples/evals/judges/faithfulness.yaml"), "{judge}");
}
