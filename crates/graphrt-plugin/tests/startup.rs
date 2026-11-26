//! A process plugin's start-up (a cold Node.js or Python process, a slow
//! Windows runner) is not counted against `limits.timeout_ms`: that is the
//! request's timeout. The start-up has the `initialize` handshake's own bound.

#![cfg(feature = "process")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::node::{Services, TokioClock};
use graphrt_core::tool::ToolRegistry;
use graphrt_core::{Event, Graph, Registry, Workflow};
use graphrt_plugin::PluginHost;
use serde_json::json;

fn has_python() -> bool {
    std::process::Command::new("python3").arg("--version").output().is_ok_and(|output| output.status.success())
}

/// The testkit fixture behind a 1.5 s start-up, with a 500 ms request timeout.
fn slow_start_plugin(name: &str) -> PathBuf {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("testkit").join("plugin.py");
    let dir = std::env::temp_dir().join(format!("graphrt-slow-start-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let start = format!(
        "import runpy, time; time.sleep(1.5); runpy.run_path({:?}, run_name='__main__')",
        fixture.to_str().unwrap()
    );
    let manifest = json!({
        "name": "testkit",
        "version": "0.0.1",
        "runtime": "process",
        "command": "python3",
        "args": ["-u", "-c", start],
        "limits": { "timeout_ms": 500 },
        "nodes": [{ "kind": "testkit.echo", "title": "Echo", "outputs": [{ "name": "config", "type": "object" }] }],
    });
    std::fs::write(dir.join("graphrt-plugin.json"), manifest.to_string()).unwrap();
    dir
}

async fn echo_once(host: &PluginHost) -> Vec<Event> {
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "p", "type": "testkit.echo", "v": 7},
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["p", "config"]}]}
        ],
        "edges": [{"source": "start", "target": "p"}, {"source": "p", "target": "end"}]
    }});
    let graph = Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap();
    let engine =
        Engine::new(Arc::new(graph)).with_services(Services::new().with(tools)).with_clock(Arc::new(TokioClock));
    tokio::time::timeout(Duration::from_secs(30), engine.run(RunInput::new()).collect::<Vec<_>>()).await.unwrap()
}

fn succeeded(events: &[Event]) -> bool {
    matches!(events.last(), Some(Event::RunSucceeded { .. }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_start_does_not_time_out_the_first_request() {
    if !has_python() {
        eprintln!("skipping: python3 is required");
        return;
    }
    let dir = slow_start_plugin("single");
    let mut host = PluginHost::new();
    host.load_dir(&dir).unwrap();
    let events = echo_once(&host).await;
    assert!(succeeded(&events), "the first request after a 1.5 s start-up: {:?}", events.last());
    let _ = std::fs::remove_dir_all(&dir);
}
