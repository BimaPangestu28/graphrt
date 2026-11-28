//! Subagents against a real model: a lead agent that delegates to two listed
//! workflows (one of them an agent whose tool needs approval), several calls
//! at once, and the approval inside a child pausing and resuming the run.
//!
//! Opt-in twice: `GRAPHRT_LIVE_PROVIDERS=1` and `OPENAI_API_KEY`.
//!
//! ```sh
//! GRAPHRT_LIVE_PROVIDERS=1 OPENAI_API_KEY=... \
//!   cargo test -p graphrt-llm --features subagents,openai --test live_subagents -- --nocapture
//! ```

#![cfg(all(feature = "subagents", feature = "openai"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
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
        println!("skipping live subagents test (set GRAPHRT_LIVE_PROVIDERS=1 and OPENAI_API_KEY to run)");
        return None;
    }
    std::env::var("OPENAI_API_KEY").ok().filter(|key| !key.is_empty())
}

fn model() -> Json {
    let name = std::env::var("GRAPHRT_LIVE_OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());
    json!({"provider": "openai", "name": name, "completion_params": {"temperature": 0}})
}

fn graph(doc: Json) -> Arc<Graph> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_model_delegates_to_listed_workflows_and_an_approval_in_a_child_pauses_the_run() {
    let Some(key) = live_key() else { return };
    // `prices`: an agent with a price list tool.
    let prices = graph(json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "product", "type": "text", "required": true}]},
        {"id": "clerk", "type": "agent", "model": model(), "instruction": "Look the price up with the tool and answer with the price only.",
         "query": "Price of {{#start.product#}}?", "tools": ["shop/price"]},
        {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["clerk", "text"]}]}],
        "edges": [{"source": "start", "target": "clerk"}, {"source": "clerk", "target": "end"}]}}));
    // `mailer`: an agent whose `shop/mail` needs approval.
    let mailer = graph(json!({"graph": {"nodes": [
        {"id": "start", "type": "start", "variables": [{"variable": "body", "type": "text", "required": true}]},
        {"id": "assistant", "type": "agent", "model": model(), "instruction": "Send the text with the mail tool, then say done.",
         "query": "{{#start.body#}}", "tools": ["shop/mail"], "require_approval": ["shop/mail"]},
        {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["assistant", "text"]}]}],
        "edges": [{"source": "start", "target": "assistant"}, {"source": "assistant", "target": "end"}]}}));
    let lead = graph(json!({"graph": {"nodes": [
        {"id": "start", "type": "start"},
        {"id": "lead", "type": "agent", "model": model(),
         "instruction": "You run a furniture shop desk. Use the prices workflow for each product (call it for all products at once), then use the mailer workflow once to send the customer a one line summary of the prices. Then answer with the summary.",
         "query": "{{#sys.query#}}",
         "subagents": [
            {"workflow": "prices", "description": "Looks up the price of one product. Input: product."},
            {"workflow": "mailer", "description": "Sends one mail to the customer. Input: body, the text of the mail."}]},
        {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["lead", "text"]}]}],
        "edges": [{"source": "start", "target": "lead"}, {"source": "lead", "target": "end"}]}}));

    let mails = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = Arc::clone(&mails);
    let price = ToolSpec {
        name: "price".into(),
        description: "The price of a product".into(),
        parameters: json!({"type": "object", "properties": {"product": {"type": "string"}}, "required": ["product"]}),
    };
    let mail = ToolSpec {
        name: "mail".into(),
        description: "Send a mail to the customer".into(),
        parameters: json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
    };
    let tools = NativeTools::new()
        .with(price, |args: Json| async move {
            let product = args.get("product").and_then(Json::as_str).unwrap_or_default().to_lowercase();
            let price = if product.contains("chair") {
                "45 USD"
            } else if product.contains("table") {
                "120 USD"
            } else {
                "unknown"
            };
            Ok(ToolOutput::text(price))
        })
        .with(mail, move |args: Json| {
            let log = Arc::clone(&log);
            async move {
                log.lock().unwrap().push(args.get("text").and_then(Json::as_str).unwrap_or_default().to_owned());
                Ok(ToolOutput::text("mail sent"))
            }
        });
    let mut registry = ToolRegistry::new();
    registry.add("shop", Arc::new(tools));
    let openai = OpenAiModel::new(OpenAiConfig::new("https://api.openai.com/v1", Some(key))).unwrap();
    let services = Services::new()
        .with(ModelRegistry::new().with("openai", Arc::new(openai) as Arc<dyn ChatModel>))
        .with(registry);
    let mut linked = IndexMap::new();
    linked.insert("prices".to_owned(), graphrt_core::engine::LinkedWorkflow::new(prices));
    linked.insert("mailer".to_owned(), graphrt_core::engine::LinkedWorkflow::new(mailer));
    let engine = Engine::new(lead).with_services(services).with_linked(linked);

    let mut input = RunInput::new();
    input.sys.insert("query".into(), Value::from("How much are a chair and a table? Mail me the prices."));
    let events: Vec<Event> = engine.run(input).collect().await;
    let child_frames: std::collections::BTreeSet<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeStarted { frame, .. } if !frame.is_empty() => Some(frame.to_string()),
            _ => None,
        })
        .collect();
    println!("child frames: {child_frames:?}");
    assert!(child_frames.iter().filter(|f| f.starts_with("lead.prices[")).count() >= 2, "two price lookups");
    let (snapshot, reasons) = match events.last() {
        Some(Event::RunPaused { snapshot, reasons, .. }) => {
            (Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(), reasons.clone())
        }
        other => panic!("expected the mail to wait for approval, got {other:?}"),
    };
    println!("paused: {:?}", reasons.iter().map(|r| (&r.frame, &r.node_id, &r.kind)).collect::<Vec<_>>());
    assert!(mails.lock().unwrap().is_empty(), "nothing sent before approval");
    let key = format!("{}/{}", reasons[0].frame, reasons[0].node_id.clone().unwrap());
    let mut inputs = IndexMap::new();
    inputs.insert(key, Value::from_json(json!({"approved": true})));
    let events: Vec<Event> = engine.run(RunInput::new().resume(snapshot, inputs)).collect().await;
    let answer = match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => outputs["text"].to_text(),
        other => panic!("expected an answer, got {other:?}"),
    };
    println!("answer: {answer}\nmail: {:?}", mails.lock().unwrap());
    assert_eq!(mails.lock().unwrap().len(), 1, "sent once");
    let mail = mails.lock().unwrap()[0].clone();
    assert!(mail.contains("45") && mail.contains("120"), "the mail has both prices: {mail}");
}
