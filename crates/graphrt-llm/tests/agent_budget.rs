//! Token and cost budgets of a run and checkpoints of the agent loop: a retry
//! or a resumed preempt goes on from the last turn, a call that was cut is
//! not repeated unless it is safe to, and the budget ends a loop with an answer.

#![cfg(feature = "mock")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

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
use graphrt_core::tool::{ToolCallContext, ToolError, ToolOutput, ToolRegistry, ToolRuntime, ToolSpec};
use graphrt_core::value::Value;
use graphrt_llm::mock::MockModel;
use graphrt_llm::registry::Price;
use graphrt_llm::{LlmError, ModelRegistry};
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

/// Tools that record each call with its idempotency key. `slow` hangs on its
/// first call (until the attempt is cut), then answers at once.
#[derive(Debug, Default)]
struct Recorder {
    calls: Mutex<Vec<(String, Option<String>)>>,
    slow_calls: AtomicUsize,
}

impl ToolRuntime for Recorder {
    fn list(&self) -> futures::future::BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        let spec = |name: &str| ToolSpec {
            name: name.into(),
            description: name.into(),
            parameters: json!({"type": "object"}),
        };
        Box::pin(async move { Ok(vec![spec("send"), spec("lookup"), spec("slow")]) })
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        _: Json,
        context: &'a ToolCallContext,
    ) -> futures::future::BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push((name.to_owned(), context.idempotency_key.clone()));
            if name == "slow" && self.slow_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                futures::future::pending::<()>().await;
            }
            Ok(ToolOutput::text(format!("{name} done")))
        })
    }
}

impl Recorder {
    fn names(&self) -> Vec<String> {
        self.calls.lock().unwrap().iter().map(|(name, _)| name.clone()).collect()
    }
}

struct Setup {
    model: Arc<MockModel>,
    tools: Arc<Recorder>,
}

fn setup(script: Vec<Vec<graphrt_llm::ChatDelta>>) -> Setup {
    Setup { model: Arc::new(MockModel::new(script)), tools: Arc::default() }
}

fn graph(agent_extra: Json) -> Arc<Graph> {
    let mut agent = json!({"id": "agent", "type": "agent", "model": {"provider": "mock", "name": "m"},
        "query": "{{#sys.query#}}", "tools": ["box/send", "box/lookup", "box/slow"]});
    if let (Some(agent), Json::Object(extra)) = (agent.as_object_mut(), agent_extra) {
        agent.extend(extra);
    }
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start"},
            agent,
            {"id": "end", "type": "end", "outputs": [
                {"variable": "text", "value_selector": ["agent", "text"]},
                {"variable": "stopped", "value_selector": ["agent", "stopped_reason"]}
            ]}
        ],
        "edges": [{"source": "start", "target": "agent"}, {"source": "agent", "target": "end"}]
    }});
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

fn engine(s: &Setup, graph: Arc<Graph>, limits: Limits, models: Option<ModelRegistry>) -> Engine {
    let mut tools = ToolRegistry::new();
    tools.add("box", Arc::clone(&s.tools) as Arc<dyn ToolRuntime>);
    let models = models.unwrap_or_default().with("mock", s.model.clone());
    Engine::new(graph).with_services(Services::new().with(models).with(tools)).with_limits(limits)
}

fn query(text: &str) -> RunInput {
    let mut input = RunInput::new();
    input.sys.insert("query".into(), Value::from(text));
    input
}

async fn run(engine: &Engine, input: RunInput) -> Vec<Event> {
    engine.run(input).collect().await
}

fn outputs(events: &[Event]) -> Json {
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => {
            Json::Object(outputs.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
        }
        other => panic!("expected success, got {other:?}"),
    }
}

fn tool_results(request: &graphrt_llm::ChatRequest) -> Vec<String> {
    request.messages.iter().filter(|m| m.role == graphrt_llm::Role::Tool).map(|m| m.text()).collect()
}

fn rate_limited() -> LlmError {
    LlmError::RateLimit { message: "slow down".into(), retry_after_ms: Some(0) }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_a_model_error_goes_on_from_the_last_turn() {
    let s =
        setup(vec![MockModel::tool_calls(&[("c1", "send", "{}")]), MockModel::tool_calls(&[("c2", "lookup", "{}")])]);
    s.model.push_error(rate_limited());
    s.model.push(MockModel::text("done"));
    let g = graph(json!({"retry": {"max_retries": 1, "interval_ms": 0}}));
    let events = run(&engine(&s, g, Limits::default(), None), query("go")).await;
    assert_eq!(outputs(&events)["text"], "done");
    // Turns 1 and 2, the failed turn 3 and turn 3 again: a fresh loop would ask 6 times.
    assert_eq!(s.model.requests().len(), 4);
    assert_eq!(s.tools.names(), ["send", "lookup"], "no tool ran twice");
    let last = s.model.requests().pop().unwrap();
    assert_eq!(tool_results(&last), ["send done", "lookup done"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_cut_by_a_timeout_is_not_repeated_unless_it_is_safe_to() {
    for (safe, expected_calls, result) in [
        (
            json!([]),
            vec!["slow"],
            "interrupted: this call was started and its outcome is unknown. It was not repeated.",
        ),
        (json!(["box/slow"]), vec!["slow", "slow"], "slow done"),
    ] {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "slow", "{}")]), MockModel::text("answered")]);
        let g =
            graph(json!({"retry": {"max_retries": 1, "interval_ms": 0}, "timeout_ms": 200, "retry_safe_tools": safe}));
        let events = run(&engine(&s, g, Limits::default(), None), query("go")).await;
        assert_eq!(outputs(&events)["text"], "answered");
        assert_eq!(s.tools.names(), expected_calls);
        assert_eq!(s.model.requests().len(), 2, "the model turn was not asked again");
        assert_eq!(tool_results(&s.model.requests()[1]), [result]);
        // Every attempt of one call has the same idempotency key.
        let keys: Vec<Option<String>> = s.tools.calls.lock().unwrap().iter().map(|(_, key)| key.clone()).collect();
        assert!(keys.iter().all(|k| k.as_ref().is_some_and(|k| k.ends_with(":c1"))), "{keys:?}");
        assert!(keys.windows(2).all(|w| w[0] == w[1]), "{keys:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preempted_loop_resumes_at_its_last_turn() {
    let s = setup(vec![
        MockModel::tool_calls(&[("c1", "send", "{}")]),
        MockModel::tool_calls(&[("c2", "slow", "{}")]),
        MockModel::text("finished"),
    ]);
    let engine = engine(&s, graph(json!({})), Limits::default(), None);
    let mut stream = engine.run(query("go"));
    let handle = stream.handle();
    let mut events = Vec::new();
    let mut asked = false;
    while let Some(event) = stream.next().await {
        if !asked && s.tools.slow_calls.load(Ordering::SeqCst) > 0 {
            asked = true;
            handle.preempt("drain");
        }
        if !asked && matches!(&event, Event::NodeProgress { name, .. } if name == "tool_call") {
            let handle = handle.clone();
            let tools = Arc::clone(&s.tools);
            tokio::spawn(async move {
                while tools.slow_calls.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                handle.preempt("drain");
            });
        }
        events.push(event);
    }
    let snapshot = match events.last() {
        Some(Event::RunPaused { snapshot, .. }) => Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        other => panic!("expected a pause, got {other:?}"),
    };
    assert!(snapshot.root.nodes["agent"].checkpoint.is_some());
    assert_eq!(snapshot.budget.tokens, 4, "two turns charged");
    let events = run(&engine, RunInput::new().resume(snapshot, IndexMap::new())).await;
    assert_eq!(outputs(&events)["text"], "finished");
    assert_eq!(s.model.requests().len(), 3, "no turn was asked twice");
    assert_eq!(s.tools.names(), ["send", "slow"], "send ran once, the cut slow call was not repeated");
    assert!(events.iter().any(|e| matches!(e, Event::NodeProgress { name, .. } if name == "checkpoint_restored")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_budget_ends_the_loop_with_an_answer() {
    let s = setup(vec![
        MockModel::tool_calls(&[("c1", "lookup", "{}")]),
        MockModel::tool_calls(&[("c2", "lookup", "{}")]),
        MockModel::text("from what I have"),
    ]);
    // Turns use 2 tokens each: after one turn, 3 left do not afford a tool turn and an answer.
    let limits = Limits { max_total_tokens: Some(5), ..Limits::default() };
    let events = run(&engine(&s, graph(json!({})), limits, None), query("go")).await;
    let out = outputs(&events);
    assert_eq!(out["stopped"], "budget");
    assert_eq!(s.model.requests().len(), 2);
    assert!(s.model.requests()[1].tool_choice == Some(graphrt_llm::ToolChoice::None), "the last turn calls no tool");
    // A run without room for even one turn fails at the limit.
    let s = setup(vec![MockModel::tool_calls(&[("c1", "lookup", "{}")]), MockModel::text("never")]);
    let limits = Limits { max_total_tokens: Some(1), ..Limits::default() };
    let events = run(&engine(&s, graph(json!({})), limits, None), query("go")).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, .. }) if error_type == "limit_exceeded"),
        "{:?}",
        events.last()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cost_follows_the_prices_and_the_counters_survive_an_approval() {
    let s = setup(vec![
        MockModel::tool_calls(&[("c1", "send", "{}")]),
        MockModel::tool_calls(&[("c2", "lookup", "{}")]),
        MockModel::text("done"),
    ]);
    // 1 prompt and 1 completion token per turn: 1 000 + 3 000 micro units.
    let mut prices = ModelRegistry::new();
    prices.set_price("mock", "m", Price::new(1_000.0, 3_000.0).unwrap());
    // 4 000 a turn: after the approval, 7 000 left do not afford a tool turn
    // and an answer, so turn 2 answers. A counter reset would let it call `lookup`.
    let limits = Limits { max_cost_micros: Some(11_000), ..Limits::default() };
    let g = graph(json!({"require_approval": ["box/send"]}));
    let engine = engine(&s, g, limits, Some(prices));
    let events = run(&engine, query("go")).await;
    let snapshot = match events.last() {
        Some(Event::RunPaused { snapshot, .. }) => Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        other => panic!("expected a pause, got {other:?}"),
    };
    assert_eq!(snapshot.budget.cost_micros, 4_000);
    let mut approve = IndexMap::new();
    approve.insert("agent".to_owned(), Value::from_json(json!({"approved": true})));
    let events = run(&engine, RunInput::new().resume(snapshot, approve)).await;
    let usage = match events.last() {
        Some(Event::RunSucceeded { usage, .. }) => *usage,
        other => panic!("expected success, got {other:?}"),
    };
    assert_eq!(usage.total_tokens, 4, "two turns");
    assert_eq!(outputs(&events)["stopped"], "budget");
    // A tighter budget: the third turn would pass 8 000.
    let s = setup(vec![
        MockModel::tool_calls(&[("c1", "lookup", "{}")]),
        MockModel::tool_calls(&[("c2", "lookup", "{}")]),
        MockModel::text("done"),
    ]);
    let mut prices = ModelRegistry::new();
    prices.set_price("mock", "m", Price::new(1_000.0, 3_000.0).unwrap());
    let limits = Limits { max_cost_micros: Some(8_500), ..Limits::default() };
    let events = run(&engine_for_cost(&s, limits, prices), query("go")).await;
    assert_eq!(outputs(&events)["stopped"], "budget");
}

fn engine_for_cost(s: &Setup, limits: Limits, prices: ModelRegistry) -> Engine {
    engine(s, graph(json!({})), limits, Some(prices))
}

fn llm_graph() -> Arc<Graph> {
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start"},
            {"id": "write", "type": "llm", "model": {"provider": "mock", "name": "m"},
             "prompt_template": [{"role": "user", "text": "{{#sys.query#}}"}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["write", "text"]}]}
        ],
        "edges": [{"source": "start", "target": "write"}, {"source": "write", "target": "end"}]
    }});
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_llm_node_is_charged_its_cost_and_a_failed_call_is_paid_for() {
    let mut prices = ModelRegistry::new();
    prices.set_price("mock", "m", Price::new(1_000.0, 3_000.0).unwrap());
    // One call: 4 000 micro units, over a 1 000 limit.
    let s = setup(vec![MockModel::text("a poem")]);
    let limits = Limits { max_cost_micros: Some(1_000), ..Limits::default() };
    let events = run(&engine(&s, llm_graph(), limits, Some(prices.clone())), query("write")).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, error, .. }) if error_type == "limit_exceeded" && error.contains("cost budget")),
        "{:?}",
        events.last()
    );
    // A model without a price under a cost budget is refused before it is called:
    // its cost could not be counted.
    let s = setup(vec![MockModel::text("a poem")]);
    let limits = Limits { max_cost_micros: Some(1_000), ..Limits::default() };
    let events = run(&engine(&s, llm_graph(), limits, None), query("write")).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, .. }) if error_type == "price_missing"),
        "{:?}",
        events.last()
    );
    assert!(s.model.requests().is_empty(), "nothing was paid for");
    // Without a cost budget a model needs no price.
    let s = setup(vec![MockModel::text("a poem")]);
    let events = run(&engine(&s, llm_graph(), Limits::default(), None), query("write")).await;
    assert_eq!(outputs(&events)["text"], "a poem");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_idempotency_key_differs_between_turns_even_for_the_same_call_id() {
    // Providers that number calls per turn send `call_0` every turn.
    let s = setup(vec![
        MockModel::tool_calls(&[("call_0", "send", "{}")]),
        MockModel::tool_calls(&[("call_0", "send", "{}")]),
        MockModel::text("sent twice"),
    ]);
    let events = run(&engine(&s, graph(json!({})), Limits::default(), None), query("go")).await;
    assert_eq!(outputs(&events)["text"], "sent twice");
    let keys: Vec<String> = s.tools.calls.lock().unwrap().iter().map(|(_, key)| key.clone().unwrap()).collect();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1], "{keys:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_call_that_needs_approval_waits_for_it_again_and_is_not_run_twice() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "slow", "{}")]), MockModel::text("done")]);
    let g = graph(
        json!({"require_approval": ["box/slow"], "timeout_ms": 200, "retry": {"max_retries": 1, "interval_ms": 0}}),
    );
    let engine = engine(&s, g, Limits::default(), None);
    let events = run(&engine, query("go")).await;
    let snapshot = match events.last() {
        Some(Event::RunPaused { snapshot, .. }) => Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        other => panic!("expected a pause, got {other:?}"),
    };
    let mut approve = IndexMap::new();
    approve.insert("agent".to_owned(), Value::from_json(json!({"approved": true})));
    // Approved, it hangs, the timeout cuts it, and the retry asks for approval again.
    let events = run(&engine, RunInput::new().resume(snapshot, approve)).await;
    assert!(matches!(events.last(), Some(Event::RunPaused { .. })), "{:?}", events.last());
    assert_eq!(s.tools.names(), ["slow"], "it ran once");
    assert_eq!(s.model.requests().len(), 1, "the model was not asked again");
    assert!(events.iter().any(|e| matches!(e, Event::NodeProgress { name, .. } if name == "tool_interrupted")));
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_one_after_another_keep_each_result_as_it_comes() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "send", "{}"), ("c2", "slow", "{}")]), MockModel::text("done")]);
    let g =
        graph(json!({"parallel_tool_calls": false, "timeout_ms": 200, "retry": {"max_retries": 1, "interval_ms": 0}}));
    let events = run(&engine(&s, g, Limits::default(), None), query("go")).await;
    assert_eq!(outputs(&events)["text"], "done");
    assert_eq!(s.tools.names(), ["send", "slow"], "send ran once");
    let last = s.model.requests().pop().unwrap();
    assert_eq!(
        tool_results(&last),
        ["send done", "interrupted: this call was started and its outcome is unknown. It was not repeated."]
    );
}

#[test]
fn prices_are_checked() {
    assert!(Price::new(1.0, 2.0).is_ok());
    for (input, output) in [(-1.0, 1.0), (f64::NAN, 1.0), (1.0, f64::INFINITY), (2.0e9, 1.0)] {
        assert!(Price::new(input, output).is_err(), "{input} {output}");
    }
    // A provider that reports only a total is priced as input.
    let price = Price::new(1_000.0, 3_000.0).unwrap();
    let total_only = graphrt_core::event::Usage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 2 };
    assert_eq!(price.cost_micros(&total_only), 2_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denial_survives_a_retry_of_the_turn_after_it() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "send", "{}")])]);
    s.model.push_error(rate_limited());
    s.model.push(MockModel::text("not sent"));
    let g = graph(json!({"require_approval": ["box/send"], "retry": {"max_retries": 1, "interval_ms": 0}}));
    let engine = engine(&s, g, Limits::default(), None);
    let events = run(&engine, query("go")).await;
    let snapshot = match events.last() {
        Some(Event::RunPaused { snapshot, .. }) => Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        other => panic!("expected a pause, got {other:?}"),
    };
    let mut deny = IndexMap::new();
    deny.insert("agent".to_owned(), Value::from_json(json!({"approved": false})));
    let events = run(&engine, RunInput::new().resume(snapshot, deny)).await;
    assert_eq!(outputs(&events)["text"], "not sent", "the retry did not ask for approval again");
    assert!(s.tools.names().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_with_nothing_left_fails_instead_of_answering_past_the_limit() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "lookup", "{}")]), MockModel::text("never")]);
    // The first turn spends all 2 tokens: no answer is affordable.
    let limits = Limits { max_total_tokens: Some(2), ..Limits::default() };
    let events = run(&engine(&s, graph(json!({})), limits, None), query("go")).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, .. }) if error_type == "limit_exceeded"),
        "{:?}",
        events.last()
    );
    assert_eq!(s.model.requests().len(), 1, "no model call with nothing left");
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_that_never_started_run_after_a_cut_and_are_not_called_interrupted() {
    let s = setup(vec![
        MockModel::tool_calls(&[("c1", "send", "{}"), ("c2", "slow", "{}"), ("c3", "lookup", "{}")]),
        MockModel::text("done"),
    ]);
    let g =
        graph(json!({"parallel_tool_calls": false, "timeout_ms": 200, "retry": {"max_retries": 1, "interval_ms": 0}}));
    let events = run(&engine(&s, g, Limits::default(), None), query("go")).await;
    assert_eq!(outputs(&events)["text"], "done");
    assert_eq!(s.tools.names(), ["send", "slow", "lookup"], "each ran once");
    let last = s.model.requests().pop().unwrap();
    assert_eq!(
        tool_results(&last),
        [
            "send done",
            "interrupted: this call was started and its outcome is unknown. It was not repeated.",
            "lookup done"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spent_budget_stops_the_run_even_past_an_error_strategy() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "lookup", "{}")]), MockModel::text("never")]);
    let limits = Limits { max_total_tokens: Some(2), ..Limits::default() };
    let g = graph(json!({"error_strategy": "default-value", "default_value": []}));
    let events = run(&engine(&s, g, limits, None), query("go")).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, .. }) if error_type == "limit_exceeded"),
        "the spent budget ends the run, not the node's default value: {:?}",
        events.last()
    );
}
