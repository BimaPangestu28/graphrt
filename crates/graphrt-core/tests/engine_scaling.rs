//! Scaling regressions: per-node scheduling cost must stay roughly constant as
//! graphs grow (no work proportional to the whole graph per completed node),
//! and commits stay all-or-nothing without copying the variable pool.
//!
//! The timing checks are `#[ignore]`d because debug builds and loaded CI hosts
//! are too noisy; run them in release:
//!
//! ```sh
//! cargo test --release -p graphrt-core --test engine_scaling -- --ignored
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Node, NodeContext, NodeOutput, NodeResult, Registry};
use graphrt_core::pool::Selector;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::value::Value;
use serde_json::{Value as Json, json};

#[derive(Debug)]
struct Noop;

impl Node for Noop {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Ok(NodeOutput::new().with("output", 1).into()) })
    }
}

/// Outputs `x`, writes `conversation.a`, then writes past the end of `conversation.list`.
#[derive(Debug)]
struct BadWriter;

impl Node for BadWriter {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async {
            Ok(NodeOutput::new()
                .with("x", 1)
                .write(Selector::parse("conversation.a").unwrap(), Value::Int(42))
                .write(Selector::parse("conversation.list.5").unwrap(), Value::Int(7))
                .into())
        })
    }
}

fn registry() -> Registry {
    let mut r = Registry::with_builtins();
    r.register("noop", |_| Ok(Arc::new(Noop) as Arc<dyn Node>));
    r.register("bad-writer", |_| Ok(Arc::new(BadWriter) as Arc<dyn Node>));
    r
}

fn engine(doc: Json) -> Engine {
    let graph = Graph::build(Workflow::from_value(doc).unwrap(), &registry()).unwrap();
    Engine::new(Arc::new(graph)).with_limits(Limits { max_steps: 10_000_000, ..Limits::default() })
}

fn edge(s: &str, t: &str) -> Json {
    json!({"source": s, "target": t})
}

/// `start -> n0 -> ... -> n(len-1) -> answer`.
fn chain(len: usize) -> Json {
    let mut nodes = vec![json!({"id": "start", "type": "start", "variables": []})];
    let mut edges = Vec::new();
    let mut prev = "start".to_owned();
    for i in 0..len {
        let id = format!("n{i}");
        nodes.push(json!({"id": id, "type": "noop"}));
        edges.push(edge(&prev, &id));
        prev = id;
    }
    nodes.push(json!({"id": "answer", "type": "answer", "answer": format!("done {{{{#{prev}.output#}}}}")}));
    edges.push(edge(&prev, "answer"));
    json!({"graph": {"nodes": nodes, "edges": edges}})
}

/// `start -> {b0..b(width-1)} -> join -> answer`.
fn fan(width: usize) -> Json {
    let mut nodes = vec![
        json!({"id": "start", "type": "start", "variables": []}),
        json!({"id": "join", "type": "noop"}),
        json!({"id": "answer", "type": "answer", "answer": "joined {{#join.output#}}"}),
    ];
    let mut edges = vec![edge("join", "answer")];
    for i in 0..width {
        let id = format!("b{i}");
        nodes.push(json!({"id": id, "type": "noop"}));
        edges.push(edge("start", &id));
        edges.push(edge(&id, "join"));
    }
    json!({"graph": {"nodes": nodes, "edges": edges}})
}

async fn run(engine: &Engine) -> Vec<Event> {
    engine.run(RunInput::new()).collect().await
}

fn answer_through_assembler(engine: &Engine, events: &[Event]) -> String {
    let mut assembler = ResponseAssembler::new(engine.graph());
    let mut text = String::new();
    for event in events {
        for chunk in assembler.push(event) {
            text.push_str(&chunk.text);
        }
    }
    for chunk in assembler.finish() {
        text.push_str(&chunk.text);
    }
    text
}

#[tokio::test]
async fn large_graphs_complete_and_assemble_correctly() {
    for (doc, nodes, expected) in [(chain(2000), 2002, "done 1"), (fan(2000), 2003, "joined 1")] {
        let e = engine(doc);
        let events = run(&e).await;
        assert!(matches!(events.last(), Some(Event::RunSucceeded { .. })), "{:?}", events.last());
        let succeeded = events.iter().filter(|ev| matches!(ev, Event::NodeSucceeded { .. })).count();
        assert_eq!(succeeded, nodes);
        assert_eq!(answer_through_assembler(&e, &events), expected);
    }
}

#[tokio::test]
async fn failed_write_rolls_back_outputs_and_earlier_writes() {
    let doc = json!({
        "conversation": [
            {"name": "a", "type": "number", "value": 0},
            {"name": "list", "type": "array[number]", "value": []}
        ],
        "graph": {
            "nodes": [
                {"id": "start", "type": "start", "variables": []},
                {"id": "bad", "type": "bad-writer", "on_error": {"strategy": "fail-branch"}},
                {"id": "end", "type": "end", "outputs": [
                    {"variable": "x", "value_selector": ["bad", "x"]},
                    {"variable": "a", "value_selector": ["conversation", "a"]},
                    {"variable": "error_type", "value_selector": ["bad", "error_type"]}
                ]}
            ],
            "edges": [
                {"source": "start", "target": "bad"},
                {"source": "bad", "target": "end", "handle": "fail-branch"}
            ]
        }
    });
    let events = run(&engine(doc)).await;
    let Some(Event::RunSucceeded { outputs, .. }) = events.last() else {
        panic!("expected success through the fail branch: {:?}", events.last());
    };
    assert_eq!(outputs.get("x"), Some(&Value::Null), "outputs of the failed commit must be rolled back");
    assert_eq!(outputs.get("a"), Some(&Value::Int(0)), "earlier writes of the failed commit must be rolled back");
    assert_eq!(outputs.get("error_type"), Some(&Value::from("invalid_write")));
    assert!(!events.iter().any(|ev| matches!(ev, Event::VariableUpdated { .. })));
}

/// Median wall time of `iters` runs of `engine`, plus assembling the answer.
fn median_run(rt: &tokio::runtime::Runtime, engine: &Engine, iters: usize) -> Duration {
    let mut samples: Vec<Duration> = (0..iters)
        .map(|_| {
            let t = Instant::now();
            let events = rt.block_on(run(engine));
            let _ = answer_through_assembler(engine, &events);
            t.elapsed()
        })
        .collect();
    samples.sort();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "timing check; run in release with --ignored"]
fn per_node_cost_does_not_grow_with_graph_size() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    for (name, build) in [("chain", chain as fn(usize) -> Json), ("fan", fan as fn(usize) -> Json)] {
        let small = engine(build(100));
        let large = engine(build(1000));
        // Warm up allocators and caches.
        let _ = median_run(&rt, &small, 20);
        let _ = median_run(&rt, &large, 3);
        let per_small = median_run(&rt, &small, 200).as_secs_f64() / 100.0;
        let per_large = median_run(&rt, &large, 30).as_secs_f64() / 1000.0;
        let ratio = per_large / per_small;
        eprintln!(
            "{name}: {:.2} us/node at 100, {:.2} us/node at 1000 (x{ratio:.2})",
            per_small * 1e6,
            per_large * 1e6
        );
        assert!(ratio < 2.5, "{name}: per-node cost grew x{ratio:.2} from 100 to 1000 nodes");
    }
}
