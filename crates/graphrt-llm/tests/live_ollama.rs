//! Live tests against a local Ollama server (OpenAI-compatible endpoint).
//!
//! Skipped unless `GRAPHRT_LIVE_OLLAMA=1`. Defaults: host `http://localhost:11434`
//! (override with `OLLAMA_HOST`), model `qwen2.5:14b` (override with
//! `GRAPHRT_LIVE_MODEL`).
//!
//! ```sh
//! GRAPHRT_LIVE_OLLAMA=1 cargo test -p graphrt-llm --test live_ollama -- --nocapture --test-threads=1
//! ```
//!
//! Assertions check structure, not exact wording, so they tolerate model
//! nondeterminism.

#![cfg(feature = "openai")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

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
use graphrt_llm::ModelRegistry;
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

fn enabled() -> bool {
    let on = std::env::var("GRAPHRT_LIVE_OLLAMA").is_ok_and(|v| v == "1");
    if !on {
        println!("skipping live Ollama test (set GRAPHRT_LIVE_OLLAMA=1 to run)");
    }
    on
}

fn host() -> String {
    std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".into())
}

fn model_name() -> String {
    std::env::var("GRAPHRT_LIVE_MODEL").unwrap_or_else(|_| "qwen2.5:14b".into())
}

fn model_cfg() -> Json {
    json!({"provider": "ollama", "name": model_name(), "completion_params": {"temperature": 0}})
}

fn models() -> ModelRegistry {
    ModelRegistry::from_vars(&[("OLLAMA_HOST".to_owned(), host())]).expect("ollama provider")
}

fn graph(doc: Json) -> Arc<Graph> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

fn engine(doc: Json, tools: Option<ToolRegistry>) -> Engine {
    let mut services = Services::new().with(models());
    if let Some(tools) = tools {
        services.insert(tools);
    }
    Engine::new(graph(doc)).with_services(services)
}

async fn collect(engine: &Engine, input: RunInput) -> Vec<Event> {
    engine.run(input).collect().await
}

fn outputs(events: &[Event]) -> IndexMap<String, Value> {
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => outputs.clone(),
        other => panic!("expected RunSucceeded, got {other:?}"),
    }
}

fn node_outputs(events: &[Event], id: &str) -> IndexMap<String, Value> {
    events
        .iter()
        .find_map(|e| match e {
            Event::NodeSucceeded { node_id, outputs, .. } if node_id.as_ref() == id => Some(outputs.to_index_map()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("node `{id}` did not succeed: {:?}", events.last()))
}

fn edge(s: &str, t: &str) -> Json {
    json!({"source": s, "target": t})
}

fn edge_h(s: &str, t: &str, h: &str) -> Json {
    json!({"source": s, "target": t, "sourceHandle": h})
}

fn start() -> Json {
    json!({"id": "start", "type": "start", "variables": []})
}

fn text_of(out: &IndexMap<String, Value>, key: &str) -> String {
    out.get(key).map(Value::to_text).unwrap_or_default()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn llm_streams_tokens_and_reports_usage() {
    if !enabled() {
        return;
    }
    let e = engine(
        json!({"graph": {
            "nodes": [
                start(),
                {"id": "llm", "type": "llm", "model": model_cfg(),
                 "prompt_template": [{"role": "system", "text": "Answer in one short sentence."},
                                     {"role": "user", "text": "{{#sys.query#}}"}]},
                {"id": "out", "type": "answer", "answer": "{{#llm.text#}}"}
            ],
            "edges": [edge("start", "llm"), edge("llm", "out")]
        }}),
        None,
    );
    let started = Instant::now();
    let mut stream = e.run(RunInput::new().sys("query", "What color is the sky on a clear day?"));
    let mut events = Vec::new();
    let mut first_chunk = None;
    while let Some(event) = stream.next().await {
        if first_chunk.is_none() && matches!(event, Event::NodeChunk { .. }) {
            first_chunk = Some(started.elapsed());
        }
        events.push(event);
    }
    let total = started.elapsed();
    let chunks: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeChunk { node_id, field, chunk, .. } if node_id.as_ref() == "llm" && field.as_ref() == "text" => {
                Some(chunk.clone())
            }
            _ => None,
        })
        .collect();
    let out = node_outputs(&events, "llm");
    let text = text_of(&out, "text");
    assert!(chunks.len() > 1, "expected several streamed chunks, got {chunks:?}");
    assert_eq!(chunks.concat(), text, "streamed chunks must equal the final text");
    assert!(text.to_lowercase().contains("blue"), "{text}");
    let usage = out.get("usage").map(Value::to_json).unwrap_or(Json::Null);
    let completion = usage.get("completion_tokens").and_then(Json::as_u64).unwrap_or(0);
    assert!(completion > 0, "usage missing: {usage}");
    assert_eq!(text_of(&out, "finish_reason"), "stop");
    let secs = total.as_secs_f64();
    println!(
        "[llm] first chunk after {:?}, total {:?}, {} chunks, {} completion tokens ({:.1} tok/s)",
        first_chunk.unwrap_or_default(),
        total,
        chunks.len(),
        completion,
        f64::from(u32::try_from(completion).unwrap_or(0)) / secs
    );
    assert_eq!(text_of(&outputs(&events), "answer"), text);
}

#[tokio::test]
async fn structured_output_is_parsed() {
    if !enabled() {
        return;
    }
    let e = engine(
        json!({"graph": {
            "nodes": [
                start(),
                {"id": "llm", "type": "llm", "model": model_cfg(),
                 "prompt_template": [{"role": "user", "text": "Give the capital and population (integer, millions) of {{#sys.query#}}."}],
                 "structured_output_switch_on": true,
                 "structured_output": {"schema": {
                    "type": "object",
                    "required": ["capital", "population_millions"],
                    "properties": {"capital": {"type": "string"}, "population_millions": {"type": "integer"}}
                 }}},
                {"id": "out", "type": "end", "outputs": [{"variable": "data", "value_selector": ["llm", "structured_output"]}]}
            ],
            "edges": [edge("start", "llm"), edge("llm", "out")]
        }}),
        None,
    );
    let events = collect(&e, RunInput::new().sys("query", "France")).await;
    let data = outputs(&events).get("data").cloned().unwrap_or_default().to_json();
    println!("[structured] {data}");
    assert_eq!(data.get("capital").and_then(Json::as_str), Some("Paris"));
    assert!(data.get("population_millions").and_then(Json::as_i64).is_some());
}

#[tokio::test]
async fn classifier_routes_billing_question() {
    if !enabled() {
        return;
    }
    let e = engine(
        json!({"graph": {
            "nodes": [
                start(),
                {"id": "qc", "type": "question-classifier", "model": model_cfg(),
                 "query_variable_selector": ["sys", "query"],
                 "classes": [{"id": "billing", "name": "Billing and payments"},
                             {"id": "bug", "name": "Software bug or crash"}]},
                {"id": "billing", "type": "answer", "answer": "billing"},
                {"id": "bug", "type": "answer", "answer": "bug"}
            ],
            "edges": [edge("start", "qc"), edge_h("qc", "billing", "billing"), edge_h("qc", "bug", "bug")]
        }}),
        None,
    );
    let events = collect(&e, RunInput::new().sys("query", "My card was charged twice for the same order.")).await;
    assert_eq!(text_of(&outputs(&events), "answer"), "billing");
    let events =
        collect(&e, RunInput::new().sys("query", "The app crashes with a null pointer when I open settings.")).await;
    assert_eq!(text_of(&outputs(&events), "answer"), "bug");
}

async fn extract(mode: &str) {
    let e = engine(
        json!({"graph": {
            "nodes": [
                start(),
                {"id": "pe", "type": "parameter-extractor", "model": model_cfg(), "query": ["sys", "query"],
                 "reasoning_mode": mode,
                 "parameters": [
                    {"name": "city", "type": "string", "description": "destination city", "required": true},
                    {"name": "days", "type": "number", "description": "trip length in days", "required": true}
                 ]},
                {"id": "out", "type": "end", "outputs": [
                    {"variable": "city", "value_selector": ["pe", "city"]},
                    {"variable": "days", "value_selector": ["pe", "days"]},
                    {"variable": "ok", "value_selector": ["pe", "__is_success"]}]}
            ],
            "edges": [edge("start", "pe"), edge("pe", "out")]
        }}),
        None,
    );
    let events = collect(&e, RunInput::new().sys("query", "I want to spend 5 days in Kyoto next spring.")).await;
    let out = outputs(&events);
    println!("[extractor:{mode}] {out:?}");
    assert_eq!(out.get("ok").and_then(Value::as_i64), Some(1), "{out:?}");
    assert!(text_of(&out, "city").contains("Kyoto"));
    assert_eq!(out.get("days").and_then(Value::as_f64), Some(5.0));
}

#[tokio::test]
async fn parameter_extractor_function_call_mode() {
    if !enabled() {
        return;
    }
    extract("function_call").await;
}

#[tokio::test]
async fn parameter_extractor_prompt_mode() {
    if !enabled() {
        return;
    }
    extract("prompt").await;
}

fn tools(calls: Arc<AtomicU32>) -> ToolRegistry {
    let weather_calls = calls.clone();
    let native = NativeTools::new()
        .with(
            ToolSpec {
                name: "get_weather".into(),
                description: "Current weather for a city.".into(),
                parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
            },
            move |args| {
                let calls = weather_calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let city = args.get("city").and_then(Json::as_str).unwrap_or("?").to_owned();
                    let temp = if city.to_lowercase().contains("oslo") { 4 } else { 31 };
                    Ok(ToolOutput::text(format!("{city}: {temp} degrees C, clear")))
                }
            },
        )
        .with(
            ToolSpec {
                name: "add".into(),
                description: "Add two numbers.".into(),
                parameters: json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                                   "required": ["a", "b"]}),
            },
            move |args| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let n = |k: &str| args.get(k).and_then(Json::as_f64).unwrap_or(0.0);
                    Ok(ToolOutput::text(format!("{}", n("a") + n("b"))))
                }
            },
        );
    let mut registry = ToolRegistry::new();
    registry.add("demo", Arc::new(native));
    registry
}

fn agent_doc(extra: Json) -> Json {
    let mut agent = json!({"id": "agent", "type": "agent", "model": model_cfg(),
        "instruction": "You are a helpful assistant. Always use the tools to look up weather and to do arithmetic.",
        "query": "{{#sys.query#}}", "tools": ["demo/*"], "max_iterations": 6, "parallel_tool_calls": true});
    if let (Some(obj), Some(extra)) = (agent.as_object_mut(), extra.as_object()) {
        obj.extend(extra.clone());
    }
    json!({"graph": {
        "nodes": [start(), agent, {"id": "out", "type": "answer", "answer": "{{#agent.text#}}"}],
        "edges": [edge("start", "agent"), edge("agent", "out")]
    }})
}

#[tokio::test]
async fn agent_uses_tools_and_answers() {
    if !enabled() {
        return;
    }
    let calls = Arc::new(AtomicU32::new(0));
    let e = engine(agent_doc(json!({})), Some(tools(calls.clone())));
    let started = Instant::now();
    let events = collect(
        &e,
        RunInput::new().sys(
            "query",
            "What is the weather in Oslo and in Jakarta? Then add the two temperatures together using the add tool.",
        ),
    )
    .await;
    let elapsed = started.elapsed();
    let out = node_outputs(&events, "agent");
    let log = out.get("tool_calls").map(Value::to_json).unwrap_or(Json::Null);
    let names: Vec<&str> = log
        .as_array()
        .map(|a| a.iter().filter_map(|c| c.get("name").and_then(Json::as_str)).collect())
        .unwrap_or_default();
    println!("[agent] {elapsed:?}, iterations {:?}, calls {names:?}", out.get("iterations"));
    println!("[agent] answer: {}", text_of(&out, "text"));
    assert!(names.iter().filter(|n| n.contains("get_weather")).count() >= 2, "{log}");
    assert!(names.iter().any(|n| n.contains("add")), "{log}");
    assert_eq!(calls.load(Ordering::SeqCst) as usize, names.len());
    assert!(text_of(&out, "text").contains("35"), "answer should mention 4+31=35: {}", text_of(&out, "text"));
    assert_eq!(text_of(&out, "stopped_reason"), "completed");

    // Runtime overhead per model turn: time between a model turn finishing (tool
    // results ready) and the next model request is negligible; report steps.
    let steps = events.iter().filter(|e| matches!(e, Event::NodeProgress { name, .. } if name == "agent_step")).count();
    println!("[agent] {steps} agent_step events");
}

#[tokio::test]
async fn agent_approval_pause_and_resume() {
    if !enabled() {
        return;
    }
    let calls = Arc::new(AtomicU32::new(0));
    let e = engine(agent_doc(json!({"require_approval": ["add"]})), Some(tools(calls.clone())));
    let events = collect(&e, RunInput::new().sys("query", "Use the add tool to compute 19 + 23.")).await;
    let snapshot = match events.last() {
        Some(Event::RunPaused { snapshot, reasons }) => {
            assert_eq!(reasons[0].kind, "tool_approval");
            (**snapshot).clone()
        }
        other => panic!("expected pause, got {other:?}"),
    };
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let snapshot = Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
    let mut inputs = IndexMap::new();
    inputs.insert("agent".to_owned(), Value::from_json(json!({"approved": true})));
    let resumed = collect(&e, RunInput::new().resume(snapshot, inputs)).await;
    let answer = text_of(&outputs(&resumed), "answer");
    println!("[approval] {answer}");
    assert!(calls.load(Ordering::SeqCst) >= 1);
    assert!(answer.contains("42"), "{answer}");
}

#[tokio::test]
async fn llm_with_memory_and_context() {
    if !enabled() {
        return;
    }
    let e = engine(
        json!({"graph": {
            "nodes": [
                start(),
                {"id": "facts", "type": "template-transform", "template": "The secret project codename is BLUE HERON.",
                 "variables": []},
                {"id": "llm", "type": "llm", "model": model_cfg(),
                 "context": {"enabled": true, "variable_selector": ["facts", "output"]},
                 "memory": {"window": {"enabled": true, "size": 5}},
                 "prompt_template": [{"role": "system", "text": "Use this context when relevant: {{#context#}}"}]},
                {"id": "out", "type": "answer", "answer": "{{#llm.text#}}"}
            ],
            "edges": [edge("start", "facts"), edge("facts", "llm"), edge("llm", "out")]
        }}),
        None,
    );
    let history = Value::from_json(json!([
        {"role": "user", "content": "My name is Rania."},
        {"role": "assistant", "content": "Nice to meet you, Rania!"}
    ]));
    let input = RunInput::new()
        .sys("history", history)
        .sys("query", "What is my name, and what is the project codename? Reply in one sentence.");
    let events = collect(&e, input).await;
    let answer = text_of(&outputs(&events), "answer");
    println!("[memory+context] {answer}");
    assert!(answer.contains("Rania"), "{answer}");
    assert!(answer.to_uppercase().contains("BLUE HERON"), "{answer}");
}

#[tokio::test]
async fn iteration_runs_llm_per_item_in_parallel() {
    if !enabled() {
        return;
    }
    let e = engine(
        json!({"graph": {
            "nodes": [
                {"id": "start", "type": "start", "variables": [{"variable": "words", "type": "array[string]", "required": true}]},
                {"id": "it", "type": "iteration", "iterator_selector": ["start", "words"], "output_selector": ["up", "text"],
                 "is_parallel": true, "parallel_nums": 3},
                {"id": "it-start", "type": "iteration-start", "parentId": "it"},
                {"id": "up", "type": "llm", "parentId": "it", "model": model_cfg(),
                 "prompt_template": [{"role": "user", "text": "Reply with only the word `{{#it.item#}}` in uppercase letters, nothing else."}]},
                {"id": "out", "type": "end", "outputs": [{"variable": "result", "value_selector": ["it", "output"]}]}
            ],
            "edges": [edge("start", "it"), edge("it-start", "up"), edge("it", "out")]
        }}),
        None,
    );
    let mut input = RunInput::new();
    input.inputs.insert("words".into(), Value::from_json(json!(["apple", "river", "stone"])));
    let started = Instant::now();
    let events = collect(&e, input).await;
    let result = outputs(&events).get("result").cloned().unwrap_or_default().to_json();
    println!("[iteration] {:?} -> {result}", started.elapsed());
    let items: Vec<String> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap_or_default().trim().trim_matches('`').to_owned())
        .collect();
    assert_eq!(items, vec!["APPLE", "RIVER", "STONE"]);
}

#[tokio::test]
async fn abort_stops_streaming_promptly() {
    if !enabled() {
        return;
    }
    let e = engine(
        json!({"graph": {
            "nodes": [
                start(),
                {"id": "llm", "type": "llm", "model": model_cfg(),
                 "prompt_template": [{"role": "user", "text": "Write a 600-word essay about rivers."}]}
            ],
            "edges": [edge("start", "llm")]
        }}),
        None,
    );
    let mut stream = e.run(RunInput::new());
    let handle = stream.handle();
    let mut chunks = 0;
    let mut abort_at = None;
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        if matches!(event, Event::NodeChunk { .. }) {
            chunks += 1;
            if chunks == 5 {
                handle.abort("test");
                abort_at = Some(Instant::now());
            }
        }
        if event.is_terminal() {
            terminal = Some(event);
        }
    }
    let latency = abort_at.map(|t| t.elapsed()).unwrap_or_default();
    println!("[abort] terminal after {latency:?}, {chunks} chunks seen");
    assert!(matches!(terminal, Some(Event::RunAborted { .. })), "{terminal:?}");
    assert!(latency < Duration::from_millis(500), "abort took {latency:?}");
}

/// Runtime-only cost of one agent tool-loop step (model and tools are instant).
/// Runs without Ollama.
#[cfg(feature = "mock")]
#[tokio::test]
async fn measure_agent_step_overhead() {
    use graphrt_llm::mock::MockModel;
    const STEPS: usize = 50;
    let mut script: Vec<_> = (0..STEPS)
        .map(|i| MockModel::tool_calls(&[(format!("c{i}").as_str(), "add", r#"{"a": 1, "b": 2}"#)]))
        .collect();
    script.push(MockModel::text("done"));
    let model = Arc::new(MockModel::new(script));
    let calls = Arc::new(AtomicU32::new(0));
    let mut doc = agent_doc(json!({"max_iterations": STEPS + 1}));
    doc["graph"]["nodes"][1]["model"] = json!({"provider": "mock", "name": "m"});
    let mut services = Services::new().with(ModelRegistry::new().with("mock", model));
    services.insert(tools(calls.clone()));
    let e = Engine::new(graph(doc)).with_services(services);
    let started = Instant::now();
    let events = collect(&e, RunInput::new().sys("query", "go")).await;
    let elapsed = started.elapsed();
    assert_eq!(text_of(&outputs(&events), "answer"), "done");
    assert_eq!(calls.load(Ordering::SeqCst) as usize, STEPS);
    #[allow(clippy::cast_possible_truncation)]
    let per_step = elapsed / (STEPS as u32 + 1);
    println!("[measure] agent: {STEPS} tool steps in {elapsed:?} ({per_step:?} runtime overhead per step)");
}

/// Prints timings to compare with a raw `curl` against the same endpoint.
#[tokio::test]
async fn measure_streaming_overhead() {
    if !enabled() {
        return;
    }
    async fn timed(e: &Engine) -> (Duration, Duration, u64) {
        let started = Instant::now();
        let mut stream = e.run(RunInput::new());
        let mut first = None;
        let mut tokens = 0;
        while let Some(event) = stream.next().await {
            match &event {
                Event::NodeChunk { .. } if first.is_none() => first = Some(started.elapsed()),
                Event::NodeSucceeded { outputs, .. } => {
                    tokens = outputs
                        .get("usage")
                        .and_then(|u| u.get_path(&["completion_tokens"]))
                        .and_then(|v| v.as_i64())
                        .and_then(|v| u64::try_from(v).ok())
                        .unwrap_or(tokens);
                }
                _ => {}
            }
        }
        (first.unwrap_or_default(), started.elapsed(), tokens)
    }
    let doc = |messages: Json| {
        json!({"graph": {
            "nodes": [start(), {"id": "llm", "type": "llm", "model": model_cfg(), "prompt_template": messages}],
            "edges": [edge("start", "llm")]
        }})
    };
    let short = engine(
        doc(json!([{"role": "system", "text": "Answer in one short sentence."},
                   {"role": "user", "text": "What color is the sky on a clear day?"}])),
        None,
    );
    for i in 0..3 {
        let (ttft, total, tokens) = timed(&short).await;
        println!("[measure] short #{i}: ttft {ttft:?}, total {total:?}, {tokens} tokens");
    }
    let long = engine(doc(json!([{"role": "user", "text": "Write 200 words about rivers."}])), None);
    let (ttft, total, tokens) = timed(&long).await;
    let gen_secs = (total - ttft).as_secs_f64().max(0.001);
    #[allow(clippy::cast_precision_loss)]
    let rate = tokens as f64 / gen_secs;
    println!("[measure] long: ttft {ttft:?}, total {total:?}, {tokens} tokens, {rate:.1} tok/s after first token");
}
