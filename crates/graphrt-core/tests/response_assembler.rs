//! Ordered response assembly over synthetic event sequences.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::{EdgeDoc, GraphDoc, NodeDoc, Workflow};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Node, NodeContext, NodeOutput, NodeResult, NodeRole, Registry};
use graphrt_core::pool::Selector;
use graphrt_core::response::{ResponseAssembler, ResponseChunk};
use graphrt_core::value::Value;
use indexmap::IndexMap;
use serde_json::json;

#[derive(Debug)]
struct Dummy(NodeRole);

impl Node for Dummy {
    fn role(&self) -> NodeRole {
        self.0
    }
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Ok(NodeOutput::new().into()) })
    }
}

fn registry() -> Registry {
    let mut r = Registry::new();
    for (kind, role) in [
        ("start", NodeRole::Root),
        ("llm", NodeRole::Executable),
        ("code", NodeRole::Executable),
        ("if-else", NodeRole::Branch),
        ("answer", NodeRole::Response),
        ("end", NodeRole::Response),
    ] {
        r.register(kind, move |_| Ok(Arc::new(Dummy(role)) as Arc<dyn Node>));
    }
    r
}

fn graph(nodes: Vec<NodeDoc>, edges: Vec<EdgeDoc>) -> Arc<Graph> {
    let wf = Workflow { graph: GraphDoc { nodes, edges }, ..Workflow::default() };
    Arc::new(Graph::build(wf, &registry()).unwrap())
}

fn n(id: &str, kind: &str) -> NodeDoc {
    NodeDoc::new(id, kind, json!({}))
}

fn answer(id: &str, template: &str) -> NodeDoc {
    NodeDoc::new(id, "answer", json!({ "answer": template }))
}

fn e(a: &str, b: &str) -> EdgeDoc {
    EdgeDoc::new(a, b)
}

fn started(id: &str) -> Event {
    Event::NodeStarted {
        frame: Arc::from(""),
        node_id: id.into(),
        exec_id: format!("{id}-x"),
        kind: "x".into(),
        title: id.into(),
        attempt: 1,
    }
}

fn chunk(id: &str, text: &str) -> Event {
    chunk_in("", id, text)
}

fn chunk_in(frame: &str, id: &str, text: &str) -> Event {
    Event::NodeChunk {
        frame: Arc::from(frame),
        node_id: id.into(),
        exec_id: format!("{id}-x"),
        field: "text".into(),
        chunk: text.into(),
    }
}

fn ok(id: &str, outputs: &[(&str, Value)]) -> Event {
    ok_branch(id, outputs, None)
}

fn ok_branch(id: &str, outputs: &[(&str, Value)], branch: Option<&str>) -> Event {
    Event::NodeSucceeded {
        frame: Arc::from(""),
        node_id: id.into(),
        exec_id: format!("{id}-x"),
        outputs: outputs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect::<IndexMap<_, _>>(),
        branch: branch.map(str::to_owned),
        usage: None,
        metadata: IndexMap::new(),
        elapsed_ms: 1,
    }
}

fn skipped(id: &str) -> Event {
    Event::NodeSkipped { frame: Arc::from(""), node_id: id.into() }
}

fn run_done() -> Event {
    Event::RunSucceeded { outputs: IndexMap::new(), usage: Default::default(), handled_errors: 0, elapsed_ms: 1 }
}

fn texts(chunks: &[ResponseChunk]) -> String {
    chunks.iter().map(|c| c.text.as_str()).collect()
}

#[test]
fn streams_llm_tokens_through_single_answer() {
    let g = graph(
        vec![n("start", "start"), n("llm", "llm"), answer("ans", "Bot: {{#llm.text#}}!")],
        vec![e("start", "llm"), e("llm", "ans")],
    );
    let mut a = ResponseAssembler::new(&g);
    assert_eq!(texts(&a.push(&started("start"))), "Bot: ");
    a.push(&ok("start", &[]));
    a.push(&started("llm"));
    assert_eq!(texts(&a.push(&chunk("llm", "Hel"))), "Hel");
    assert_eq!(texts(&a.push(&chunk("llm", "lo"))), "lo");
    let out = a.push(&ok("llm", &[("text", Value::from("Hello"))]));
    assert_eq!(texts(&out), "!");
    assert!(out.last().is_some_and(|c| c.is_final && c.node_id == "ans"));
    a.push(&started("ans"));
    a.push(&ok("ans", &[]));
    assert!(a.push(&run_done()).is_empty());
    assert_eq!(a.text_so_far(), "Bot: Hello!");
}

#[test]
fn second_answer_buffers_until_first_completes() {
    // start -> llm1 -> a1 -> llm2 -> a2, but also start -> llm2 path ordering via a1.
    // Here llm2 runs in parallel (start -> llm2) and finishes first.
    let g = graph(
        vec![
            n("start", "start"),
            n("llm1", "llm"),
            n("llm2", "llm"),
            answer("a1", "[{{#llm1.text#}}]"),
            answer("a2", "<{{#llm2.text#}}>"),
        ],
        vec![e("start", "llm1"), e("start", "llm2"), e("llm1", "a1"), e("a1", "a2"), e("llm2", "a2")],
    );
    let mut a = ResponseAssembler::new(&g);
    let mut all = String::new();
    for ev in [
        started("start"),
        ok("start", &[]),
        started("llm1"),
        started("llm2"),
        chunk("llm2", "two"),
        ok("llm2", &[("text", Value::from("two"))]),
        chunk("llm1", "one"),
    ] {
        all.push_str(&texts(&a.push(&ev)));
    }
    assert_eq!(all, "[one");
    all.push_str(&texts(&a.push(&ok("llm1", &[("text", Value::from("one"))]))));
    assert_eq!(all, "[one]<two>");
}

#[test]
fn skipped_branch_answer_renders_nothing() {
    let g = graph(
        vec![n("start", "start"), n("if", "if-else"), answer("yes", "YES"), answer("no", "NO")],
        vec![e("start", "if"), e("if", "yes").with_handle("true"), e("if", "no").with_handle("false")],
    );
    let mut a = ResponseAssembler::new(&g);
    let mut all = String::new();
    for ev in [started("start"), ok("start", &[]), started("if")] {
        all.push_str(&texts(&a.push(&ev)));
    }
    assert_eq!(all, "", "nothing is certain before the branch decides");
    all.push_str(&texts(&a.push(&ok_branch("if", &[], Some("false")))));
    all.push_str(&texts(&a.push(&skipped("yes"))));
    all.push_str(&texts(&a.push(&run_done())));
    assert_eq!(all, "NO");
}

#[test]
fn non_streaming_value_emitted_on_completion_with_path() {
    let g = graph(
        vec![n("start", "start"), n("code", "code"), answer("ans", "n={{#code.result.count#}} q={{#sys.query#}}")],
        vec![e("start", "code"), e("code", "ans")],
    );
    let q = Selector::parse("sys.query").unwrap();
    let mut a = ResponseAssembler::new(&g).with_value(&q, Value::from("hi"));
    let mut all = String::new();
    for ev in [started("start"), ok("start", &[]), started("code")] {
        all.push_str(&texts(&a.push(&ev)));
    }
    assert_eq!(all, "n=");
    let result = Value::from_json(json!({"count": 3}));
    all.push_str(&texts(&a.push(&ok("code", &[("result", result)]))));
    assert_eq!(all, "n=3 q=hi");
}

#[test]
fn parallel_branches_join_into_one_answer() {
    let g = graph(
        vec![n("start", "start"), n("l1", "llm"), n("l2", "llm"), answer("ans", "{{#l1.text#}}+{{#l2.text#}}")],
        vec![e("start", "l1"), e("start", "l2"), e("l1", "ans"), e("l2", "ans")],
    );
    let mut a = ResponseAssembler::new(&g);
    let mut all = String::new();
    for ev in [
        started("start"),
        ok("start", &[]),
        started("l1"),
        started("l2"),
        chunk("l2", "B"),
        chunk("l1", "A"),
        ok("l1", &[("text", Value::from("A"))]),
        ok("l2", &[("text", Value::from("B"))]),
    ] {
        all.push_str(&texts(&a.push(&ev)));
    }
    assert_eq!(all, "A+B");
}

#[test]
fn child_frames_and_unreferenced_chunks() {
    let g = graph(
        vec![n("start", "start"), n("llm", "llm"), n("other", "llm"), answer("ans", "{{#llm.text#}}")],
        vec![e("start", "llm"), e("start", "other"), e("llm", "ans")],
    );
    let mut a = ResponseAssembler::new(&g);
    a.push(&started("start"));
    assert!(a.push(&chunk("other", "x")).is_empty());
    assert!(a.push(&chunk_in("it[0]", "llm", "child")).is_empty());

    let mut p = ResponseAssembler::new(&g).pass_through_unreferenced(true);
    p.push(&started("start"));
    let out = p.push(&chunk("other", "x"));
    assert_eq!(out, vec![ResponseChunk { node_id: "other".into(), text: "x".into(), is_final: false }]);
}

#[test]
fn end_nodes_optional_and_flush_on_failure() {
    let end = NodeDoc::new(
        "end",
        "end",
        json!({"outputs": [{"variable": "a", "value_selector": ["llm", "text"]}, {"variable": "b", "value_selector": "llm.extra"}]}),
    );
    let g = graph(vec![n("start", "start"), n("llm", "llm"), end], vec![e("start", "llm"), e("llm", "end")]);

    let mut silent = ResponseAssembler::new(&g);
    silent.push(&started("start"));
    assert!(silent.push(&chunk("llm", "x")).is_empty());

    let mut a = ResponseAssembler::new(&g).include_end(true);
    a.push(&started("start"));
    a.push(&ok("start", &[]));
    assert_eq!(texts(&a.push(&chunk("llm", "partial"))), "partial");
    let out = a.push(&Event::RunFailed { error: "boom".into(), usage: Default::default(), elapsed_ms: 1 });
    assert_eq!(texts(&out), "\n");
    assert!(out.last().is_some_and(|c| c.is_final));
    assert!(a.push(&chunk("llm", "late")).is_empty());
    assert_eq!(a.text_so_far(), "partial\n");
}

// ---- retries and handled failures -------------------------------------------------------

use graphrt_core::dsl::{ErrorStrategy, NodePolicy, RetryPolicy};

fn retry_policy() -> NodePolicy {
    NodePolicy { retry: RetryPolicy { max_retries: 2, interval_ms: 0, backoff: 1.0 }, ..NodePolicy::default() }
}

fn retry(id: &str) -> Event {
    Event::NodeRetry {
        frame: Arc::from(""),
        node_id: id.into(),
        exec_id: format!("{id}-x"),
        attempt: 2,
        error: "network".into(),
        delay_ms: 0,
    }
}

fn handled(id: &str, outputs: &[(&str, Value)]) -> Event {
    Event::NodeFailed {
        frame: Arc::from(""),
        node_id: id.into(),
        exec_id: format!("{id}-x"),
        error: "boom".into(),
        error_type: "model".into(),
        handled: true,
        outputs: outputs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect::<IndexMap<_, _>>(),
        elapsed_ms: 1,
    }
}

fn feed(a: &mut ResponseAssembler, events: &[Event]) -> String {
    events.iter().map(|ev| texts(&a.push(ev))).collect()
}

#[test]
fn retried_node_never_duplicates_partial_attempt() {
    let g = graph(
        vec![n("start", "start"), n("llm", "llm").with_policy(retry_policy()), answer("ans", "A:{{#llm.text#}}.")],
        vec![e("start", "llm"), e("llm", "ans")],
    );
    let mut a = ResponseAssembler::new(&g);
    let before = feed(&mut a, &[started("start"), ok("start", &[]), started("llm"), chunk("llm", "Hel")]);
    assert_eq!(before, "A:", "retry-enabled nodes are not streamed live");
    let rest = feed(
        &mut a,
        &[
            retry("llm"),
            chunk("llm", "Hello"),
            chunk("llm", " world"),
            ok("llm", &[("text", Value::from("Hello world"))]),
        ],
    );
    assert_eq!(rest, "Hello world.");
    assert_eq!(a.text_so_far(), "A:Hello world.");
}

#[test]
fn retry_then_default_value_emits_default_not_partial() {
    let policy = NodePolicy {
        retry: RetryPolicy { max_retries: 1, interval_ms: 0, backoff: 1.0 },
        on_error: ErrorStrategy::DefaultValue {
            values: [("text".to_owned(), Value::from("fallback"))].into_iter().collect(),
        },
        ..NodePolicy::default()
    };
    let g = graph(
        vec![n("start", "start"), n("llm", "llm").with_policy(policy), answer("ans", "[{{#llm.text#}}]")],
        vec![e("start", "llm"), e("llm", "ans")],
    );
    let mut a = ResponseAssembler::new(&g);
    let all = feed(
        &mut a,
        &[
            started("start"),
            ok("start", &[]),
            started("llm"),
            chunk("llm", "par"),
            retry("llm"),
            chunk("llm", "tial"),
            handled("llm", &[("text", Value::from("fallback")), ("error_message", Value::from("boom"))]),
            run_done(),
        ],
    );
    assert_eq!(all, "[fallback]");
}

#[test]
fn fail_branch_failure_renders_nothing_for_variable() {
    let policy = NodePolicy { on_error: ErrorStrategy::FailBranch, ..NodePolicy::default() };
    let g = graph(
        vec![
            n("start", "start"),
            n("llm", "llm").with_policy(policy),
            answer("ok", "ok:{{#llm.text#}}"),
            answer("err", "err:{{#llm.error_message#}}"),
        ],
        vec![e("start", "llm"), e("llm", "ok"), e("llm", "err").with_handle("fail-branch")],
    );
    let mut a = ResponseAssembler::new(&g);
    let all = feed(
        &mut a,
        &[
            started("start"),
            ok("start", &[]),
            started("llm"),
            chunk("llm", "partial"),
            handled("llm", &[("error_message", Value::from("boom"))]),
            skipped("ok"),
            run_done(),
        ],
    );
    assert_eq!(all, "err:boom");
}

#[test]
fn branch_default_value_follows_named_handle_for_ordering() {
    let policy = NodePolicy {
        on_error: ErrorStrategy::DefaultValue {
            values: [("branch".to_owned(), Value::from("b"))].into_iter().collect(),
        },
        ..NodePolicy::default()
    };
    let g = graph(
        vec![
            n("start", "start"),
            n("if", "if-else").with_policy(policy),
            answer("x", "X"),
            answer("y", "Y"),
            n("llm", "llm"),
            answer("z", "{{#llm.text#}}"),
        ],
        vec![
            e("start", "if"),
            e("if", "x").with_handle("a"),
            e("if", "y").with_handle("b"),
            e("start", "llm"),
            e("llm", "z"),
        ],
    );
    let mut a = ResponseAssembler::new(&g);
    let mut all = feed(
        &mut a,
        &[
            started("start"),
            ok("start", &[]),
            started("llm"),
            chunk("llm", "zz"),
            ok("llm", &[("text", Value::from("zz"))]),
        ],
    );
    assert_eq!(all, "zz");
    // The engine follows only handle `b`: `x` (handle `a`) must not be treated as certain.
    all.push_str(&feed(&mut a, &[started("if"), handled("if", &[])]));
    assert_eq!(all, "zzY");
    all.push_str(&feed(&mut a, &[skipped("x"), run_done()]));
    assert_eq!(all, "zzY");
}

// ---- resume -----------------------------------------------------------------------------

use graphrt_core::pool::VariablePool;
use graphrt_core::snapshot::{EdgeStatus, FrameSnapshot, NodeSnapshot, NodeStatus};

fn edge_id(a: &str, b: &str, id: &str) -> EdgeDoc {
    let mut edge = EdgeDoc::new(a, b);
    edge.id = Some(id.to_owned());
    edge
}

fn node_snap(status: NodeStatus) -> NodeSnapshot {
    NodeSnapshot { status, exec_id: "x".into(), attempt: 1, resume_state: None }
}

/// start(amount) -> review (pauses) -> answer; a second answer `intro` rendered before the pause.
fn approval_graph() -> Arc<Graph> {
    graph(
        vec![
            n("start", "start"),
            answer("intro", "Reviewing {{#start.amount#}}. "),
            n("review", "if-else"),
            answer("ans", "Approved {{#start.amount#}} ({{#review.note#}})"),
        ],
        vec![
            edge_id("start", "intro", "e1"),
            edge_id("intro", "review", "e2"),
            edge_id("review", "ans", "e3").with_handle("approve"),
        ],
    )
}

fn paused_snapshot() -> FrameSnapshot {
    let mut pool = VariablePool::new();
    pool.insert("start", "amount", Value::Int(100));
    let mut snap = FrameSnapshot { pool: pool.to_snapshot(), ..FrameSnapshot::default() };
    snap.nodes.insert("start".into(), node_snap(NodeStatus::Succeeded));
    snap.nodes.insert("intro".into(), node_snap(NodeStatus::Succeeded));
    snap.nodes.insert("review".into(), node_snap(NodeStatus::Paused));
    snap.edges.insert("e1".into(), EdgeStatus::Taken);
    snap.edges.insert("e2".into(), EdgeStatus::Taken);
    snap.edges.insert("e3".into(), EdgeStatus::Unknown);
    snap
}

#[test]
fn resume_uses_snapshot_values_and_skips_rendered_answers() {
    let g = approval_graph();
    let mut a = ResponseAssembler::new(&g).resume_from(&paused_snapshot());
    let all = feed(
        &mut a,
        &[
            started("review"),
            ok_branch("review", &[("note", Value::from("fine"))], Some("approve")),
            started("ans"),
            ok("ans", &[]),
            run_done(),
        ],
    );
    assert_eq!(all, "Approved 100 (fine)", "start never re-runs; its outputs come from the snapshot");
}

#[test]
fn resume_without_snapshot_would_lose_values() {
    // Guard for the test above: without `resume_from` the start output is unknown.
    let g = approval_graph();
    let mut a = ResponseAssembler::new(&g);
    let all = feed(
        &mut a,
        &[ok_branch("review", &[("note", Value::from("fine"))], Some("approve")), ok("ans", &[]), run_done()],
    );
    assert!(all.contains("Approved  (fine)"));
}
