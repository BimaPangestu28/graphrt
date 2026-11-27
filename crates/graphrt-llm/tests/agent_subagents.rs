//! `subagents` of the `agent` node: listed workflows run as child runs of the
//! same run, several at once, with approval inside a child pausing the whole
//! run and resuming it where it stopped.

#![cfg(all(feature = "mock", feature = "subagents"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::{Event, PauseReason};
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_core::snapshot::Snapshot;
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_core::value::Value;
use graphrt_llm::ModelRegistry;
use graphrt_llm::mock::MockModel;
use indexmap::IndexMap;
use serde_json::{Value as Json, json};

fn graph(doc: Json) -> Arc<Graph> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

/// `research`: returns what it was asked, and the `sys.query` it sees.
fn research() -> Arc<Graph> {
    graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "question", "type": "text", "required": true}]},
            {"id": "end", "type": "end", "outputs": [
                {"variable": "text", "value_selector": ["start", "question"]}
            ]}
        ],
        "edges": [{"source": "start", "target": "end"}]
    }}))
}

/// `mailer`: an agent whose `mail/send` needs approval.
fn mailer() -> Arc<Graph> {
    graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "to", "type": "text", "required": true}]},
            {"id": "assistant", "type": "agent", "model": {"provider": "child", "name": "m"},
             "query": "Mail {{#start.to#}}", "tools": ["mail/send"], "require_approval": ["mail/send"]},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["assistant", "text"]}]}
        ],
        "edges": [{"source": "start", "target": "assistant"}, {"source": "assistant", "target": "end"}]
    }}))
}

/// `napper`: calls the slow `mail/nap` tool directly.
fn napper() -> Arc<Graph> {
    graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start"},
            {"id": "nap", "type": "tool", "provider_id": "mail", "tool_name": "nap", "arguments": {}},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["nap", "text"]}]}
        ],
        "edges": [{"source": "start", "target": "nap"}, {"source": "nap", "target": "end"}]
    }}))
}

/// Small linked workflows of the tests below.
fn extra_workflows() -> Vec<(&'static str, Json)> {
    let edge = |a: &str, b: &str| json!({"source": a, "target": b});
    vec![
        // What a child sees of the request and of the environment.
        (
            "spy",
            json!({"graph": {"nodes": [
            {"id": "start", "type": "start"},
            {"id": "end", "type": "end", "outputs": [
                {"variable": "query", "value_selector": ["sys", "query"]},
                {"variable": "run_id", "value_selector": ["sys", "run_id"]},
                {"variable": "parent_env", "value_selector": ["env", "PARENT_ONLY"]},
                {"variable": "region", "value_selector": ["env", "REGION"]}
            ]}],
            "edges": [edge("start", "end")]},
            "env": [{"name": "REGION", "type": "string", "value": "eu"}]}),
        ),
        // Echoes its secret through a tool, as plain text.
        (
            "leaky",
            json!({"graph": {"nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "prefix", "type": "text"}]},
            {"id": "echo", "type": "tool", "provider_id": "mail", "tool_name": "echo",
             "arguments": {"text": "{{#start.prefix#}}{{#env.API_TOKEN#}}"}},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["echo", "text"]}]}],
            "edges": [edge("start", "echo"), edge("echo", "end")]},
            "env": [{"name": "API_TOKEN", "type": "secret"}]}),
        ),
        // An agent that delegates to `mailer`.
        (
            "middle",
            json!({"graph": {"nodes": [
            {"id": "start", "type": "start"},
            {"id": "agent", "type": "agent", "model": {"provider": "mid", "name": "m"}, "query": "delegate",
             "subagents": [{"workflow": "mailer", "description": "Mails"}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["agent", "text"]}]}],
            "edges": [edge("start", "agent"), edge("agent", "end")]}}),
        ),
        // Finishes without an answer.
        (
            "silent",
            json!({"graph": {"nodes": [
            {"id": "start", "type": "start"},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["start", "nothing"]}]}],
            "edges": [edge("start", "end")]}}),
        ),
        // A declared topic, an optional secret and a number.
        (
            "picky",
            json!({"graph": {"nodes": [
            {"id": "start", "type": "start", "variables": [
                {"variable": "topic", "type": "select", "options": ["a", "b"], "required": true},
                {"variable": "count", "type": "number"},
                {"variable": "token", "type": "secret"}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["start", "topic"]}]}],
            "edges": [edge("start", "end")]}}),
        ),
        // Needs a secret a model cannot pass.
        (
            "locked",
            json!({"graph": {"nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "token", "type": "secret", "required": true}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["start", "token"]}]}],
            "edges": [edge("start", "end")]}}),
        ),
    ]
}

/// A workflow whose agent calls itself.
fn recursive() -> Arc<Graph> {
    graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "n", "type": "text"}]},
            {"id": "loop", "type": "agent", "model": {"provider": "deep", "name": "m"}, "query": "go",
             "subagents": [{"workflow": "recursive", "description": "Itself"}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "text", "value_selector": ["loop", "text"]}]}
        ],
        "edges": [{"source": "start", "target": "loop"}, {"source": "loop", "target": "end"}]
    }}))
}

fn lead(subagents: Json, extra: Json) -> Arc<Graph> {
    let mut agent = json!({"id": "lead", "type": "agent", "model": {"provider": "mock", "name": "m"},
        "instruction": "Delegate.", "subagents": subagents});
    if let (Some(agent), Json::Object(extra)) = (agent.as_object_mut(), extra) {
        agent.extend(extra);
    }
    graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start"},
            agent,
            {"id": "end", "type": "end", "outputs": [
                {"variable": "text", "value_selector": ["lead", "text"]},
                {"variable": "stopped", "value_selector": ["lead", "stopped_reason"]}
            ]}
        ],
        "edges": [{"source": "start", "target": "lead"}, {"source": "lead", "target": "end"}]
    }}))
}

struct Setup {
    parent: Arc<MockModel>,
    child: Arc<MockModel>,
    deep: Arc<MockModel>,
    /// The model of `middle`'s agent.
    mid: Arc<MockModel>,
    sent: Arc<Mutex<Vec<String>>>,
    /// `mail/nap` calls running now, and the most seen at once.
    naps: Arc<(AtomicUsize, AtomicUsize)>,
}

fn engine(root: Arc<Graph>, s: &Setup, limits: Limits) -> Engine {
    let log = Arc::clone(&s.sent);
    let spec =
        ToolSpec { name: "send".into(), description: "send a mail".into(), parameters: json!({"type": "object"}) };
    let naps = Arc::clone(&s.naps);
    let nap = ToolSpec { name: "nap".into(), description: "sleep".into(), parameters: json!({"type": "object"}) };
    let tools = NativeTools::new().with(nap, move |_: Json| {
        let naps = Arc::clone(&naps);
        async move {
            let now = naps.0.fetch_add(1, Ordering::SeqCst) + 1;
            naps.1.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            naps.0.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolOutput::text("rested"))
        }
    });
    let echo = ToolSpec { name: "echo".into(), description: "echo".into(), parameters: json!({"type": "object"}) };
    let tools = tools.with(echo, |args: Json| async move {
        let text = args.get("text").and_then(Json::as_str).unwrap_or_default().to_owned();
        if text.starts_with("fail:") {
            return Err(graphrt_core::tool::ToolError {
                tool: "echo".into(),
                message: format!("could not echo {text}"),
            });
        }
        Ok(ToolOutput::text(text))
    });
    let tools = tools.with(spec, move |args: Json| {
        let log = Arc::clone(&log);
        async move {
            let to = args.get("to").and_then(Json::as_str).unwrap_or_default().to_owned();
            log.lock().unwrap().push(to.clone());
            Ok(ToolOutput::text(format!("sent to {to}")))
        }
    });
    let mut tool_registry = ToolRegistry::new();
    tool_registry.add("mail", Arc::new(tools));
    let services = Services::new()
        .with(
            ModelRegistry::new()
                .with("mock", s.parent.clone())
                .with("child", s.child.clone())
                .with("deep", s.deep.clone())
                .with("mid", s.mid.clone()),
        )
        .with(tool_registry);
    let mut linked = IndexMap::new();
    linked.insert("research".to_owned(), graphrt_core::engine::LinkedWorkflow::new(research()));
    linked.insert("mailer".to_owned(), graphrt_core::engine::LinkedWorkflow::new(mailer()));
    linked.insert("napper".to_owned(), graphrt_core::engine::LinkedWorkflow::new(napper()));
    for (name, doc) in extra_workflows() {
        linked.insert(name.to_owned(), graphrt_core::engine::LinkedWorkflow::new(graph(doc)));
    }
    linked.insert("recursive".to_owned(), graphrt_core::engine::LinkedWorkflow::new(recursive()));
    Engine::new(root).with_services(services).with_limits(limits).with_linked(linked)
}

fn setup(parent: Vec<Vec<graphrt_llm::ChatDelta>>, child: Vec<Vec<graphrt_llm::ChatDelta>>) -> Setup {
    Setup {
        parent: Arc::new(MockModel::new(parent)),
        child: Arc::new(MockModel::new(child)),
        deep: Arc::new(MockModel::default()),
        mid: Arc::new(MockModel::default()),
        sent: Arc::default(),
        naps: Arc::default(),
    }
}

async fn run(engine: &Engine, input: RunInput) -> Vec<Event> {
    engine.run(input).collect().await
}

fn outputs(events: &[Event]) -> Json {
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => {
            Json::Object(outputs.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
        }
        other => panic!("expected RunSucceeded, got {other:?}"),
    }
}

fn query(text: &str) -> RunInput {
    let mut input = RunInput::new();
    input.sys.insert("query".into(), Value::from(text));
    input
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_picks_listed_workflows_and_calls_several_at_once() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[
                ("c1", "research", r#"{"question":"price of chairs"}"#),
                ("c2", "research", r#"{"question":"price of tables"}"#),
                ("c3", "research", r#"{"question":"price of lamps"}"#),
            ]),
            MockModel::text("Chairs, tables and lamps are priced."),
        ],
        vec![],
    );
    let root = lead(
        json!([{"workflow": "research", "description": "Researches one question"}]),
        json!({"max_parallel_subagents": 2}),
    );
    let events = run(&engine(root, &s, Limits::default()), query("the secret plan")).await;
    assert_eq!(outputs(&events)["text"], "Chairs, tables and lamps are priced.");
    // The model was offered the workflow as a tool with its start variables.
    let first = s.parent.requests()[0].clone();
    let tool = first.tools.iter().find(|t| t.name == "research").unwrap();
    assert_eq!(tool.parameters["required"], json!(["question"]));
    // Each call ran in its own child frame, and its answer came back as the result.
    let frames: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeStarted { frame, node_id, .. } if node_id.as_ref() == "end" && !frame.is_empty() => {
                Some(frame.to_string())
            }
            _ => None,
        })
        .collect();
    let mut sorted = frames.clone();
    sorted.sort();
    assert_eq!(sorted, ["lead.research[0]", "lead.research[1]", "lead.research[2]"]);
    let second = s.parent.requests()[1].clone();
    let results: Vec<String> =
        second.messages.iter().filter(|m| m.role == graphrt_llm::Role::Tool).map(|m| m.text()).collect();
    assert_eq!(results, ["price of chairs", "price of tables", "price of lamps"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workflow_that_is_not_listed_is_not_a_tool_and_a_child_sees_nothing_of_the_parent() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[("c1", "mailer", r#"{"to":"x@example.com"}"#)]),
            MockModel::tool_calls(&[("c2", "research", r#"{"question":"what do you see"}"#)]),
            MockModel::text("ok"),
        ],
        vec![],
    );
    let root = lead(json!([{"workflow": "research", "description": "Researches"}]), json!({}));
    let events = run(&engine(root, &s, Limits::default()), query("the secret plan")).await;
    outputs(&events);
    let names: Vec<String> = s.parent.requests()[0].tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(names, ["research"], "only what is listed");
    assert!(s.sent.lock().unwrap().is_empty(), "the unlisted workflow never ran");
    // The child's pool held its own input only: the parent's query is nowhere in its events.
    let child_values: String = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeSucceeded { frame, outputs, .. } if !frame.is_empty() => Some(format!("{outputs:?}")),
            _ => None,
        })
        .collect();
    assert!(!child_values.contains("secret plan"), "{child_values}");
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_inside_a_child_pauses_the_run_and_resumes_it_where_it_stopped() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[("c1", "mailer", r#"{"to":"ana@example.com"}"#)]),
            MockModel::text("The mail is out."),
        ],
        vec![MockModel::tool_calls(&[("m1", "send", r#"{"to":"ana@example.com"}"#)]), MockModel::text("Sent.")],
    );
    let root = lead(json!([{"workflow": "mailer", "description": "Mails one person"}]), json!({}));
    let engine = engine(root, &s, Limits::default());
    let events = run(&engine, query("mail ana")).await;
    let (snapshot, reasons): (Snapshot, Vec<PauseReason>) = match events.last() {
        Some(Event::RunPaused { snapshot, reasons, .. }) => {
            (Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(), reasons.clone())
        }
        other => panic!("expected a pause, got {other:?}"),
    };
    assert_eq!(reasons.len(), 1, "one reason, from the node that waits: {reasons:?}");
    assert_eq!(reasons[0].frame.as_ref(), "lead.mailer[0]");
    assert_eq!(reasons[0].node_id.as_deref(), Some("assistant"));
    assert_eq!(reasons[0].kind, "tool_approval");
    assert!(s.sent.lock().unwrap().is_empty());

    let mut inputs = IndexMap::new();
    inputs.insert("lead.mailer[0]/assistant".to_owned(), Value::from_json(json!({"approved": true})));
    let events = run(&engine, RunInput::new().resume(snapshot, inputs)).await;
    assert_eq!(outputs(&events)["text"], "The mail is out.");
    assert_eq!(*s.sent.lock().unwrap(), ["ana@example.com"], "sent once");
    let last = s.parent.requests().pop().unwrap();
    assert!(last.messages.iter().any(|m| m.text() == "Sent."), "the child's answer is the tool result");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workflow_that_calls_itself_ends_at_the_depth_limit() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "recursive", "{}")]), MockModel::text("done")], vec![]);
    // Every level asks to go deeper (the levels ask one after another), then answers.
    for _ in 0..3 {
        s.deep.push(MockModel::tool_calls(&[("d", "recursive", "{}")]));
    }
    for _ in 0..3 {
        s.deep.push(MockModel::text("level done"));
    }
    let root = lead(json!([{"workflow": "recursive", "description": "Goes deeper"}]), json!({}));
    let limits = Limits { max_depth: 3, ..Limits::default() };
    let events = run(&engine(root, &s, limits), query("go")).await;
    assert_eq!(outputs(&events)["text"], "done");
    let deepest = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeStarted { frame, .. } => Some(frame.matches('/').count()),
            _ => None,
        })
        .max()
        .unwrap();
    assert_eq!(deepest, 2, "children nest until the limit");
    let refused = s.deep.requests().iter().any(|r| r.messages.iter().any(|m| m.text().contains("nesting depth limit")));
    assert!(refused, "the deepest call is refused with the limit");
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_past_the_limit_are_refused_and_a_handoff_answers_for_the_agent() {
    let s = setup(
        vec![MockModel::tool_calls(&[
            ("c1", "research", r#"{"question":"one"}"#),
            ("c2", "research", r#"{"question":"two"}"#),
        ])],
        vec![],
    );
    let root = lead(
        json!([{"workflow": "research", "description": "Answers for the agent", "handoff": true}]),
        json!({"max_subagent_calls": 1}),
    );
    let events = run(&engine(root, &s, Limits::default()), query("hand off")).await;
    let out = outputs(&events);
    assert_eq!(out["text"], "one", "the first answer is the agent's answer");
    assert_eq!(out["stopped"], "handoff");
    assert_eq!(s.parent.requests().len(), 1, "no second model turn after a handoff");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_childs_secret_environment_is_masked_in_what_reaches_the_parent_even_as_plain_text() {
    const TOKEN: &str = "tok-0123456789-secret";
    let s = setup(
        vec![
            MockModel::tool_calls(&[("c1", "leaky", "{}"), ("c2", "leaky", r#"{"prefix":"fail:"}"#)]),
            MockModel::text("ok"),
        ],
        vec![],
    );
    let root = lead(json!([{"workflow": "leaky", "description": "Leaks"}]), json!({}));
    // A host gives the environment as plain strings: the child's `secret`
    // declaration makes the value a secret.
    let mut env = IndexMap::new();
    env.insert("API_TOKEN".to_owned(), Value::from(TOKEN));
    let mut linked = IndexMap::new();
    let doc = extra_workflows().into_iter().find(|(n, _)| *n == "leaky").unwrap().1;
    linked.insert("leaky".to_owned(), graphrt_core::engine::LinkedWorkflow::new(graph(doc)).with_env(env));
    let engine = engine(root, &s, Limits::default()).with_linked(linked);
    let events = run(&engine, query("leak it")).await;
    assert_eq!(outputs(&events)["text"], "ok");
    let second = s.parent.requests()[1].clone();
    let results: Vec<String> =
        second.messages.iter().filter(|m| m.role == graphrt_llm::Role::Tool).map(|m| m.text()).collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| !r.contains(TOKEN)), "{results:?}");
    assert!(results[1].starts_with("error"), "the failing child is an error result: {results:?}");
    // The child's failure reaches the host masked, like a failure of the run's own nodes.
    for event in events.iter().filter(|e| matches!(e, Event::NodeFailed { .. } | Event::NodeRetry { .. })) {
        let text = serde_json::to_string(event).unwrap();
        assert!(!text.contains(TOKEN), "{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_sees_its_own_environment_and_the_requests_ids_and_nothing_else_of_the_parent() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "spy", "{}")]), MockModel::text("seen")], vec![]);
    let root = lead(json!([{"workflow": "spy", "description": "Looks"}]), json!({}));
    let input = query("the secret plan").env("PARENT_ONLY", "parent value");
    let events = run(&engine(root, &s, Limits::default()), input).await;
    let run_id = events
        .iter()
        .find_map(|e| match e {
            Event::RunStarted { run_id, .. } => Some(run_id.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(outputs(&events)["text"], "seen");
    let child_end = events
        .iter()
        .find_map(|e| match e {
            Event::NodeSucceeded { frame, node_id, outputs, .. }
                if frame.as_ref() == "lead.spy[0]" && node_id.as_ref() == "end" =>
            {
                Some(outputs.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(child_end.get("run_id"), Some(&Value::from(run_id.as_str())), "the parent's run id");
    assert_eq!(child_end.get("region"), Some(&Value::from("eu")), "its own declared default");
    for withheld in ["query", "parent_env"] {
        assert!(child_end.get(withheld).is_none_or(|v| matches!(v, Value::Null)), "{withheld}: {child_end:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_call_limit_holds_across_turns_without_a_handoff() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[("c1", "research", r#"{"question":"one"}"#)]),
            MockModel::tool_calls(&[
                ("c2", "research", r#"{"question":"two"}"#),
                ("c3", "research", r#"{"question":"three"}"#),
            ]),
            MockModel::text("enough"),
        ],
        vec![],
    );
    let root = lead(json!([{"workflow": "research", "description": "Researches"}]), json!({"max_subagent_calls": 2}));
    let events = run(&engine(root, &s, Limits::default()), query("go")).await;
    assert_eq!(outputs(&events)["text"], "enough");
    let last = s.parent.requests().pop().unwrap();
    let results: Vec<String> =
        last.messages.iter().filter(|m| m.role == graphrt_llm::Role::Tool).map(|m| m.text()).collect();
    assert_eq!(results[..2], ["one".to_owned(), "two".to_owned()]);
    assert!(results[2].starts_with("error: at most 2 workflow calls"), "{results:?}");
    let mut frames: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeStarted { frame, node_id, .. } if node_id.as_ref() == "start" && !frame.is_empty() => {
                Some(frame.to_string())
            }
            _ => None,
        })
        .collect();
    frames.sort();
    assert_eq!(frames, ["lead.research[0]", "lead.research[1]"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_count_of_calls_survives_a_pause_and_a_handoff_answers_on_resume() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[("c1", "mailer", r#"{"to":"a"}"#)]),
            MockModel::tool_calls(&[
                ("c2", "research", r#"{"question":"one"}"#),
                ("c3", "research", r#"{"question":"two"}"#),
            ]),
            MockModel::text("done"),
        ],
        vec![MockModel::tool_calls(&[("m", "send", r#"{"to":"a"}"#)]), MockModel::text("Sent.")],
    );
    let root = lead(
        json!([{"workflow": "mailer", "description": "Mails"}, {"workflow": "research", "description": "Researches"}]),
        json!({"max_subagent_calls": 2}),
    );
    let engine = engine(root, &s, Limits::default());
    let (snapshot, _) = paused(&run(&engine, query("go")).await);
    let events = run(&engine, RunInput::new().resume(snapshot, approve(&["lead.mailer[0]/assistant"]))).await;
    assert_eq!(outputs(&events)["text"], "done");
    let last = s.parent.requests().pop().unwrap();
    let results: Vec<String> =
        last.messages.iter().filter(|m| m.role == graphrt_llm::Role::Tool).map(|m| m.text()).collect();
    assert_eq!(results[..2], ["Sent.".to_owned(), "one".to_owned()]);
    assert!(results[2].starts_with("error: at most 2"), "{results:?}");
    assert!(
        events.iter().any(|e| matches!(e, Event::NodeStarted { frame, .. } if frame.as_ref() == "lead.research[1]"))
    );

    // A handoff child that finishes on resume answers for the agent at once.
    let s = setup(
        vec![MockModel::tool_calls(&[("c1", "mailer", r#"{"to":"a"}"#)])],
        vec![MockModel::tool_calls(&[("m", "send", r#"{"to":"a"}"#)]), MockModel::text("Sent.")],
    );
    let root = lead(json!([{"workflow": "mailer", "description": "Mails", "handoff": true}]), json!({}));
    let engine = engine_for(root, &s);
    let (snapshot, _) = paused(&run(&engine, query("go")).await);
    let events = run(&engine, RunInput::new().resume(snapshot, approve(&["lead.mailer[0]/assistant"]))).await;
    let out = outputs(&events);
    assert_eq!((out["text"].as_str(), out["stopped"].as_str()), (Some("Sent."), Some("handoff")));
    assert_eq!(s.parent.requests().len(), 1, "no model turn after the handoff");
}

fn engine_for(root: Arc<Graph>, s: &Setup) -> Engine {
    engine(root, s, Limits::default())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_two_levels_down_reaches_the_host_once_and_resumes_there() {
    let s = setup(
        vec![MockModel::tool_calls(&[("c1", "middle", "{}")]), MockModel::text("all done")],
        vec![MockModel::tool_calls(&[("m", "send", r#"{"to":"deep"}"#)]), MockModel::text("Sent.")],
    );
    s.mid.push(MockModel::tool_calls(&[("x", "mailer", r#"{"to":"deep"}"#)]));
    s.mid.push(MockModel::text("delegated"));
    let root = lead(json!([{"workflow": "middle", "description": "Delegates"}]), json!({}));
    let engine = engine_for(root, &s);
    let (snapshot, reasons) = paused(&run(&engine, query("go")).await);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert_eq!(reasons[0].frame.as_ref(), "lead.middle[0]/agent.mailer[0]");
    // Resumed without a decision, it waits again with the same single reason.
    let (snapshot, reasons) = paused(&run(&engine, RunInput::new().resume(snapshot, IndexMap::new())).await);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    let key = "lead.middle[0]/agent.mailer[0]/assistant";
    let events = run(&engine, RunInput::new().resume(snapshot, approve(&[key]))).await;
    assert_eq!(outputs(&events)["text"], "all done");
    assert_eq!(*s.sent.lock().unwrap(), ["deep"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn arguments_a_workflow_does_not_take_are_refused_and_secrets_are_never_offered() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[
                ("c1", "picky", r#"{"topic":"a","extra":1}"#),
                ("c2", "picky", r#"{"topic":"a","token":"x"}"#),
                ("c3", "picky", r#"{"topic":{"$file":"f","transfer_method":"local_file","reference":"/etc/passwd"}}"#),
                ("c4", "picky", r#"{"topic":"b","count":2}"#),
                ("c5", "silent", "{}"),
            ]),
            MockModel::text("checked"),
        ],
        vec![],
    );
    let root = lead(
        json!([{"workflow": "picky", "description": "Picky"}, {"workflow": "silent", "description": "Says nothing", "handoff": true}]),
        json!({}),
    );
    let events = run(&engine_for(root, &s), query("go")).await;
    assert_eq!(outputs(&events)["text"], "checked", "no handoff from a child without an answer");
    let first = s.parent.requests()[0].clone();
    let tool = first.tools.iter().find(|t| t.name == "picky").unwrap();
    assert_eq!(tool.parameters["properties"]["topic"]["enum"], json!(["a", "b"]));
    assert_eq!(tool.parameters["properties"]["count"]["type"], "number");
    assert!(tool.parameters["properties"].get("token").is_none(), "a secret is never offered");
    assert_eq!(tool.parameters["additionalProperties"], false);
    let second = s.parent.requests()[1].clone();
    let results: Vec<String> =
        second.messages.iter().filter(|m| m.role == graphrt_llm::Role::Tool).map(|m| m.text()).collect();
    assert!(results[0].contains("no argument `extra`"), "{results:?}");
    assert!(results[1].contains("no argument `token`"), "{results:?}");
    assert!(results[2].contains("`$file`"), "{results:?}");
    assert_eq!(results[3], "b");
    assert!(results[4].contains("without an answer"), "{results:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_misconfigured_list_fails_the_agent_before_its_first_model_call() {
    // No workflows linked at all.
    let s = setup(vec![MockModel::text("never")], vec![]);
    let root = lead(json!([{"workflow": "research", "description": "Researches"}]), json!({}));
    let mut registry = graphrt_core::tool::ToolRegistry::new();
    registry.add("mail", Arc::new(NativeTools::new()));
    let plain = Engine::new(Arc::clone(&root))
        .with_services(Services::new().with(ModelRegistry::new().with("mock", s.parent.clone())).with(registry));
    let failed = |events: &[Event]| match events.last() {
        Some(Event::RunFailed { error_type, .. }) => error_type.clone(),
        other => panic!("expected a failure, got {other:?}"),
    };
    assert_eq!(failed(&run(&plain, query("go")).await), "subagents_unavailable");
    // Listed but not linked, or needing a secret a model cannot pass.
    for (listed, kind) in [("unlinked", "unknown_workflow"), ("locked", "unusable_workflow")] {
        let root = lead(json!([{"workflow": listed, "description": "x"}]), json!({}));
        assert_eq!(failed(&run(&engine_for(root, &s), query("go")).await), kind);
    }
    assert!(s.parent.requests().is_empty(), "no model call was paid for");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abort_stops_running_children_with_the_run() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "napper", "{}")]), MockModel::text("never")], vec![]);
    let root = lead(json!([{"workflow": "napper", "description": "Rests"}]), json!({}));
    let engine = engine_for(root, &s);
    let mut stream = engine.run(query("rest"));
    let handle = stream.handle();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if matches!(&event, Event::NodeStarted { frame, node_id, .. } if frame.as_ref() == "lead.napper[0]" && node_id.as_ref() == "nap")
        {
            handle.abort("stop now");
        }
        events.push(event);
    }
    assert!(matches!(events.last(), Some(Event::RunFailed { .. } | Event::RunAborted { .. })), "{:?}", events.last());
    assert_eq!(s.parent.requests().len(), 1, "the parent never saw the child as a result");
}

#[test]
fn an_engine_without_links_keeps_the_fingerprint_of_its_graph() {
    let g = research();
    assert_eq!(Engine::new(Arc::clone(&g)).fingerprint(), g.fingerprint());
    let empty: Vec<(String, graphrt_core::engine::LinkedWorkflow)> = Vec::new();
    assert_eq!(Engine::new(Arc::clone(&g)).with_linked(empty).fingerprint(), g.fingerprint());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_inside_an_iteration_starts_its_children_in_frames_of_its_own() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[("c1", "research", r#"{"question":"first"}"#)]),
            MockModel::text("item done"),
            MockModel::tool_calls(&[("c1", "research", r#"{"question":"second"}"#)]),
            MockModel::text("item done"),
        ],
        vec![],
    );
    let root = graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "items", "type": "json"}]},
            {"id": "each", "type": "iteration", "iterator_selector": ["start", "items"], "output_selector": ["lead", "text"]},
            {"id": "each_start", "type": "iteration-start", "parentId": "each"},
            {"id": "lead", "type": "agent", "parentId": "each", "model": {"provider": "mock", "name": "m"},
             "query": "{{#each.item#}}", "subagents": [{"workflow": "research", "description": "Researches"}]},
            {"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["each", "output"]}]}
        ],
        "edges": [{"source": "start", "target": "each"}, {"source": "each_start", "target": "lead"}, {"source": "each", "target": "end"}]
    }}));
    let input = RunInput::new().input("items", Value::from_json(json!(["a", "b"])));
    let events = run(&engine(root, &s, Limits::default()), input).await;
    assert_eq!(outputs(&events)["out"], json!(["item done", "item done"]));
    let mut frames: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeStarted { frame, node_id, .. } if node_id.as_ref() == "end" && frame.contains("research") => {
                Some(frame.to_string())
            }
            _ => None,
        })
        .collect();
    frames.sort();
    assert_eq!(frames, ["each[0]/lead.research[0]", "each[1]/lead.research[0]"]);
}

fn paused(events: &[Event]) -> (Snapshot, Vec<PauseReason>) {
    match events.last() {
        Some(Event::RunPaused { snapshot, reasons, .. }) => {
            (Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(), reasons.clone())
        }
        other => panic!("expected a pause, got {other:?}"),
    }
}

fn approve(keys: &[&str]) -> IndexMap<String, Value> {
    keys.iter().map(|k| ((*k).to_owned(), Value::from_json(json!({"approved": true})))).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn several_children_wait_and_are_resumed_one_by_one_each_sending_once() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[
                ("c1", "mailer", r#"{"to":"a@example.com"}"#),
                ("c2", "mailer", r#"{"to":"b@example.com"}"#),
            ]),
            MockModel::text("both sent"),
        ],
        vec![],
    );
    // Each child's agent asks to send, then says sent (the child model serves both, in any order).
    for _ in 0..2 {
        s.child.push(MockModel::tool_calls(&[("m", "send", r#"{"to":"x"}"#)]));
    }
    s.child.push(MockModel::text("Sent."));
    s.child.push(MockModel::text("Sent."));
    let root = lead(json!([{"workflow": "mailer", "description": "Mails"}]), json!({"max_parallel_subagents": 1}));
    let engine = engine(root, &s, Limits::default());
    let (snapshot, reasons) = paused(&run(&engine, query("mail both")).await);
    let mut frames: Vec<&str> = reasons.iter().map(|r| r.frame.as_ref()).collect();
    frames.sort_unstable();
    assert_eq!(frames, ["lead.mailer[0]", "lead.mailer[1]"]);
    // Only the first is approved: the run waits for the second alone.
    let (snapshot, reasons) =
        paused(&run(&engine, RunInput::new().resume(snapshot, approve(&["lead.mailer[0]/assistant"]))).await);
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].frame.as_ref(), "lead.mailer[1]");
    assert_eq!(s.sent.lock().unwrap().len(), 1);
    let events = run(&engine, RunInput::new().resume(snapshot, approve(&["lead.mailer[1]/assistant"]))).await;
    assert_eq!(outputs(&events)["text"], "both sent");
    assert_eq!(s.sent.lock().unwrap().len(), 2, "each child sent once");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_key_meant_for_the_parent_never_approves_inside_a_child() {
    // The child's agent is named like the parent's.
    let s = setup(
        vec![MockModel::tool_calls(&[("c1", "mailer", r#"{"to":"a@example.com"}"#)]), MockModel::text("done")],
        vec![MockModel::tool_calls(&[("m", "send", r#"{"to":"a"}"#)]), MockModel::text("Sent.")],
    );
    let root = lead(json!([{"workflow": "mailer", "description": "Mails"}]), json!({}));
    let engine = engine(root, &s, Limits::default());
    let (snapshot, _) = paused(&run(&engine, query("mail")).await);
    // `assistant` alone (no frame) is not the child's key.
    let (_, reasons) = paused(&run(&engine, RunInput::new().resume(snapshot, approve(&["assistant"]))).await);
    assert_eq!(reasons[0].frame.as_ref(), "lead.mailer[0]");
    assert!(s.sent.lock().unwrap().is_empty(), "nothing approved by a plain key");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_parents_own_approval_and_a_waiting_child_are_both_reported() {
    let s = setup(
        vec![
            MockModel::tool_calls(&[("p1", "send", r#"{"to":"parent"}"#), ("c1", "mailer", r#"{"to":"child"}"#)]),
            MockModel::text("all done"),
        ],
        vec![MockModel::tool_calls(&[("m", "send", r#"{"to":"child"}"#)]), MockModel::text("Sent.")],
    );
    let root = lead(
        json!([{"workflow": "mailer", "description": "Mails"}]),
        json!({"tools": ["mail/send"], "require_approval": ["mail/send"]}),
    );
    let engine = engine(root, &s, Limits::default());
    let (snapshot, reasons) = paused(&run(&engine, query("mail")).await);
    let mut keys: Vec<String> = reasons.iter().map(|r| format!("{}/{}", r.frame, r.node_id.clone().unwrap())).collect();
    keys.sort();
    assert_eq!(keys, ["/lead", "lead.mailer[0]/assistant"]);
    let events = run(&engine, RunInput::new().resume(snapshot, approve(&["lead", "lead.mailer[0]/assistant"]))).await;
    assert_eq!(outputs(&events)["text"], "all done");
    let mut sent = s.sent.lock().unwrap().clone();
    sent.sort();
    assert_eq!(sent, ["child", "parent"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_child_is_an_error_result_and_a_changed_child_refuses_the_resume() {
    // A child that cannot start: its required input is missing.
    let s = setup(vec![MockModel::tool_calls(&[("c1", "research", "{}")]), MockModel::text("handled")], vec![]);
    let root = lead(json!([{"workflow": "research", "description": "Researches"}]), json!({}));
    let events = run(&engine(root, &s, Limits::default()), query("go")).await;
    assert_eq!(outputs(&events)["text"], "handled");
    let second = s.parent.requests()[1].clone();
    assert!(second.messages.iter().any(|m| m.role == graphrt_llm::Role::Tool && m.text().starts_with("error")));

    // A paused run is not resumed against another version of its child.
    let s = setup(
        vec![MockModel::tool_calls(&[("c1", "mailer", r#"{"to":"a"}"#)]), MockModel::text("x")],
        vec![MockModel::tool_calls(&[("m", "send", r#"{"to":"a"}"#)]), MockModel::text("Sent.")],
    );
    let root = lead(json!([{"workflow": "mailer", "description": "Mails"}]), json!({}));
    let (snapshot, _) = paused(&run(&engine(Arc::clone(&root), &s, Limits::default()), query("mail")).await);
    let mut changed = IndexMap::new();
    changed.insert("mailer".to_owned(), graphrt_core::engine::LinkedWorkflow::new(research()));
    let engine = engine(root, &s, Limits::default()).with_linked(changed);
    let events = run(&engine, RunInput::new().resume(snapshot, approve(&["lead.mailer[0]/assistant"]))).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, .. }) if error_type == "snapshot_mismatch"),
        "{:?}",
        events.last()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn children_spend_the_parents_step_budget() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "recursive", "{}")]), MockModel::text("done")], vec![]);
    for _ in 0..8 {
        s.deep.push(MockModel::tool_calls(&[("d", "recursive", "{}")]));
        s.deep.push(MockModel::text("level done"));
    }
    let root = lead(json!([{"workflow": "recursive", "description": "Goes deeper"}]), json!({}));
    // Few steps: the nodes of the children count, so the whole run stops.
    let limits = Limits { max_steps: 6, ..Limits::default() };
    let events = run(&engine(root, &s, limits), query("go")).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, .. }) if error_type == "limit_exceeded"),
        "{:?}",
        events.last()
    );
    assert_eq!(s.parent.requests().len(), 1, "the parent stops with its child, no turn after");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_more_children_run_at_once_than_the_agent_allows() {
    for (allowed, expected) in [(2, 2), (4, 4)] {
        let calls: Vec<(String, &str, &str)> = (0..4).map(|n| (format!("c{n}"), "napper", "{}")).collect();
        let calls: Vec<(&str, &str, &str)> = calls.iter().map(|(id, name, args)| (id.as_str(), *name, *args)).collect();
        let s = setup(vec![MockModel::tool_calls(&calls), MockModel::text("rested")], vec![]);
        let root =
            lead(json!([{"workflow": "napper", "description": "Rests"}]), json!({"max_parallel_subagents": allowed}));
        let events = run(&engine(root, &s, Limits::default()), query("rest")).await;
        assert_eq!(outputs(&events)["text"], "rested");
        assert_eq!(s.naps.1.load(Ordering::SeqCst), expected, "allowed {allowed}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_uses_the_services_its_host_gave_its_workflow() {
    let s = setup(vec![MockModel::tool_calls(&[("c1", "napper", "{}")]), MockModel::text("done")], vec![]);
    let root = lead(json!([{"workflow": "napper", "description": "Rests"}]), json!({}));
    let nap = ToolSpec { name: "nap".into(), description: "sleep".into(), parameters: json!({"type": "object"}) };
    let tools = NativeTools::new().with(nap, |_: Json| async { Ok(ToolOutput::text("own rest")) });
    let mut registry = ToolRegistry::new();
    registry.add("mail", Arc::new(tools));
    let own = Arc::new(Services::new().with(registry));
    let mut linked = IndexMap::new();
    linked.insert("napper".to_owned(), graphrt_core::engine::LinkedWorkflow::new(napper()).with_services(own));
    let engine = engine_for(root, &s).with_linked(linked);
    let events = run(&engine, query("rest")).await;
    assert_eq!(outputs(&events)["text"], "done");
    let second = s.parent.requests()[1].clone();
    assert!(second.messages.iter().any(|m| m.text() == "own rest"), "the child's own tools answered");
    assert_eq!(s.naps.1.load(Ordering::SeqCst), 0, "not the parent's tool");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_edited_to_resume_an_unlisted_workflow_is_refused() {
    let s = setup(
        vec![MockModel::tool_calls(&[("c1", "mailer", r#"{"to":"a"}"#)]), MockModel::text("refused")],
        vec![MockModel::tool_calls(&[("m", "send", r#"{"to":"a"}"#)]), MockModel::text("Sent.")],
    );
    let root = lead(json!([{"workflow": "mailer", "description": "Mails"}]), json!({}));
    let engine = engine_for(root, &s);
    let (snapshot, _) = paused(&run(&engine, query("go")).await);
    let text = snapshot.to_json().unwrap();
    assert!(text.contains(r#""workflow":"mailer""#));
    let forged = Snapshot::from_json(&text.replace(r#""workflow":"mailer""#, r#""workflow":"napper""#)).unwrap();
    let events = run(&engine, RunInput::new().resume(forged, approve(&["lead.mailer[0]/assistant"]))).await;
    assert_eq!(outputs(&events)["text"], "refused");
    let last = s.parent.requests().pop().unwrap();
    assert!(last.messages.iter().any(|m| m.text().contains("`napper` is not a listed workflow")));
    assert!(!events.iter().any(|e| matches!(e, Event::NodeStarted { frame, .. } if frame.contains("napper"))));
    assert!(s.sent.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_children_run_at_the_lowest_concurrency() {
    let s = setup(
        vec![MockModel::tool_calls(&[("c1", "middle", "{}")]), MockModel::text("all done")],
        vec![MockModel::tool_calls(&[("m", "send", r#"{"to":"deep"}"#)]), MockModel::text("Sent.")],
    );
    s.mid.push(MockModel::tool_calls(&[("x", "mailer", r#"{"to":"deep"}"#)]));
    s.mid.push(MockModel::text("delegated"));
    let root = lead(json!([{"workflow": "middle", "description": "Delegates"}]), json!({}));
    // A phone host runs one node at a time.
    let engine = engine(root, &s, Limits { max_concurrency: 1, ..Limits::default() });
    let (snapshot, reasons) = paused(&run(&engine, query("go")).await);
    assert_eq!(reasons[0].frame.as_ref(), "lead.middle[0]/agent.mailer[0]", "{reasons:?}");
    let events =
        run(&engine, RunInput::new().resume(snapshot, approve(&["lead.middle[0]/agent.mailer[0]/assistant"]))).await;
    assert_eq!(outputs(&events)["text"], "all done");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_handoff_in_the_models_order_wins_whatever_finishes_first() {
    let s = setup(
        vec![MockModel::tool_calls(&[("c1", "napper", "{}"), ("c2", "research", r#"{"question":"fast"}"#)])],
        vec![],
    );
    let root = lead(
        json!([
            {"workflow": "napper", "description": "Rests", "handoff": true},
            {"workflow": "research", "description": "Researches", "handoff": true}
        ]),
        json!({"max_parallel_subagents": 2}),
    );
    let events = run(&engine_for(root, &s), query("go")).await;
    let out = outputs(&events);
    assert_eq!((out["text"].as_str(), out["stopped"].as_str()), (Some("rested"), Some("handoff")));
}
