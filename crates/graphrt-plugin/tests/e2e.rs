//! End-to-end tests: plugins driven through the real engine.

#![cfg(feature = "process")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::node::{Services, TokioClock};
use graphrt_core::tool::ToolRegistry;
use graphrt_core::{Event, Graph, Registry, Value, Workflow};
use graphrt_plugin::{LoadedManifest, PluginError, PluginHost, PluginManifest};
use serde_json::json;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

fn temp_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-plugin-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

struct Setup {
    registry: Registry,
    tools: ToolRegistry,
}

fn load(dirs: &[PathBuf]) -> Setup {
    let mut host = PluginHost::new();
    for dir in dirs {
        host.load_dir(dir).unwrap();
    }
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();
    Setup { registry, tools }
}

fn engine(setup: &Setup, doc: serde_json::Value) -> Engine {
    let wf = Workflow::from_value(doc).unwrap();
    let graph = Graph::build(wf, &setup.registry).unwrap();
    Engine::new(Arc::new(graph))
        .with_services(Services::new().with(setup.tools.clone()))
        .with_clock(Arc::new(TokioClock))
}

async fn collect(engine: &Engine, input: RunInput) -> Vec<Event> {
    let stream = engine.run(input);
    tokio::time::timeout(Duration::from_secs(30), stream.collect::<Vec<_>>()).await.unwrap()
}

fn terminal(events: &[Event]) -> &Event {
    events.last().unwrap()
}

fn outputs(events: &[Event]) -> serde_json::Value {
    match terminal(events) {
        Event::RunSucceeded { outputs, .. } => serde_json::to_value(outputs).unwrap(),
        other => panic!("run did not succeed: {other:?}"),
    }
}

fn chunks(events: &[Event], node: &str) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            Event::NodeChunk { node_id, chunk, .. } if node_id.as_ref() == node => Some(chunk.as_str()),
            _ => None,
        })
        .collect()
}

fn single_node(kind: &str, config: serde_json::Value) -> serde_json::Value {
    let mut node = json!({"id": "p", "type": kind});
    if let (Some(n), Some(c)) = (node.as_object_mut(), config.as_object()) {
        for (k, v) in c {
            n.insert(k.clone(), v.clone());
        }
    }
    json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "text", "type": "string"}]},
            node,
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["p", "result"]}]}
        ],
        "edges": [{"source": "start", "target": "p"}, {"source": "p", "target": "end"}]
    }})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_example_streams_and_branches() {
    let setup = load(&[repo_root().join("plugins/examples/python-sentiment")]);
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "review", "type": "paragraph"}]},
            {"id": "mood", "type": "sentiment.analyze", "text": "{{#start.review#}}"},
            {"id": "happy", "type": "answer", "answer": "Thanks! {{#mood.label#}} {{#mood.score#}}"},
            {"id": "sad", "type": "answer", "answer": "Sorry."},
            {"id": "meh", "type": "answer", "answer": "Noted."}
        ],
        "edges": [
            {"source": "start", "target": "mood"},
            {"source": "mood", "target": "happy", "handle": "positive"},
            {"source": "mood", "target": "sad", "handle": "negative"},
            {"source": "mood", "target": "meh", "handle": "neutral"}
        ]
    }});
    let e = engine(&setup, doc);
    let events = collect(&e, RunInput::new().input("review", "I love it, great and fast!")).await;
    assert_eq!(outputs(&events)["answer"], "Thanks! positive 1.0");
    assert_eq!(chunks(&events, "mood"), "love great fast ");
    assert!(events.iter().any(|e| matches!(e, Event::NodeSkipped { node_id, .. } if node_id.as_ref() == "sad")));

    // The catalog shows the plugin node with its source.
    let d = setup.registry.descriptor("sentiment.analyze").unwrap();
    assert_eq!(d.source, "plugin:sentiment");
    assert_eq!(d.outputs.len(), 3);

    // Tools are registered under the plugin name.
    let runtime = setup.tools.get("sentiment").unwrap();
    let specs = runtime.list().await.unwrap();
    assert_eq!(specs[0].name, "score");
    let out = runtime.call("score", json!({"text": "awful and slow"})).await.unwrap();
    assert_eq!(out.text, "-1.00");
    assert!(!out.is_error);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_is_rendered_and_events_flow() {
    let setup = load(&[fixture("testkit")]);
    let doc = single_node(
        "testkit.echo",
        json!({"greeting": "hi {{#start.text#}}", "value_selector": ["start", "text"], "nested": {"t": "{{#start.text#}}"}}),
    );
    let e = engine(&setup, doc);
    let events = collect(&e, RunInput::new().input("text", "Ada")).await;
    assert!(matches!(terminal(&events), Event::RunSucceeded { .. }), "{:?}", terminal(&events));
    let echoed = events
        .iter()
        .find_map(|e| match e {
            Event::NodeSucceeded { node_id, outputs, .. } if node_id.as_ref() == "p" => {
                Some(serde_json::to_value(outputs).unwrap())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(echoed["config"]["greeting"], "hi Ada");
    assert_eq!(echoed["config"]["greeting_raw"], "hi {{#start.text#}}");
    assert_eq!(echoed["config"]["value_selector_value"], "Ada");
    assert_eq!(echoed["config"]["nested"]["t"], "Ada");
    assert_eq!(echoed["attempt"], 1);
    assert_eq!(echoed["node_id"], "p");
    assert_eq!(chunks(&events, "p"), "hello");
    assert!(events.iter().any(|e| matches!(e, Event::NodeProgress { name, .. } if name == "step")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_map_to_node_errors() {
    let setup = load(&[fixture("testkit")]);
    let e = engine(&setup, single_node("testkit.fail", json!({})));
    let events = collect(&e, RunInput::new()).await;
    match terminal(&events) {
        Event::RunFailed { error, error_type, .. } => {
            assert_eq!(error_type, "upstream");
            assert!(error.contains("upstream unavailable"), "{error}");
        }
        other => panic!("{other:?}"),
    }
    // Retryable errors use the retry policy.
    let mut doc = single_node("testkit.fail", json!({"retryable": true}));
    doc["graph"]["nodes"][1]["retry"] = json!({"max_retries": 2, "interval_ms": 1});
    let events = collect(&engine(&setup, doc), RunInput::new()).await;
    let retries = events.iter().filter(|e| matches!(e, Event::NodeRetry { .. })).count();
    assert_eq!(retries, 2);
    // Non-retryable errors skip retries even with a policy.
    let mut doc = single_node("testkit.fail", json!({}));
    doc["graph"]["nodes"][1]["retry"] = json!({"max_retries": 2, "interval_ms": 1});
    let events = collect(&engine(&setup, doc), RunInput::new()).await;
    assert!(!events.iter().any(|e| matches!(e, Event::NodeRetry { .. })));
    // Unexpected exceptions.
    let events = collect(&engine(&setup, single_node("testkit.boom", json!({}))), RunInput::new()).await;
    assert!(matches!(terminal(&events), Event::RunFailed { error_type, .. } if error_type == "plugin_exception"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_cancels_the_plugin_request() {
    let setup = load(&[fixture("testkit")]);
    // Start the plugin process first: on slow interpreters (e.g. a pyenv shim,
    // ~0.3 s) the 300 ms timeout below would otherwise expire before the handler
    // starts, and a task cancelled before it runs never reaches its `except`.
    let warmup = collect(&engine(&setup, single_node("testkit.echo", json!({}))), RunInput::new()).await;
    assert!(matches!(terminal(&warmup), Event::RunSucceeded { .. }), "{warmup:?}");
    let marker = temp_file("cancel-marker");
    let mut doc = single_node("testkit.sleep", json!({"ms": 5000, "cancel_marker": marker.to_str().unwrap()}));
    doc["graph"]["nodes"][1]["timeout_ms"] = json!(300);
    let started = std::time::Instant::now();
    let events = collect(&engine(&setup, doc), RunInput::new()).await;
    assert!(matches!(terminal(&events), Event::RunFailed { error_type, .. } if error_type == "timeout"));
    assert!(started.elapsed() < Duration::from_secs(4));
    // The plugin received node.cancel and stopped the handler.
    let mut seen = false;
    for _ in 0..40 {
        if marker.exists() {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seen, "plugin did not observe the cancellation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_plugin_restarts_and_retry_succeeds() {
    let setup = load(&[fixture("testkit")]);
    let marker = temp_file("crash-marker");
    let mut doc = single_node("testkit.crash_once", json!({"marker": marker.to_str().unwrap()}));
    doc["graph"]["nodes"][1]["retry"] = json!({"max_retries": 2, "interval_ms": 1});
    doc["graph"]["nodes"][2]["outputs"] = json!([{"variable": "ok", "value_selector": ["p", "recovered"]}]);
    let events = collect(&engine(&setup, doc), RunInput::new()).await;
    let retry_error = events.iter().find_map(|e| match e {
        Event::NodeRetry { error, .. } => Some(error.clone()),
        _ => None,
    });
    let retry_error = retry_error.expect("expected a retry after the crash");
    assert!(retry_error.contains("crashed"), "{retry_error}");
    assert_eq!(outputs(&events)["ok"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_environment_is_isolated() {
    let setup = load(&[fixture("testkit")]);
    let mut doc = single_node("testkit.env", json!({}));
    doc["graph"]["nodes"][2]["outputs"] = json!([
        {"variable": "has_secret", "value_selector": ["p", "has_secret"]},
        {"variable": "allowed", "value_selector": ["p", "allowed"]},
        {"variable": "fixed", "value_selector": ["p", "fixed"]},
        {"variable": "plugin", "value_selector": ["p", "plugin"]}
    ]);
    let out = outputs(&collect(&engine(&setup, doc), RunInput::new()).await);
    assert_eq!(out["has_secret"], false, "a host variable outside the allow-list leaked");
    assert_eq!(out["allowed"], "graphrt-plugin");
    assert_eq!(out["fixed"], "fixed");
    assert_eq!(out["plugin"], "testkit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_are_masked_unless_permitted() {
    let setup = load(&[fixture("testkit")]);
    let mut doc = single_node("testkit.echo", json!({"key": "{{#env.API_KEY#}}"}));
    doc["env"] = json!([{"name": "API_KEY", "type": "secret"}]);
    let events =
        collect(&engine(&setup, doc), RunInput::new().env("API_KEY", Value::Secret(Arc::from("sk-live")))).await;
    let echoed = events
        .iter()
        .find_map(|e| match e {
            Event::NodeSucceeded { node_id, outputs, .. } if node_id.as_ref() == "p" => {
                Some(serde_json::to_value(outputs).unwrap())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(echoed["config"]["key"], "******");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_and_resume_through_json_snapshot() {
    let setup = load(&[fixture("testkit")]);
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "ask", "type": "testkit.approve", "question": "Ship it?"},
            {"id": "yes", "type": "answer", "answer": "shipped ({{#ask.asked#}})"},
            {"id": "no", "type": "answer", "answer": "held"}
        ],
        "edges": [
            {"source": "start", "target": "ask"},
            {"source": "ask", "target": "yes", "handle": "yes"},
            {"source": "ask", "target": "no", "handle": "no"}
        ]
    }});
    let e = engine(&setup, doc);
    let events = collect(&e, RunInput::new()).await;
    let snapshot = match terminal(&events) {
        Event::RunPaused { reasons, snapshot } => {
            assert_eq!(reasons[0].kind, "approval");
            snapshot.to_json().unwrap()
        }
        other => panic!("{other:?}"),
    };
    let snapshot = graphrt_core::Snapshot::from_json(&snapshot).unwrap();
    let mut inputs = graphrt_core::IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from("yes"));
    let events = collect(&e, RunInput::new().resume(snapshot, inputs)).await;
    assert_eq!(outputs(&events)["answer"], "shipped (Ship it?)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_writes_and_parallel_requests() {
    let setup = load(&[fixture("testkit")]);
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "w", "type": "testkit.write", "topic": "rust"},
            {"id": "a", "type": "testkit.sleep", "ms": 300},
            {"id": "b", "type": "testkit.sleep", "ms": 300},
            {"id": "c", "type": "testkit.sleep", "ms": 300},
            {"id": "end", "type": "end", "outputs": [{"variable": "topic", "value_selector": ["conversation", "topic"]}]}
        ],
        "edges": [
            {"source": "start", "target": "w"},
            {"source": "w", "target": "a"}, {"source": "w", "target": "b"}, {"source": "w", "target": "c"},
            {"source": "a", "target": "end"}, {"source": "b", "target": "end"}, {"source": "c", "target": "end"}
        ]
    }});
    let started = std::time::Instant::now();
    let events = collect(&engine(&setup, doc), RunInput::new()).await;
    assert_eq!(outputs(&events)["topic"], "rust");
    // Three 300 ms requests run concurrently over one process.
    assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_example_streams_and_pauses() {
    let setup = load(&[repo_root().join("plugins/examples/node-uppercase")]);
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "text", "type": "string"}]},
            {"id": "up", "type": "upper.transform", "text": "{{#start.text#}}", "mode": "title"},
            {"id": "ok", "type": "upper.approve", "text": "{{#up.text#}}"},
            {"id": "pub", "type": "answer", "answer": "published: {{#ok.text#}}"},
            {"id": "rej", "type": "answer", "answer": "rejected"}
        ],
        "edges": [
            {"source": "start", "target": "up"},
            {"source": "up", "target": "ok"},
            {"source": "ok", "target": "pub", "handle": "approved"},
            {"source": "ok", "target": "rej", "handle": "rejected"}
        ]
    }});
    let e = engine(&setup, doc);
    let events = collect(&e, RunInput::new().input("text", "hello plugin world")).await;
    assert_eq!(chunks(&events, "up"), "Hello Plugin World");
    let Event::RunPaused { snapshot, .. } = terminal(&events) else { panic!("{:?}", terminal(&events)) };
    let mut inputs = graphrt_core::IndexMap::new();
    inputs.insert("ok".to_owned(), Value::from_json(json!({"approved": true})));
    let events = collect(&e, RunInput::new().resume((**snapshot).clone(), inputs)).await;
    assert_eq!(outputs(&events)["answer"], "published: Hello Plugin World");

    let shout = setup.tools.get("upper").unwrap().call("shout", json!({"text": "hey"})).await.unwrap();
    assert_eq!(shout.text, "HEY");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relative_command_resolves_against_plugin_dir() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("graphrt-plugin-relcmd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let script = dir.join("bin").join("start.sh");
    let plugin_py = fixture("testkit").join("plugin.py");
    std::fs::write(&script, format!("#!/bin/sh\nexec python3 -u \"{}\"\n", plugin_py.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let manifest = json!({
        "name": "testkit",
        "runtime": "process",
        "command": "./bin/start.sh",
        "nodes": [{"kind": "testkit.echo", "title": "Echo"}]
    });
    std::fs::write(dir.join("graphrt-plugin.json"), manifest.to_string()).unwrap();
    let setup = load(&[dir]);
    let events = collect(&engine(&setup, single_node("testkit.echo", json!({"v": 1}))), RunInput::new()).await;
    assert!(matches!(terminal(&events), Event::RunSucceeded { .. }), "{:?}", terminal(&events));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_time_validation() {
    let setup = load(&[fixture("testkit")]);
    // Missing required config.
    let wf = Workflow::from_value(single_node("testkit.crash_once", json!({}))).unwrap();
    let err = Graph::build(wf, &setup.registry).unwrap_err().to_string();
    assert!(err.contains("marker"), "{err}");
    // Unknown references in templates and selectors are rejected.
    let wf = Workflow::from_value(single_node("testkit.echo", json!({"x": "{{#nope.value#}}"}))).unwrap();
    assert!(Graph::build(wf, &setup.registry).is_err());
    let wf = Workflow::from_value(single_node("testkit.echo", json!({"value_selector": ["missing", "v"]}))).unwrap();
    assert!(Graph::build(wf, &setup.registry).is_err());

    // Registering the same plugin kinds twice is a conflict, never a silent override.
    let mut host = PluginHost::new();
    host.load_dir(&fixture("testkit")).unwrap();
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();
    let err = host.register(&mut registry, &mut tools).unwrap_err();
    assert!(matches!(err, PluginError::KindConflict { .. }), "{err}");
    assert!(matches!(host.load_dir(&fixture("testkit")), Err(PluginError::DuplicatePlugin(_))));

    // A wasm plugin without a loader.
    let mut host = PluginHost::new();
    let manifest = PluginManifest::from_json(
        r#"{"name":"w","runtime":"wasm","module":"m.wasm","nodes":[{"kind":"w.x","title":"X"}]}"#,
    )
    .unwrap();
    host.add(LoadedManifest { manifest, dir: PathBuf::from(".") }).unwrap();
    let err = host.register(&mut Registry::new(), &mut ToolRegistry::new()).unwrap_err();
    assert!(matches!(err, PluginError::NoLoader { .. }), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_failure_is_reported() {
    let manifest = PluginManifest::from_json(
        r#"{"name":"ghost","runtime":"process","command":"/nonexistent/graphrt-ghost","nodes":[{"kind":"ghost.x","title":"X"}]}"#,
    )
    .unwrap();
    let mut host = PluginHost::new();
    host.add(LoadedManifest { manifest, dir: std::env::temp_dir() }).unwrap();
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();
    let setup = Setup { registry, tools };
    let events = collect(&engine(&setup, single_node("ghost.x", json!({}))), RunInput::new()).await;
    assert!(matches!(terminal(&events), Event::RunFailed { error_type, .. } if error_type == "plugin_start"));
}

fn error_type(events: &[Event]) -> String {
    match terminal(events) {
        Event::RunFailed { error_type, .. } => error_type.clone(),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wedged_plugin_is_killed_and_restarted() {
    let setup = load(&[fixture("testkit")]);
    let mut hang = single_node("testkit.hang", json!({"seconds": 30}));
    hang["graph"]["nodes"][1]["timeout_ms"] = json!(300);
    // The blocked event loop ignores `node.cancel`; after repeated timeouts the
    // host kills the process instead of queueing more work into it.
    for _ in 0..graphrt_plugin::MAX_CONSECUTIVE_TIMEOUTS {
        let events = collect(&engine(&setup, hang.clone()), RunInput::new()).await;
        assert_eq!(error_type(&events), "timeout");
    }
    let started = std::time::Instant::now();
    let mut echo = single_node("testkit.echo", json!({"v": 1}));
    echo["graph"]["nodes"][2]["outputs"] = json!([{"variable": "c", "value_selector": ["p", "config"]}]);
    let events = collect(&engine(&setup, echo), RunInput::new()).await;
    assert_eq!(outputs(&events)["c"]["v"], 1);
    assert!(started.elapsed() < Duration::from_secs(10), "restart took {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeclared_branch_handle_is_an_error() {
    let setup = load(&[fixture("testkit")]);
    // The edge must use a declared handle: graph building rejects undeclared ones.
    let mut doc = single_node("testkit.badbranch", json!({}));
    doc["graph"]["edges"][1]["handle"] = json!("yes");
    let events = collect(&engine(&setup, doc), RunInput::new()).await;
    assert_eq!(error_type(&events), "plugin_protocol");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unserializable_output_is_an_error_not_a_crash() {
    let setup = load(&[fixture("testkit")]);
    let events = collect(&engine(&setup, single_node("testkit.nan", json!({}))), RunInput::new()).await;
    assert_eq!(error_type(&events), "invalid_output");
    // The same process keeps serving.
    let mut echo = single_node("testkit.echo", json!({"v": 2}));
    echo["graph"]["nodes"][2]["outputs"] = json!([{"variable": "c", "value_selector": ["p", "config"]}]);
    assert_eq!(outputs(&collect(&engine(&setup, echo), RunInput::new()).await)["c"]["v"], 2);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_kills_the_whole_process_group() {
    let setup = load(&[fixture("testkit")]);
    let marker = temp_file("grandchild-pid");
    let events = collect(
        &engine(&setup, single_node("testkit.grandchild", json!({"marker": marker.to_str().unwrap()}))),
        RunInput::new(),
    )
    .await;
    let failure = match terminal(&events) {
        Event::RunFailed { error, .. } => error.clone(),
        other => panic!("{other:?}"),
    };
    // The crash report says how the process ended.
    assert!(failure.contains("exited with code 4"), "{failure}");
    let pid: u32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    let alive = |pid: u32| match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // A zombie (state `Z`) is dead, merely not yet reaped.
        Ok(stat) => !stat.rsplit(')').next().unwrap_or_default().trim_start().starts_with('Z'),
        Err(_) => false,
    };
    let mut gone = false;
    for _ in 0..40 {
        if !alive(pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "grandchild {pid} survived the plugin crash");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_input_secrets_are_masked() {
    let setup = load(&[fixture("testkit")]);
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "ask", "type": "testkit.approve", "question": "Key?"},
            {"id": "end", "type": "end", "outputs": [{"variable": "answer", "value_selector": ["ask", "answer"]}]}
        ],
        "edges": [
            {"source": "start", "target": "ask"},
            {"source": "ask", "target": "end", "handle": "yes"},
            {"source": "ask", "target": "end", "handle": "no"}
        ]
    }});
    let e = engine(&setup, doc);
    let events = collect(&e, RunInput::new()).await;
    let Event::RunPaused { snapshot, .. } = terminal(&events) else { panic!("{:?}", terminal(&events)) };
    let mut inputs = graphrt_core::IndexMap::new();
    inputs.insert("ask".to_owned(), Value::Secret(Arc::from("sk-typed-by-a-human")));
    let events = collect(&e, RunInput::new().resume((**snapshot).clone(), inputs)).await;
    assert_eq!(outputs(&events)["answer"], "******");
}

/// The testkit plugin with a manifest that requests plaintext secrets.
fn secret_requesting_manifest() -> LoadedManifest {
    let script = fixture("testkit").join("plugin.py");
    let manifest = PluginManifest::from_json(
        &json!({
            "name": "testkit",
            "runtime": "process",
            "command": "python3",
            "args": ["-u", script.to_str().unwrap()],
            "permissions": {"secrets": true},
            "nodes": [{"kind": "testkit.echo", "title": "Echo"}]
        })
        .to_string(),
    )
    .unwrap();
    LoadedManifest { manifest, dir: fixture("testkit") }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_permission_must_be_granted_by_the_host() {
    let mut doc = single_node("testkit.echo", json!({"key": "{{#env.API_KEY#}}"}));
    doc["env"] = json!([{"name": "API_KEY", "type": "secret"}]);
    doc["graph"]["nodes"][2]["outputs"] = json!([{"variable": "c", "value_selector": ["p", "config"]}]);
    let input = || RunInput::new().env("API_KEY", Value::Secret(Arc::from("sk-live")));

    for (trusted, expected) in [(false, "******"), (true, "sk-live")] {
        let mut host = if trusted { PluginHost::new().trust("testkit") } else { PluginHost::new() };
        let info = host.add(secret_requesting_manifest()).unwrap();
        assert_eq!(info.permissions.secrets, trusted);
        assert_eq!(info.warnings.is_empty(), trusted, "{:?}", info.warnings);
        let mut registry = Registry::with_builtins();
        let mut tools = ToolRegistry::new();
        host.register(&mut registry, &mut tools).unwrap();
        let setup = Setup { registry, tools };
        let events = collect(&engine(&setup, doc.clone()), input()).await;
        assert_eq!(outputs(&events)["c"]["key"], expected, "trusted={trusted}");
    }
}
