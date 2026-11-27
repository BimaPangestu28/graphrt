//! The context guard against a real model: an agent whose conversation
//! outgrows the model's context window, and an agent with a rolling summary.
//!
//! Opt-in twice, like `live_providers.rs`: `GRAPHRT_LIVE_PROVIDERS=1` and
//! `OPENAI_API_KEY`. Without them the tests print a skip line and pass.
//!
//! ```sh
//! GRAPHRT_LIVE_PROVIDERS=1 OPENAI_API_KEY=... \
//!   cargo test -p graphrt-llm --features context --test live_context -- --nocapture
//! ```
//!
//! The first test sends about half a million input tokens to a small model
//! (a few cents). The model is `gpt-4o-mini`, override with
//! `GRAPHRT_LIVE_OPENAI_MODEL` (the test expects a window of at most 200 000
//! tokens). The key is never printed.

#![cfg(all(feature = "context", feature = "openai"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::event::Event;
use graphrt_core::tool::{ToolCallContext, ToolError, ToolOutput, ToolRegistry, ToolRuntime, ToolSpec};
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, NodeOutcome, NodeOutput, Services, VariablePool};
use graphrt_llm::nodes::agent::AgentNode;
use graphrt_llm::openai::{OpenAiConfig, OpenAiModel};
use graphrt_llm::{ChatModel, ModelRegistry};
use serde_json::{Value as Json, json};

fn live_key() -> Option<String> {
    if std::env::var("GRAPHRT_LIVE_PROVIDERS").ok().as_deref() != Some("1") {
        println!("skipping live context test (set GRAPHRT_LIVE_PROVIDERS=1 and OPENAI_API_KEY to run)");
        return None;
    }
    let key = std::env::var("OPENAI_API_KEY").ok().filter(|key| !key.is_empty());
    if key.is_none() {
        println!("skipping live context test: OPENAI_API_KEY is not set");
    }
    key
}

fn model_name() -> String {
    std::env::var("GRAPHRT_LIVE_OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned())
}

/// The code hidden at the start of page `n`.
fn code(n: u64) -> String {
    format!("ZETA-{n}-{}", n * 7919 % 10_000)
}

/// Tool `page {number}`: a page that starts with its code, followed by filler.
#[derive(Debug)]
struct Pages {
    filler_chars: usize,
    served: Mutex<Vec<u64>>,
}

impl ToolRuntime for Pages {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        let spec = ToolSpec {
            name: "page".into(),
            description: "Returns one page of the report by its number.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"number": {"type": "integer"}},
                "required": ["number"]
            }),
        };
        Box::pin(async move { Ok(vec![spec]) })
    }

    fn call<'a>(
        &'a self,
        _name: &'a str,
        arguments: Json,
        _context: &'a ToolCallContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        let number = arguments.get("number").and_then(Json::as_u64).unwrap_or(0);
        self.served.lock().unwrap().push(number);
        let filler = "The quick brown fox jumps over the lazy dog. ".repeat(self.filler_chars / 45);
        Box::pin(async move {
            Ok(ToolOutput::text(format!("Page {number}. The code of this page is {}.\n{filler}", code(number))))
        })
    }
}

/// The pages that were read, each once. A model may ask for a page again
/// after its result became a placeholder, which is what placeholders are for.
fn pages_read(served: &[u64]) -> Vec<u64> {
    let mut pages = served.to_vec();
    pages.sort_unstable();
    pages.dedup();
    pages
}

struct Run {
    output: NodeOutput,
    served: Vec<u64>,
    trims: Vec<Json>,
}

/// Run one agent against the real model with the `page` tool.
async fn run_agent(key: String, filler_chars: usize, instruction: &str, extra: Json) -> Run {
    let model = OpenAiModel::new(OpenAiConfig::new("https://api.openai.com/v1", Some(key))).expect("openai model");
    let pages = Arc::new(Pages { filler_chars, served: Mutex::default() });
    let mut tools = ToolRegistry::new();
    tools.add("report", Arc::clone(&pages) as Arc<dyn ToolRuntime>);
    let services =
        Services::new().with(ModelRegistry::new().with("openai", Arc::new(model) as Arc<dyn ChatModel>)).with(tools);
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let sink = Arc::clone(&events);
    let mut pool = VariablePool::new();
    pool.insert("sys", "query", Value::from("Go."));
    let ctx = NodeContext::for_test("agent", pool)
        .with_services(Arc::new(services))
        .with_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));
    let mut config = json!({
        "model": {"provider": "openai", "name": model_name(), "completion_params": {"temperature": 0}},
        "instruction": instruction,
        "tools": ["report/page"],
        "max_iterations": 12,
        "max_tool_output_chars": 400_000,
        "stream_tool_turns": false,
    });
    if let (Some(config), Json::Object(extra)) = (config.as_object_mut(), extra) {
        config.extend(extra);
    }
    let node = AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap();
    let outcome = match node.run(ctx).await {
        Ok(outcome) => outcome,
        // Provider errors do not contain the key (it travels in a header), so they are safe to show.
        Err(error) => panic!("the agent failed: {error}"),
    };
    let NodeOutcome::Completed(output) = outcome else { panic!("the agent paused") };
    let trims = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            Event::NodeProgress { name, data, .. } if name == "context_trimmed" => Some(data.to_json()),
            _ => None,
        })
        .collect();
    let served = pages.served.lock().unwrap().clone();
    Run { output, served, trims }
}

/// Six pages of about 40 000 tokens each do not fit a 128 000 token window.
/// Without the guard the fourth or fifth turn fails with `model_context_length`.
/// With it the provider's refusal trims the conversation and the loop finishes.
#[tokio::test]
async fn a_conversation_larger_than_the_window_finishes_live() {
    let Some(key) = live_key() else { return };
    let instruction = "Read pages 1 to 6 of the report with the `page` tool, one page per turn, in order. \
                       Every page starts with its code. When you have read page 6, answer with the code of page 6 \
                       and nothing else.";
    let run = run_agent(key, 180_000, instruction, json!({})).await;
    let answer = run.output.outputs["text"].to_text();
    println!("answer: {answer}");
    println!("pages served: {:?}", run.served);
    for trim in &run.trims {
        println!("trimmed: {trim}");
    }
    println!("usage: {}", run.output.outputs["usage"].to_json());
    assert!(answer.contains(&code(6)), "the answer has the code of the last page: {answer}");
    assert_eq!(pages_read(&run.served), [1, 2, 3, 4, 5, 6], "every page was read");
    assert!(!run.trims.is_empty(), "the conversation was trimmed");
    assert_eq!(run.trims[0]["reason"], "provider_limit", "the first trim follows the provider's refusal");
    assert!(run.trims[0]["budget_tokens"].as_u64().unwrap() < 200_000, "the window was read from the refusal");
    assert!(
        run.trims.iter().skip(1).all(|trim| trim["reason"] == "budget"),
        "after the refusal the loop trims before it sends"
    );
}

/// With a small limit and `summarize`, codes of pages that were removed from
/// the conversation are still known at the end, through the rolling summary.
#[tokio::test]
async fn a_rolling_summary_keeps_what_was_removed_live() {
    let Some(key) = live_key() else { return };
    let instruction = "Read pages 1 to 5 of the report with the `page` tool, in order. Make exactly one tool call \
                       per turn: a second call in the same turn is rejected. Every page starts with its code. \
                       When you have read page 5, answer with the codes of all five pages, in order, and nothing else.";
    // One call per turn, so that earlier pages are old when the limit is reached.
    let window = json!({
        "context_window": {"max_tokens": 5_000, "summarize": true, "keep_tool_results": 1, "keep_turns": 1},
        "max_tool_calls_per_turn": 1,
    });
    let run = run_agent(key, 6_000, instruction, window).await;
    let answer = run.output.outputs["text"].to_text();
    println!("answer: {answer}");
    for trim in &run.trims {
        println!("trimmed: {trim}");
    }
    println!("usage: {}", run.output.outputs["usage"].to_json());
    assert_eq!(pages_read(&run.served), [1, 2, 3, 4, 5]);
    assert!(run.trims.iter().any(|trim| trim["summarized"] == true), "a summary was made");
    assert!(run.trims.iter().all(|trim| trim.get("summary_error").is_none()));
    for page in 1..=5 {
        assert!(answer.contains(&code(page)), "the code of page {page} survived: {answer}");
    }
}
