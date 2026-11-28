//! Token budgets of real builds: `graphrt run --max-tokens-per-run`, `graphrt
//! serve --max-tokens-per-run` and a request's `limits`, against a fake OpenAI
//! endpoint that reports usage the way OpenAI does. A run past its budget fails
//! with `limit_exceeded` instead of finishing.
#![cfg(all(feature = "llm", feature = "serve"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use graphrt_cli::serve::{build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

mod common;
use common::FakeModel;

/// An agent without tools: every model turn is an answer.
const SOLO: &str = r#"version: "1"
name: solo
graph:
  nodes:
    - { id: start, type: start }
    - id: helper
      type: agent
      model: { provider: openai, name: gpt-test }
      query: "{{#sys.query#}}"
    - { id: end, type: end, outputs: [{ variable: text, value_selector: [helper, text] }] }
  edges:
    - { source: start, target: helper }
    - { source: helper, target: end }
"#;

fn workflow(dir: &std::path::Path) -> PathBuf {
    let file = dir.join("solo.yaml");
    std::fs::write(&file, SOLO).unwrap();
    file
}

async fn served(model: &FakeModel, limits: Limits) -> (axum::Router, PathBuf) {
    served_with(model, limits, None).await
}

/// [`served`] with a providers file: `prices` per model, for cost budgets.
async fn served_with(model: &FakeModel, limits: Limits, providers: Option<&str>) -> (axum::Router, PathBuf) {
    let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-run-budgets").unwrap();
    let file = workflow(&dir);
    let providers = providers.map(|text| {
        let path = dir.join("providers.yaml");
        std::fs::write(&path, text).unwrap();
        path
    });
    let options =
        ServiceOptions { plugins: common::no_plugins(), env_file: model.env(), providers, ..ServiceOptions::default() };
    let state = build_state(&discover(&file).unwrap(), &options, limits, None).await.unwrap();
    (router(Arc::new(state), 1 << 20), dir)
}

async fn start(app: &axum::Router, body: &Json) -> Json {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/workflows/solo/runs")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn serve_stops_a_run_past_the_servers_token_budget() {
    let model = FakeModel::with_usage(&["A long answer."], 120);
    let (app, dir) = served(&model, Limits { max_total_tokens: Some(50), ..Limits::default() }).await;
    let run = start(&app, &json!({"sys": {"query": "Tell me about Jakarta."}, "wait": true})).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert!(run.to_string().contains("limit_exceeded"), "{run}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn serve_stops_a_run_past_the_budget_its_request_set() {
    let model = FakeModel::with_usage(&["A long answer."], 120);
    let (app, dir) = served(&model, Limits::default()).await;
    let body = json!({"sys": {"query": "Tell me about Jakarta."}, "limits": {"max_total_tokens": 50}, "wait": true});
    let run = start(&app, &body).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert!(run.to_string().contains("limit_exceeded"), "{run}");
    // Within the budget it answers.
    let model = FakeModel::with_usage(&["A short answer."], 30);
    let (app, dir2) = served(&model, Limits { max_total_tokens: Some(50), ..Limits::default() }).await;
    let run = start(&app, &json!({"sys": {"query": "Hi"}, "wait": true})).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    let _ = (std::fs::remove_dir_all(dir), std::fs::remove_dir_all(dir2));
}

#[test]
fn graphrt_run_stops_a_run_past_its_token_budget() {
    let model = FakeModel::with_usage(&["A long answer."], 120);
    let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-run-budgets-cli").unwrap();
    let file = workflow(&dir);
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_graphrt"));
    command.args([
        "run",
        file.to_str().unwrap(),
        "--max-tokens-per-run",
        "50",
        "--sys",
        "query=Tell me about Jakarta.",
    ]);
    command.env("HOME", &dir).env_remove("GRAPHRT_PLUGIN_PATH");
    for (name, value) in model.env() {
        command.env(name, value);
    }
    let out = command.output().unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("token budget of 50"), "{said}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_request_never_raises_the_servers_budget() {
    let model = FakeModel::with_usage(&["A long answer."], 120);
    let (app, dir) = served(&model, Limits { max_total_tokens: Some(50), ..Limits::default() }).await;
    let body = json!({"sys": {"query": "Hi"}, "limits": {"max_total_tokens": 100000}, "wait": true});
    let run = start(&app, &body).await;
    assert_eq!(run["status"], "failed", "{run}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn serve_stops_a_run_past_its_cost_budget_priced_by_the_providers_file() {
    let model = FakeModel::with_usage(&["A long answer."], 2_000_000);
    // 2 million tokens at 1 per million in and out: a cost of 2, past a budget of 0.5.
    let providers = format!(
        "providers:\n  - name: openai\n    kind: openai\n    base_url: {}\n    api_key_env: OPENAI_API_KEY\n    prices:\n      gpt-test: {{ input_per_million: 1, output_per_million: 1 }}\n",
        model.url
    );
    let limits = Limits { max_cost_micros: Some(500_000), ..Limits::default() };
    let (app, dir) = served_with(&model, limits, Some(&providers)).await;
    let run = start(&app, &json!({"sys": {"query": "Hi"}, "wait": true})).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert!(run.to_string().contains("cost budget"), "{run}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_cost_budget_refuses_a_model_without_a_price_instead_of_counting_it_free() {
    let model = FakeModel::with_usage(&["A long answer."], 2_000_000);
    let providers = format!(
        "providers:\n  - name: openai\n    kind: openai\n    base_url: {}\n    api_key_env: OPENAI_API_KEY\n",
        model.url
    );
    let limits = Limits { max_cost_micros: Some(500_000), ..Limits::default() };
    let (app, dir) = served_with(&model, limits, Some(&providers)).await;
    let run = start(&app, &json!({"sys": {"query": "Hi"}, "wait": true})).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert!(run.to_string().contains("price_missing"), "{run}");
    assert!(model.requests().is_empty(), "refused before the model was paid");
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(feature = "bundle")]
#[tokio::test]
async fn serve_stops_a_run_past_the_budget_its_bundle_carries() {
    let model = FakeModel::with_usage(&["A long answer."], 120);
    let dir = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-run-budgets-bundle").unwrap();
    let bytes = graphrt_bundle::BundleBuilder::new("solo", "1.0.0")
        .workflow("workflow.yaml", SOLO)
        .unwrap()
        .limits(graphrt_bundle::BundleLimits { max_total_tokens: Some(50), max_cost_micros: None })
        .build()
        .unwrap();
    let path = dir.join("solo.grt");
    std::fs::write(&path, bytes).unwrap();
    let options = ServiceOptions { plugins: common::no_plugins(), env_file: model.env(), ..ServiceOptions::default() };
    let state = build_state(&[("solo".to_owned(), path)], &options, Limits::default(), None).await.unwrap();
    let app = router(Arc::new(state), 1 << 20);
    let run = start(&app, &json!({"sys": {"query": "Tell me about Jakarta."}, "wait": true})).await;
    assert_eq!(run["status"], "failed", "{run}");
    assert!(run.to_string().contains("limit_exceeded"), "{run}");
    let _ = std::fs::remove_dir_all(dir);
}
