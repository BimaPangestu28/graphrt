//! What checkpoints and budget charges cost the agent loop: 99 tool turns
//! (the conversation reaches 200 messages of about 1 KiB) run by the engine,
//! which keeps a checkpoint and charges the budget per turn, against the same
//! node run directly, which does neither. Also the cost of one charge.
//!
//! ```sh
//! cargo run --release -p graphrt-llm --features mock --example checkpoint_bench
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::budget::RunBudget;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::Usage;
use graphrt_core::graph::Graph;
use graphrt_core::node::{NoTimerClock, NodeContext, Registry, Services};
use graphrt_core::pool::VariablePool;
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_llm::ModelRegistry;
use graphrt_llm::mock::MockModel;
use serde_json::json;

const TURNS: usize = 99;

fn agent_doc() -> serde_json::Value {
    json!({"id": "agent", "type": "agent", "model": {"provider": "mock", "name": "m"}, "query": "go",
        "tools": ["native/fetch"], "max_iterations": TURNS + 1})
}

fn services() -> Services {
    let mut script: Vec<_> = (0..TURNS).map(|n| MockModel::tool_calls(&[(&format!("c{n}"), "fetch", "{}")])).collect();
    script.push(MockModel::text("done"));
    let spec = ToolSpec { name: "fetch".into(), description: "fetch".into(), parameters: json!({"type": "object"}) };
    let page = "x".repeat(1_000);
    let tools = NativeTools::new().with(spec, move |_| {
        let page = page.clone();
        async move { Ok(ToolOutput::text(page)) }
    });
    let mut registry = ToolRegistry::new();
    registry.add("native", Arc::new(tools));
    Services::new().with(ModelRegistry::new().with("mock", Arc::new(MockModel::new(script)))).with(registry)
}

fn registry() -> Registry {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    registry
}

async fn in_engine(limits: Limits) -> Duration {
    let doc = json!({"graph": {"nodes": [{"id": "start", "type": "start"}, agent_doc(),
        {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["agent", "text"]}]}],
        "edges": [{"source": "start", "target": "agent"}, {"source": "agent", "target": "end"}]}});
    let graph = Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry()).unwrap());
    let engine = Engine::new(graph).with_services(services()).with_limits(limits);
    let started = Instant::now();
    let events: Vec<_> = engine.run(RunInput::new()).collect().await;
    assert!(matches!(events.last(), Some(graphrt_core::event::Event::RunSucceeded { .. })), "{:?}", events.last());
    started.elapsed()
}

async fn direct() -> Duration {
    let mut config = agent_doc();
    if let Some(fields) = config.as_object_mut() {
        fields.remove("id");
        fields.remove("type");
    }
    let doc = NodeDoc::new("agent", "agent", config);
    let node = registry().build(&doc).unwrap();
    let ctx = NodeContext::new(
        "agent",
        "agent-exec",
        Arc::from(""),
        1,
        VariablePool::new(),
        Arc::new(services()),
        Arc::new(NoTimerClock),
        Arc::new(|_| {}),
    );
    let started = Instant::now();
    node.run(ctx).await.unwrap();
    started.elapsed()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

#[tokio::main]
async fn main() {
    let rounds = 15;
    let mut engine = Vec::new();
    let mut budgeted = Vec::new();
    let mut plain = Vec::new();
    for _ in 0..rounds {
        plain.push(direct().await);
        engine.push(in_engine(Limits::default()).await);
        budgeted.push(in_engine(Limits { max_total_tokens: Some(u64::MAX / 2), ..Limits::default() }).await);
    }
    let (plain, engine, budgeted) = (median(plain), median(engine), median(budgeted));
    let per_turn = |d: Duration| d.as_secs_f64() * 1e6 / TURNS as f64;
    println!("{TURNS} tool turns, about 1 KiB per result, conversation of {} messages", 2 * TURNS + 2);
    println!("node run directly (no checkpoints): {plain:?} ({:.1} µs per turn)", per_turn(plain));
    println!("in the engine (checkpoint per turn): {engine:?} ({:.1} µs per turn)", per_turn(engine));
    println!("in the engine with a token budget:   {budgeted:?} ({:.1} µs per turn)", per_turn(budgeted));
    println!(
        "checkpoint and charge per turn: about {:.1} µs over the direct run",
        per_turn(engine.saturating_sub(plain))
    );
    let budget = RunBudget::limited(Some(u64::MAX / 2), Some(u64::MAX / 2));
    let usage = Usage { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 };
    let charges = 1_000_000;
    let started = Instant::now();
    for _ in 0..charges {
        budget.charge(&usage, 7).unwrap();
    }
    println!("one charge: {:.1} ns", started.elapsed().as_secs_f64() * 1e9 / f64::from(charges));
}
