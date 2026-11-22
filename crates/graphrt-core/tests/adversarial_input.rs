//! Hostile workflow documents: deep nesting, YAML alias bombs, huge scalars and
//! loops that grow a variable exponentially. Each must fail fast with an error,
//! never overflow the stack, hang or exhaust memory.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::Registry;
use serde_json::json;

/// Generous bound for "fails fast": the hostile inputs below take seconds to
/// hours (or never finish) without their limits.
const FAST: Duration = Duration::from_secs(10);

fn assert_fast<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = f();
    assert!(started.elapsed() < FAST, "{what} took {:?}", started.elapsed());
    result
}

/// A minimal valid document with `extra` spliced in as a top-level YAML key.
fn yaml_doc_with(extra: &str) -> String {
    format!("graph:\n  nodes: []\n  edges: []\n{extra}")
}

#[test]
fn deeply_nested_json_is_a_parse_error() {
    for open in ["[", "{\"a\":"] {
        let text = format!("{{\"graph\": {}", open.repeat(200_000));
        assert_fast("Workflow::parse", || assert!(Workflow::parse(&text).is_err()));
        let validation =
            assert_fast("validate_text", || graphrt_core::validate::validate_text(&text, &Registry::with_builtins()));
        assert!(validation.has_errors());
    }
}

#[cfg(feature = "yaml")]
#[test]
fn deeply_nested_yaml_is_a_parse_error() {
    let flow = yaml_doc_with(&format!("x: {}\n", "[".repeat(200_000)));
    let mut block = String::from("graph:\n  nodes: []\n  edges: []\nx:\n");
    for depth in 1..5_000 {
        block.push_str(&" ".repeat(depth));
        block.push_str("- \n");
    }
    for text in [flow, block] {
        assert_fast("Workflow::parse", || assert!(Workflow::parse(&text).is_err()));
        let validation =
            assert_fast("validate_text", || graphrt_core::validate::validate_text(&text, &Registry::with_builtins()));
        assert!(validation.has_errors());
    }
}

#[cfg(feature = "yaml")]
#[test]
fn yaml_alias_bombs_are_refused() {
    // Billion laughs: ten levels of ten aliases each.
    let mut laughs = String::from("x:\n  l0: &l0 [lol, lol, lol, lol, lol, lol, lol, lol, lol, lol]\n");
    for level in 1..10 {
        let previous = format!("*l{}", level - 1);
        laughs.push_str(&format!("  l{level}: &l{level} [{}]\n", vec![previous; 10].join(", ")));
    }
    // One large scalar referenced many times, with dummy anchors to defeat an
    // alias-to-anchor ratio check.
    let mut scalar = format!("x:\n  big: &big {}\n", "x".repeat(200 * 1024));
    for anchor in 0..600 {
        scalar.push_str(&format!("  d{anchor}: &d{anchor} 0\n"));
    }
    scalar.push_str(&format!("  refs: [{}]\n", vec!["*big"; 5_000].join(", ")));
    for extra in [laughs, scalar] {
        let text = yaml_doc_with(&extra);
        let error = assert_fast("Workflow::parse", || Workflow::parse(&text).unwrap_err());
        assert!(error.to_string().contains("budget"), "{error}");
    }
}

#[test]
fn huge_strings_parse_in_linear_time() {
    let text = format!(r#"{{"graph": {{"nodes": [], "edges": []}}, "description": "{}"}}"#, "é".repeat(4 << 20));
    assert_fast("Workflow::parse", || Workflow::parse(&text).map(|_| ()).ok());
}

/// A loop that applies `operation` with a variable as its own input.
async fn run_self_feeding_loop(operation: &str, rounds: u64) -> Vec<Event> {
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "lp", "type": "loop", "loop_count": rounds,
             "loop_variables": [{"label": "xs", "var_type": "any", "value_type": "constant", "value": [[]]}]},
            {"id": "lp_start", "type": "loop-start", "parentId": "lp"},
            {"id": "grow", "type": "assigner", "parentId": "lp", "items": [
                {"variable_selector": ["lp", "xs"], "input_type": "variable", "operation": operation, "value": ["lp", "xs"]}
            ]},
            {"id": "end", "type": "end", "outputs": [{"variable": "xs", "value_selector": ["lp", "xs"]}]}
        ],
        "edges": [
            {"source": "start", "target": "lp"},
            {"source": "lp_start", "target": "grow"},
            {"source": "lp", "target": "end"}
        ]
    }});
    let graph = Graph::build(Workflow::from_value(doc).unwrap(), &Registry::with_builtins()).unwrap();
    Engine::new(Arc::new(graph)).run(RunInput::new()).collect().await
}

#[tokio::test]
async fn a_variable_fed_into_itself_cannot_grow_exponentially() {
    // `extend` doubles the array's real size each round (2^64 elements);
    // `append` doubles its logical size through shared children.
    for operation in ["extend", "append"] {
        let started = Instant::now();
        let events = run_self_feeding_loop(operation, 64).await;
        assert!(started.elapsed() < FAST, "{operation} took {:?}", started.elapsed());
        match events.last() {
            Some(Event::RunFailed { error, .. }) => assert!(error.contains("more than"), "{operation}: {error}"),
            other => panic!("{operation}: expected RunFailed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_variable_fed_into_itself_still_works_within_the_limits() {
    let events = run_self_feeding_loop("extend", 4).await;
    let Some(Event::RunSucceeded { outputs, .. }) = events.last() else { panic!("{:?}", events.last()) };
    assert_eq!(outputs["xs"].as_array().map(<[_]>::len), Some(16));
}
