//! Regression tests for review findings: write threading, secrets across
//! snapshots, write restrictions, graph validation and clock safety.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::{Graph, GraphError};
use graphrt_core::node::{Node, NodeContext, NodeError, NodeOutput, NodeResult, Registry};
use graphrt_core::pool::Selector;
use graphrt_core::snapshot::Snapshot;
use graphrt_core::value::Value;
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

fn registry() -> Registry {
    let mut r = Registry::with_builtins();
    r.register("needs-secret", |doc| {
        let sel: Selector = serde_json::from_value(doc.config.get("secret").cloned().unwrap_or(Json::Null))
            .map_err(|e| graphrt_core::node::ConfigError::new(doc, e.to_string()))?;
        Ok(Arc::new(NeedsSecret(sel)))
    });
    r
}

/// Reads a secret's plaintext and fails if it is unavailable.
#[derive(Debug)]
struct NeedsSecret(Selector);

impl Node for NeedsSecret {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let value = ctx.get(&self.0).unwrap_or(Value::Null);
            let plain = value.require_secret().map_err(|e| NodeError::fatal("secret", e.to_string()))?;
            Ok(NodeOutput::new().with("length", Value::Int(i64::try_from(plain.len()).unwrap_or(0))).into())
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        vec![self.0.clone()]
    }
}

fn build(doc: Json) -> Result<Graph, GraphError> {
    Graph::build(Workflow::from_value(doc).unwrap(), &registry())
}

fn engine(doc: Json) -> Engine {
    Engine::new(Arc::new(build(doc).unwrap()))
}

async fn collect(engine: &Engine, input: RunInput) -> Vec<Event> {
    engine.run(input).collect().await
}

fn edge(s: &str, t: &str) -> Json {
    json!({"source": s, "target": t})
}

#[tokio::test]
async fn sequential_iteration_threads_writes_between_items() {
    let doc = json!({
        "conversation": [{"name": "log", "type": "array[string]", "value": []}],
        "graph": {"nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "items", "type": "json"}]},
            {"id": "it", "type": "iteration", "iterator_selector": ["start", "items"]},
            {"id": "it_start", "type": "iteration-start", "parentId": "it"},
            {"id": "add", "type": "assigner", "parentId": "it", "items": [
                {"variable_selector": ["conversation", "log"], "input_type": "variable",
                 "operation": "append", "value": ["it", "item"]}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "log", "value_selector": ["conversation", "log"]}]}
        ], "edges": [edge("start", "it"), edge("it_start", "add"), edge("it", "end")]}
    });
    let events = collect(&engine(doc), RunInput::new().input("items", Value::from_json(json!(["a", "b", "c"])))).await;
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => {
            assert_eq!(outputs.get("log"), Some(&Value::from_json(json!(["a", "b", "c"]))));
        }
        other => panic!("unexpected terminal event {other:?}"),
    }
}

fn secret_doc() -> Json {
    json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "key", "type": "secret", "required": true}]},
        {"id": "ask", "type": "human-input", "actions": [{"id": "go"}]},
        {"id": "use", "type": "needs-secret", "secret": ["start", "key"]},
        {"id": "end", "type": "end", "outputs": [{"variable": "len", "value_selector": ["use", "length"]}]}
    ], "edges": [edge("start", "ask"), json!({"source": "ask", "target": "use", "sourceHandle": "go"}), edge("use", "end")]}})
}

async fn pause_with_secret(engine: &Engine) -> Snapshot {
    let events = collect(engine, RunInput::new().input("key", "sk-live-123")).await;
    let Some(Event::RunPaused { snapshot, .. }) = events.last() else { panic!("expected pause: {events:?}") };
    let text = snapshot.to_json().unwrap();
    assert!(!text.contains("sk-live-123"), "plaintext secret in snapshot");
    assert!(!text.contains("******"), "mask string in snapshot");
    Snapshot::from_json(&text).unwrap()
}

#[tokio::test]
async fn redacted_secret_fails_loudly_after_resume() {
    let engine = engine(secret_doc());
    let snapshot = pause_with_secret(&engine).await;
    let mut inputs = IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from_json(json!({"action": "go", "values": {}})));
    let events = collect(&engine, RunInput::new().resume(snapshot, inputs)).await;
    match events.last() {
        Some(Event::RunFailed { error, error_type, .. }) => {
            assert!(error.contains("not available after resume"), "{error}");
            assert_eq!(error_type, "secret");
        }
        other => panic!("expected failure, got {other:?}"),
    }
}

#[tokio::test]
async fn resupplied_secret_is_used_after_resume() {
    let engine = engine(secret_doc());
    let snapshot = pause_with_secret(&engine).await;
    let mut inputs = IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from_json(json!({"action": "go", "values": {}})));
    let mut input = RunInput::new().resume(snapshot, inputs);
    if let Some(resume) = input.resume.take() {
        input.resume = Some(resume.secret("start.key", "sk-live-123"));
    }
    let events = collect(&engine, input).await;
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => assert_eq!(outputs.get("len"), Some(&Value::Int(11))),
        other => panic!("expected success, got {other:?}"),
    }
}

#[tokio::test]
async fn nodes_cannot_write_sys_or_env() {
    for target in [json!(["sys", "user_id"]), json!(["env", "KEY"])] {
        let doc = json!({"graph": {"nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "w", "type": "assigner", "items": [
                {"variable_selector": target, "input_type": "constant", "operation": "over-write", "value": "x"}]}
        ], "edges": [edge("start", "w")]}});
        let events = collect(&engine(doc), RunInput::new().sys("user_id", "u1").env("KEY", "k")).await;
        match events.last() {
            Some(Event::RunFailed { error_type, .. }) => assert_eq!(error_type, "forbidden_write"),
            other => panic!("expected failure, got {other:?}"),
        }
    }
}

#[test]
fn invalid_node_ids_are_rejected() {
    for id in ["sys", "env", "conversation", "a.b", "has space"] {
        let doc = json!({"graph": {"nodes": [{"id": id, "type": "start", "variables": []}]}});
        assert!(matches!(build(doc), Err(GraphError::InvalidNodeId { .. })), "{id}");
    }
}

#[test]
fn unknown_references_are_rejected() {
    let doc = json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": []},
        {"id": "a", "type": "answer", "answer": "{{#strat.name#}}"}
    ], "edges": [edge("start", "a")]}});
    match build(doc) {
        Err(GraphError::UnknownReference { node, namespace, .. }) => {
            assert_eq!(node, "a");
            assert_eq!(namespace, "strat");
        }
        other => panic!("expected unknown reference, got {other:?}"),
    }
    // Reserved namespaces and real nodes are fine, including in if-else right-hand sides.
    let ok = json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "x", "type": "string"}]},
        {"id": "if", "type": "if-else", "cases": [{"case_id": "t", "logical_operator": "and", "conditions": [
            {"variable_selector": ["start", "x"], "comparison_operator": "is", "value": "{{#sys.query#}}"}]}]},
        {"id": "a", "type": "answer", "answer": "{{#conversation.c#}} {{#env.E#}}"}
    ], "edges": [edge("start", "if"), json!({"source": "if", "target": "a", "sourceHandle": "t"})]}});
    assert!(build(ok).is_ok());
    let bad_rhs = json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "x", "type": "string"}]},
        {"id": "if", "type": "if-else", "cases": [{"case_id": "t", "logical_operator": "and", "conditions": [
            {"variable_selector": ["start", "x"], "comparison_operator": "is", "value": "{{#nope.v#}}"}]}]}
    ], "edges": [edge("start", "if")]}});
    assert!(matches!(build(bad_rhs), Err(GraphError::UnknownReference { .. })));
}

#[test]
fn engine_with_timeouts_runs_outside_tokio() {
    // The default (Tokio) clock must not panic without a Tokio runtime.
    let doc = json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "x", "type": "string"}], "timeout_ms": 50,
         "retry": {"max_retries": 1, "interval_ms": 10}},
        {"id": "end", "type": "end", "outputs": [{"variable": "x", "value_selector": ["start", "x"]}]}
    ], "edges": [edge("start", "end")]}});
    let limits = Limits { max_duration_ms: Some(1_000), default_node_timeout_ms: Some(100), ..Limits::default() };
    let engine = engine(doc).with_limits(limits);
    let event = futures::executor::block_on(engine.run_to_end(RunInput::new().input("x", "hi")));
    match event {
        Event::RunSucceeded { outputs, .. } => assert_eq!(outputs.get("x"), Some(&Value::from("hi"))),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn snapshot_and_workflow_fingerprint_are_stable() {
    let doc = secret_doc();
    let a = build(doc.clone()).unwrap();
    let reserialized = Workflow::from_json(&a.workflow().to_json().unwrap()).unwrap();
    let b = Graph::build(reserialized, &registry()).unwrap();
    assert_eq!(a.fingerprint(), b.fingerprint());
}
