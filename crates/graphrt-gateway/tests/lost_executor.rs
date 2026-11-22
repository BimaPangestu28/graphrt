//! A run whose executor is lost without an outcome is not lost (plan §3.4, I1):
//! the gateway re-dispatches it from the latest stored snapshot, else from its
//! request with the same run id, also on platforms that expose only one URL
//! (`Addressing::Url`: Cloud Run, ACA, Lambda), where no orphan can be pulled.
//! `owner_lost` only when that is impossible, with the reason.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{AgentWorld, DEPLOYMENT, Options, crash_mid_stream, start_gateway};
use graphrt_core::store::{RunStatus, RunStore};
use graphrt_gateway::store::SqliteStore;
use graphrt_gateway::supervisor::{RunInput, StartOptions, Started};
use serde_json::json;

fn runs() -> String {
    format!("/v1/deployments/{DEPLOYMENT}/runs")
}

fn url_platform(reclaim_grace: Duration) -> Options {
    Options { url_provider: true, reclaim_grace, ..Options::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_instance_on_a_url_platform_restarts_the_run_from_its_request() {
    // A long grace proves the gateway does not wait for an orphan it cannot pull.
    let gateway = start_gateway(url_platform(Duration::from_secs(30))).await;
    let started = tokio::time::Instant::now();
    let (status, record) = gateway.post(&runs(), json!({ "inputs": { "mode": "vanish_once", "text": "hi" } })).await;
    assert_eq!(status, 200, "{record}");
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["outputs"]["executions"], 2, "restarted once: {record}");
    assert_eq!(record["outputs"]["echo"], "hi", "the restart got the request's inputs");
    assert!(started.elapsed() < Duration::from_secs(10), "no reclaim-grace wait: {:?}", started.elapsed());
    assert_eq!(gateway.gateway.metrics.lost_executions.load(Ordering::Relaxed), 1);
    let run_id = record["run_id"].as_str().unwrap();
    let stored = gateway.gateway.store.get(run_id).await.unwrap().unwrap();
    assert_eq!(stored.epoch, 2, "the restart ran at the next epoch");
    let events = gateway.gateway.store.events(run_id, 0).await.unwrap();
    assert_eq!(events.len() as u64, stored.event_count, "one log, no gap");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_dispatch_waits_while_the_instance_still_holds_the_lost_execution() {
    let gateway = start_gateway(url_platform(Duration::from_secs(30))).await;
    // The platform routes the re-dispatch to the instance that lost the stream:
    // it refuses the new epoch until its reattach window passed.
    let world = gateway.provider.world.clone();
    let body = json!({ "async": true, "inputs": { "mode": "vanish_once", "events": 3, "delay_ms": 50 } });
    let (_, accepted) = gateway.post(&runs(), body).await;
    world.epoch_conflicts.store(3, Ordering::SeqCst);
    let record = gateway.wait_settled(accepted["run_id"].as_str().unwrap()).await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["outputs"]["executions"], 2, "{record}");
    assert_eq!(world.epoch_conflicts.load(Ordering::SeqCst), 0, "the conflicts were retried");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_instance_resumes_from_the_latest_stored_snapshot() {
    let gateway = start_gateway(url_platform(Duration::from_secs(30))).await;
    let (_, record) = gateway.post(&runs(), json!({ "inputs": { "mode": "preempt_then_vanish" } })).await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["outputs"]["executions"], 3, "{record}");
    let bodies = gateway.provider.world.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    assert!(bodies[2]["resume"]["snapshot"].is_object(), "third execution resumed from the stored snapshot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vanished_replica_without_an_orphan_is_re_dispatched_after_the_grace() {
    let gateway = start_gateway(Options { reclaim_grace: Duration::from_millis(300), ..Options::default() }).await;
    let (_, record) = gateway.post(&runs(), json!({ "inputs": { "mode": "vanish_once" } })).await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["outputs"]["executions"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_always_loses_its_executor_ends_owner_lost_with_the_reason() {
    let gateway = start_gateway(url_platform(Duration::from_secs(30))).await;
    let (_, record) = gateway.post(&runs(), json!({ "inputs": { "mode": "vanish" } })).await;
    assert_eq!((record["status"].as_str(), record["error_type"].as_str()), (Some("failed"), Some("owner_lost")));
    assert!(record["error"].as_str().unwrap().contains("lost 4 times in a row"), "{record}");
    let run_id = record["run_id"].as_str().unwrap();
    assert_eq!(gateway.provider.world.executions.lock().unwrap()[run_id].len(), 4, "re-dispatched 3 times");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_credentials_end_owner_lost_with_the_reason() {
    let gateway = start_gateway(url_platform(Duration::from_secs(30))).await;
    let mut body = serde_json::Map::new();
    body.insert("inputs".into(), json!({ "mode": "vanish_once" }));
    // The user's JWT expired long ago: re-dispatching would act with it.
    let input = RunInput { body, expires_at: Some(1) };
    let Started::New(run) =
        gateway.gateway.start_run(&gateway.deployment, input, StartOptions::default()).await.unwrap()
    else {
        panic!("new run expected");
    };
    let record = gateway.wait_settled(&run.run_id).await;
    assert_eq!((record["status"].as_str(), record["error_type"].as_str()), (Some("failed"), Some("owner_lost")));
    assert!(record["error"].as_str().unwrap().contains("credentials expired"), "{record}");
}

fn temp_store(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-gw-lost-{tag}-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("gateway.db")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_gateway_crash_a_url_run_resumes_from_its_stored_snapshot() {
    let path = temp_store("snapshot");
    let world = Arc::new(AgentWorld::default());
    // Preempted once (snapshot stored), then crashed during the resumed execution.
    let body = json!({ "async": true, "inputs": { "mode": "preempt_once", "events": 40, "delay_ms": 20 } });
    let (run_id, at_crash) = crash_mid_stream(&path, &world, body, Duration::from_millis(1200), true);
    assert_eq!(at_crash, RunStatus::Running, "crashed while it executed");
    let second = start_gateway(Options {
        store: SqliteStore::open(&path).unwrap(),
        world: world.clone(),
        url_provider: true,
        ..Options::default()
    })
    .await;
    assert_eq!(second.gateway.recover().await.unwrap(), (0, 1));
    let done = second.wait_settled(&run_id).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    let _cleanup = std::fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_gateway_crash_a_url_run_without_a_checkpoint_says_why_it_is_lost() {
    let path = temp_store("no-checkpoint");
    let world = Arc::new(AgentWorld::default());
    let body = json!({ "async": true, "inputs": { "events": 40, "delay_ms": 20 } });
    let (run_id, at_crash) = crash_mid_stream(&path, &world, body, Duration::from_millis(300), true);
    assert_eq!(at_crash, RunStatus::Running, "crashed while it executed");
    let second = start_gateway(Options {
        store: SqliteStore::open(&path).unwrap(),
        world: world.clone(),
        url_provider: true,
        ..Options::default()
    })
    .await;
    second.gateway.recover().await.unwrap();
    let done = second.wait_settled(&run_id).await;
    // The request inputs lived only in the dead gateway's memory (decision 7).
    assert_eq!((done["status"].as_str(), done["error_type"].as_str()), (Some("failed"), Some("owner_lost")), "{done}");
    assert!(done["error"].as_str().unwrap().contains("no checkpoint"), "{done}");
    let _cleanup = std::fs::remove_dir_all(path.parent().unwrap());
}
