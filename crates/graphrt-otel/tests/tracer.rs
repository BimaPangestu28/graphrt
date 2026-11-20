//! Span tree shape for real runs through the engine.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::engine::RunInput;
use graphrt_core::node::{NodeResult, Registry};
use graphrt_core::{Engine, Graph, Node, NodeContext, NodeError, NodeOutput, Value};
use graphrt_otel::{AttrValue, EventTracer, InMemoryExporter, Metrics, Span, StatusCode, TraceParent};

#[derive(Debug, Default)]
struct Flaky {
    calls: AtomicU32,
}

impl Node for Flaky {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(NodeError::new("transient", "first attempt fails"))
            } else {
                Ok(NodeOutput::new().with("ok", true).into())
            }
        })
    }
}

#[derive(Debug)]
struct Boom;

impl Node for Boom {
    fn run(&self, _ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async { Err(NodeError::fatal("kaput", "always fails")) })
    }
}

#[derive(Debug)]
struct Secretive;

impl Node for Secretive {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            ctx.progress("step", Value::from("working"));
            Ok(NodeOutput::new()
                .with("token", Value::Secret(Arc::from("sk-super-secret")))
                .with("text", "visible")
                .usage(graphrt_core::Usage { prompt_tokens: 7, completion_tokens: 3, total_tokens: 10 })
                .into())
        })
    }
}

fn registry() -> Registry {
    let mut r = Registry::with_builtins();
    r.register("flaky", |_| Ok(Arc::new(Flaky::default())));
    r.register("boom", |_| Ok(Arc::new(Boom)));
    r.register("secretive", |_| Ok(Arc::new(Secretive)));
    r
}

async fn trace(yaml: &str, input: RunInput, configure: impl FnOnce(EventTracer) -> EventTracer) -> Vec<Span> {
    let graph = Arc::new(Graph::from_text(yaml, &registry()).unwrap());
    let exporter = InMemoryExporter::new();
    let mut tracer = configure(EventTracer::new(Arc::new(exporter.clone())).with_graph(graph.clone()));
    let engine = Engine::new(graph);
    let mut run = engine.run(input);
    while let Some(event) = run.next().await {
        tracer.on_event(&event);
    }
    drop(tracer);
    exporter.take()
}

fn by_name<'a>(spans: &'a [Span], name: &str) -> &'a Span {
    spans.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no span `{name}` in {:?}", names(spans)))
}

fn names(spans: &[Span]) -> Vec<&str> {
    spans.iter().map(|s| s.name.as_str()).collect()
}

fn root(spans: &[Span]) -> &Span {
    spans.iter().find(|s| s.name.starts_with("invoke_workflow ")).unwrap()
}

fn str_attr<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
    match span.attr(key) {
        Some(AttrValue::Str(s)) => Some(s),
        _ => None,
    }
}

const LINEAR: &str = r#"
name: linear
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: name, type: string }] }
    - { id: greet, type: template-transform, title: Greet, template: "hi {{ n }}", variables: [{ variable: n, value_selector: [start, name] }] }
    - { id: end, type: end, outputs: [{ variable: g, value_selector: [greet, output] }] }
  edges:
    - { source: start, target: greet }
    - { source: greet, target: end }
"#;

#[tokio::test]
async fn linear_run_has_root_and_node_children() {
    let spans = trace(LINEAR, RunInput::new().input("name", "ada"), |t| t).await;
    assert_eq!(spans.len(), 4, "{:?}", names(&spans));
    let root = root(&spans);
    assert_eq!(root.name, "invoke_workflow linear");
    assert!(root.parent_span_id.is_none());
    assert_eq!(str_attr(root, "graphrt.run.status"), Some("succeeded"));
    assert!(str_attr(root, "graphrt.workflow.fingerprint").is_some());
    assert!(str_attr(root, "graphrt.run.id").is_some());
    let greet = by_name(&spans, "Greet");
    assert_eq!(greet.parent_span_id, Some(root.span_id));
    assert_eq!(greet.trace_id, root.trace_id);
    assert_eq!(str_attr(greet, "graphrt.node.kind"), Some("template-transform"));
    assert_eq!(str_attr(greet, "graphrt.node.status"), Some("succeeded"));
    assert!(greet.end_unix_nano >= greet.start_unix_nano);
    // No content captured by default.
    assert!(greet.attr("graphrt.node.outputs").is_none());
}

const BRANCH: &str = r#"
name: branch
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: x, type: number }] }
    - id: check
      type: if-else
      cases:
        - case_id: big
          logical_operator: and
          conditions: [{ variable_selector: [start, x], comparison_operator: ">", value: "5" }]
    - { id: yes, type: template-transform, template: "big", variables: [] }
    - { id: no, type: template-transform, template: "small", variables: [] }
  edges:
    - { source: start, target: check }
    - { source: check, target: yes, handle: big }
    - { source: check, target: no, handle: "false" }
"#;

#[tokio::test]
async fn branch_records_handle_and_skips() {
    let spans = trace(BRANCH, RunInput::new().input("x", 9), |t| t).await;
    let check = by_name(&spans, "check");
    assert_eq!(str_attr(check, "graphrt.node.branch"), Some("big"));
    assert!(spans.iter().all(|s| s.name != "no"));
    let root = root(&spans);
    let skipped = root.events.iter().find(|e| e.name == "node_skipped").unwrap();
    assert!(skipped.attributes.contains(&("graphrt.node.id".into(), AttrValue::from("no"))));
}

const ITERATION: &str = r#"
name: iter
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: items, type: "array[string]" }] }
    - { id: each, type: iteration, iterator_selector: [start, items], output_selector: [up, output] }
    - { id: each_start, type: iteration-start, parent: each }
    - { id: up, type: template-transform, parent: each, template: "{{ i | upper }}", variables: [{ variable: i, value_selector: [each, item] }] }
  edges:
    - { source: start, target: each }
    - { source: each_start, target: up }
"#;

#[tokio::test]
async fn iteration_bodies_nest_under_the_container_span() {
    let items = Value::array([Value::from("a"), Value::from("b")]);
    let spans = trace(ITERATION, RunInput::new().input("items", items), |t| t).await;
    let each = by_name(&spans, "each");
    let bodies: Vec<&Span> = spans.iter().filter(|s| s.name.starts_with("iteration each[")).collect();
    assert_eq!(bodies.len(), 2, "{:?}", names(&spans));
    for body in &bodies {
        assert_eq!(body.parent_span_id, Some(each.span_id));
        assert_eq!(str_attr(body, "graphrt.container.id"), Some("each"));
        let inner: Vec<&Span> =
            spans.iter().filter(|s| s.name == "up" && s.parent_span_id == Some(body.span_id)).collect();
        assert_eq!(inner.len(), 1);
    }
    let root = root(&spans);
    assert_eq!(each.parent_span_id, Some(root.span_id));
}

const RETRY_THEN_FAIL: &str = r#"
name: failing
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - { id: flaky, type: flaky, retry: { max_retries: 2, interval_ms: 0 } }
    - { id: boom, type: boom }
  edges:
    - { source: start, target: flaky }
    - { source: flaky, target: boom }
"#;

#[tokio::test]
async fn retries_are_events_and_failures_mark_spans() {
    let metrics = Arc::new(Metrics::new());
    let m = metrics.clone();
    let spans = trace(RETRY_THEN_FAIL, RunInput::new(), move |t| t.with_metrics(m)).await;
    let flaky = by_name(&spans, "flaky");
    assert_eq!(flaky.status, StatusCode::Unset);
    assert_eq!(flaky.events.iter().filter(|e| e.name == "retry").count(), 1);
    let boom = by_name(&spans, "boom");
    assert_eq!(boom.status, StatusCode::Error);
    assert_eq!(str_attr(boom, "error.type"), Some("kaput"));
    assert!(boom.events.iter().any(|e| e.name == "exception"));
    let root = root(&spans);
    assert_eq!(root.status, StatusCode::Error);
    assert_eq!(str_attr(root, "graphrt.run.status"), Some("failed"));
    assert_eq!(metrics.total_runs(), 1);
    assert!(metrics.encode().iter().any(|m| m["name"] == "graphrt.nodes"));
}

const PAUSE: &str = r#"
name: approval
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - { id: review, type: human-input, actions: [{ id: ok, title: OK }] }
    - { id: done, type: template-transform, template: "done", variables: [] }
  edges:
    - { source: start, target: review }
    - { source: review, target: done, handle: ok }
"#;

#[tokio::test]
async fn paused_runs_end_with_unset_status() {
    let spans = trace(PAUSE, RunInput::new(), |t| t).await;
    let root = root(&spans);
    assert_eq!(root.status, StatusCode::Unset);
    assert_eq!(root.attr("graphrt.run.paused"), Some(&AttrValue::Bool(true)));
    assert_eq!(str_attr(root, "graphrt.pause.kinds"), Some("human_input"));
    let review = by_name(&spans, "review");
    assert_eq!(review.attr("graphrt.node.paused"), Some(&AttrValue::Bool(true)));
}

const SECRET: &str = r#"
name: secretive
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - { id: llm, type: secretive, model: { provider: openai, name: gpt-test } }
  edges:
    - { source: start, target: llm }
"#;

#[tokio::test]
async fn upstream_trace_and_content_capture_keep_secrets_masked() {
    let parent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let spans = trace(SECRET, RunInput::new(), |t| t.with_parent(parent).capture_content(true)).await;
    let upstream = TraceParent::parse(parent).unwrap();
    let root = root(&spans);
    assert_eq!(root.trace_id, upstream.trace_id);
    assert_eq!(root.parent_span_id, Some(upstream.span_id));
    let llm = by_name(&spans, "llm");
    let outputs = str_attr(llm, "graphrt.node.outputs").unwrap();
    assert!(outputs.contains("visible") && !outputs.contains("sk-super-secret"), "{outputs}");
    assert_eq!(llm.attr("gen_ai.usage.input_tokens"), Some(&AttrValue::Int(7)));
    assert_eq!(llm.attr("gen_ai.usage.output_tokens"), Some(&AttrValue::Int(3)));
    assert!(llm.events.iter().any(|e| e.name == "step"));
    assert_eq!(root.attr("gen_ai.usage.input_tokens"), Some(&AttrValue::Int(7)));

    // An unsampled upstream trace exports nothing.
    let spans =
        trace(SECRET, RunInput::new(), |t| t.with_parent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00"))
            .await;
    assert!(spans.is_empty());
}

#[tokio::test]
async fn dropping_the_tracer_mid_run_closes_spans_as_abandoned() {
    let graph = Arc::new(Graph::from_text(PAUSE, &registry()).unwrap());
    let exporter = InMemoryExporter::new();
    let mut tracer = EventTracer::new(Arc::new(exporter.clone())).with_graph(graph.clone());
    let mut run = Engine::new(graph).run(RunInput::new());
    // Consume only the first two events, then drop.
    for _ in 0..2 {
        if let Some(event) = run.next().await {
            tracer.on_event(&event);
        }
    }
    assert!(tracer.traceparent().is_some());
    drop(tracer);
    let spans = exporter.take();
    let root = root(&spans);
    assert_eq!(str_attr(root, "graphrt.run.status"), Some("abandoned"));
    assert_eq!(root.status, StatusCode::Error);
}
