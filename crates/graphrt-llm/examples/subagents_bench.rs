//! What a child run costs: an agent that calls a linked two node workflow 50
//! times in one turn, against the same agent calling a native tool 50 times.
//!
//! ```sh
//! cargo run --release -p graphrt-llm --features subagents,mock --example subagents_bench
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_llm::ModelRegistry;
use graphrt_llm::mock::MockModel;
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

const CALLS: usize = 50;

fn graph(doc: Json) -> Arc<Graph> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

fn lead(agent_extra: Json) -> Arc<Graph> {
    let mut agent = json!({"id": "lead", "type": "agent", "model": {"provider": "mock", "name": "m"}, "query": "go",
        "max_subagent_calls": 64, "max_parallel_subagents": 8, "max_tool_calls_per_turn": 64});
    if let (Some(agent), Json::Object(extra)) = (agent.as_object_mut(), agent_extra) {
        agent.extend(extra);
    }
    graph(json!({"graph": {"nodes": [{"id": "start", "type": "start"}, agent,
        {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["lead", "text"]}]}],
        "edges": [{"source": "start", "target": "lead"}, {"source": "lead", "target": "end"}]}}))
}

async fn time(root: Arc<Graph>, tool: &str, linked: bool) -> Duration {
    let calls: Vec<(String, String, String)> =
        (0..CALLS).map(|n| (format!("c{n}"), tool.to_owned(), r#"{"question":"q"}"#.to_owned())).collect();
    let borrowed: Vec<(&str, &str, &str)> =
        calls.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())).collect();
    let model = Arc::new(MockModel::new(vec![MockModel::tool_calls(&borrowed), MockModel::text("done")]));
    let spec = ToolSpec { name: "echo".into(), description: "echo".into(), parameters: json!({"type": "object"}) };
    let tools = NativeTools::new().with(spec, |_args: Json| async move { Ok(ToolOutput::text("q")) });
    let mut registry = ToolRegistry::new();
    registry.add("native", Arc::new(tools));
    let services = Services::new().with(ModelRegistry::new().with("mock", model)).with(registry);
    let mut engine = Engine::new(root).with_services(services);
    if linked {
        let research = graph(json!({"graph": {"nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "question", "type": "text"}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["start", "question"]}]}],
            "edges": [{"source": "start", "target": "end"}]}}));
        let mut map = IndexMap::new();
        map.insert("research".to_owned(), graphrt_core::engine::LinkedWorkflow::new(research));
        engine = engine.with_linked(map);
    }
    let started = Instant::now();
    let events: Vec<_> = engine.run(RunInput::new()).collect().await;
    let elapsed = started.elapsed();
    assert!(matches!(events.last(), Some(graphrt_core::event::Event::RunSucceeded { .. })));
    elapsed
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut children = Vec::new();
    let mut native = Vec::new();
    for _ in 0..20 {
        children.push(
            time(lead(json!({"subagents": [{"workflow": "research", "description": "r"}]})), "research", true).await,
        );
        native.push(time(lead(json!({"tools": ["native/echo"]})), "echo", false).await);
    }
    children.sort();
    native.sort();
    let (c, n) = (children[10], native[10]);
    println!("agent turn with {CALLS} calls: as child runs {c:?}, as native tools {n:?}");
    let per = c.saturating_sub(n) / u32::try_from(CALLS).unwrap();
    println!("cost of one child run over a native tool call: {per:?}");
}
