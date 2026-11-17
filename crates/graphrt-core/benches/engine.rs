//! Engine throughput sanity benchmark (no external harness).
//!
//! Run with `cargo bench -p graphrt-core --bench engine`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stdout)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::Registry;
use graphrt_core::value::Value;
use serde_json::{Value as Json, json};

/// `start -> n1 -> n2 -> ... -> nN -> end`, each node an aggregator of its predecessor.
fn chain(len: usize) -> Json {
    let mut nodes = vec![json!({"id": "start", "type": "start", "variables": [{"variable": "x", "type": "string"}]})];
    let mut edges = Vec::new();
    let mut prev = ("start".to_owned(), "x");
    for i in 0..len {
        let id = format!("n{i}");
        nodes.push(json!({"id": id, "type": "variable-aggregator", "variables": [[prev.0, prev.1]]}));
        edges.push(json!({"source": prev.0, "target": id}));
        prev = (id, "output");
    }
    nodes.push(
        json!({"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": [prev.0, prev.1]}]}),
    );
    edges.push(json!({"source": prev.0, "target": "end"}));
    json!({"graph": {"nodes": nodes, "edges": edges}})
}

/// A parallel iteration over `items` with a small body.
fn iteration() -> Json {
    json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "items", "type": "json"}]},
            {"id": "it", "type": "iteration", "iterator_selector": ["start", "items"],
             "output_selector": ["b", "output"], "is_parallel": true, "parallel_nums": 100},
            {"id": "s", "type": "iteration-start", "parentId": "it"},
            {"id": "a", "type": "variable-aggregator", "parentId": "it", "variables": [["it", "item"]]},
            {"id": "b", "type": "variable-aggregator", "parentId": "it", "variables": [["a", "output"]]},
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["it", "output"]}]}
        ],
        "edges": [
            {"source": "start", "target": "it"}, {"source": "s", "target": "a"},
            {"source": "a", "target": "b"}, {"source": "it", "target": "end"}
        ]
    }})
}

fn build(doc: Json) -> Engine {
    let graph = Graph::build(Workflow::from_value(doc).unwrap(), &Registry::with_builtins()).unwrap();
    Engine::new(Arc::new(graph)).with_limits(Limits { max_steps: 1_000_000, ..Limits::default() })
}

fn measure(name: &str, runs: u32, rt: &tokio::runtime::Runtime, engine: &Engine, input: impl Fn() -> RunInput) {
    // Warm up once.
    let first = rt.block_on(engine.run_to_end(input()));
    assert!(matches!(first, Event::RunSucceeded { .. }), "{name}: {first:?}");
    let started = Instant::now();
    for _ in 0..runs {
        let end = rt.block_on(engine.run_to_end(input()));
        assert!(matches!(end, Event::RunSucceeded { .. }));
    }
    let per_run = started.elapsed() / runs;
    println!("{name:<40} {:>10.3} ms/run", as_ms(per_run));
}

fn as_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();

    let build_started = Instant::now();
    let chain_engine = build(chain(1000));
    println!("{:<40} {:>10.3} ms", "build 1000-node chain", as_ms(build_started.elapsed()));
    measure("run 1000-node linear chain", 20, &rt, &chain_engine, || RunInput::new().input("x", "hello"));

    let iter_engine = build(iteration());
    let items: Vec<i64> = (0..100).collect();
    let items = Value::from_json(json!(items));
    measure("run 100-item parallel iteration", 50, &rt, &iter_engine, || RunInput::new().input("items", items.clone()));
}
