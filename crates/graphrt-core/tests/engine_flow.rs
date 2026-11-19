//! Engine integration tests: scheduling, failures, limits, pause / resume and containers.
//!
//! Test nodes are defined here so the engine is exercised independently of the
//! built-in node library.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::dsl::{EdgeDoc, ErrorStrategy, GraphDoc, NodeDoc, NodePolicy, RetryPolicy, VariableDecl, Workflow};
use graphrt_core::engine::{Engine, Limits, RunInput};
use graphrt_core::event::{Event, Usage};
use graphrt_core::graph::Graph;
use graphrt_core::node::{
    ConfigError, Node, NodeContext, NodeError, NodeOutcome, NodeOutput, NodeResult, NodeRole, PauseRequest, Registry,
    SubgraphRequest, SubgraphResult,
};
use graphrt_core::pool::Selector;
use graphrt_core::snapshot::Snapshot;
use graphrt_core::template::Template;
use graphrt_core::value::{Value, ValueType};
use indexmap::IndexMap;
use serde_json::json;

// ---------------------------------------------------------------------------
// Test nodes
// ---------------------------------------------------------------------------

/// Root node: re-emits whatever the engine put into its namespace (run inputs).
#[derive(Debug)]
struct Source;
impl Node for Source {
    fn role(&self) -> NodeRole {
        NodeRole::Root
    }
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let mut out = NodeOutput::new();
            if let Some(vars) = ctx.pool.namespace(&ctx.node_id) {
                for (k, v) in vars {
                    out.outputs.insert(k.to_string(), v.clone());
                }
            }
            Ok(out.into())
        })
    }
}

/// Body entry of a container.
#[derive(Debug)]
struct ScopeStart;
impl Node for ScopeStart {
    fn role(&self) -> NodeRole {
        NodeRole::ScopeStart
    }
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Ok(NodeOutput::new().into()) })
    }
}

/// Renders `template` into output `out`, optionally after sleeping and streaming chunks.
#[derive(Debug)]
struct Echo {
    template: Template,
    delay_ms: u64,
    chunks: Vec<String>,
    usage: u64,
    writes: Vec<(Selector, Value)>,
}
impl Node for Echo {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            if self.delay_ms > 0 {
                ctx.clock.sleep(Duration::from_millis(self.delay_ms)).await;
            }
            for c in &self.chunks {
                ctx.chunk("out", c.clone());
            }
            let mut out = NodeOutput::new().with("out", self.template.resolve(&ctx.pool));
            if self.usage > 0 {
                out = out.usage(Usage { prompt_tokens: self.usage, completion_tokens: 0, total_tokens: self.usage });
            }
            out.writes = self.writes.clone();
            Ok(out.into())
        })
    }
}

/// Branch node selecting `handle` (a template, so it can depend on inputs).
#[derive(Debug)]
struct Branch {
    handle: Option<Template>,
}
impl Node for Branch {
    fn role(&self) -> NodeRole {
        NodeRole::Branch
    }
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let mut out = NodeOutput::new();
            out.branch = self.handle.as_ref().map(|t| t.render(&ctx.pool));
            Ok(out.into())
        })
    }
}

/// Fails the first `fail_times` attempts.
#[derive(Debug)]
struct Flaky {
    fail_times: u32,
    retryable: bool,
    calls: AtomicU32,
}
impl Node for Flaky {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                let mut e = NodeError::new("flaky", format!("failure {}", n + 1));
                e.retryable = self.retryable;
                return Err(e);
            }
            Ok(NodeOutput::new().with("calls", Value::Int(i64::from(n + 1))).into())
        })
    }
}

/// Pauses until resumed, then outputs the resume input and saved state.
#[derive(Debug)]
struct Pauser;
impl Node for Pauser {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            match ctx.resume {
                None => Ok(NodeOutcome::Paused(PauseRequest {
                    kind: "human_input".into(),
                    data: Value::from("please answer"),
                    state: json!({"asked": true}),
                })),
                Some(resume) => Ok(NodeOutput::new()
                    .with("answer", resume.input.unwrap_or(Value::Null))
                    .with("state", Value::from_json(resume.state))
                    .into()),
            }
        })
    }
}

/// Response node: renders `text` into `answer`.
#[derive(Debug)]
struct Answer {
    text: Template,
}
impl Node for Answer {
    fn role(&self) -> NodeRole {
        NodeRole::Response
    }
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move { Ok(NodeOutput::new().with("answer", self.text.render(&ctx.pool)).into()) })
    }
}

/// Response node: outputs `name` = resolved `value`.
#[derive(Debug)]
struct End {
    name: String,
    value: Template,
}
impl Node for End {
    fn role(&self) -> NodeRole {
        NodeRole::Response
    }
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move { Ok(NodeOutput::new().with(self.name.clone(), self.value.resolve(&ctx.pool)).into()) })
    }
}

/// Minimal sequential container: runs its body `count` times, collecting `collect`.
/// Pause-aware: saves the next index, collected values and the paused child snapshot.
#[derive(Debug)]
struct BoxContainer {
    count: usize,
    collect: Selector,
}
impl Node for BoxContainer {
    fn role(&self) -> NodeRole {
        NodeRole::Container
    }
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let subgraph = ctx.subgraph.clone().ok_or_else(|| NodeError::fatal("container", "no subgraph"))?;
            let (mut index, mut items, mut child_resume) = match &ctx.resume {
                Some(r) => {
                    let index = r.state.get("index").and_then(serde_json::Value::as_u64).unwrap_or(0);
                    let items: Vec<Value> = r
                        .state
                        .get("items")
                        .cloned()
                        .map(|v| serde_json::from_value(v).unwrap_or_default())
                        .unwrap_or_default();
                    let snap = r.state.get("child").cloned().and_then(|v| serde_json::from_value(v).ok());
                    (usize::try_from(index).unwrap_or(0), items, snap)
                }
                None => (0, Vec::new(), None),
            };
            let mut writes = Vec::new();
            let mut broke = false;
            while index < self.count && !broke {
                let mut pool = ctx.pool.clone();
                pool.insert(&ctx.node_id, "index", Value::Int(i64::try_from(index).unwrap_or(0)));
                let result = subgraph.run(SubgraphRequest { index, pool, resume: child_resume.take() }).await;
                match result {
                    SubgraphResult::Completed { pool, writes: w, broke: b, .. } => {
                        items.push(pool.get_or_null(&self.collect));
                        writes.extend(w);
                        broke = b;
                        index += 1;
                    }
                    SubgraphResult::Failed { error, .. } => return Err(error),
                    SubgraphResult::Paused { snapshot, .. } => {
                        return Ok(NodeOutcome::Paused(PauseRequest {
                            kind: "container".into(),
                            data: Value::Null,
                            state: json!({
                                "index": index,
                                "items": serde_json::to_value(&items).unwrap_or_default(),
                                "child": serde_json::to_value(&snapshot).unwrap_or_default(),
                            }),
                        }));
                    }
                }
            }
            let mut out = NodeOutput::new().with("items", Value::array(items));
            out.writes = writes;
            Ok(out.into())
        })
    }
}

/// Marks a loop break.
#[derive(Debug)]
struct LoopEnd;
impl Node for LoopEnd {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Ok(NodeOutput::new().into()) })
    }
}

/// Panics.
#[derive(Debug)]
struct Panicker;
impl Node for Panicker {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { panic!("boom") })
    }
}

fn cfg_str(doc: &NodeDoc, key: &str) -> String {
    doc.config.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_owned()
}

fn cfg_u64(doc: &NodeDoc, key: &str) -> u64 {
    doc.config.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

fn registry() -> Registry {
    let mut r = Registry::new();
    r.register("src", |_| Ok(Arc::new(Source)));
    r.register("scope-start", |_| Ok(Arc::new(ScopeStart)));
    r.register("echo", |d| {
        let writes = match d.config.get("writes") {
            Some(serde_json::Value::Object(m)) => {
                m.iter().filter_map(|(k, v)| Some((Selector::parse(k).ok()?, Value::from_json(v.clone())))).collect()
            }
            _ => Vec::new(),
        };
        Ok(Arc::new(Echo {
            template: Template::parse(&cfg_str(d, "template")),
            delay_ms: cfg_u64(d, "delay_ms"),
            chunks: d.config.get("chunks").and_then(|c| serde_json::from_value(c.clone()).ok()).unwrap_or_default(),
            usage: cfg_u64(d, "usage"),
            writes,
        }))
    });
    r.register("branch", |d| {
        Ok(Arc::new(Branch { handle: d.config.get("handle").and_then(|h| h.as_str()).map(Template::parse) }))
    });
    r.register("flaky", |d| {
        Ok(Arc::new(Flaky {
            fail_times: u32::try_from(cfg_u64(d, "fail_times")).unwrap_or(0),
            retryable: d.config.get("retryable").and_then(serde_json::Value::as_bool).unwrap_or(true),
            calls: AtomicU32::new(0),
        }))
    });
    r.register("pause", |_| Ok(Arc::new(Pauser)));
    r.register("answer", |d| Ok(Arc::new(Answer { text: Template::parse(&cfg_str(d, "text")) })));
    r.register("end", |d| Ok(Arc::new(End { name: cfg_str(d, "name"), value: Template::parse(&cfg_str(d, "value")) })));
    r.register("box", |d| {
        let collect = Selector::parse(&cfg_str(d, "collect")).map_err(|e| ConfigError::new(d, e.to_string()))?;
        Ok(Arc::new(BoxContainer { count: usize::try_from(cfg_u64(d, "count")).unwrap_or(0), collect }))
    });
    r.register("loop-end", |_| Ok(Arc::new(LoopEnd)));
    r.register("panic", |_| Ok(Arc::new(Panicker)));
    r
}

fn node(id: &str, kind: &str, config: serde_json::Value) -> NodeDoc {
    NodeDoc::new(id, kind, config)
}

fn edge(s: &str, t: &str) -> EdgeDoc {
    EdgeDoc::new(s, t)
}

fn build(nodes: Vec<NodeDoc>, edges: Vec<EdgeDoc>) -> Arc<Graph> {
    build_wf(Workflow { graph: GraphDoc { nodes, edges }, ..Workflow::default() })
}

fn build_wf(wf: Workflow) -> Arc<Graph> {
    Arc::new(Graph::build(wf, &registry()).expect("graph builds"))
}

async fn collect(engine: &Engine, input: RunInput) -> Vec<Event> {
    engine.run(input).collect().await
}

fn terminal(events: &[Event]) -> &Event {
    events.last().expect("at least one event")
}

fn outputs(events: &[Event]) -> IndexMap<String, Value> {
    match terminal(events) {
        Event::RunSucceeded { outputs, .. } => outputs.clone(),
        other => panic!("expected success, got {other:?}"),
    }
}

fn started(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::NodeStarted { node_id, frame, .. } if frame.is_empty() => Some(node_id.to_string()),
            _ => None,
        })
        .collect()
}

fn skipped(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::NodeSkipped { node_id, .. } => Some(node_id.to_string()),
            _ => None,
        })
        .collect()
}

fn paused_snapshot(events: &[Event]) -> Snapshot {
    match terminal(events) {
        Event::RunPaused { snapshot, .. } => (**snapshot).clone(),
        other => panic!("expected pause, got {other:?}"),
    }
}

/// Serialize and deserialize a snapshot, as a host persisting it would.
fn through_json(snapshot: &Snapshot) -> Snapshot {
    Snapshot::from_json(&snapshot.to_json().expect("serialize")).expect("deserialize")
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn linear_flow_passes_values() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("greet", "echo", json!({"template": "Hello {{#start.name#}}"})),
            node("end", "end", json!({"name": "greeting", "value": "{{#greet.out#}}"})),
        ],
        vec![edge("start", "greet"), edge("greet", "end")],
    );
    let events = collect(&Engine::new(g), RunInput::new().input("name", "Ada")).await;
    assert!(matches!(events.first(), Some(Event::RunStarted { resumed: false, .. })));
    assert_eq!(outputs(&events).get("greeting"), Some(&Value::from("Hello Ada")));
    assert_eq!(started(&events), ["start", "greet", "end"]);
}

#[tokio::test]
async fn inputs_do_not_shadow_sys() {
    let g = build(
        vec![node("start", "src", json!({})), node("end", "end", json!({"name": "q", "value": "{{#sys.query#}}"}))],
        vec![edge("start", "end")],
    );
    let input = RunInput::new().input("query", "from-input").sys("query", "from-sys");
    let events = collect(&Engine::new(g), input).await;
    assert_eq!(outputs(&events).get("q"), Some(&Value::from("from-sys")));
}

#[tokio::test]
async fn parallel_fan_out_runs_concurrently_and_joins() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("a", "echo", json!({"template": "A", "delay_ms": 80})),
            node("b", "echo", json!({"template": "B", "delay_ms": 80})),
            node("join", "end", json!({"name": "ab", "value": "{{#a.out#}}{{#b.out#}}"})),
        ],
        vec![edge("start", "a"), edge("start", "b"), edge("a", "join"), edge("b", "join")],
    );
    let t0 = std::time::Instant::now();
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert!(t0.elapsed() < Duration::from_millis(150), "branches must overlap");
    assert_eq!(outputs(&events).get("ab"), Some(&Value::from("AB")));
    // The join runs exactly once, after both branches.
    assert_eq!(started(&events).iter().filter(|n| *n == "join").count(), 1);
    assert_eq!(started(&events).last().map(String::as_str), Some("join"));
}

#[tokio::test]
async fn branch_skip_propagation_with_mixed_join() {
    // start -> if -(yes)-> y ; if -(no)-> n -> n2 ; y -> join ; n2 -> join
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("if", "branch", json!({"handle": "{{#start.pick#}}"})),
            node("y", "echo", json!({"template": "yes"})),
            node("n", "echo", json!({"template": "no"})),
            node("n2", "echo", json!({"template": "no2"})),
            node("join", "end", json!({"name": "r", "value": "{{#y.out#}}{{#n2.out#}}"})),
        ],
        vec![
            edge("start", "if"),
            edge("if", "y").with_handle("yes"),
            edge("if", "n").with_handle("no"),
            edge("n", "n2"),
            edge("y", "join"),
            edge("n2", "join"),
        ],
    );
    let events = collect(&Engine::new(g), RunInput::new().input("pick", "yes")).await;
    assert_eq!(outputs(&events).get("r"), Some(&Value::from("yes")));
    let mut sk = skipped(&events);
    sk.sort();
    assert_eq!(sk, ["n", "n2"]);
    assert!(!started(&events).contains(&"n".to_owned()));
    // The branch node reports its selection.
    assert!(events.iter().any(
        |e| matches!(e, Event::NodeSucceeded { node_id, branch: Some(b), .. } if node_id.as_ref() == "if" && b == "yes")
    ));
}

#[tokio::test]
async fn all_paths_skipped_skips_downstream() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("if", "branch", json!({"handle": "none"})),
            node("a", "echo", json!({"template": "a"})),
            node("b", "echo", json!({"template": "b"})),
        ],
        vec![edge("start", "if"), edge("if", "a").with_handle("x"), edge("a", "b")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert!(matches!(terminal(&events), Event::RunSucceeded { .. }));
    assert_eq!(skipped(&events), ["a", "b"]);
}

#[tokio::test]
async fn branch_without_selection_fails_node_not_engine() {
    let g = build(vec![node("start", "src", json!({})), node("if", "branch", json!({}))], vec![edge("start", "if")]);
    let events = collect(&Engine::new(g), RunInput::new()).await;
    match terminal(&events) {
        Event::RunFailed { error, .. } => assert!(error.contains("invalid_branch"), "{error}"),
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::NodeFailed { node_id, handled: false, .. } if node_id.as_ref() == "if"))
    );
}

#[tokio::test]
async fn answers_concatenate_into_run_outputs() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("a1", "answer", json!({"text": "Hello "})),
            node("a2", "answer", json!({"text": "world"})),
        ],
        vec![edge("start", "a1"), edge("a1", "a2")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert_eq!(outputs(&events).get("answer"), Some(&Value::from("Hello world")));
}

#[tokio::test]
async fn chunks_follow_node_started_and_precede_success() {
    let g = build(
        vec![node("start", "src", json!({})), node("llm", "echo", json!({"template": "ab", "chunks": ["a", "b"]}))],
        vec![edge("start", "llm")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    let pos = |pred: &dyn Fn(&Event) -> bool| events.iter().position(pred).expect("event present");
    let started = pos(&|e| matches!(e, Event::NodeStarted { node_id, .. } if node_id.as_ref() == "llm"));
    let chunk = pos(&|e| matches!(e, Event::NodeChunk { node_id, .. } if node_id.as_ref() == "llm"));
    let done = pos(&|e| matches!(e, Event::NodeSucceeded { node_id, .. } if node_id.as_ref() == "llm"));
    assert!(started < chunk && chunk < done);
}

#[tokio::test]
async fn usage_is_summed() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("a", "echo", json!({"template": "", "usage": 3})),
            node("b", "echo", json!({"template": "", "usage": 4})),
        ],
        vec![edge("start", "a"), edge("a", "b")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    match terminal(&events) {
        Event::RunSucceeded { usage, .. } => assert_eq!(usage.total_tokens, 7),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn variable_writes_are_applied_and_reported() {
    let g = build_wf(Workflow {
        conversation: vec![VariableDecl {
            name: "count".into(),
            ty: ValueType::Integer,
            value: None,
            description: None,
        }],
        graph: GraphDoc {
            nodes: vec![
                node("start", "src", json!({})),
                node("set", "echo", json!({"template": "", "writes": {"conversation.count": 5}})),
                node("end", "end", json!({"name": "c", "value": "{{#conversation.count#}}"})),
            ],
            edges: vec![edge("start", "set"), edge("set", "end")],
        },
        ..Workflow::default()
    });
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert_eq!(outputs(&events).get("c"), Some(&Value::Int(5)));
    assert!(events.iter().any(|e| matches!(e, Event::VariableUpdated { node_id, .. } if node_id.as_ref() == "set")));
}

#[tokio::test]
async fn env_decls_are_typed_and_secret() {
    let g = build_wf(Workflow {
        env: vec![
            VariableDecl {
                name: "KEY".into(),
                ty: ValueType::Secret,
                value: Some(Value::from("doc")),
                description: None,
            },
            VariableDecl { name: "N".into(), ty: ValueType::Integer, value: Some(Value::Int(1)), description: None },
        ],
        graph: GraphDoc {
            nodes: vec![
                node("start", "src", json!({})),
                node("e", "end", json!({"name": "key", "value": "{{#env.KEY#}}"})),
            ],
            edges: vec![edge("start", "e")],
        },
        ..Workflow::default()
    });
    let events = collect(&Engine::new(g.clone()), RunInput::new().env("KEY", "override")).await;
    match outputs(&events).get("key") {
        Some(Value::Secret(s)) => assert_eq!(s.as_ref(), "override"),
        other => panic!("expected secret, got {other:?}"),
    }
    // Wrong type for a declared env variable is rejected.
    let events = collect(&Engine::new(g), RunInput::new().env("N", "not a number")).await;
    assert!(matches!(terminal(&events), Event::RunFailed { .. }));
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fail_branch_routes_errors() {
    let policy = NodePolicy { on_error: ErrorStrategy::FailBranch, ..NodePolicy::default() };
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("f", "flaky", json!({"fail_times": 1})).with_policy(policy),
            node("ok", "echo", json!({"template": "ok"})),
            node("err", "end", json!({"name": "msg", "value": "{{#f.error_message#}}"})),
        ],
        vec![edge("start", "f"), edge("f", "ok"), edge("f", "err").with_handle("fail-branch")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert_eq!(outputs(&events).get("msg"), Some(&Value::from("failure 1")));
    assert_eq!(skipped(&events), ["ok"]);
    match terminal(&events) {
        Event::RunSucceeded { handled_errors, .. } => assert_eq!(*handled_errors, 1),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn fail_branch_success_skips_error_path() {
    let policy = NodePolicy { on_error: ErrorStrategy::FailBranch, ..NodePolicy::default() };
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("f", "flaky", json!({"fail_times": 0})).with_policy(policy),
            node("ok", "echo", json!({"template": "ok"})),
            node("err", "echo", json!({"template": "err"})),
        ],
        vec![edge("start", "f"), edge("f", "ok"), edge("f", "err").with_handle("fail-branch")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert_eq!(skipped(&events), ["err"]);
}

#[tokio::test]
async fn default_value_continues() {
    let mut values = IndexMap::new();
    values.insert("calls".to_owned(), Value::Int(-1));
    let policy = NodePolicy { on_error: ErrorStrategy::DefaultValue { values }, ..NodePolicy::default() };
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("f", "flaky", json!({"fail_times": 5})).with_policy(policy),
            node("end", "end", json!({"name": "calls", "value": "{{#f.calls#}}"})),
        ],
        vec![edge("start", "f"), edge("f", "end")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert_eq!(outputs(&events).get("calls"), Some(&Value::Int(-1)));
    assert!(events.iter().any(|e| matches!(e, Event::NodeFailed { handled: true, .. })));
}

#[tokio::test]
async fn default_value_on_branch_node_does_not_crash() {
    let policy =
        NodePolicy { on_error: ErrorStrategy::DefaultValue { values: IndexMap::new() }, ..NodePolicy::default() };
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("if", "branch", json!({})).with_policy(policy),
            node("a", "echo", json!({"template": "a"})),
        ],
        vec![edge("start", "if"), edge("if", "a").with_handle("x")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert!(matches!(terminal(&events), Event::RunSucceeded { handled_errors: 1, .. }));
    assert_eq!(skipped(&events), ["a"]);
}

#[tokio::test]
async fn retries_with_backoff_do_not_block_parallel_nodes() {
    let policy =
        NodePolicy { retry: RetryPolicy { max_retries: 2, interval_ms: 60, backoff: 1.0 }, ..NodePolicy::default() };
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("f", "flaky", json!({"fail_times": 2})).with_policy(policy),
            node("fast", "echo", json!({"template": "fast", "delay_ms": 5})),
        ],
        vec![edge("start", "f"), edge("start", "fast")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    let retries = events.iter().filter(|e| matches!(e, Event::NodeRetry { .. })).count();
    assert_eq!(retries, 2);
    let fast_done = events
        .iter()
        .position(|e| matches!(e, Event::NodeSucceeded { node_id, .. } if node_id.as_ref() == "fast"))
        .expect("fast done");
    let second_retry = events.iter().rposition(|e| matches!(e, Event::NodeRetry { .. })).expect("retry present");
    assert!(fast_done < second_retry, "the parallel node must finish while retries wait");
    match terminal(&events) {
        Event::RunSucceeded { handled_errors, .. } => {
            assert_eq!(*handled_errors, 0, "successful retry is not an error")
        }
        other => panic!("unexpected {other:?}"),
    }
    // Execution id is stable across attempts.
    let ids: Vec<&std::sync::Arc<str>> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeRetry { exec_id, .. } | Event::NodeSucceeded { exec_id, node_id: _, .. } => Some(exec_id),
            _ => None,
        })
        .collect();
    assert!(!ids.is_empty());
}

#[tokio::test]
async fn non_retryable_errors_skip_retries() {
    let policy =
        NodePolicy { retry: RetryPolicy { max_retries: 3, interval_ms: 0, backoff: 1.0 }, ..NodePolicy::default() };
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("f", "flaky", json!({"fail_times": 1, "retryable": false})).with_policy(policy),
        ],
        vec![edge("start", "f")],
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert!(!events.iter().any(|e| matches!(e, Event::NodeRetry { .. })));
    assert!(matches!(terminal(&events), Event::RunFailed { .. }));
}

#[cfg(feature = "tokio")] // needs a timer-capable clock
#[tokio::test]
async fn timeout_fails_slow_node() {
    let policy = NodePolicy { timeout_ms: Some(20), ..NodePolicy::default() };
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("slow", "echo", json!({"template": "x", "delay_ms": 5000})).with_policy(policy),
        ],
        vec![edge("start", "slow")],
    );
    let t0 = std::time::Instant::now();
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert!(t0.elapsed() < Duration::from_millis(1000));
    assert!(events.iter().any(|e| matches!(e, Event::NodeFailed { error_type, .. } if error_type == "timeout")));
}

#[tokio::test]
async fn panicking_node_fails_cleanly() {
    let g = build(vec![node("start", "src", json!({})), node("p", "panic", json!({}))], vec![edge("start", "p")]);
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert!(events.iter().any(|e| matches!(e, Event::NodeFailed { error_type, .. } if error_type == "panic")));
    assert!(matches!(terminal(&events), Event::RunFailed { .. }));
}

// ---------------------------------------------------------------------------
// Limits and control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_steps_stops_the_run() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("a", "echo", json!({"template": "a"})),
            node("b", "echo", json!({"template": "b"})),
        ],
        vec![edge("start", "a"), edge("a", "b")],
    );
    let engine = Engine::new(g).with_limits(Limits { max_steps: 2, ..Limits::default() });
    let events = collect(&engine, RunInput::new()).await;
    match terminal(&events) {
        Event::RunFailed { error, .. } => assert!(error.contains("step limit"), "{error}"),
        other => panic!("unexpected {other:?}"),
    }
}

#[cfg(feature = "tokio")] // needs a timer-capable clock
#[tokio::test]
async fn max_duration_stops_hung_run() {
    let g = build(
        vec![node("start", "src", json!({})), node("slow", "echo", json!({"template": "", "delay_ms": 10_000}))],
        vec![edge("start", "slow")],
    );
    let engine = Engine::new(g).with_limits(Limits { max_duration_ms: Some(30), ..Limits::default() });
    let events = collect(&engine, RunInput::new()).await;
    match terminal(&events) {
        Event::RunFailed { error, .. } => assert!(error.contains("duration"), "{error}"),
        other => panic!("unexpected {other:?}"),
    }
}

#[cfg(feature = "tokio")] // needs a timer-capable clock
#[tokio::test]
async fn abort_cancels_in_flight_node() {
    let g = build(
        vec![node("start", "src", json!({})), node("slow", "echo", json!({"template": "", "delay_ms": 10_000}))],
        vec![edge("start", "slow")],
    );
    let mut stream = Engine::new(g).run(RunInput::new());
    let handle = stream.handle();
    let t0 = std::time::Instant::now();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        if matches!(&e, Event::NodeStarted { node_id, .. } if node_id.as_ref() == "slow") {
            handle.abort("user cancelled");
        }
        events.push(e);
    }
    assert!(t0.elapsed() < Duration::from_millis(1000));
    assert!(matches!(terminal(&events), Event::RunAborted { reason, .. } if reason == "user cancelled"));
    assert!(!events.iter().any(|e| matches!(e, Event::NodeSucceeded { node_id, .. } if node_id.as_ref() == "slow")));
}

#[cfg(feature = "tokio")] // needs a timer-capable clock
#[tokio::test]
async fn external_variable_update_applies_to_root() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("wait", "echo", json!({"template": "", "delay_ms": 50})),
            node("end", "end", json!({"name": "v", "value": "{{#conversation.flag#}}"})),
        ],
        vec![edge("start", "wait"), edge("wait", "end")],
    );
    let mut stream = Engine::new(g).run(RunInput::new());
    let handle = stream.handle();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        if matches!(&e, Event::NodeStarted { node_id, .. } if node_id.as_ref() == "wait") {
            handle.update_variable(Selector::of("conversation", "flag"), Value::Bool(true));
        }
        events.push(e);
    }
    assert_eq!(outputs(&events).get("v"), Some(&Value::Bool(true)));
}

// ---------------------------------------------------------------------------
// Pause / resume
// ---------------------------------------------------------------------------

#[cfg(feature = "tokio")] // needs a timer-capable clock
#[tokio::test]
async fn external_pause_and_resume_round_trip() {
    let g = build_wf(Workflow {
        env: vec![VariableDecl { name: "TOKEN".into(), ty: ValueType::Secret, value: None, description: None }],
        graph: GraphDoc {
            nodes: vec![
                node("start", "src", json!({})),
                node("a", "echo", json!({"template": "A{{#start.x#}}", "delay_ms": 30})),
                node("b", "echo", json!({"template": "{{#a.out#}}B"})),
                node("end", "end", json!({"name": "r", "value": "{{#b.out#}}"})),
                node("tok", "end", json!({"name": "token", "value": "{{#env.TOKEN#}}"})),
            ],
            edges: vec![edge("start", "a"), edge("a", "b"), edge("b", "end"), edge("end", "tok")],
        },
        ..Workflow::default()
    });
    let engine = Engine::new(g);
    let mut stream = engine.run(RunInput::new().input("x", "1").env("TOKEN", "super-secret-token"));
    let handle = stream.handle();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        if matches!(&e, Event::NodeStarted { node_id, .. } if node_id.as_ref() == "a") {
            handle.pause("maintenance");
        }
        events.push(e);
    }
    let snapshot = paused_snapshot(&events);
    match terminal(&events) {
        Event::RunPaused { reasons, .. } => assert_eq!(reasons.first().map(|r| r.kind.as_str()), Some("external")),
        other => panic!("unexpected {other:?}"),
    }
    // In-flight `a` finished; `b` is pending.
    assert!(events.iter().any(|e| matches!(e, Event::NodeSucceeded { node_id, .. } if node_id.as_ref() == "a")));
    assert!(!started(&events).contains(&"b".to_owned()));

    let json_text = snapshot.to_json().expect("json");
    assert!(!json_text.contains("super-secret-token"), "secrets must not be persisted");

    let restored = through_json(&snapshot);
    let events =
        collect(&engine, RunInput::new().env("TOKEN", "super-secret-token").resume(restored, IndexMap::new())).await;
    assert!(matches!(events.first(), Some(Event::RunStarted { resumed: true, .. })));
    assert_eq!(started(&events), ["b", "end", "tok"]);
    let out = outputs(&events);
    assert_eq!(out.get("r"), Some(&Value::from("A1B")));
    // Env secrets are re-supplied on resume and stay typed as secrets.
    match out.get("token") {
        Some(Value::Secret(s)) => assert_eq!(s.as_ref(), "super-secret-token"),
        other => panic!("expected secret, got {other:?}"),
    }
}

#[tokio::test]
async fn node_pause_lets_other_branches_finish_then_resumes_with_input() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("ask", "pause", json!({})),
            node("other", "echo", json!({"template": "other", "delay_ms": 20})),
            node("end", "end", json!({"name": "r", "value": "{{#ask.answer#}}/{{#other.out#}}"})),
        ],
        vec![edge("start", "ask"), edge("start", "other"), edge("ask", "end"), edge("other", "end")],
    );
    let engine = Engine::new(g);
    let events = collect(&engine, RunInput::new()).await;
    assert!(events.iter().any(|e| matches!(e, Event::NodePaused { node_id, .. } if node_id.as_ref() == "ask")));
    assert!(events.iter().any(|e| matches!(e, Event::NodeSucceeded { node_id, .. } if node_id.as_ref() == "other")));
    match terminal(&events) {
        Event::RunPaused { reasons, .. } => {
            assert_eq!(reasons.len(), 1);
            assert_eq!(reasons[0].kind, "human_input");
            assert_eq!(reasons[0].node_id.as_deref(), Some("ask"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let snapshot = through_json(&paused_snapshot(&events));
    let mut inputs = IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from("42"));
    let events = collect(&engine, RunInput::new().resume(snapshot, inputs)).await;
    assert_eq!(started(&events), ["ask", "end"]);
    assert_eq!(outputs(&events).get("r"), Some(&Value::from("42/other")));
}

#[tokio::test]
async fn snapshot_from_other_graph_is_rejected() {
    let g1 = build(vec![node("start", "src", json!({})), node("ask", "pause", json!({}))], vec![edge("start", "ask")]);
    let g2 = build(
        vec![node("start", "src", json!({})), node("ask", "pause", json!({})), node("x", "echo", json!({}))],
        vec![edge("start", "ask"), edge("ask", "x")],
    );
    let events = collect(&Engine::new(g1), RunInput::new()).await;
    let snapshot = paused_snapshot(&events);
    let events = collect(&Engine::new(g2), RunInput::new().resume(snapshot.clone(), IndexMap::new())).await;
    match terminal(&events) {
        Event::RunFailed { error, .. } => assert!(error.contains("fingerprint"), "{error}"),
        other => panic!("unexpected {other:?}"),
    }
    let mut bad = snapshot;
    bad.format = 99;
    let g1b = build(vec![node("start", "src", json!({})), node("ask", "pause", json!({}))], vec![edge("start", "ask")]);
    let events = collect(&Engine::new(g1b), RunInput::new().resume(bad, IndexMap::new())).await;
    assert!(matches!(terminal(&events), Event::RunFailed { error, .. } if error.contains("format")));
}

#[tokio::test]
async fn steps_persist_across_resume() {
    let g = build(
        vec![
            node("start", "src", json!({})),
            node("ask", "pause", json!({})),
            node("a", "echo", json!({"template": ""})),
        ],
        vec![edge("start", "ask"), edge("ask", "a")],
    );
    let engine = Engine::new(g).with_limits(Limits { max_steps: 3, ..Limits::default() });
    let events = collect(&engine, RunInput::new()).await;
    let snapshot = paused_snapshot(&events);
    assert_eq!(snapshot.steps, 2);
    // Resuming needs 2 more steps (ask, a) but only 1 is left.
    let events = collect(&engine, RunInput::new().resume(snapshot, IndexMap::new())).await;
    assert!(matches!(terminal(&events), Event::RunFailed { error, .. } if error.contains("step limit")));
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

fn container_graph(body: Vec<NodeDoc>, body_edges: Vec<EdgeDoc>, count: u64, collect: &str) -> Arc<Graph> {
    let mut nodes = vec![
        node("start", "src", json!({})),
        node("box", "box", json!({"count": count, "collect": collect})),
        node("end", "end", json!({"name": "items", "value": "{{#box.items#}}"})),
    ];
    nodes.extend(body.into_iter().map(|n| n.with_parent("box")));
    let mut edges = vec![edge("start", "box"), edge("box", "end")];
    edges.extend(body_edges);
    build(nodes, edges)
}

#[tokio::test]
async fn container_runs_body_frames() {
    let g = container_graph(
        vec![node("bs", "scope-start", json!({})), node("dbl", "echo", json!({"template": "#{{#box.index#}}"}))],
        vec![edge("bs", "dbl")],
        3,
        "dbl.out",
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert_eq!(
        outputs(&events).get("items"),
        Some(&Value::array([Value::from("#0"), Value::from("#1"), Value::from("#2")]))
    );
    let frames: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeStarted { node_id, frame, .. } if node_id.as_ref() == "dbl" => Some(frame.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(frames, ["box[0]", "box[1]", "box[2]"]);
}

#[tokio::test]
async fn container_body_failure_fails_container() {
    let g = container_graph(
        vec![node("bs", "scope-start", json!({})), node("bad", "flaky", json!({"fail_times": 10}))],
        vec![edge("bs", "bad")],
        2,
        "bad.calls",
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    assert!(events.iter().any(
        |e| matches!(e, Event::NodeFailed { node_id, frame, .. } if node_id.as_ref() == "bad" && frame.as_ref() == "box[0]")
    ));
    assert!(events.iter().any(|e| matches!(e, Event::NodeFailed { node_id, .. } if node_id.as_ref() == "box")));
    assert!(matches!(terminal(&events), Event::RunFailed { .. }));
}

#[tokio::test]
async fn container_external_writes_and_loop_end() {
    let g = container_graph(
        vec![
            node("bs", "scope-start", json!({})),
            node("w", "echo", json!({"template": "{{#box.index#}}", "writes": {"conversation.last": "seen"}})),
            node("stop", "loop-end", json!({})),
        ],
        vec![edge("bs", "w"), edge("w", "stop")],
        5,
        "w.out",
    );
    let events = collect(&Engine::new(g), RunInput::new()).await;
    // loop-end breaks after the first body run.
    assert_eq!(outputs(&events).get("items"), Some(&Value::array([Value::Int(0)])));
    // The write to `conversation` was forwarded to the root frame.
    assert!(events.iter().any(
        |e| matches!(e, Event::VariableUpdated { node_id, frame, .. } if node_id.as_ref() == "box" && frame.is_empty())
    ));
}

#[tokio::test]
async fn container_depth_limit() {
    let g = container_graph(
        vec![node("bs", "scope-start", json!({})), node("x", "echo", json!({"template": "x"}))],
        vec![edge("bs", "x")],
        1,
        "x.out",
    );
    let engine = Engine::new(g).with_limits(Limits { max_depth: 0, ..Limits::default() });
    let events = collect(&engine, RunInput::new()).await;
    assert!(matches!(terminal(&events), Event::RunFailed { error, .. } if error.contains("depth")));
}

#[tokio::test]
async fn container_pause_and_resume() {
    let g = container_graph(
        vec![node("bs", "scope-start", json!({})), node("ask", "pause", json!({}))],
        vec![edge("bs", "ask")],
        2,
        "ask.answer",
    );
    let engine = Engine::new(g);
    let events = collect(&engine, RunInput::new()).await;
    match terminal(&events) {
        Event::RunPaused { reasons, .. } => {
            assert_eq!(reasons.len(), 1, "only the inner node reports a reason");
            assert_eq!(reasons[0].frame.as_ref(), "box[0]");
        }
        other => panic!("unexpected {other:?}"),
    }
    // Round 0 resumes with the answer; round 1 pauses again.
    let mut inputs = IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from("first"));
    let snapshot = through_json(&paused_snapshot(&events));
    let events = collect(&engine, RunInput::new().resume(snapshot, inputs)).await;
    assert!(matches!(terminal(&events), Event::RunPaused { .. }));
    let mut inputs = IndexMap::new();
    inputs.insert("ask".to_owned(), Value::from("second"));
    let snapshot = through_json(&paused_snapshot(&events));
    let events = collect(&engine, RunInput::new().resume(snapshot, inputs)).await;
    assert_eq!(outputs(&events).get("items"), Some(&Value::array([Value::from("first"), Value::from("second")])));
}

#[tokio::test]
async fn graph_can_run_concurrently() {
    let g = build(
        vec![node("start", "src", json!({})), node("e", "end", json!({"name": "n", "value": "{{#start.n#}}"}))],
        vec![edge("start", "e")],
    );
    let engine = Engine::new(g);
    let runs = (0..20).map(|i| {
        let engine = engine.clone();
        async move { engine.run_to_end(RunInput::new().input("n", i64::from(i))).await }
    });
    let results = futures::future::join_all(runs).await;
    for (i, ev) in results.iter().enumerate() {
        match ev {
            Event::RunSucceeded { outputs, .. } => {
                assert_eq!(outputs.get("n"), Some(&Value::Int(i64::try_from(i).unwrap())))
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn run_stream_is_send_and_static() {
    fn assert_send_static<T: Send + 'static>(_: &T) {}
    let g = build(vec![node("start", "src", json!({}))], vec![]);
    let stream = Engine::new(g).run(RunInput::new());
    assert_send_static(&stream);
}

#[test]
fn runs_without_any_async_runtime() {
    let g = build(
        vec![node("start", "src", json!({})), node("e", "end", json!({"name": "n", "value": "{{#start.n#}}"}))],
        vec![edge("start", "e")],
    );
    let engine = Engine::new(g).with_clock(Arc::new(graphrt_core::node::NoTimerClock));
    // A bare current-thread runtime without the timer driver: the engine needs no runtime services.
    let rt = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let ev = rt.block_on(engine.run_to_end(RunInput::new().input("n", 7)));
    assert!(matches!(ev, Event::RunSucceeded { outputs, .. } if outputs.get("n") == Some(&Value::Int(7))));
}
