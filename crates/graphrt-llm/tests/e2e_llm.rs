//! End-to-end tests: model-calling nodes running inside the real engine with a scripted model.

#![cfg(feature = "mock")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{NoTimerClock, Registry, Services};
use graphrt_core::response::ResponseAssembler;
use graphrt_core::snapshot::Snapshot;
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_core::value::Value;
use graphrt_llm::mock::MockModel;
use graphrt_llm::nodes::extractor::EXTRACT_TOOL;
use graphrt_llm::{ChatDelta, FinishReason, LlmError, ModelRegistry, Role};
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

fn graph(doc: Json) -> Arc<Graph> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

fn services(model: &Arc<MockModel>, tools: Option<ToolRegistry>) -> Services {
    let mut services = Services::new().with(ModelRegistry::new().with("mock", model.clone()));
    if let Some(tools) = tools {
        services.insert(tools);
    }
    services
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

fn answer(events: &[Event]) -> String {
    outputs(events).get("answer").map(Value::to_text).unwrap_or_default()
}

fn model_cfg() -> Json {
    json!({"provider": "mock", "name": "test-model"})
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

// ---------------------------------------------------------------------------

#[tokio::test]
async fn llm_tokens_stream_into_the_answer_in_order() {
    let model = Arc::new(MockModel::new(vec![vec![
        ChatDelta::Text("Hel".into()),
        ChatDelta::Text("lo".into()),
        ChatDelta::Text("!".into()),
        ChatDelta::Finish(FinishReason::Stop),
    ]]));
    let g = graph(json!({"graph": {
        "nodes": [
            start(),
            {"id": "intro", "type": "answer", "answer": "AI: "},
            {"id": "llm", "type": "llm", "model": model_cfg(),
             "prompt_template": [{"role": "user", "text": "{{#sys.query#}}"}]},
            {"id": "reply", "type": "answer", "answer": "{{#llm.text#}} (done)"}
        ],
        "edges": [edge("start", "intro"), edge("start", "llm"), edge("intro", "reply"), edge("llm", "reply")]
    }}));
    let engine = Engine::new(g.clone()).with_services(services(&model, None));
    let events = collect(&engine, RunInput::new().sys("query", "say hello")).await;

    let mut assembler = ResponseAssembler::new(&g);
    let mut pieces = Vec::new();
    let mut streamed_before_llm_finished = 0;
    let mut llm_finished = false;
    for event in &events {
        if matches!(event, Event::NodeSucceeded { node_id, .. } if node_id.as_ref() == "llm") {
            llm_finished = true;
        }
        for chunk in assembler.push(event) {
            if !chunk.text.is_empty() {
                if !llm_finished {
                    streamed_before_llm_finished += 1;
                }
                pieces.push(chunk.text);
            }
        }
    }
    pieces.extend(assembler.finish().into_iter().map(|c| c.text).filter(|t| !t.is_empty()));
    assert_eq!(pieces.concat(), "AI: Hello! (done)");
    assert!(streamed_before_llm_finished >= 3, "tokens were not streamed early: {pieces:?}");
    assert_eq!(answer(&events), "AI: Hello! (done)");

    let request = &model.requests()[0];
    assert_eq!(request.model, "test-model");
    assert!(format!("{:?}", request.messages).contains("say hello"));
}

#[tokio::test]
async fn question_classifier_routes_to_the_selected_branch() {
    let model = Arc::new(MockModel::new(vec![MockModel::text(r#"{"category_id": "2"}"#)]));
    let g = graph(json!({"graph": {
        "nodes": [
            start(),
            {"id": "qc", "type": "question-classifier", "model": model_cfg(),
             "query_variable_selector": ["sys", "query"],
             "classes": [{"id": "1", "name": "billing"}, {"id": "2", "name": "technical support"}]},
            {"id": "billing", "type": "answer", "answer": "billing team"},
            {"id": "support", "type": "answer", "answer": "support team ({{#qc.class_name#}})"}
        ],
        "edges": [edge("start", "qc"), edge_h("qc", "billing", "1"), edge_h("qc", "support", "2")]
    }}));
    let engine = Engine::new(g).with_services(services(&model, None));
    let events = collect(&engine, RunInput::new().sys("query", "my app crashes")).await;
    assert_eq!(answer(&events), "support team (technical support)");
    assert!(events.iter().any(|e| matches!(e, Event::NodeSkipped { node_id, .. } if node_id.as_ref() == "billing")));
}

#[tokio::test]
async fn parameter_extractor_feeds_an_if_else() {
    let model = Arc::new(MockModel::new(vec![MockModel::tool_calls(&[(
        "c1",
        EXTRACT_TOOL,
        r#"{"city": "Paris", "days": 5}"#,
    )])]));
    let g = graph(json!({"graph": {
        "nodes": [
            start(),
            {"id": "pe", "type": "parameter-extractor", "model": model_cfg(), "query": ["sys", "query"],
             "parameters": [
                {"name": "city", "type": "string", "required": true},
                {"name": "days", "type": "number", "required": true}
             ]},
            {"id": "len", "type": "if-else", "cases": [{"case_id": "long", "logical_operator": "and",
             "conditions": [{"variable_selector": ["pe", "days"], "comparison_operator": ">", "value": "3"}]}]},
            {"id": "long", "type": "answer", "answer": "long trip to {{#pe.city#}}"},
            {"id": "short", "type": "answer", "answer": "short trip"}
        ],
        "edges": [edge("start", "pe"), edge("pe", "len"), edge_h("len", "long", "long"), edge_h("len", "short", "false")]
    }}));
    let engine = Engine::new(g).with_services(services(&model, None));
    let events = collect(&engine, RunInput::new().sys("query", "5 days in Paris")).await;
    assert_eq!(answer(&events), "long trip to Paris");
}

fn sleepy_tools(calls: Arc<AtomicU32>) -> ToolRegistry {
    let spec = |name: &str| ToolSpec {
        name: name.into(),
        description: format!("{name} two numbers"),
        parameters: json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                           "required": ["a", "b"]}),
    };
    let num = |args: &Json, key: &str| args.get(key).and_then(Json::as_i64).unwrap_or(0);
    let add_calls = calls.clone();
    let tools = NativeTools::new()
        .with(spec("add"), move |args| {
            let calls = add_calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(ToolOutput::text((num(&args, "a") + num(&args, "b")).to_string()))
            }
        })
        .with(spec("mul"), move |args| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(ToolOutput::text((num(&args, "a") * num(&args, "b")).to_string()))
            }
        });
    let mut registry = ToolRegistry::new();
    registry.add("calc", Arc::new(tools));
    registry
}

fn agent_doc(extra: Json) -> Json {
    let mut agent = json!({"id": "agent", "type": "agent", "model": model_cfg(),
        "instruction": "You are a calculator.", "query": "{{#sys.query#}}", "tools": ["calc/*"]});
    if let (Some(obj), Some(extra)) = (agent.as_object_mut(), extra.as_object()) {
        obj.extend(extra.clone());
    }
    json!({"graph": {
        "nodes": [start(), agent, {"id": "out", "type": "answer", "answer": "{{#agent.text#}}"}],
        "edges": [edge("start", "agent"), edge("agent", "out")]
    }})
}

#[tokio::test]
async fn agent_runs_parallel_tool_calls_then_answers() {
    let model = Arc::new(MockModel::new(vec![
        MockModel::tool_calls(&[("c1", "add", r#"{"a": 1, "b": 2}"#), ("c2", "mul", r#"{"a": 3, "b": 4}"#)]),
        MockModel::text("1+2=3 and 3*4=12"),
    ]));
    let calls = Arc::new(AtomicU32::new(0));
    let engine =
        Engine::new(graph(agent_doc(json!({})))).with_services(services(&model, Some(sleepy_tools(calls.clone()))));
    let started = Instant::now();
    let events = collect(&engine, RunInput::new().sys("query", "compute")).await;
    let elapsed = started.elapsed();

    assert_eq!(answer(&events), "1+2=3 and 3*4=12");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(elapsed < Duration::from_millis(390), "tool calls were not parallel: {elapsed:?}");

    let log = events
        .iter()
        .find_map(|e| match e {
            Event::NodeSucceeded { node_id, outputs, .. } if node_id.as_ref() == "agent" => {
                outputs.get("tool_calls").cloned()
            }
            _ => None,
        })
        .unwrap()
        .to_json();
    assert_eq!(log.as_array().map(Vec::len), Some(2));
    let progress: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeProgress { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(progress.iter().filter(|n| *n == "tool_result").count() == 2, "{progress:?}");

    // The second model call sees both tool results.
    let second = &model.requests()[1];
    let tool_messages = second.messages.iter().filter(|m| m.role == Role::Tool).count();
    assert_eq!(tool_messages, 2);
    assert!(!second.tools.is_empty());
}

async fn approval_run(decision: Json) -> (Vec<Event>, u32, Arc<MockModel>) {
    let model = Arc::new(MockModel::new(vec![
        MockModel::tool_calls(&[("c1", "add", r#"{"a": 2, "b": 2}"#)]),
        MockModel::text("final"),
    ]));
    let calls = Arc::new(AtomicU32::new(0));
    let engine = Engine::new(graph(agent_doc(json!({"require_approval": ["add"]}))))
        .with_services(services(&model, Some(sleepy_tools(calls.clone()))));
    let events = collect(&engine, RunInput::new().sys("query", "2+2")).await;
    let snapshot = match events.last() {
        Some(Event::RunPaused { snapshot, reasons }) => {
            assert_eq!(reasons[0].kind, "tool_approval");
            assert!(reasons[0].data.to_text().contains("add"));
            (**snapshot).clone()
        }
        other => panic!("expected pause, got {other:?}"),
    };
    assert_eq!(calls.load(Ordering::SeqCst), 0, "tool ran before approval");
    let snapshot = Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
    let mut inputs = IndexMap::new();
    inputs.insert("agent".to_owned(), Value::from_json(decision));
    let resumed = collect(&engine, RunInput::new().resume(snapshot, inputs)).await;
    (resumed, calls.load(Ordering::SeqCst), model)
}

#[tokio::test]
async fn agent_tool_approval_approved() {
    let (events, calls, model) = approval_run(json!({"approved": true})).await;
    assert_eq!(answer(&events), "final");
    assert_eq!(calls, 1);
    let second = &model.requests()[1];
    let tool_result = second.messages.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(format!("{:?}", tool_result.content).contains('4'));
}

#[tokio::test]
async fn agent_tool_approval_denied() {
    let (events, calls, model) = approval_run(json!({"approved": false, "reason": "not allowed"})).await;
    assert_eq!(answer(&events), "final");
    assert_eq!(calls, 0);
    let second = &model.requests()[1];
    let tool_result = second.messages.iter().find(|m| m.role == Role::Tool).unwrap();
    assert!(format!("{:?}", tool_result.content).contains("not allowed"), "{tool_result:?}");
}

#[tokio::test]
async fn rate_limit_retry_after_is_honored() {
    let model = Arc::new(MockModel::default());
    model.push_error(LlmError::RateLimit { message: "slow down".into(), retry_after_ms: Some(1500) });
    model.push(MockModel::text("recovered"));
    let g = graph(json!({"graph": {
        "nodes": [
            start(),
            {"id": "llm", "type": "llm", "model": model_cfg(), "retry": {"max_retries": 2, "interval_ms": 10},
             "prompt_template": [{"role": "user", "text": "hi"}]},
            {"id": "out", "type": "answer", "answer": "{{#llm.text#}}"}
        ],
        "edges": [edge("start", "llm"), edge("llm", "out")]
    }}));
    let engine = Engine::new(g).with_services(services(&model, None)).with_clock(Arc::new(NoTimerClock));
    let events = collect(&engine, RunInput::new()).await;
    assert_eq!(answer(&events), "recovered");
    let delays: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeRetry { delay_ms, .. } => Some(*delay_ms),
            _ => None,
        })
        .collect();
    assert_eq!(delays, vec![1500]);
    assert!(matches!(events.last(), Some(Event::RunSucceeded { handled_errors: 0, .. })));
}

#[tokio::test]
async fn non_retryable_model_errors_fail_fast() {
    let model = Arc::new(MockModel::default());
    model.push_error(LlmError::Auth("bad key".into()));
    let g = graph(json!({"graph": {
        "nodes": [
            start(),
            {"id": "llm", "type": "llm", "model": model_cfg(), "retry": {"max_retries": 3, "interval_ms": 10},
             "prompt_template": [{"role": "user", "text": "hi"}]}
        ],
        "edges": [edge("start", "llm")]
    }}));
    let engine = Engine::new(g).with_services(services(&model, None)).with_clock(Arc::new(NoTimerClock));
    let events = collect(&engine, RunInput::new()).await;
    assert!(!events.iter().any(|e| matches!(e, Event::NodeRetry { .. })));
    assert!(matches!(events.last(), Some(Event::RunFailed { .. })));
    assert_eq!(model.requests().len(), 1);
}
