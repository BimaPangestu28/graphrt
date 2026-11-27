//! An agent graph through the embedding API, as an app runs it: a scripted
//! model, native tools of the app (`ToolProvider`), a pause for approval, the
//! resume and the streamed answer, with typed events.

#![cfg(all(feature = "scripted-llm", feature = "ui"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphrt_ffi::{
    GraphrtError, HostToolCall, HostToolResult, HostToolSpec, PausedRun, RequestKind, RunEvent, RunListener, RunStatus,
    Runtime, ToolCallError, ToolProvider, WorkflowHandle,
};

const AGENT: &str = include_str!("../../../examples/mobile/agent.yaml");
const CONFIG: &str = include_str!("../../../examples/mobile/runtime-config.json");

/// The app's tools: `battery_level` answers at once, `send_message` records what it sent.
#[derive(Default)]
struct Device {
    sent: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ToolProvider for Device {
    async fn call_tool(&self, call: HostToolCall) -> Result<HostToolResult, ToolCallError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let args: serde_json::Value = serde_json::from_str(&call.arguments_json).unwrap();
        match call.name.as_str() {
            "battery_level" => Ok(HostToolResult {
                text: "81 %".into(),
                data_json: Some(
                    r#"{"$ui": "stats", "version": 1, "items": [{"label": "Battery", "value": 81}]}"#.into(),
                ),
                is_error: false,
            }),
            "send_message" => {
                let to = args["to"].as_str().unwrap_or_default().to_owned();
                self.sent.lock().unwrap().push(to.clone());
                Ok(HostToolResult { text: format!("sent to {to}"), data_json: None, is_error: false })
            }
            other => Err(ToolCallError::Failed { reason: format!("no tool {other}") }),
        }
    }
}

fn device_tools() -> Vec<HostToolSpec> {
    vec![
        HostToolSpec {
            name: "battery_level".into(),
            description: "The battery level of the phone.".into(),
            parameters_json: r#"{"type":"object","properties":{}}"#.into(),
            timeout_ms: None,
        },
        HostToolSpec {
            name: "send_message".into(),
            description: "Send a text message.".into(),
            parameters_json: r#"{"type":"object","properties":{"to":{"type":"string"},"text":{"type":"string"}},"required":["to","text"]}"#.into(),
            timeout_ms: Some(5_000),
        },
    ]
}

/// Collects the typed events of one run and hands over the terminal one.
struct Recorder {
    events: Mutex<Vec<RunEvent>>,
    done: Mutex<mpsc::Sender<RunEvent>>,
}

impl RunListener for Recorder {
    fn on_event(&self, event: RunEvent) {
        if matches!(event, RunEvent::Paused { .. } | RunEvent::Finished { .. }) {
            self.done.lock().unwrap().send(event.clone()).unwrap();
        }
        self.events.lock().unwrap().push(event);
    }
}

fn recorder() -> (Arc<Recorder>, mpsc::Receiver<RunEvent>) {
    let (tx, rx) = mpsc::channel();
    (Arc::new(Recorder { events: Mutex::default(), done: Mutex::new(tx) }), rx)
}

fn terminal(rx: &mpsc::Receiver<RunEvent>) -> RunEvent {
    rx.recv_timeout(Duration::from_secs(20)).expect("the run ended")
}

fn workflow(provider: Arc<dyn ToolProvider>) -> Arc<WorkflowHandle> {
    let runtime = Runtime::new(CONFIG.into()).unwrap();
    runtime.register_tool_provider("device".into(), device_tools(), provider).unwrap();
    runtime.load_workflow(AGENT.into()).unwrap()
}

fn paused_run(workflow: &WorkflowHandle) -> (Arc<PausedRun>, Vec<RunEvent>) {
    let (listener, rx) = recorder();
    workflow.start(r#"{"query": "Tell Ada my battery is fine"}"#.into(), "{}".into(), listener.clone()).unwrap();
    let RunEvent::Paused { run } = terminal(&rx) else { panic!("the run should pause for approval") };
    let events = listener.events.lock().unwrap().clone();
    (run, events)
}

fn answer_of(events: &[RunEvent]) -> String {
    events
        .iter()
        .filter_map(|e| if let RunEvent::AnswerChunk { text } = e { Some(text.as_str()) } else { None })
        .collect()
}

#[test]
fn a_host_tool_runs_a_call_waits_for_approval_and_the_answer_streams() {
    let device = Arc::new(Device::default());
    let workflow = workflow(device.clone());

    let (paused, events) = paused_run(&workflow);
    assert!(matches!(&events[0], RunEvent::Started { resumed: false, .. }));
    assert!(events.iter().any(|e| matches!(e, RunEvent::ToolCalled { name, .. } if name == "battery_level")));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::ToolFinished { name, is_error: false, .. } if name == "battery_level"))
    );
    assert!(device.sent.lock().unwrap().is_empty(), "nothing is sent before approval");

    let requests = paused.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!((request.key.as_str(), &request.kind), ("assistant", &RequestKind::ToolApproval));
    assert_eq!(request.tool_calls[0].name, "send_message");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&request.tool_calls[0].arguments_json).unwrap()["to"], "Ada");

    paused.approve("assistant".into(), None).unwrap();
    let (listener, rx) = recorder();
    workflow.resume_paused(paused, String::new(), listener.clone()).unwrap();
    let RunEvent::Finished { result } = terminal(&rx) else { panic!("the resumed run should finish") };
    assert_eq!(result.status, RunStatus::Succeeded, "{result:?}");
    assert_eq!(result.answer, "Done: sent to Ada");
    let events = listener.events.lock().unwrap().clone();
    assert!(matches!(&events[0], RunEvent::Started { resumed: true, .. }));
    let chunks = events.iter().filter(|e| matches!(e, RunEvent::AnswerChunk { .. })).count();
    assert!(chunks > 1, "the answer streams in pieces, got {chunks}");
    assert_eq!(answer_of(&events), "Done: sent to Ada");
    // The agent's `ui` output carries the blocks of its tool results once it is done.
    let ui = events.iter().find_map(|e| if let RunEvent::Ui { block, .. } = e { Some(block.clone()) } else { None });
    assert_eq!(ui.map(|b| b.kind), Some("stats".to_owned()), "the tool's UI block reaches the app, checked");
    assert_eq!(*device.sent.lock().unwrap(), ["Ada"]);
    assert_eq!(device.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_rejected_call_never_reaches_the_app_and_the_model_hears_why() {
    let device = Arc::new(Device::default());
    let workflow = workflow(device.clone());
    let (paused, _) = paused_run(&workflow);
    paused.reject("assistant".into(), Some("not now".into())).unwrap();
    let (listener, rx) = recorder();
    workflow.resume_paused(paused, String::new(), listener.clone()).unwrap();
    let RunEvent::Finished { result } = terminal(&rx) else { panic!("finished") };
    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.answer, "Done: The user did not approve this tool call: not now");
    assert!(
        listener
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, RunEvent::ToolDenied { name, .. } if name == "send_message"))
    );
    assert!(device.sent.lock().unwrap().is_empty());
}

#[test]
fn a_paused_run_resumes_from_what_the_app_stored() {
    let workflow = workflow(Arc::new(Device::default()));
    let (paused, _) = paused_run(&workflow);
    let stored = paused.to_json();
    let restored = PausedRun::from_json(stored).unwrap();
    assert_eq!(restored.requests(), paused.requests());
    assert_eq!(restored.unanswered(), ["assistant"]);
    restored.decide_tool_call("assistant".into(), "message".into(), true).unwrap();
    let (listener, rx) = recorder();
    workflow.resume_paused(restored, String::new(), listener).unwrap();
    let RunEvent::Finished { result } = terminal(&rx) else { panic!("finished") };
    assert_eq!(result.answer, "Done: sent to Ada");
}

#[test]
fn the_raw_result_of_a_paused_run_reads_as_a_paused_run() {
    let workflow = workflow(Arc::new(Device::default()));
    let (listener, _rx) = recorder();
    let run = workflow.start(r#"{"query": "q"}"#.into(), "{}".into(), listener).unwrap();
    let result = run.wait_json(20_000).unwrap();
    let paused = PausedRun::from_json(result).unwrap();
    assert_eq!(paused.requests()[0].kind, RequestKind::ToolApproval);
}

/// A tool that never answers and notices when it is cancelled.
struct Stuck {
    cancelled: Arc<AtomicBool>,
}

struct OnDrop(Arc<AtomicBool>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ToolProvider for Stuck {
    async fn call_tool(&self, _call: HostToolCall) -> Result<HostToolResult, ToolCallError> {
        let _guard = OnDrop(self.cancelled.clone());
        futures::future::pending::<()>().await;
        Ok(HostToolResult::default())
    }
}

const ONE_TOOL: &str = r#"
version: "1"
graph:
  nodes:
    - { id: start, type: start }
    - id: assistant
      type: agent
      model: { provider: one, name: scripted }
      query: go
      tools: [app/*]
    - { id: reply, type: answer, answer: "{{#assistant.text#}}" }
  edges:
    - { source: start, target: assistant }
    - { source: assistant, target: reply }
"#;

fn one_tool_runtime(arguments: &str, timeout_ms: u64, provider: Arc<dyn ToolProvider>) -> Arc<WorkflowHandle> {
    let config = format!(
        r#"{{"scripted_models": {{"one": [
            {{"tool_calls": [{{"name": "work", "arguments": {arguments}}}]}},
            {{"text": "Result: {{tool_result}}"}}
        ]}}}}"#
    );
    let runtime = Runtime::new(config).unwrap();
    let spec = HostToolSpec {
        name: "work".into(),
        description: "Does the work.".into(),
        parameters_json: r#"{"type":"object","properties":{"n":{"type":"integer"}},"required":["n"]}"#.into(),
        timeout_ms: Some(timeout_ms),
    };
    runtime.register_tool_provider("app".into(), vec![spec], provider).unwrap();
    runtime.load_workflow(ONE_TOOL.into()).unwrap()
}

fn finish(workflow: &WorkflowHandle) -> (graphrt_ffi::RunResult, Vec<RunEvent>) {
    let (listener, rx) = recorder();
    workflow.start("{}".into(), "{}".into(), listener.clone()).unwrap();
    let RunEvent::Finished { result } = terminal(&rx) else { panic!("finished") };
    let events = listener.events.lock().unwrap().clone();
    (result, events)
}

#[test]
fn a_call_past_its_time_limit_is_cancelled_and_the_model_is_told() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let workflow = one_tool_runtime(r#"{"n": 1}"#, 50, Arc::new(Stuck { cancelled: cancelled.clone() }));
    let (result, events) = finish(&workflow);
    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.answer, "Result: the tool did not answer within 50 ms and was cancelled");
    assert!(events.iter().any(|e| matches!(e, RunEvent::ToolFinished { is_error: true, .. })));
    assert!(cancelled.load(Ordering::SeqCst), "the app's call was cancelled");
}

#[test]
fn an_aborted_run_cancels_the_call_in_progress() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let workflow = one_tool_runtime(r#"{"n": 1}"#, 60_000, Arc::new(Stuck { cancelled: cancelled.clone() }));
    let (listener, rx) = recorder();
    let run = workflow.start("{}".into(), "{}".into(), listener).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    run.abort("the person left".into());
    let RunEvent::Finished { result } = terminal(&rx) else { panic!("finished") };
    assert_eq!(result.status, RunStatus::Aborted);
    assert_eq!(result.error.as_deref(), Some("the person left"));
    for _ in 0..50 {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(cancelled.load(Ordering::SeqCst), "the app's call was cancelled");
}

struct Failing;

#[async_trait::async_trait]
impl ToolProvider for Failing {
    async fn call_tool(&self, _call: HostToolCall) -> Result<HostToolResult, ToolCallError> {
        Err(ToolCallError::Failed { reason: "the contacts permission is off".into() })
    }
}

#[test]
fn an_error_of_the_app_reaches_the_model_as_an_error_result() {
    let (result, events) = finish(&one_tool_runtime(r#"{"n": 1}"#, 1_000, Arc::new(Failing)));
    assert_eq!(result.answer, "Result: the contacts permission is off");
    assert!(events.iter().any(|e| matches!(e, RunEvent::ToolFinished { is_error: true, .. })));
}

#[test]
fn arguments_that_do_not_match_the_schema_never_reach_the_app() {
    let device = Arc::new(Device::default());
    let (result, _) = finish(&one_tool_runtime(r#"{"n": "one"}"#, 1_000, device.clone()));
    assert!(
        result.answer.starts_with("Result: invalid arguments: `arguments.n` must be of type integer"),
        "{}",
        result.answer
    );
    assert_eq!(device.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn provider_names_are_unique_and_declarations_checked() {
    let runtime = Runtime::new(CONFIG.into()).unwrap();
    let device: Arc<dyn ToolProvider> = Arc::new(Device::default());
    runtime.register_tool_provider("device".into(), device_tools(), device.clone()).unwrap();
    let again = runtime.register_tool_provider("device".into(), device_tools(), device.clone());
    assert!(matches!(again, Err(GraphrtError::InvalidInput { .. })));
    let bad =
        HostToolSpec { name: "x".into(), description: String::new(), parameters_json: "[]".into(), timeout_ms: None };
    assert!(runtime.register_tool_provider("other".into(), vec![bad], device).is_err());
    assert_eq!(runtime.tool_providers(), ["device"]);
}

#[test]
fn workflows_loaded_before_a_provider_keep_their_tools() {
    let runtime = Runtime::new(CONFIG.into()).unwrap();
    let before = runtime.clone().load_workflow(AGENT.into()).unwrap();
    runtime.register_tool_provider("device".into(), device_tools(), Arc::new(Device::default())).unwrap();
    let (listener, rx) = recorder();
    before.start(r#"{"query": "q"}"#.into(), "{}".into(), listener).unwrap();
    let RunEvent::Finished { result } = terminal(&rx) else { panic!("finished") };
    assert_eq!(result.status, RunStatus::Failed);
    assert!(result.error.unwrap_or_default().contains("device"), "the agent has no `device` tools");
}
