//! Integration tests: real guest modules through the real engine.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::print_stdout)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::catalog::Handles;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::{Graph, GraphError};
use graphrt_core::node::{NodeContext, NodeOutcome, Registry, TokioClock};
use graphrt_core::pool::VariablePool;
use graphrt_core::value::Value;
use graphrt_wasm::{WasmError, WasmLimits, WasmPlugin};

const TEXT_TOOLS: &[u8] = include_bytes!("fixtures/text_tools.wasm");

fn text_tools() -> WasmPlugin {
    WasmPlugin::load(TEXT_TOOLS, WasmLimits::default()).expect("load fixture")
}

fn registry_with(plugin: &WasmPlugin) -> Registry {
    let mut registry = Registry::with_builtins();
    plugin.register(&mut registry).expect("register");
    registry
}

fn engine(doc: serde_json::Value, registry: &Registry) -> Engine {
    let wf = Workflow::from_value(doc).expect("doc");
    let graph = Graph::build(wf, registry).expect("graph");
    Engine::new(Arc::new(graph)).with_clock(Arc::new(TokioClock))
}

async fn collect(engine: &Engine, input: RunInput) -> Vec<Event> {
    engine.run(input).collect().await
}

fn terminal(events: &[Event]) -> &Event {
    events.last().expect("events")
}

/// Minimal hand-written guest. `run_body` must leave an i64 on the stack.
fn wat_module(manifest: &str, run_body: &str, extra_data: &str) -> Vec<u8> {
    wat_module_with_imports("", manifest, run_body, extra_data)
}

/// Like [`wat_module`], with host imports (which must precede other definitions).
fn wat_module_with_imports(imports: &str, manifest: &str, run_body: &str, extra_data: &str) -> Vec<u8> {
    let escaped = manifest.replace('\\', "\\\\").replace('"', "\\\"");
    let len = manifest.len();
    let text = format!(
        r#"(module
  {imports}
  (memory (export "memory") 1)
  (global $bump (mut i32) (i32.const 32768))
  (data (i32.const 1024) "{escaped}")
  {extra_data}
  (func (export "graphrt_abi_version") (result i32) (i32.const 1))
  (func (export "graphrt_alloc") (param $len i32) (result i32) (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $len)))
    (local.get $p))
  (func (export "graphrt_describe") (result i64)
    (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {len})))
  (func (export "graphrt_run") (param $ptr i32) (param $len i32) (result i64)
    {run_body})
)"#
    );
    wat::parse_str(text).expect("valid wat")
}

const ONE_NODE: &str = r#"{"name":"t","nodes":[{"kind":"probe","title":"Probe"}]}"#;

fn probe_doc() -> serde_json::Value {
    serde_json::json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "p", "type": "probe"},
            {"id": "end", "type": "end", "outputs": [{"variable": "x", "value_selector": ["p", "x"]}]}
        ],
        "edges": [{"source": "start", "target": "p"}, {"source": "p", "target": "end"}]
    }})
}

async fn run_probe(module: &[u8], limits: WasmLimits) -> Event {
    let plugin = WasmPlugin::load(module, limits).expect("load");
    let registry = registry_with(&plugin);
    let engine = engine(probe_doc(), &registry);
    engine.run_to_end(RunInput::new()).await
}

fn failure_type(event: &Event) -> String {
    match event {
        Event::RunFailed { error_type, error, .. } => format!("{error_type}: {error}"),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn loads_and_describes_fixture() {
    let plugin = text_tools();
    assert_eq!(plugin.manifest().name, "text-tools");
    let kinds: Vec<String> = plugin.descriptors().into_iter().map(|d| d.kind).collect();
    assert_eq!(kinds, ["slugify", "word-count", "length-router"]);
    let router = plugin.descriptors().into_iter().find(|d| d.kind == "length-router").expect("router");
    assert_eq!(router.handles, Handles::Fixed { handles: vec!["short".into(), "long".into()] });
    assert_eq!(router.source, "wasm:text-tools");
    let registry = registry_with(&plugin);
    let d = registry.descriptor("slugify").expect("registered descriptor");
    assert_eq!(d.outputs.first().map(|o| o.name.as_str()), Some("slug"));
    assert!(plugin.factory("nope").is_err());
}

#[tokio::test]
async fn slugify_through_engine_with_placeholders() {
    let plugin = text_tools();
    let registry = registry_with(&plugin);
    let engine = engine(
        serde_json::json!({"graph": {
            "nodes": [
                {"id": "start", "type": "start", "variables": [{"variable": "title", "type": "string", "required": true}]},
                {"id": "slug", "type": "slugify", "text": "{{#start.title#}} v2", "separator": "_"},
                {"id": "end", "type": "end", "outputs": [{"variable": "slug", "value_selector": ["slug", "slug"]}]}
            ],
            "edges": [{"source": "start", "target": "slug"}, {"source": "slug", "target": "end"}]
        }}),
        &registry,
    );
    let event = engine.run_to_end(RunInput::new().input("title", "Hello, WebAssembly World")).await;
    match event {
        Event::RunSucceeded { outputs, .. } => {
            assert_eq!(outputs.get("slug"), Some(&Value::from("hello_webassembly_world_v2")));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn word_count_streams_chunks_progress_and_logs() {
    let plugin = text_tools();
    let registry = registry_with(&plugin);
    let engine = engine(
        serde_json::json!({"graph": {
            "nodes": [
                {"id": "start", "type": "start", "variables": []},
                {"id": "wc", "type": "word-count", "text": "one two three"},
                {"id": "end", "type": "end", "outputs": [{"variable": "n", "value_selector": ["wc", "words"]}]}
            ],
            "edges": [{"source": "start", "target": "wc"}, {"source": "wc", "target": "end"}]
        }}),
        &registry,
    );
    let events = collect(&engine, RunInput::new()).await;
    let chunks: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeChunk { node_id, field, chunk, .. } if node_id.as_ref() == "wc" && field == "report" => {
                Some(chunk.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(chunks, ["1: one\n", "2: two\n", "3: three\n"]);
    assert!(events.iter().any(|e| matches!(e, Event::NodeProgress { name, .. } if name == "counted")));
    let meta = events.iter().find_map(|e| match e {
        Event::NodeSucceeded { node_id, metadata, .. } if node_id.as_ref() == "wc" => Some(metadata.clone()),
        _ => None,
    });
    let logs = meta.and_then(|m| m.get("logs").cloned()).expect("logs metadata");
    assert_eq!(logs.get_path(&["0", "message"]), Some(Value::from("counted 3 words")));
    match terminal(&events) {
        Event::RunSucceeded { outputs, .. } => assert_eq!(outputs.get("n"), Some(&Value::Int(3))),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn branching_plugin_node_routes_edges() {
    let plugin = text_tools();
    let registry = registry_with(&plugin);
    let doc = |text: &str| {
        serde_json::json!({"graph": {
            "nodes": [
                {"id": "start", "type": "start", "variables": []},
                {"id": "route", "type": "length-router", "text": text, "threshold": 5},
                {"id": "s", "type": "answer", "answer": "short"},
                {"id": "l", "type": "answer", "answer": "long"}
            ],
            "edges": [
                {"source": "start", "target": "route"},
                {"source": "route", "target": "s", "handle": "short"},
                {"source": "route", "target": "l", "handle": "long"}
            ]
        }})
    };
    for (text, expected, skipped) in [("hi", "short", "l"), ("a much longer text", "long", "s")] {
        let events = collect(&engine(doc(text), &registry), RunInput::new()).await;
        assert!(events.iter().any(|e| matches!(e, Event::NodeSkipped { node_id, .. } if node_id.as_ref() == skipped)));
        match terminal(&events) {
            Event::RunSucceeded { outputs, .. } => assert_eq!(outputs.get("answer"), Some(&Value::from(expected))),
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn secrets_are_masked_unless_exposed() {
    let doc = serde_json::json!({
        "env": [{"name": "KEY", "type": "secret"}],
        "graph": {
            "nodes": [
                {"id": "start", "type": "start", "variables": []},
                {"id": "slug", "type": "slugify", "text": "{{#env.KEY#}}"},
                {"id": "end", "type": "end", "outputs": [{"variable": "slug", "value_selector": ["slug", "slug"]}]}
            ],
            "edges": [{"source": "start", "target": "slug"}, {"source": "slug", "target": "end"}]
        }
    });
    for (expose, expected) in [(false, ""), (true, "sk-abc123")] {
        let plugin = WasmPlugin::load(TEXT_TOOLS, WasmLimits::default().with_expose_secrets(expose)).expect("load");
        let registry = registry_with(&plugin);
        let event = engine(doc.clone(), &registry)
            .run_to_end(RunInput::new().env("KEY", Value::Secret(Arc::from("sk-abc123"))))
            .await;
        match event {
            Event::RunSucceeded { outputs, .. } => assert_eq!(outputs.get("slug"), Some(&Value::from(expected))),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn unknown_references_fail_graph_build() {
    let plugin = text_tools();
    let registry = registry_with(&plugin);
    let wf = Workflow::from_value(serde_json::json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "slug", "type": "slugify", "text": "{{#nowhere.x#}}"}
        ],
        "edges": [{"source": "start", "target": "slug"}]
    }}))
    .expect("doc");
    assert!(matches!(Graph::build(wf, &registry), Err(GraphError::UnknownReference { .. })));
}

#[tokio::test]
async fn fuel_exhaustion_is_fatal() {
    let module = wat_module(ONE_NODE, "(loop $l (br $l)) (i64.const 0)", "");
    let event = run_probe(&module, WasmLimits::default().with_fuel(100_000)).await;
    assert!(failure_type(&event).starts_with("wasm_fuel_exhausted"), "{event:?}");
}

#[tokio::test]
async fn memory_cap_is_enforced() {
    let module = wat_module(ONE_NODE, "(drop (memory.grow (i32.const 1000))) (i64.const 0)", "");
    let event = run_probe(&module, WasmLimits::default().with_memory_bytes(2 * 65536)).await;
    assert!(failure_type(&event).starts_with("wasm_memory_limit"), "{event:?}");
}

const CHUNK_IMPORT: &str = r#"(import "graphrt" "chunk" (func $chunk (param i32 i32 i32 i32)))"#;
const LOG_IMPORT: &str = r#"(import "graphrt" "log" (func $log (param i32 i32 i32)))"#;

/// A guest that streams the same 64 KiB chunk forever.
fn chunk_flood() -> Vec<u8> {
    wat_module_with_imports(
        CHUNK_IMPORT,
        ONE_NODE,
        "(loop $l (call $chunk (i32.const 2048) (i32.const 4) (i32.const 0) (i32.const 65536)) (br $l)) (i64.const 0)",
        r#"(data (i32.const 2048) "text")"#,
    )
}

#[tokio::test]
async fn host_calls_are_charged_against_fuel() {
    // 1000 calls copying 64 KiB each cost ~65 M fuel: a 10 M budget must run out
    // long before the event caps are reached.
    let limits = WasmLimits { max_events: usize::MAX, max_stream_bytes: usize::MAX, ..WasmLimits::default() }
        .with_fuel(10_000_000);
    let event = run_probe(&chunk_flood(), limits).await;
    assert!(failure_type(&event).starts_with("wasm_fuel_exhausted"), "{event:?}");
}

#[tokio::test]
async fn streamed_events_are_capped_per_call() {
    let limits = WasmLimits { max_events: 10, ..WasmLimits::default() };
    let event = run_probe(&chunk_flood(), limits).await;
    let failure = failure_type(&event);
    assert!(failure.starts_with("wasm_invalid_output") && failure.contains("streamed events"), "{failure}");

    let limits = WasmLimits { max_stream_bytes: 4 * 65536, ..WasmLimits::default() };
    let failure = failure_type(&run_probe(&chunk_flood(), limits).await);
    assert!(failure.contains("streamed bytes"), "{failure}");
}

#[tokio::test]
async fn guest_logs_are_attached_to_failures() {
    let module = wat_module_with_imports(
        LOG_IMPORT,
        ONE_NODE,
        "(call $log (i32.const 3) (i32.const 2048) (i32.const 17)) unreachable",
        r#"(data (i32.const 2048) "about to give up!")"#,
    );
    let failure = failure_type(&run_probe(&module, WasmLimits::default()).await);
    assert!(failure.starts_with("wasm_trap") && failure.contains("about to give up!"), "{failure}");
}

#[test]
fn modules_cannot_replace_or_squat_node_kinds() {
    // A module declaring a built-in kind never replaces it.
    let llm_like = wat_module(r#"{"name":"t","nodes":[{"kind":"answer","title":"Fake"}]}"#, "(i64.const 0)", "");
    let plugin = WasmPlugin::load(&llm_like, WasmLimits::default()).expect("load");
    let mut registry = Registry::with_builtins();
    assert!(matches!(plugin.register(&mut registry), Err(WasmError::KindConflict(k)) if k == "answer"));
    assert_ne!(registry.descriptor("answer").map(|d| d.source), Some("wasm:t".to_owned()));
    // Namespaced kinds must use the module's own name.
    let squat = wat_module(r#"{"name":"t","nodes":[{"kind":"other.x","title":"X"}]}"#, "(i64.const 0)", "");
    assert!(matches!(WasmPlugin::load(&squat, WasmLimits::default()), Err(WasmError::Manifest(_))));
    let own = wat_module(r#"{"name":"t","nodes":[{"kind":"t.x","title":"X"}]}"#, "(i64.const 0)", "");
    assert!(WasmPlugin::load(&own, WasmLimits::default()).is_ok());
}

#[tokio::test]
async fn malformed_and_out_of_bounds_output() {
    // Response points at non-JSON bytes.
    let module = wat_module(
        ONE_NODE,
        "(i64.or (i64.shl (i64.const 4096) (i64.const 32)) (i64.const 8))",
        r#"(data (i32.const 4096) "not json")"#,
    );
    let event = run_probe(&module, WasmLimits::default()).await;
    assert!(failure_type(&event).starts_with("wasm_invalid_output"), "{event:?}");

    // Response points past the end of memory.
    let module = wat_module(ONE_NODE, "(i64.or (i64.shl (i64.const 65530) (i64.const 32)) (i64.const 100))", "");
    let event = run_probe(&module, WasmLimits::default()).await;
    assert!(failure_type(&event).contains("out-of-bounds"), "{event:?}");

    // Valid JSON without outputs / pause / error.
    let module = wat_module(
        ONE_NODE,
        "(i64.or (i64.shl (i64.const 4096) (i64.const 32)) (i64.const 2))",
        r#"(data (i32.const 4096) "{}")"#,
    );
    let event = run_probe(&module, WasmLimits::default()).await;
    assert!(failure_type(&event).starts_with("wasm_invalid_output"), "{event:?}");

    // Oversized response.
    let module = wat_module(ONE_NODE, "(i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const 60000))", "");
    let event = run_probe(&module, WasmLimits::default().with_output_bytes(1000)).await;
    assert!(failure_type(&event).contains("above the 1000-byte limit"), "{event:?}");
}

#[tokio::test]
async fn guest_trap_and_guest_error() {
    let module = wat_module(ONE_NODE, "unreachable", "");
    let event = run_probe(&module, WasmLimits::default()).await;
    assert!(failure_type(&event).starts_with("wasm_trap"), "{event:?}");

    let body = r#"{"error":{"kind":"bad_thing","message":"nope"}}"#;
    let module = wat_module(
        ONE_NODE,
        &format!("(i64.or (i64.shl (i64.const 4096) (i64.const 32)) (i64.const {}))", body.len()),
        &format!(r#"(data (i32.const 4096) "{}")"#, body.replace('"', "\\\"")),
    );
    let event = run_probe(&module, WasmLimits::default()).await;
    assert_eq!(failure_type(&event), "bad_thing: bad_thing: nope");
}

#[tokio::test]
async fn guest_outputs_and_pause() {
    let body = r#"{"outputs":{"x":42}}"#;
    let module = wat_module(
        ONE_NODE,
        &format!("(i64.or (i64.shl (i64.const 4096) (i64.const 32)) (i64.const {}))", body.len()),
        &format!(r#"(data (i32.const 4096) "{}")"#, body.replace('"', "\\\"")),
    );
    match run_probe(&module, WasmLimits::default()).await {
        Event::RunSucceeded { outputs, .. } => assert_eq!(outputs.get("x"), Some(&Value::Int(42))),
        other => panic!("{other:?}"),
    }

    let body = r#"{"pause":{"kind":"approval","data":{"q":"ok?"},"state":{"step":1}}}"#;
    let module = wat_module(
        ONE_NODE,
        &format!("(i64.or (i64.shl (i64.const 4096) (i64.const 32)) (i64.const {}))", body.len()),
        &format!(r#"(data (i32.const 4096) "{}")"#, body.replace('"', "\\\"")),
    );
    let plugin = WasmPlugin::load(&module, WasmLimits::default()).expect("load");
    let factory = plugin.factory("probe").expect("factory");
    let node = factory(&graphrt_core::dsl::NodeDoc::new("p", "probe", serde_json::json!({}))).expect("node");
    let outcome = node.run(NodeContext::for_test("p", VariablePool::new())).await.expect("ok");
    match outcome {
        NodeOutcome::Paused(p) => {
            assert_eq!(p.kind, "approval");
            assert_eq!(p.state, serde_json::json!({"step": 1}));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn rejects_bad_modules() {
    let limits = WasmLimits::default;
    assert!(matches!(WasmPlugin::load(b"not wasm", limits()), Err(WasmError::InvalidModule(_))));

    let bad_import =
        wat::parse_str(r#"(module (import "env" "open" (func)) (memory (export "memory") 1))"#).expect("wat");
    assert!(
        matches!(WasmPlugin::load(&bad_import, limits()), Err(WasmError::InvalidModule(m)) if m.contains("env::open"))
    );

    let no_version = wat::parse_str(r#"(module (memory (export "memory") 1))"#).expect("wat");
    assert!(matches!(WasmPlugin::load(&no_version, limits()), Err(WasmError::MissingExport(_))));

    let wrong_version = wat::parse_str(
        r#"(module (memory (export "memory") 1) (func (export "graphrt_abi_version") (result i32) (i32.const 9)))"#,
    )
    .expect("wat");
    assert!(matches!(WasmPlugin::load(&wrong_version, limits()), Err(WasmError::AbiVersion { found: 9, .. })));

    let dup =
        wat_module(r#"{"name":"t","nodes":[{"kind":"a","title":"A"},{"kind":"a","title":"B"}]}"#, "(i64.const 0)", "");
    assert!(matches!(WasmPlugin::load(&dup, limits()), Err(WasmError::DuplicateKind(k)) if k == "a"));

    let container =
        wat_module(r#"{"name":"t","nodes":[{"kind":"c","title":"C","container":true}]}"#, "(i64.const 0)", "");
    assert!(matches!(WasmPlugin::load(&container, limits()), Err(WasmError::Manifest(_))));

    let huge = vec![0u8; 2048];
    let small = WasmLimits { module_bytes: 1024, ..WasmLimits::default() };
    assert!(matches!(WasmPlugin::load(&huge, small), Err(WasmError::ModuleTooLarge { .. })));

    let start_loop = wat::parse_str(
        r#"(module (memory (export "memory") 1) (func $s (loop $l (br $l))) (start $s)
             (func (export "graphrt_abi_version") (result i32) (i32.const 1)))"#,
    )
    .expect("wat");
    assert!(matches!(
        WasmPlugin::load(&start_loop, WasmLimits::default().with_fuel(10_000)),
        Err(WasmError::FuelExhausted)
    ));
}

/// Prints per-run overhead (fresh instance + JSON round trip) for the fixture.
#[tokio::test]
async fn per_run_overhead() {
    let plugin = text_tools();
    let factory = plugin.factory("slugify").expect("factory");
    let node = factory(&graphrt_core::dsl::NodeDoc::new("s", "slugify", serde_json::json!({"text": "Hello World"})))
        .expect("node");
    let seen = Arc::new(Mutex::new(0u32));
    let runs = 200u32;
    let started = Instant::now();
    for _ in 0..runs {
        let outcome = node.run(NodeContext::for_test("s", VariablePool::new())).await.expect("run");
        assert!(matches!(outcome, NodeOutcome::Completed(_)));
        if let Ok(mut n) = seen.lock() {
            *n += 1;
        }
    }
    let per_run = started.elapsed() / runs;
    println!("wasm per-run overhead: {per_run:?} ({} runs)", seen.lock().map(|n| *n).unwrap_or(0));
    assert!(per_run.as_millis() < 250, "per-run overhead too high: {per_run:?}");
}

/// Rebuilds the fixture from `sdk/wasm/examples/text-tools` and checks it still loads.
/// Opt-in (slow): `GRAPHRT_WASM_REBUILD=1 cargo test -p graphrt-wasm rebuild_fixture`.
#[test]
fn rebuild_fixture() {
    if std::env::var("GRAPHRT_WASM_REBUILD").ok().as_deref() != Some("1") {
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/wasm/examples/text-tools");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(&root)
        .status()
        .expect("spawn cargo");
    assert!(status.success(), "building the example plugin failed (is the wasm32-unknown-unknown target installed?)");
    let built = root.join("target/wasm32-unknown-unknown/release/text_tools.wasm");
    let plugin = WasmPlugin::load_file(&built, WasmLimits::default()).expect("load rebuilt");
    assert_eq!(plugin.manifest().name, "text-tools");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text_tools.wasm");
    std::fs::copy(&built, &fixture).expect("update fixture");
}
