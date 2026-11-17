//! Rust-side tests of the embedding API (no binding layer involved).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use graphrt_ffi::{EventListener, GraphrtError, Runtime};

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
    chunks: Mutex<Vec<String>>,
    finished: Mutex<Vec<String>>,
}

impl EventListener for Recorder {
    fn on_event(&self, event_json: String) {
        self.events.lock().unwrap().push(event_json);
    }
    fn on_answer_chunk(&self, text: String) {
        self.chunks.lock().unwrap().push(text);
    }
    fn on_finish(&self, result_json: String) {
        self.finished.lock().unwrap().push(result_json);
    }
}

/// Listener whose callbacks panic: the run must still complete.
struct Panicky;

impl EventListener for Panicky {
    fn on_event(&self, _: String) {
        panic!("listener bug");
    }
    fn on_answer_chunk(&self, _: String) {
        panic!("listener bug");
    }
    fn on_finish(&self, _: String) {
        panic!("listener bug");
    }
}

const GREET: &str = r#"
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: name, type: string, required: true }] }
    - { id: answer, type: answer, answer: "Hello, {{#start.name#}}!" }
  edges:
    - { source: start, target: answer }
"#;

const APPROVAL: &str = r#"
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: amount, type: number, required: true }] }
    - id: review
      type: human-input
      prompt: "Approve {{#start.amount#}}?"
      actions: [{ id: approve, title: Approve }, { id: reject, title: Reject }]
    - { id: ok, type: answer, answer: "Approved {{#start.amount#}}" }
    - { id: no, type: answer, answer: "Rejected" }
  edges:
    - { source: start, target: review }
    - { source: review, target: ok, handle: approve }
    - { source: review, target: no, handle: reject }
"#;

const SLOW: &str = r#"
graph:
  nodes:
    - { id: start, type: start }
    - id: js
      type: code
      code_language: javascript
      code: "function main() { let x = 0; for (let i = 0; i < 4e9; i++) { x += i; } return { x }; }"
      variables: []
      outputs: { x: { type: number } }
    - { id: end, type: end, outputs: [{ variable: x, value_selector: [js, x] }] }
  edges:
    - { source: start, target: js }
    - { source: js, target: end }
"#;

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}

#[test]
fn runs_a_workflow_and_streams_callbacks() {
    let rt = Runtime::new("{}".into()).unwrap();
    let wf = rt.load_workflow(GREET.into()).unwrap();
    let rec = Arc::new(Recorder::default());
    let run = wf.run(r#"{"name":"Ada"}"#.into(), "{}".into(), rec.clone()).unwrap();
    let result = json(&run.wait_json(10_000).expect("finished"));
    assert_eq!(result["status"], "succeeded");
    assert_eq!(result["answer"], "Hello, Ada!");
    assert!(run.is_finished());
    // on_finish may land just after wait_json returns.
    for _ in 0..100 {
        if !rec.finished.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(rec.finished.lock().unwrap().len(), 1);
    assert_eq!(rec.chunks.lock().unwrap().concat(), "Hello, Ada!");
    let events = rec.events.lock().unwrap();
    assert!(json(events.first().unwrap())["type"] == "run_started");
    assert!(events.iter().any(|e| json(e)["type"] == "node_succeeded"));
}

#[test]
fn invalid_inputs_are_reported() {
    let rt = Runtime::new("{}".into()).unwrap();
    assert!(matches!(rt.clone().load_workflow("graph: [".into()), Err(GraphrtError::InvalidWorkflow { .. })));
    assert!(matches!(Runtime::new(r#"{"nope": 1}"#.into()), Err(GraphrtError::InvalidConfig { .. })));
    let wf = rt.load_workflow(GREET.into()).unwrap();
    let rec = Arc::new(Recorder::default());
    assert!(matches!(wf.run("[1]".into(), "{}".into(), rec.clone()), Err(GraphrtError::InvalidInput { .. })));
    // Missing required input: the run itself fails with a typed error.
    let run = wf.run("{}".into(), "{}".into(), rec).unwrap();
    let result = json(&run.wait_json(10_000).unwrap());
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error_type"], "invalid_input");
}

#[test]
fn pause_and_resume_through_json() {
    let rt = Runtime::new("{}".into()).unwrap();
    let wf = rt.load_workflow(APPROVAL.into()).unwrap();
    let run = wf.run(r#"{"amount": 42}"#.into(), "{}".into(), Arc::new(Recorder::default())).unwrap();
    let paused = json(&run.wait_json(10_000).unwrap());
    assert_eq!(paused["status"], "paused");
    let snapshot = paused["snapshot"].to_string();

    let rec = Arc::new(Recorder::default());
    let resumed =
        wf.resume(snapshot, r#"{"review": {"action": "approve", "values": {}}}"#.into(), String::new(), rec).unwrap();
    let result = json(&resumed.wait_json(10_000).unwrap());
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(result["answer"], "Approved 42");

    assert!(matches!(
        wf.resume("{}".into(), "{}".into(), String::new(), Arc::new(Recorder::default())),
        Err(GraphrtError::InvalidInput { .. })
    ));
}

#[test]
fn abort_cancels_a_running_workflow() {
    let rt = Runtime::new("{}".into()).unwrap();
    let wf = rt.load_workflow(SLOW.into()).unwrap();
    let run = wf.run("{}".into(), "{}".into(), Arc::new(Recorder::default())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let started = std::time::Instant::now();
    run.abort("user cancelled".into());
    let result = json(&run.wait_json(15_000).expect("finished after abort"));
    assert_eq!(result["status"], "aborted", "{result}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn panicking_listener_does_not_break_the_run() {
    let rt = Runtime::new("{}".into()).unwrap();
    let wf = rt.load_workflow(GREET.into()).unwrap();
    let run = wf.run(r#"{"name":"Ada"}"#.into(), "{}".into(), Arc::new(Panicky)).unwrap();
    let result = json(&run.wait_json(10_000).unwrap());
    assert_eq!(result["status"], "succeeded");
}

#[test]
fn catalog_and_config() {
    let rt = Runtime::new(
        r#"{"providers": {"OLLAMA_HOST": "http://localhost:11434"}, "env": {"TOKEN": "s3cret"},
            "limits": {"max_steps": 100}}"#
            .into(),
    )
    .unwrap();
    let catalog = json(&rt.catalog_json());
    let kinds: Vec<&str> = catalog.as_array().unwrap().iter().filter_map(|d| d["kind"].as_str()).collect();
    for kind in ["start", "llm", "agent", "http-request", "code", "document-extractor", "knowledge-retrieval"] {
        assert!(kinds.contains(&kind), "missing {kind} in {kinds:?}");
    }
    let wf = rt.load_workflow(GREET.into()).unwrap();
    let used = json(&wf.catalog_json());
    assert_eq!(used.as_array().unwrap().len(), 2);
    assert!(!graphrt_ffi::graphrt_version().is_empty());
    assert!(matches!(
        Runtime::new(r#"{"mcp_servers": {"mcpServers": {"x": {"command": "npx"}}}}"#.into()),
        Err(GraphrtError::InvalidConfig { .. })
    ));
}

#[test]
fn wasm_plugin_registration() {
    let rt = Runtime::new("{}".into()).unwrap();
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../graphrt-wasm/tests/fixtures/text_tools.wasm");
    let bytes = std::fs::read(fixture).unwrap();
    let kinds = rt.register_wasm_plugin(bytes).unwrap();
    assert!(!kinds.is_empty());
    assert!(matches!(rt.register_wasm_plugin(vec![0, 1, 2]), Err(GraphrtError::Plugin { .. })));
    let catalog = rt.catalog_json();
    assert!(kinds.iter().all(|k| catalog.contains(k.as_str())));
}
