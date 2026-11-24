//! Durable runs: async starts, resume by id, restart recovery, event replay.
#![cfg(feature = "serve")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_cli::store::{FileStore, MemoryStore};
use graphrt_core::engine::Limits;
use graphrt_core::store::{RunRecord, RunStatus, RunStore};
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const SECRET: &str = "sk-store-test-supersecret";

/// A directory with the approval example plus a workflow that pauses after
/// rendering an env secret.
fn workflows_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-serve-store-wf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    std::fs::copy(examples.join("approval.yaml"), dir.join("approval.yaml")).unwrap();
    std::fs::write(
        dir.join("secretive.yaml"),
        r#"version: "1"
name: secretive
env:
  - { name: API_KEY, type: secret }
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - id: show
      type: answer
      answer: "key={{#env.API_KEY#}}"
    - id: ask
      type: human-input
      prompt: "Continue? key={{#env.API_KEY#}}"
      actions: [{ id: go, title: Go }]
    - { id: done, type: answer, answer: " done" }
  edges:
    - { source: start, target: show }
    - { source: show, target: ask }
    - { source: ask, target: done, handle: go }
"#,
    )
    .unwrap();
    dir
}

fn store_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-serve-store-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn state_with(workflows: &Path, store: Arc<dyn RunStore>) -> Arc<AppState> {
    let files = discover(workflows).expect("discover");
    let options = ServiceOptions {
        env_file: vec![("API_KEY".to_owned(), SECRET.to_owned())],
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
        },
        ..ServiceOptions::default()
    };
    let state = build_state(&files, &options, Limits::default(), None).await.expect("state").with_store(store);
    Arc::new(state)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn post(uri: &str, body: Json) -> Request<Body> {
    Request::post(uri).header("content-type", "application/json").body(Body::from(body.to_string())).expect("request")
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).expect("request")
}

async fn get_json(app: &axum::Router, uri: &str) -> Json {
    let (status, body) = send(app, get(uri)).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    serde_json::from_str(&body).expect("json")
}

/// Poll `GET /v1/runs/{id}` until the status leaves `running`.
async fn wait_settled(app: &axum::Router, run_id: &str) -> Json {
    for _ in 0..500 {
        let run = get_json(app, &format!("/v1/runs/{run_id}")).await;
        if run["status"] != "running" && run["active"] == false {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} did not settle");
}

async fn start_async(app: &axum::Router, workflow: &str, inputs: Json) -> String {
    let (status, body) =
        send(app, post(&format!("/v1/workflows/{workflow}/runs"), json!({ "inputs": inputs, "async": true }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let json: Json = serde_json::from_str(&body).unwrap();
    json["run_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn async_run_pauses_and_resumes_by_id_once() {
    let wf = workflows_dir("resume");
    let app = router(state_with(&wf, Arc::new(MemoryStore::new())).await, 1 << 20);
    let run_id = start_async(&app, "approval", json!({ "amount": 50, "reason": "Lunch" })).await;

    let run = wait_settled(&app, &run_id).await;
    assert_eq!(run["status"], "paused", "{run}");
    assert_eq!(run["workflow"], "approval");
    let expected = &run["resume_inputs"][0];
    assert_eq!(expected["key"], "review");
    assert_eq!(expected["kind"], "human_input");
    assert!(expected["data"].is_object(), "the form is described: {run}");

    // Resume without a snapshot: the server uses the one it stored.
    let resume = json!({ "inputs": { "review": { "action": "approve", "values": { "note": "ok" } } } });
    let (status, body) = send(&app, post(&format!("/v1/runs/{run_id}/resume"), resume.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let result: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(result["answer"], "Approved 50 (ok)");

    // A second resume of the same pause is rejected.
    let (status, body) = send(&app, post(&format!("/v1/runs/{run_id}/resume"), resume)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let run = get_json(&app, &format!("/v1/runs/{run_id}")).await;
    assert_eq!(run["status"], "succeeded");
    assert_eq!(run["answer"], "Approved 50 (ok)");
}

#[tokio::test]
async fn concurrent_resumes_only_one_wins() {
    let wf = workflows_dir("race");
    let app = router(state_with(&wf, Arc::new(MemoryStore::new())).await, 1 << 20);
    let run_id = start_async(&app, "approval", json!({ "amount": 5, "reason": "Tea" })).await;
    assert_eq!(wait_settled(&app, &run_id).await["status"], "paused");
    let body = json!({ "inputs": { "review": { "action": "reject", "values": { "note": "no" } } }, "async": true });
    let uri = format!("/v1/runs/{run_id}/resume");
    let (a, b) = tokio::join!(send(&app, post(&uri, body.clone())), send(&app, post(&uri, body)));
    let statuses = [a.0, b.0];
    assert!(statuses.contains(&StatusCode::ACCEPTED) && statuses.contains(&StatusCode::CONFLICT), "{statuses:?}");
    assert_eq!(wait_settled(&app, &run_id).await["status"], "succeeded");
}

/// A paused run of a workflow the server no longer serves: the refused resume
/// must leave it paused, not `running` until the next restart.
#[tokio::test]
async fn resuming_a_run_of_an_unserved_workflow_leaves_it_paused() {
    let wf = workflows_dir("unserved");
    let store: Arc<dyn RunStore> = Arc::new(MemoryStore::new());
    let app = router(state_with(&wf, Arc::clone(&store)).await, 1 << 20);
    let run_id = start_async(&app, "approval", json!({ "amount": 9, "reason": "Taxi" })).await;
    assert_eq!(wait_settled(&app, &run_id).await["status"], "paused");

    // The same store served with only an unrelated workflow (approval was retired).
    let other = std::env::temp_dir().join(format!("graphrt-serve-store-wf-unserved-only-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&other);
    std::fs::create_dir_all(&other).unwrap();
    std::fs::copy(wf.join("secretive.yaml"), other.join("secretive.yaml")).unwrap();
    let app = router(state_with(&other, Arc::clone(&store)).await, 1 << 20);
    let resume = json!({ "inputs": { "review": { "action": "approve", "values": { "note": "ok" } } } });
    let (status, body) = send(&app, post(&format!("/v1/runs/{run_id}/resume"), resume)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("is not served"), "{body}");
    let run = get_json(&app, &format!("/v1/runs/{run_id}")).await;
    assert_eq!(run["status"], "paused", "{run}");
    let _ = std::fs::remove_dir_all(&other);
}

/// A paused run whose stored snapshot is gone: the refused resume (409) must
/// release its claim and leave the run paused.
#[tokio::test]
async fn resuming_without_a_stored_snapshot_leaves_the_run_paused() {
    let wf = workflows_dir("no-snapshot");
    let store: Arc<dyn RunStore> = Arc::new(MemoryStore::new());
    let mut record = RunRecord::running("run-lost-snapshot", "approval", "fp", 1);
    store.create(record.clone()).await.unwrap();
    record.status = RunStatus::Paused;
    store.put(record).await.unwrap();
    let app = router(state_with(&wf, Arc::clone(&store)).await, 1 << 20);
    let (status, body) = send(&app, post("/v1/runs/run-lost-snapshot/resume", json!({ "inputs": {} }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("no stored snapshot"), "{body}");
    assert_eq!(get_json(&app, "/v1/runs/run-lost-snapshot").await["status"], "paused");
}

#[tokio::test]
async fn file_store_survives_restart() {
    let wf = workflows_dir("restart");
    let dir = store_dir("restart");
    let paused_id;
    {
        let store: Arc<dyn RunStore> = Arc::new(FileStore::open(&dir).unwrap());
        let app = router(state_with(&wf, Arc::clone(&store)).await, 1 << 20);
        paused_id = start_async(&app, "approval", json!({ "amount": 70, "reason": "Books" })).await;
        assert_eq!(wait_settled(&app, &paused_id).await["status"], "paused");
        // A run that was executing when the server went down.
        store.create(RunRecord::running("run-crashed", "approval", "fp", 1)).await.unwrap();
    }

    // "Restart": a fresh state over the same directory.
    let state = state_with(&wf, Arc::new(FileStore::open(&dir).unwrap())).await;
    assert_eq!(state.recover().await.unwrap(), 1);
    let app = router(state, 1 << 20);

    let crashed = get_json(&app, "/v1/runs/run-crashed").await;
    assert_eq!(crashed["status"], "failed");
    assert_eq!(crashed["error"], "interrupted by restart");

    let resume = json!({ "inputs": { "review": { "action": "approve", "values": { "note": "fine" } } } });
    let (status, body) = send(&app, post(&format!("/v1/runs/{paused_id}/resume"), resume)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let result: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(result["answer"], "Approved 70 (fine)");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn listing_filters_and_pages() {
    let wf = workflows_dir("list");
    let app = router(state_with(&wf, Arc::new(MemoryStore::new())).await, 1 << 20);
    let paused = start_async(&app, "approval", json!({ "amount": 1, "reason": "a" })).await;
    let done = start_async(&app, "secretive", json!({})).await;
    wait_settled(&app, &paused).await;
    wait_settled(&app, &done).await;
    let third = start_async(&app, "approval", json!({ "amount": 2, "reason": "b" })).await;
    wait_settled(&app, &third).await;

    let all = get_json(&app, "/v1/runs").await;
    assert_eq!(all["runs"].as_array().unwrap().len(), 3);
    let approval = get_json(&app, "/v1/runs?workflow=approval&status=paused").await;
    let ids: Vec<&str> = approval["runs"].as_array().unwrap().iter().map(|r| r["run_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&paused.as_str()) && ids.contains(&third.as_str()) && ids.len() == 2, "{ids:?}");

    let first = get_json(&app, "/v1/runs?limit=2").await;
    assert_eq!(first["runs"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().unwrap();
    let rest = get_json(&app, &format!("/v1/runs?limit=2&cursor={cursor}")).await;
    assert_eq!(rest["runs"].as_array().unwrap().len(), 1);
    assert!(rest["next_cursor"].is_null());

    let (status, _) = send(&app, get("/v1/runs?status=sleeping")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = send(&app, Request::delete(format!("/v1/runs/{third}")).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, get(&format!("/v1/runs/{third}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, get("/v1/runs/..%2Fetc")).await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND, "{status}");
}

#[tokio::test]
async fn events_replay_and_secrets_stay_out_of_the_store() {
    let wf = workflows_dir("events");
    let dir = store_dir("events");
    let app = router(state_with(&wf, Arc::new(FileStore::open(&dir).unwrap())).await, 1 << 20);
    let run_id = start_async(&app, "secretive", json!({})).await;
    let run = wait_settled(&app, &run_id).await;
    assert_eq!(run["status"], "paused");

    // JSON replay.
    let events = get_json(&app, &format!("/v1/runs/{run_id}/events?stream=false")).await;
    let types: Vec<&str> = events.as_array().unwrap().iter().filter_map(|e| e["type"].as_str()).collect();
    assert_eq!(types.first(), Some(&"run_started"));
    assert_eq!(types.last(), Some(&"run_paused"));
    let tail = get_json(&app, &format!("/v1/runs/{run_id}/events?stream=false&from=1")).await;
    assert_eq!(tail.as_array().unwrap().len(), types.len() - 1);

    // SSE replay of a finished execution ends by itself.
    let (status, sse) = send(&app, get(&format!("/v1/runs/{run_id}/events"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(sse.contains("event: run_started") && sse.contains("event: run_paused"), "{sse}");

    // Resume, then the full history spans both executions.
    let resume = json!({ "inputs": { "ask": { "action": "go", "values": {} } } });
    let (status, body) = send(&app, post(&format!("/v1/runs/{run_id}/resume"), resume)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = get_json(&app, &format!("/v1/runs/{run_id}/events?stream=false")).await;
    let starts = events.as_array().unwrap().iter().filter(|e| e["type"] == "run_started").count();
    assert_eq!(starts, 2, "{events}");

    // Nothing under the store directory contains the secret.
    let mut stack = vec![dir.clone()];
    let mut files = 0;
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files += 1;
                let text = String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned();
                assert!(!text.contains(SECRET), "secret leaked into {}", p.display());
            }
        }
    }
    assert!(files >= 3, "record, events and snapshot were written");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn old_snapshot_resume_still_works() {
    let wf = workflows_dir("legacy");
    let app = router(state_with(&wf, Arc::new(MemoryStore::new())).await, 1 << 20);
    let (status, body) =
        send(&app, post("/v1/workflows/approval/runs", json!({ "inputs": { "amount": 9, "reason": "x" } }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let paused: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(paused["status"], "paused");
    let run_id = paused["run_id"].as_str().unwrap();
    let resume = json!({
        "snapshot": paused["snapshot"],
        "inputs": { "review": { "action": "approve", "values": { "note": "y" } } },
    });
    let (status, body) = send(&app, post(&format!("/v1/runs/{run_id}/resume"), resume)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let result: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(result["answer"], "Approved 9 (y)");
}
