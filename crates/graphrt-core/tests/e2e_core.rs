//! End-to-end tests: real workflow documents, the real engine and the built-in nodes.

#![cfg(feature = "jinja")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_core::response::ResponseAssembler;
use graphrt_core::snapshot::Snapshot;
use graphrt_core::tool::{ToolError, ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_core::value::Value;
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn graph(doc: Json) -> Arc<Graph> {
    let wf = Workflow::from_value(doc).expect("valid document");
    Arc::new(Graph::build(wf, &Registry::with_builtins()).expect("valid graph"))
}

fn engine(doc: Json) -> Engine {
    Engine::new(graph(doc))
}

async fn collect(engine: &Engine, input: RunInput) -> Vec<Event> {
    engine.run(input).collect().await
}

fn terminal(events: &[Event]) -> &Event {
    events.last().expect("at least one event")
}

fn outputs(events: &[Event]) -> IndexMap<String, Value> {
    match terminal(events) {
        Event::RunSucceeded { outputs, .. } => outputs.clone(),
        other => panic!("expected RunSucceeded, got {other:?}"),
    }
}

fn skipped(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::NodeSkipped { node_id, .. } => Some(node_id.to_string()),
            _ => None,
        })
        .collect()
}

fn json_of(v: &Value) -> Json {
    v.to_json()
}

/// `start -> ...` start node with the given variable declarations.
fn start(vars: Json) -> Json {
    json!({"id": "start", "type": "start", "variables": vars})
}

fn tt(id: &str, template: &str, vars: Json) -> Json {
    json!({"id": id, "type": "template-transform", "template": template, "variables": vars})
}

fn edge(s: &str, t: &str) -> Json {
    json!({"source": s, "target": t})
}

fn edge_h(s: &str, t: &str, h: &str) -> Json {
    json!({"source": s, "target": t, "sourceHandle": h})
}

/// Tool runtime with `double(n)` (fails for n == 2 when `fail_two`) and `slow` (sleeps).
fn math_tools(fail_two: bool) -> ToolRegistry {
    let spec = |name: &str| ToolSpec {
        name: name.into(),
        description: name.into(),
        parameters: json!({"type": "object", "properties": {"n": {"type": "number"}}}),
    };
    let tools = NativeTools::new()
        .with(spec("double"), move |args| async move {
            let n =
                args.get("n").and_then(Json::as_i64).ok_or(ToolError { tool: "double".into(), message: "n".into() })?;
            // Larger items finish first, so parallel completion order differs from input order.
            tokio::time::sleep(Duration::from_millis(u64::try_from(40 - n * 10).unwrap_or(0))).await;
            if fail_two && n == 2 {
                return Ok(ToolOutput::error("two is not allowed"));
            }
            Ok(ToolOutput {
                text: (n * 2).to_string(),
                data: Value::Int(n * 2),
                is_error: false,
                ..ToolOutput::default()
            })
        })
        .with(spec("slow"), |_| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(ToolOutput::text("late"))
        });
    let mut registry = ToolRegistry::new();
    registry.add("math", Arc::new(tools));
    registry
}

// ---------------------------------------------------------------------------
// Branching
// ---------------------------------------------------------------------------

fn branching_doc() -> Json {
    json!({"graph": {
        "nodes": [
            start(json!([{"variable": "n", "type": "number", "required": true}])),
            {"id": "check", "type": "if-else", "cases": [{
                "case_id": "big", "logical_operator": "and",
                "conditions": [{"variable_selector": ["start", "n"], "comparison_operator": ">", "value": "10"}]
            }]},
            tt("big", "big {{ n }}", json!([{"variable": "n", "value_selector": ["start", "n"]}])),
            tt("small", "small {{ n }}", json!([{"variable": "n", "value_selector": ["start", "n"]}])),
            {"id": "agg", "type": "variable-aggregator", "variables": [["big", "output"], ["small", "output"]]},
            {"id": "end", "type": "end", "outputs": [{"variable": "result", "value_selector": ["agg", "output"]}]}
        ],
        "edges": [
            edge("start", "check"), edge_h("check", "big", "big"), edge_h("check", "small", "false"),
            edge("big", "agg"), edge("small", "agg"), edge("agg", "end")
        ]
    }})
}

#[tokio::test]
async fn if_else_routes_to_one_branch_and_aggregates() {
    let e = engine(branching_doc());
    let events = collect(&e, RunInput::new().input("n", 42)).await;
    assert_eq!(json_of(&outputs(&events)["result"]), json!("big 42"));
    assert_eq!(skipped(&events), vec!["small".to_owned()]);

    let events = collect(&e, RunInput::new().input("n", 3)).await;
    assert_eq!(json_of(&outputs(&events)["result"]), json!("small 3"));
    assert_eq!(skipped(&events), vec!["big".to_owned()]);
}

#[tokio::test]
async fn number_inputs_given_as_strings_are_coerced() {
    let e = engine(branching_doc());
    let events = collect(&e, RunInput::new().input("n", "12")).await;
    assert_eq!(json_of(&outputs(&events)["result"]), json!("big 12"));
}

#[tokio::test]
async fn missing_required_input_fails_the_run() {
    let e = engine(branching_doc());
    let events = collect(&e, RunInput::new()).await;
    match terminal(&events) {
        Event::RunFailed { error, .. } => assert!(error.contains('n'), "{error}"),
        other => panic!("expected RunFailed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Iteration
// ---------------------------------------------------------------------------

fn iteration_doc(parallel: bool, mode: &str) -> Json {
    json!({"graph": {
        "nodes": [
            start(json!([{"variable": "items", "type": "json"}])),
            {"id": "it", "type": "iteration", "iterator_selector": ["start", "items"],
             "output_selector": ["call", "json"], "is_parallel": parallel, "parallel_nums": 4,
             "error_handle_mode": mode},
            {"id": "it_start", "type": "iteration-start", "parentId": "it"},
            {"id": "call", "type": "tool", "parentId": "it", "provider_id": "math", "tool_name": "double",
             "arguments": {"n": "{{#it.item#}}"}},
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["it", "output"]}]}
        ],
        "edges": [edge("start", "it"), edge("it_start", "call"), edge("it", "end")]
    }})
}

async fn run_iteration(parallel: bool, mode: &str, fail_two: bool) -> Vec<Event> {
    let e = engine(iteration_doc(parallel, mode)).with_services(Services::new().with(math_tools(fail_two)));
    collect(&e, RunInput::new().input("items", Value::from_json(json!([1, 2, 3])))).await
}

#[tokio::test]
async fn iteration_preserves_input_order_sequential_and_parallel() {
    for parallel in [false, true] {
        let events = run_iteration(parallel, "terminated", false).await;
        assert_eq!(json_of(&outputs(&events)["out"]), json!([2, 4, 6]), "parallel={parallel}");
        let item_frames: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                Event::NodeSucceeded { node_id, frame, .. } if node_id.as_ref() == "call" => Some(frame.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(item_frames.len(), 3);
        if parallel {
            // The largest item sleeps the least, so it completes first.
            assert_eq!(item_frames.first().map(String::as_str), Some("it[2]"));
        } else {
            assert_eq!(item_frames, vec!["it[0]", "it[1]", "it[2]"]);
        }
    }
}

#[tokio::test]
async fn iteration_error_modes() {
    for parallel in [false, true] {
        let events = run_iteration(parallel, "terminated", true).await;
        assert!(matches!(terminal(&events), Event::RunFailed { .. }), "parallel={parallel}");

        let events = run_iteration(parallel, "continue-on-error", true).await;
        assert_eq!(json_of(&outputs(&events)["out"]), json!([2, null, 6]), "parallel={parallel}");

        let events = run_iteration(parallel, "remove-abnormal-output", true).await;
        assert_eq!(json_of(&outputs(&events)["out"]), json!([2, 6]), "parallel={parallel}");
    }
}

#[tokio::test]
async fn iteration_over_empty_list_outputs_empty_array() {
    let e = engine(iteration_doc(true, "terminated")).with_services(Services::new().with(math_tools(false)));
    let events = collect(&e, RunInput::new().input("items", Value::from_json(json!([])))).await;
    assert_eq!(json_of(&outputs(&events)["out"]), json!([]));
}

// ---------------------------------------------------------------------------
// Loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loop_with_variables_assigner_and_break_condition() {
    let doc = json!({"graph": {
        "nodes": [
            start(json!([])),
            {"id": "lp", "type": "loop", "loop_count": 10,
             "loop_variables": [{"label": "count", "var_type": "integer", "value_type": "constant", "value": 0}],
             "logical_operator": "and",
             "break_conditions": [{"variable_selector": ["lp", "count"], "comparison_operator": "≥", "value": "3"}]},
            {"id": "lp_start", "type": "loop-start", "parentId": "lp"},
            {"id": "inc", "type": "assigner", "parentId": "lp", "items": [
                {"variable_selector": ["lp", "count"], "input_type": "constant", "operation": "+=", "value": 1}
            ]},
            {"id": "end", "type": "end", "outputs": [
                {"variable": "count", "value_selector": ["lp", "count"]},
                {"variable": "rounds", "value_selector": ["lp", "loop_round"]}
            ]}
        ],
        "edges": [edge("start", "lp"), edge("lp_start", "inc"), edge("lp", "end")]
    }});
    let events = collect(&engine(doc), RunInput::new()).await;
    let out = outputs(&events);
    assert_eq!(json_of(&out["count"]), json!(3));
    assert_eq!(json_of(&out["rounds"]), json!(3));
}

#[tokio::test]
async fn loop_end_node_breaks_the_loop() {
    let doc = json!({"graph": {
        "nodes": [
            start(json!([])),
            {"id": "lp", "type": "loop", "loop_count": 10,
             "loop_variables": [{"label": "count", "var_type": "integer", "value": 0}]},
            {"id": "lp_start", "type": "loop-start", "parentId": "lp"},
            {"id": "inc", "type": "assigner", "parentId": "lp", "items": [
                {"variable_selector": ["lp", "count"], "operation": "+=", "value": 1}
            ]},
            {"id": "check", "type": "if-else", "parentId": "lp", "cases": [{
                "case_id": "done", "logical_operator": "and",
                "conditions": [{"variable_selector": ["lp", "count"], "comparison_operator": "=", "value": "2"}]
            }]},
            {"id": "stop", "type": "loop-end", "parentId": "lp"},
            {"id": "end", "type": "end", "outputs": [{"variable": "count", "value_selector": ["lp", "count"]}]}
        ],
        "edges": [
            edge("start", "lp"), edge("lp_start", "inc"), edge("inc", "check"), edge_h("check", "stop", "done"),
            edge("lp", "end")
        ]
    }});
    let events = collect(&engine(doc), RunInput::new()).await;
    assert_eq!(json_of(&outputs(&events)["count"]), json!(2));
}

#[tokio::test]
async fn iteration_nested_inside_loop() {
    let doc = json!({"graph": {
        "nodes": [
            start(json!([{"variable": "items", "type": "json"}])),
            {"id": "lp", "type": "loop", "loop_count": 2,
             "loop_variables": [{"label": "last", "var_type": "any", "value": null}]},
            {"id": "lp_start", "type": "loop-start", "parentId": "lp"},
            {"id": "it", "type": "iteration", "parentId": "lp", "iterator_selector": ["start", "items"],
             "output_selector": ["fmt", "output"], "is_parallel": true},
            {"id": "it_start", "type": "iteration-start", "parentId": "it"},
            {"id": "fmt", "type": "template-transform", "parentId": "it", "template": "{{ r }}-{{ x }}",
             "variables": [{"variable": "x", "value_selector": ["it", "item"]},
                           {"variable": "r", "value_selector": ["lp", "index"]}]},
            {"id": "save", "type": "assigner", "parentId": "lp", "items": [
                {"variable_selector": ["lp", "last"], "input_type": "variable", "operation": "over-write",
                 "value": ["it", "output"]}
            ]},
            {"id": "end", "type": "end", "outputs": [{"variable": "last", "value_selector": ["lp", "last"]}]}
        ],
        "edges": [
            edge("start", "lp"), edge("lp_start", "it"), edge("it_start", "fmt"), edge("it", "save"),
            edge("lp", "end")
        ]
    }});
    let events = collect(&engine(doc), RunInput::new().input("items", Value::from_json(json!(["a", "b"])))).await;
    assert_eq!(json_of(&outputs(&events)["last"]), json!(["1-a", "1-b"]));
}

// ---------------------------------------------------------------------------
// Human input / pause / resume
// ---------------------------------------------------------------------------

fn approval_doc() -> Json {
    json!({
        "env": [{"name": "API_KEY", "type": "secret"}],
        "graph": {
        "nodes": [
            start(json!([{"variable": "topic", "type": "text-input"}])),
            {"id": "ask", "type": "human-input", "prompt": "Approve {{#start.topic#}}?",
             "inputs": [{"name": "comment", "type": "text"}],
             "actions": [{"id": "approve"}, {"id": "reject"}]},
            {"id": "yes", "type": "answer", "answer": "approved: {{#ask.comment#}} key={{#env.API_KEY#}}"},
            {"id": "no", "type": "answer", "answer": "rejected"}
        ],
        "edges": [edge("start", "ask"), edge_h("ask", "yes", "approve"), edge_h("ask", "no", "reject")]
    }})
}

fn paused_snapshot(events: &[Event]) -> Snapshot {
    match terminal(events) {
        Event::RunPaused { snapshot, .. } => (**snapshot).clone(),
        other => panic!("expected RunPaused, got {other:?}"),
    }
}

#[tokio::test]
async fn human_input_pause_snapshot_roundtrip_and_resume() {
    let e = engine(approval_doc());
    let secret = "sk-live-do-not-leak";
    let events = collect(&e, RunInput::new().input("topic", "launch").env("API_KEY", secret)).await;
    let reasons = match terminal(&events) {
        Event::RunPaused { reasons, .. } => reasons.clone(),
        other => panic!("expected pause, got {other:?}"),
    };
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].node_id.as_deref(), Some("ask"));
    assert!(reasons[0].data.to_text().contains("Approve launch?"), "{:?}", reasons[0].data);

    // Persist and reload the snapshot like a host would.
    let snapshot = paused_snapshot(&events);
    let text = snapshot.to_json().unwrap();
    assert!(!text.contains(secret), "secret leaked into snapshot");
    let snapshot = Snapshot::from_json(&text).unwrap();

    let mut inputs = IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from_json(json!({"action": "approve", "values": {"comment": "ship it"}})));
    let resumed = collect(&e, RunInput::new().env("API_KEY", secret).resume(snapshot.clone(), inputs)).await;
    assert!(matches!(resumed.first(), Some(Event::RunStarted { resumed: true, .. })));
    let out = outputs(&resumed);
    // The secret is usable by nodes but masked in every rendered / serialized form.
    assert_eq!(json_of(&out["answer"]), json!("approved: ship it key=******"));
    assert_eq!(skipped(&resumed), vec!["no".to_owned()]);
    for event in events.iter().chain(resumed.iter()) {
        assert!(!serde_json::to_string(event).unwrap().contains(secret), "secret leaked in {event:?}");
    }

    let mut inputs = IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from_json(json!({"action": "reject"})));
    let resumed = collect(&e, RunInput::new().resume(snapshot.clone(), inputs)).await;
    assert_eq!(json_of(&outputs(&resumed)["answer"]), json!("rejected"));

    // Resuming without input pauses again.
    let resumed = collect(&e, RunInput::new().resume(snapshot, IndexMap::new())).await;
    assert!(matches!(terminal(&resumed), Event::RunPaused { .. }));
}

#[tokio::test]
async fn human_input_inside_iteration_resumes_per_frame() {
    let doc = json!({"graph": {
        "nodes": [
            start(json!([{"variable": "items", "type": "json"}])),
            {"id": "it", "type": "iteration", "iterator_selector": ["start", "items"],
             "output_selector": ["fmt", "output"], "is_parallel": true},
            {"id": "it_start", "type": "iteration-start", "parentId": "it"},
            {"id": "ask", "type": "human-input", "parentId": "it", "inputs": [{"name": "note"}],
             "actions": [{"id": "ok"}]},
            {"id": "fmt", "type": "template-transform", "parentId": "it", "template": "{{ item }}:{{ note }}",
             "variables": [{"variable": "item", "value_selector": ["it", "item"]},
                           {"variable": "note", "value_selector": ["ask", "note"]}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["it", "output"]}]}
        ],
        "edges": [edge("start", "it"), edge("it_start", "ask"), edge_h("ask", "fmt", "ok"), edge("it", "end")]
    }});
    let e = engine(doc);
    let events = collect(&e, RunInput::new().input("items", Value::from_json(json!(["a", "b"])))).await;
    let reasons = match terminal(&events) {
        Event::RunPaused { reasons, .. } => reasons.clone(),
        other => panic!("expected pause, got {other:?}"),
    };
    let mut frames: Vec<String> = reasons.iter().map(|r| r.frame.to_string()).collect();
    frames.sort();
    assert_eq!(frames, vec!["it[0]", "it[1]"]);

    let snapshot = Snapshot::from_json(&paused_snapshot(&events).to_json().unwrap()).unwrap();
    let mut inputs = IndexMap::new();
    inputs.insert("it[0]/ask".to_owned(), Value::from_json(json!({"action": "ok", "values": {"note": "first"}})));
    inputs.insert("it[1]/ask".to_owned(), Value::from_json(json!({"action": "ok", "values": {"note": "second"}})));
    let resumed = collect(&e, RunInput::new().resume(snapshot, inputs)).await;
    assert_eq!(json_of(&outputs(&resumed)["out"]), json!(["a:first", "b:second"]));
}

#[tokio::test]
async fn external_pause_and_resume_completes() {
    let e = engine(branching_doc());
    let stream = e.run(RunInput::new().input("n", 50));
    stream.handle().pause("maintenance");
    let events: Vec<Event> = stream.collect().await;
    let snapshot = paused_snapshot(&events);
    let resumed = collect(&e, RunInput::new().resume(snapshot, IndexMap::new())).await;
    assert_eq!(json_of(&outputs(&resumed)["result"]), json!("big 50"));
}

// ---------------------------------------------------------------------------
// Variables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conversation_variable_assignment_is_visible_downstream() {
    let doc = json!({
        "conversation": [{"name": "turns", "type": "integer", "value": 1}],
        "graph": {
        "nodes": [
            start(json!([])),
            {"id": "bump", "type": "assigner", "items": [
                {"variable_selector": ["conversation", "turns"], "operation": "+=", "value": 1}
            ]},
            tt("say", "turns={{ t }}", json!([{"variable": "t", "value_selector": ["conversation", "turns"]}])),
            {"id": "end", "type": "end", "outputs": [
                {"variable": "turns", "value_selector": ["conversation", "turns"]},
                {"variable": "text", "value_selector": ["say", "output"]}
            ]}
        ],
        "edges": [edge("start", "bump"), edge("bump", "say"), edge("say", "end")]
    }});
    let e = engine(doc);
    let events = collect(&e, RunInput::new()).await;
    let out = outputs(&events);
    assert_eq!(json_of(&out["turns"]), json!(2));
    assert_eq!(json_of(&out["text"]), json!("turns=2"));
    assert!(
        events.iter().any(
            |e| matches!(e, Event::VariableUpdated { selector, .. } if selector.to_string() == "conversation.turns")
        )
    );

    // The host supplies the persisted value on the next turn.
    let mut input = RunInput::new();
    input.conversation.insert("turns".into(), Value::Int(7));
    let events = collect(&e, input).await;
    assert_eq!(json_of(&outputs(&events)["turns"]), json!(8));
}

// ---------------------------------------------------------------------------
// Error strategies
// ---------------------------------------------------------------------------

fn failing_list_doc(strategy: Json) -> Json {
    let mut lister = json!({"id": "pick", "type": "list-operator", "variable": ["start", "name"],
        "extract_by": {"enabled": true, "serial": "1"}});
    if let (Some(obj), Some(extra)) = (lister.as_object_mut(), strategy.as_object()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    json!({"graph": {
        "nodes": [
            start(json!([{"variable": "name", "type": "text-input"}])),
            lister,
            {"id": "ok", "type": "end", "outputs": [{"variable": "first", "value_selector": ["pick", "first_record"]}]},
            {"id": "bad", "type": "end", "outputs": [{"variable": "error", "value_selector": ["pick", "error_message"]}]}
        ],
        "edges": [edge("start", "pick"), edge("pick", "ok"), edge_h("pick", "bad", "fail-branch")]
    }})
}

#[tokio::test]
async fn fail_branch_routes_real_node_errors() {
    let e = engine(failing_list_doc(json!({"error_strategy": "fail-branch"})));
    let events = collect(&e, RunInput::new().input("name", "not a list")).await;
    let out = outputs(&events);
    assert!(out.contains_key("error"), "{out:?}");
    assert!(!out.contains_key("first"));
    assert!(matches!(terminal(&events), Event::RunSucceeded { handled_errors: 1, .. }));
}

#[tokio::test]
async fn default_value_continues_with_defaults() {
    let e = engine(failing_list_doc(json!({"error_strategy": "default-value",
        "default_value": [{"key": "first_record", "type": "string", "value": "fallback"}]})));
    let events = collect(&e, RunInput::new().input("name", "not a list")).await;
    let out = outputs(&events);
    assert_eq!(json_of(&out["first"]), json!("fallback"));
    assert!(!out.contains_key("error"), "fail-branch edge must be skipped: {out:?}");
}

#[tokio::test]
async fn unhandled_node_error_fails_the_run() {
    let e = engine(failing_list_doc(json!({})));
    let events = collect(&e, RunInput::new().input("name", "not a list")).await;
    assert!(matches!(terminal(&events), Event::RunFailed { .. }));
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn answers_are_assembled_in_order() {
    let doc = json!({"graph": {
        "nodes": [
            start(json!([{"variable": "name", "type": "text-input"}])),
            {"id": "hello", "type": "answer", "answer": "Hello {{#start.name#}}. "},
            tt("shout", "{{ n | upper }}!", json!([{"variable": "n", "value_selector": ["start", "name"]}])),
            {"id": "bye", "type": "answer", "answer": "{{#shout.output#}}"}
        ],
        "edges": [edge("start", "hello"), edge("hello", "shout"), edge("shout", "bye")]
    }});
    let g = graph(doc);
    let e = Engine::new(g.clone());
    let events = collect(&e, RunInput::new().input("name", "ada")).await;
    let mut assembler = ResponseAssembler::new(&g);
    let mut text = String::new();
    for event in &events {
        for chunk in assembler.push(event) {
            text.push_str(&chunk.text);
        }
    }
    for chunk in assembler.finish() {
        text.push_str(&chunk.text);
    }
    assert_eq!(text, "Hello ada. ADA!");
    assert_eq!(json_of(&outputs(&events)["answer"]), json!("Hello ada. ADA!"));
}

// ---------------------------------------------------------------------------
// Abort and tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn abort_cancels_in_flight_tool_calls() {
    let doc = json!({"graph": {
        "nodes": [
            start(json!([])),
            {"id": "wait", "type": "tool", "provider_id": "math", "tool_name": "slow", "arguments": {}},
            {"id": "end", "type": "end", "outputs": []}
        ],
        "edges": [edge("start", "wait"), edge("wait", "end")]
    }});
    let e = engine(doc).with_services(Services::new().with(math_tools(false)));
    let mut stream = e.run(RunInput::new());
    let handle = stream.handle();
    let started = Instant::now();
    let mut last = None;
    while let Some(event) = stream.next().await {
        if matches!(&event, Event::NodeStarted { node_id, .. } if node_id.as_ref() == "wait") {
            handle.abort("user cancelled");
        }
        last = Some(event);
    }
    assert!(matches!(last, Some(Event::RunAborted { ref reason, .. }) if reason == "user cancelled"), "{last:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "abort did not cancel the slow tool");
}

#[tokio::test]
async fn tool_node_runs_inside_parallel_iteration() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let tools = NativeTools::new().with(
        ToolSpec { name: "inc".into(), description: "add one".into(), parameters: json!({"type": "object"}) },
        move |args| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let n = args.get("n").and_then(Json::as_i64).unwrap_or(0);
                Ok(ToolOutput {
                    text: String::new(),
                    data: Value::Int(n + 1),
                    is_error: false,
                    ..ToolOutput::default()
                })
            }
        },
    );
    let mut registry = ToolRegistry::new();
    registry.add("util", Arc::new(tools));
    let doc = json!({"graph": {
        "nodes": [
            start(json!([{"variable": "items", "type": "json"}])),
            {"id": "it", "type": "iteration", "iterator_selector": ["start", "items"],
             "output_selector": ["call", "json"], "is_parallel": true, "parallel_nums": 3},
            {"id": "it_start", "type": "iteration-start", "parentId": "it"},
            {"id": "call", "type": "tool", "parentId": "it", "provider_id": "util", "tool_name": "inc",
             "tool_parameters": {"n": {"type": "variable", "value": ["it", "item"]}}},
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["it", "output"]}]}
        ],
        "edges": [edge("start", "it"), edge("it_start", "call"), edge("it", "end")]
    }});
    let e = engine(doc).with_services(Services::new().with(registry));
    let items: Vec<i64> = (0..20).collect();
    let events = collect(&e, RunInput::new().input("items", Value::from_json(json!(items)))).await;
    let expected: Vec<i64> = (1..21).collect();
    assert_eq!(json_of(&outputs(&events)["out"]), json!(expected));
    assert_eq!(calls.load(Ordering::SeqCst), 20);
}
