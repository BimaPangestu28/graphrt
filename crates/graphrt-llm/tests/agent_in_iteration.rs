//! An `agent` node inside an `iteration`: each item runs its own agent loop,
//! and a tool that needs approval pauses the run per item and resumes per item.

#![cfg(feature = "mock")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::{Event, PauseReason};
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_core::snapshot::Snapshot;
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_core::value::Value;
use graphrt_llm::ModelRegistry;
use graphrt_llm::mock::MockModel;
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

/// A workflow that runs one agent per item of `start.items`. The agent may
/// call `mail/send`, which needs approval.
fn engine(model: &Arc<MockModel>, sent: &Arc<Mutex<Vec<String>>>, parallel: bool) -> Engine {
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "items", "type": "json"}]},
            {"id": "each", "type": "iteration", "iterator_selector": ["start", "items"],
             "output_selector": ["worker", "text"], "is_parallel": parallel},
            {"id": "each_start", "type": "iteration-start", "parentId": "each"},
            {"id": "worker", "type": "agent", "parentId": "each",
             "model": {"provider": "mock", "name": "test-model"},
             "instruction": "Send one mail per customer.",
             "query": "Customer: {{#each.item#}}",
             "tools": ["mail/send"], "require_approval": ["mail/send"]},
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["each", "output"]}]}
        ],
        "edges": [
            {"source": "start", "target": "each"},
            {"source": "each_start", "target": "worker"},
            {"source": "each", "target": "end"}
        ]
    }});
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    let graph = Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap());
    let log = Arc::clone(sent);
    let spec =
        ToolSpec { name: "send".into(), description: "send a mail".into(), parameters: json!({"type": "object"}) };
    let tools = NativeTools::new().with(spec, move |args: Json| {
        let log = Arc::clone(&log);
        async move {
            let to = args.get("to").and_then(Json::as_str).unwrap_or_default().to_owned();
            log.lock().unwrap().push(to.clone());
            Ok(ToolOutput::text(format!("sent to {to}")))
        }
    });
    let mut tool_registry = ToolRegistry::new();
    tool_registry.add("mail", Arc::new(tools));
    let services = Services::new().with(ModelRegistry::new().with("mock", model.clone())).with(tool_registry);
    Engine::new(graph).with_services(services)
}

async fn run(engine: &Engine, input: RunInput) -> Vec<Event> {
    engine.run(input).collect().await
}

fn paused(events: &[Event]) -> (Snapshot, Vec<PauseReason>) {
    match events.last() {
        Some(Event::RunPaused { snapshot, reasons, .. }) => {
            // What a host stores and reads back.
            (Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(), reasons.clone())
        }
        other => panic!("expected RunPaused, got {other:?}"),
    }
}

fn output(events: &[Event]) -> Json {
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => outputs["out"].to_json(),
        other => panic!("expected RunSucceeded, got {other:?}"),
    }
}

fn items() -> RunInput {
    RunInput::new().input("items", Value::from_json(json!(["ana", "budi"])))
}

fn frames(reasons: &[PauseReason]) -> Vec<String> {
    let mut frames: Vec<String> = reasons.iter().map(|reason| reason.frame.to_string()).collect();
    frames.sort();
    frames
}

#[tokio::test]
async fn parallel_items_pause_for_approval_and_resume_per_frame() {
    // Both items ask for the tool before either gets its second turn.
    let model = Arc::new(MockModel::new(vec![
        MockModel::tool_calls(&[("call-1", "send", r#"{"to":"first"}"#)]),
        MockModel::tool_calls(&[("call-1", "send", r#"{"to":"second"}"#)]),
        MockModel::text("mail sent"),
        MockModel::text("mail sent"),
    ]));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let engine = engine(&model, &sent, true);

    let events = run(&engine, items()).await;
    let (snapshot, reasons) = paused(&events);
    assert_eq!(frames(&reasons), ["each[0]", "each[1]"]);
    for reason in &reasons {
        assert_eq!(reason.node_id.as_deref(), Some("worker"));
        assert_eq!(reason.kind, "tool_approval");
        assert_eq!(reason.data.get_path(&["calls", "0", "name"]), Some(Value::from("send")));
    }
    assert!(sent.lock().unwrap().is_empty(), "nothing is sent before approval");

    // One item is approved, the other gets no decision: the run pauses again for that one only.
    let mut inputs = IndexMap::new();
    inputs.insert("each[0]/worker".to_owned(), Value::from_json(json!({"approved": true})));
    let events = run(&engine, RunInput::new().resume(snapshot, inputs)).await;
    let (snapshot, reasons) = paused(&events);
    assert_eq!(frames(&reasons), ["each[1]"]);
    assert_eq!(sent.lock().unwrap().len(), 1);

    // The other item is denied: its agent is told and answers without the tool.
    let mut inputs = IndexMap::new();
    inputs.insert("each[1]/worker".to_owned(), Value::from_json(json!({"approved": false, "reason": "not today"})));
    let events = run(&engine, RunInput::new().resume(snapshot, inputs)).await;
    assert_eq!(output(&events), json!(["mail sent", "mail sent"]));
    assert_eq!(sent.lock().unwrap().len(), 1, "the approved call ran once, the denied one never");
    let last = model.requests().pop().unwrap();
    assert!(last.messages.iter().any(|message| message.text().contains("not today")));
    // Each item had its own conversation: no request mixes the two customers.
    for request in model.requests() {
        let text: String = request.messages.iter().map(graphrt_llm::Message::text).collect();
        assert!(!(text.contains("Customer: ana") && text.contains("Customer: budi")), "{text}");
    }
}

#[tokio::test]
async fn sequential_items_pause_one_after_another_and_each_tool_runs_once() {
    let model = Arc::new(MockModel::new(vec![
        MockModel::tool_calls(&[("call-1", "send", r#"{"to":"ana"}"#)]),
        MockModel::text("done ana"),
        MockModel::tool_calls(&[("call-1", "send", r#"{"to":"budi"}"#)]),
        MockModel::text("done budi"),
    ]));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let engine = engine(&model, &sent, false);

    let events = run(&engine, items()).await;
    let (snapshot, reasons) = paused(&events);
    assert_eq!(frames(&reasons), ["each[0]"], "the second item has not started");
    assert_eq!(model.requests().len(), 1);

    // A plain node id addresses the node in every frame.
    let approve = || {
        let mut inputs = IndexMap::new();
        inputs.insert("worker".to_owned(), Value::from_json(json!({"approved": true})));
        inputs
    };
    let events = run(&engine, RunInput::new().resume(snapshot, approve())).await;
    let (snapshot, reasons) = paused(&events);
    assert_eq!(frames(&reasons), ["each[1]"]);
    assert_eq!(sent.lock().unwrap().as_slice(), ["ana"]);

    // Resuming without a decision changes nothing and runs nothing again.
    let events = run(&engine, RunInput::new().resume(snapshot.clone(), IndexMap::new())).await;
    let (_, reasons) = paused(&events);
    assert_eq!(frames(&reasons), ["each[1]"]);
    assert_eq!(sent.lock().unwrap().as_slice(), ["ana"]);
    assert_eq!(model.requests().len(), 3, "no model turn was repeated");

    let events = run(&engine, RunInput::new().resume(snapshot, approve())).await;
    assert_eq!(output(&events), json!(["done ana", "done budi"]));
    assert_eq!(sent.lock().unwrap().as_slice(), ["ana", "budi"]);
    assert_eq!(model.requests().len(), 4);
}
