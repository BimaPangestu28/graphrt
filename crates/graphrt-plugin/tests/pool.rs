//! Warm process pools (`PluginHost::with_pool`) driven through the real engine.

#![cfg(feature = "process")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::node::{Services, TokioClock};
use graphrt_core::tool::ToolRegistry;
use graphrt_core::{Event, Graph, Registry, Workflow};
use graphrt_plugin::{LoadedManifest, PluginHost, PluginManifest, PoolConfig};
use serde_json::json;

fn testkit() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("testkit")
}

struct Pooled {
    host: PluginHost,
    registry: Registry,
    tools: ToolRegistry,
}

fn pooled(size: usize, idle_timeout: Option<Duration>) -> Pooled {
    let mut host = PluginHost::new().with_pool(PoolConfig { size, idle_timeout });
    host.load_dir(&testkit()).unwrap();
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();
    Pooled { host, registry, tools }
}

impl Pooled {
    async fn run(&self, doc: serde_json::Value) -> Vec<Event> {
        let graph = Graph::build(Workflow::from_value(doc).unwrap(), &self.registry).unwrap();
        let engine = Engine::new(Arc::new(graph))
            .with_services(Services::new().with(self.tools.clone()))
            .with_clock(Arc::new(TokioClock));
        let stream = engine.run(RunInput::new());
        tokio::time::timeout(Duration::from_secs(30), stream.collect::<Vec<_>>()).await.unwrap()
    }

    fn is_warm(&self) -> bool {
        self.host.warm_plugins().iter().any(|name| name == "testkit")
    }

    /// Wait (up to 10 s) until the testkit pool is warm (`true`) or cold (`false`).
    async fn wait_until_warm(&self, warm: bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.is_warm() != warm {
            assert!(Instant::now() < deadline, "the pool never became {}", if warm { "warm" } else { "cold" });
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// A start → `kind` → end workflow; `kind`'s output `field` becomes output `out`.
fn single_node(kind: &str, config: serde_json::Value, field: &str) -> serde_json::Value {
    let mut node = json!({"id": "p", "type": kind});
    for (key, value) in config.as_object().unwrap() {
        node[key] = value.clone();
    }
    json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            node,
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["p", field]}]}
        ],
        "edges": [{"source": "start", "target": "p"}, {"source": "p", "target": "end"}]
    }})
}

fn output(events: &[Event]) -> serde_json::Value {
    match events.last().unwrap() {
        Event::RunSucceeded { outputs, .. } => serde_json::to_value(outputs).unwrap()["out"].clone(),
        other => panic!("run did not succeed: {other:?}"),
    }
}

fn now_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

/// When the process that served a `testkit.started` node started.
async fn process_started_ns(pool: &Pooled) -> u128 {
    let out = output(&pool.run(single_node("testkit.started", json!({}), "started_ns")).await);
    u128::from(out.as_u64().expect("started_ns"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prewarmed_pool_serves_the_first_node_without_a_start() {
    let pool = pooled(1, None);
    assert!(!pool.is_warm(), "nothing starts before a prewarm or a request");
    tokio::time::timeout(Duration::from_secs(30), pool.host.prewarm()).await.unwrap();
    pool.wait_until_warm(true).await;
    let before_run = now_ns();
    assert!(process_started_ns(&pool).await < before_run, "the process started with the run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crashed_process_is_replaced_before_the_next_request() {
    let pool = pooled(1, None);
    pool.host.prewarm().await;
    pool.wait_until_warm(true).await;
    let marker = std::env::temp_dir().join(format!("graphrt-pool-crash-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let crash = single_node("testkit.crash_once", json!({"marker": marker.to_str().unwrap()}), "recovered");
    let events = pool.run(crash).await;
    assert!(matches!(events.last().unwrap(), Event::RunFailed { .. }), "the crash fails the node");
    let crashed_at = now_ns();
    // The keeper starts a new process without waiting for a request.
    pool.wait_until_warm(true).await;
    let before_run = now_ns();
    let started = process_started_ns(&pool).await;
    assert!(started < before_run, "the replacement started with the run, not in the background");
    assert!(started + 2_000_000_000 > crashed_at, "a fresh process serves after the crash");
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_pool_stops_its_processes_and_a_request_refills_it() {
    let pool = pooled(1, Some(Duration::from_millis(300)));
    pool.host.prewarm().await;
    pool.wait_until_warm(true).await;
    pool.wait_until_warm(false).await;
    let echo = single_node("testkit.echo", json!({"v": 7}), "config");
    assert_eq!(output(&pool.run(echo).await)["v"], 7, "a cold pool still serves (starting on demand)");
    pool.wait_until_warm(true).await;
    // And a prewarm (a gateway warm hint) refills it after the next idle stop.
    pool.wait_until_warm(false).await;
    pool.host.prewarm().await;
    pool.wait_until_warm(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_spread_over_the_processes_of_a_pool() {
    let pool = pooled(2, None);
    pool.host.prewarm().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    // Both members up (a request for each would otherwise wait for a start).
    while pids(&pool).await.len() < 2 {
        assert!(Instant::now() < deadline, "the second member never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // `testkit.hang` blocks the plugin's event loop: two of them only overlap
    // on two processes.
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "a", "type": "testkit.hang", "seconds": 0.6},
            {"id": "b", "type": "testkit.hang", "seconds": 0.6},
            {"id": "end", "type": "end", "outputs": []}
        ],
        "edges": [
            {"source": "start", "target": "a"}, {"source": "start", "target": "b"},
            {"source": "a", "target": "end"}, {"source": "b", "target": "end"}
        ]
    }});
    let started = Instant::now();
    let events = pool.run(doc).await;
    assert!(matches!(events.last().unwrap(), Event::RunSucceeded { .. }), "{:?}", events.last());
    assert!(started.elapsed() < Duration::from_millis(1100), "served one after the other: {:?}", started.elapsed());
}

/// Distinct process start times seen over a few parallel requests.
async fn pids(pool: &Pooled) -> std::collections::HashSet<u128> {
    let runs = (0..4).map(|_| process_started_ns(pool));
    futures::future::join_all(runs).await.into_iter().collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_member_is_killed_and_replaced() {
    let pool = pooled(1, None);
    pool.host.prewarm().await;
    let mut hang = single_node("testkit.hang", json!({"seconds": 30}), "x");
    hang["graph"]["nodes"][1]["timeout_ms"] = json!(300);
    hang["graph"]["nodes"][2]["outputs"] = json!([]);
    for _ in 0..graphrt_plugin::MAX_CONSECUTIVE_TIMEOUTS {
        let events = pool.run(hang.clone()).await;
        assert!(matches!(events.last().unwrap(), Event::RunFailed { error_type, .. } if error_type == "timeout"));
    }
    let started = Instant::now();
    let echo = single_node("testkit.echo", json!({"v": 1}), "config");
    assert_eq!(output(&pool.run(echo).await)["v"], 1);
    assert!(started.elapsed() < Duration::from_secs(10), "restart took {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_that_cannot_start_fails_its_nodes_instead_of_hanging() {
    let manifest = PluginManifest::from_json(
        r#"{"name":"ghost","runtime":"process","command":"/nonexistent/graphrt-ghost","nodes":[{"kind":"ghost.x","title":"X"}]}"#,
    )
    .unwrap();
    let mut host = PluginHost::new().with_pool(PoolConfig { size: 2, idle_timeout: None });
    host.add(LoadedManifest { manifest, dir: std::env::temp_dir() }).unwrap();
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();
    host.prewarm().await;
    let pool = Pooled { host, registry, tools };
    let events = pool.run(single_node("ghost.x", json!({}), "x")).await;
    assert!(matches!(events.last().unwrap(), Event::RunFailed { error_type, .. } if error_type == "plugin_start"));
    assert!(pool.host.warm_plugins().is_empty());
}
