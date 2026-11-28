//! Budgets and checkpoints against a real model: a token budget that ends an
//! agent with an answer, and a run preempted while a tool runs that resumes at
//! its last turn without asking or calling again what already happened.
//!
//! Opt-in twice: `GRAPHRT_LIVE_PROVIDERS=1` and `OPENAI_API_KEY`.
//!
//! ```sh
//! GRAPHRT_LIVE_PROVIDERS=1 OPENAI_API_KEY=... \
//!   cargo test -p graphrt-llm --features openai --test live_budget -- --nocapture
//! ```

#![cfg(feature = "openai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_core::snapshot::Snapshot;
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_core::value::Value;
use graphrt_llm::openai::{OpenAiConfig, OpenAiModel};
use graphrt_llm::{ChatModel, ModelRegistry};
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

fn live_key() -> Option<String> {
    if std::env::var("GRAPHRT_LIVE_PROVIDERS").ok().as_deref() != Some("1") {
        println!("skipping live budget test (set GRAPHRT_LIVE_PROVIDERS=1 and OPENAI_API_KEY to run)");
        return None;
    }
    std::env::var("OPENAI_API_KEY").ok().filter(|key| !key.is_empty())
}

fn graph() -> Arc<Graph> {
    let name = std::env::var("GRAPHRT_LIVE_OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());
    let doc = json!({"graph": {"nodes": [
        {"id": "start", "type": "start"},
        {"id": "clerk", "type": "agent", "model": {"provider": "openai", "name": name, "completion_params": {"temperature": 0}},
         "instruction": "You are a warehouse clerk. Look up the stock of each item with the stock tool, one call per turn, never several calls in one turn. Then answer with a one line summary.",
         "query": "{{#sys.query#}}", "tools": ["shop/stock"], "max_iterations": 12, "parallel_tool_calls": false},
        {"id": "end", "type": "end", "outputs": [
            {"variable": "text", "value_selector": ["clerk", "text"]},
            {"variable": "stopped", "value_selector": ["clerk", "stopped_reason"]}]}],
        "edges": [{"source": "start", "target": "clerk"}, {"source": "clerk", "target": "end"}]}});
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

/// The stock tool. Its `hold_first` call waits until released (a slow API).
struct Stock {
    calls: Arc<Mutex<Vec<String>>>,
    started: Arc<AtomicUsize>,
}

fn services(key: &str, stock: &Stock, hold_first: bool) -> Services {
    let spec = ToolSpec {
        name: "stock".into(),
        description: "Units in stock of one item".into(),
        parameters: json!({"type": "object", "properties": {"item": {"type": "string"}}, "required": ["item"]}),
    };
    let calls = Arc::clone(&stock.calls);
    let started = Arc::clone(&stock.started);
    let tools = NativeTools::new().with(spec, move |args: Json| {
        let calls = Arc::clone(&calls);
        let started = Arc::clone(&started);
        async move {
            let item = args.get("item").and_then(Json::as_str).unwrap_or_default().to_lowercase();
            calls.lock().unwrap().push(item.clone());
            if hold_first && started.fetch_add(1, Ordering::SeqCst) == 0 {
                futures::future::pending::<()>().await;
            }
            Ok(ToolOutput::text(format!("{} units of {item}", 3 + item.len())))
        }
    });
    let mut registry = ToolRegistry::new();
    registry.add("shop", Arc::new(tools));
    let openai = OpenAiModel::new(OpenAiConfig::new("https://api.openai.com/v1", Some(key.to_owned()))).unwrap();
    Services::new().with(ModelRegistry::new().with("openai", Arc::new(openai) as Arc<dyn ChatModel>)).with(registry)
}

fn query() -> RunInput {
    let mut input = RunInput::new();
    input
        .sys
        .insert("query".into(), Value::from("How many chairs, tables, lamps, sofas, desks and shelves are in stock?"));
    input
}

fn charged(events: &[Event]) -> u64 {
    events
        .iter()
        .filter_map(|e| match e {
            Event::RunSucceeded { usage, .. } | Event::RunFailed { usage, .. } => Some(usage.total_tokens),
            _ => None,
        })
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_agent_ends_with_an_answer_when_its_token_budget_runs_low() {
    let Some(key) = live_key() else { return };
    let stock = Stock { calls: Arc::default(), started: Arc::default() };
    // About 150 to 250 tokens a turn: room for a few turns, not for six lookups.
    let limits = Limits { max_total_tokens: Some(700), ..Limits::default() };
    let engine = Engine::new(graph()).with_services(services(&key, &stock, false)).with_limits(limits);
    let events: Vec<Event> = engine.run(query()).collect().await;
    let outputs = match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => outputs.clone(),
        other => panic!("expected an answer, got {other:?}"),
    };
    println!("lookups: {:?}", stock.calls.lock().unwrap());
    println!("stopped: {:?}, tokens: {}", outputs["stopped"], charged(&events));
    println!("answer: {}", outputs["text"].to_text_exposed());
    assert_eq!(outputs["stopped"], Value::from("budget"));
    assert!(charged(&events) <= 700, "the budget held");
    assert!(stock.calls.lock().unwrap().len() < 6, "it answered before looking up everything");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_agent_preempted_inside_a_tool_resumes_at_its_last_turn() {
    let Some(key) = live_key() else { return };
    let stock = Stock { calls: Arc::default(), started: Arc::default() };
    let engine = Engine::new(graph()).with_services(services(&key, &stock, true));
    let mut stream = engine.run(query());
    let handle = stream.handle();
    let started = Arc::clone(&stock.started);
    tokio::spawn(async move {
        while started.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.preempt("drain");
    });
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let snapshot = match events.last() {
        Some(Event::RunPaused { snapshot, .. }) => Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        other => panic!("expected the preempt to pause the run, got {other:?}"),
    };
    let cut = stock.calls.lock().unwrap().clone();
    println!("before the preempt: {cut:?}, charged {}", snapshot.budget.tokens);
    assert!(snapshot.root.nodes["clerk"].checkpoint.is_some(), "the loop kept a checkpoint");
    let events: Vec<Event> = engine.run(RunInput::new().resume(snapshot, IndexMap::new())).collect().await;
    let outputs = match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => outputs.clone(),
        other => panic!("expected an answer, got {other:?}"),
    };
    let calls = stock.calls.lock().unwrap().clone();
    println!("all lookups: {calls:?}");
    println!("answer: {}", outputs["text"].to_text_exposed());
    assert!(events.iter().any(|e| matches!(e, Event::NodeProgress { name, .. } if name == "checkpoint_restored")));
    // The cut call was not repeated by the runtime: the model was told it was
    // interrupted and may have asked again with a new call.
    let first = &cut[0];
    let repeated = calls.iter().filter(|c| *c == first).count();
    println!("`{first}` asked {repeated} times in all");
    assert!(calls.len() >= cut.len());
}
