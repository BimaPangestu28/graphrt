//! What the context guard costs: the estimate, a trim, and an agent loop
//! with the guard on and off.
//!
//! ```sh
//! cargo run --release -p graphrt-llm --features context,mock --example context_bench
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::tool::{ToolCallContext, ToolError, ToolOutput, ToolRegistry, ToolRuntime, ToolSpec};
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, Services, VariablePool};
use graphrt_llm::context::{Keep, Scope, estimate_messages, trim};
use graphrt_llm::mock::MockModel;
use graphrt_llm::nodes::agent::AgentNode;
use graphrt_llm::{ChatModel, Message, ModelRegistry, ToolCall};
use serde_json::{Value as Json, json};

/// system, the request, then `turns` tool turns with results of `chars` characters.
fn conversation(turns: usize, chars: usize) -> Vec<Message> {
    let mut messages = vec![Message::system("You are helpful."), Message::user("the request")];
    for turn in 0..turns {
        messages.push(Message {
            tool_calls: vec![ToolCall { id: format!("c{turn}"), name: "search".into(), arguments: "{}".into() }],
            ..Message::assistant("")
        });
        messages.push(Message::tool(format!("c{turn}"), "lorem ipsum ".repeat(chars / 12)));
    }
    messages
}

/// The fastest of `rounds` runs of `work`, so a busy machine does not skew it.
fn fastest<T>(rounds: usize, mut work: impl FnMut() -> T) -> Duration {
    (0..rounds)
        .map(|_| {
            let started = Instant::now();
            std::hint::black_box(work());
            started.elapsed()
        })
        .min()
        .unwrap_or_default()
}

#[derive(Debug)]
struct PageTool;

impl ToolRuntime for PageTool {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        let spec = ToolSpec { name: "page".into(), description: "fetch a page".into(), parameters: json!({}) };
        Box::pin(async move { Ok(vec![spec]) })
    }

    fn call<'a>(
        &'a self,
        _name: &'a str,
        _arguments: Json,
        _context: &'a ToolCallContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async { Ok(ToolOutput::text("lorem ipsum ".repeat(400))) })
    }
}

/// One agent run of `turns` tool turns against a scripted model.
async fn agent_run(turns: usize, context_window: Json) -> Duration {
    let mut script: Vec<_> = (0..turns).map(|n| MockModel::tool_calls(&[(&format!("p{n}"), "page", "{}")])).collect();
    script.push(MockModel::text("done"));
    let model = Arc::new(MockModel::new(script));
    let mut tools = ToolRegistry::new();
    tools.add("web", Arc::new(PageTool) as Arc<dyn ToolRuntime>);
    let services = Services::new().with(ModelRegistry::new().with("openai", model as Arc<dyn ChatModel>)).with(tools);
    let mut pool = VariablePool::new();
    pool.insert("sys", "query", Value::from("read the pages"));
    let ctx = NodeContext::for_test("agent", pool).with_services(Arc::new(services));
    let config = json!({
        "model": {"provider": "openai", "name": "gpt"},
        "tools": ["web/*"], "max_iterations": 100, "context_window": context_window,
    });
    let node = AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap();
    let started = Instant::now();
    node.run(ctx).await.unwrap();
    started.elapsed()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let megabyte = conversation(100, 10_000);
    let bytes: usize = megabyte.iter().map(|m| m.text().len()).sum();
    let estimate = fastest(200, || estimate_messages(&megabyte));
    println!("estimate, {} messages, {} KiB of text: {estimate:?}", megabyte.len(), bytes / 1024);

    let long = conversation(500, 2_000);
    let size = estimate_messages(&long);
    let keep = Keep { tool_results: 4, turns: 2 };
    let trimmed = fastest(50, || {
        let mut messages = long.clone();
        trim(&mut messages, size / 4, size / 3, keep, Scope::default())
    });
    let cloned = fastest(50, || long.clone());
    println!(
        "trim, {} messages to a third (clone of the input excluded): {:?}",
        long.len(),
        trimmed.saturating_sub(cloned)
    );

    for turns in [10, 50] {
        let mut off = Duration::MAX;
        let mut auto = Duration::MAX;
        let mut limited = Duration::MAX;
        for _ in 0..30 {
            off = off.min(agent_run(turns, json!("off")).await);
            auto = auto.min(agent_run(turns, json!("auto")).await);
            limited = limited.min(agent_run(turns, json!({"max_tokens": 1_000_000})).await);
        }
        println!(
            "agent loop, {turns} tool turns of 4.8 KiB: off {off:?}, auto {auto:?}, with a limit that is never reached {limited:?}"
        );
    }
}
