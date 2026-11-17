//! [`EventTracer`]: turns a run's [`Event`] stream into OpenTelemetry spans.
//!
//! Span tree:
//!
//! ```text
//! workflow <name>                     (run; root, or child of an upstream traceparent)
//! ├── <node title>                    (one per node execution, retries are span events)
//! └── <iteration node>
//!     └── iteration <id>[<index>]     (one per container body run)
//!         └── <node title>
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use graphrt_core::event::{Event, PauseReason, Usage};
use graphrt_core::graph::Graph;
use graphrt_core::value::Value;

use crate::exporter::SpanExporter;
use crate::metrics::Metrics;
use crate::span::{AttrValue, Span, SpanId, SpanKind, TraceId, TraceParent, now_unix_nanos, truncate};

/// Node kinds that call a language / decision model.
const MODEL_KINDS: &[&str] = &["llm", "agent", "question-classifier", "parameter-extractor", "system-one"];

/// Maximum bytes of captured content per attribute.
const CONTENT_LIMIT: usize = 4096;

#[derive(Debug)]
struct OpenNode {
    span: Span,
    frame: String,
    node_id: String,
    kind: String,
    chunks: u64,
}

/// Builds spans for one run. Feed it every event with [`EventTracer::on_event`];
/// spans are exported as they finish. Dropping the tracer before the terminal
/// event closes the remaining spans as abandoned.
#[derive(Debug)]
pub struct EventTracer {
    exporter: Arc<dyn SpanExporter>,
    metrics: Option<Arc<Metrics>>,
    graph: Option<Arc<Graph>>,
    workflow_name: String,
    parent: Option<TraceParent>,
    capture_content: bool,
    trace_id: TraceId,
    sampled: bool,
    root: Option<Span>,
    /// Open node spans by execution id.
    nodes: HashMap<String, OpenNode>,
    /// Latest execution id per `(frame, node id)`, used to find container spans.
    by_node: HashMap<(String, String), String>,
    /// Container node span ids by `(frame, node id)` (kept after the node ends).
    containers: HashMap<(String, String), SpanId>,
    /// Open container body (frame) spans by frame id.
    frames: HashMap<String, Span>,
    finished: bool,
}

impl EventTracer {
    /// Tracer exporting to `exporter`.
    pub fn new(exporter: Arc<dyn SpanExporter>) -> Self {
        Self {
            exporter,
            metrics: None,
            graph: None,
            workflow_name: "workflow".to_owned(),
            parent: None,
            capture_content: false,
            trace_id: TraceId::random(),
            sampled: true,
            root: None,
            nodes: HashMap::new(),
            by_node: HashMap::new(),
            containers: HashMap::new(),
            frames: HashMap::new(),
            finished: false,
        }
    }

    /// Workflow name used for the root span (`workflow <name>`).
    pub fn workflow_name(mut self, name: impl Into<String>) -> Self {
        self.workflow_name = name.into();
        self
    }

    /// Graph of the run: adds the workflow fingerprint and model attributes
    /// (`gen_ai.request.model`, `gen_ai.system`) from node configuration.
    pub fn with_graph(mut self, graph: Arc<Graph>) -> Self {
        if self.workflow_name == "workflow" {
            if let Some(name) = graph.workflow().name.clone() {
                self.workflow_name = name;
            }
        }
        self.graph = Some(graph);
        self
    }

    /// Continue an upstream trace from a W3C `traceparent` header. Invalid headers
    /// are ignored (a new trace starts). An unsampled parent disables export.
    pub fn with_parent(mut self, traceparent: &str) -> Self {
        if let Some(parent) = TraceParent::parse(traceparent) {
            self.trace_id = parent.trace_id;
            self.sampled = parent.sampled;
            self.parent = Some(parent);
        }
        self
    }

    /// Record run / node / token metrics into a shared [`Metrics`].
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Attach truncated node outputs and progress data to spans. Secrets stay
    /// masked (values serialize secrets as `******`). Off by default.
    pub fn capture_content(mut self, capture: bool) -> Self {
        self.capture_content = capture;
        self
    }

    /// Trace id of this run.
    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    /// `traceparent` header pointing at the run span, for propagating the trace to
    /// downstream calls. `None` before the run starts.
    pub fn traceparent(&self) -> Option<String> {
        self.root.as_ref().map(|root| {
            TraceParent { trace_id: self.trace_id, span_id: root.span_id, sampled: self.sampled }.to_header()
        })
    }

    /// Process one event.
    pub fn on_event(&mut self, event: &Event) {
        if self.finished {
            return;
        }
        let now = now_unix_nanos();
        match event {
            Event::RunStarted { run_id, resumed } => {
                let root = self.ensure_root(now);
                root.set("graphrt.run.id", run_id.as_str());
                root.set("graphrt.run.resumed", *resumed);
            }
            Event::NodeStarted { frame, node_id, exec_id, kind, title, attempt } => {
                self.ensure_root(now);
                if let Some(open) = self.nodes.get_mut(exec_id) {
                    open.span.set("graphrt.node.attempt", *attempt);
                    return;
                }
                let parent = self.parent_for_frame(frame, now);
                let name = if title.is_empty() { kind.clone() } else { title.clone() };
                let mut span = Span::new(self.trace_id, Some(parent), name, now);
                span.set("graphrt.node.id", node_id.as_str());
                span.set("graphrt.node.kind", kind.as_str());
                span.set("graphrt.node.exec_id", exec_id.as_str());
                span.set("graphrt.node.attempt", *attempt);
                if !frame.is_empty() {
                    span.set("graphrt.frame", frame.as_ref());
                }
                self.model_attributes(&mut span, node_id, kind);
                let key = (frame.to_string(), node_id.clone());
                self.by_node.insert(key.clone(), exec_id.clone());
                if self.is_container(node_id) {
                    self.containers.insert(key, span.span_id);
                }
                self.nodes.insert(
                    exec_id.clone(),
                    OpenNode {
                        span,
                        frame: frame.to_string(),
                        node_id: node_id.clone(),
                        kind: kind.clone(),
                        chunks: 0,
                    },
                );
            }
            Event::NodeChunk { exec_id, .. } => {
                if let Some(open) = self.nodes.get_mut(exec_id) {
                    open.chunks += 1;
                }
            }
            Event::NodeProgress { exec_id, name, data, .. } => {
                let capture = self.capture_content;
                if let Some(open) = self.nodes.get_mut(exec_id) {
                    let mut attrs = Vec::new();
                    if capture {
                        attrs.push(("graphrt.progress.data".to_owned(), AttrValue::Str(json_text(data))));
                    }
                    open.span.add_event(name, now, attrs);
                }
            }
            Event::NodeRetry { exec_id, attempt, error, delay_ms, .. } => {
                if let Some(open) = self.nodes.get_mut(exec_id) {
                    open.span.add_event(
                        "retry",
                        now,
                        vec![
                            ("graphrt.retry.attempt".to_owned(), AttrValue::from(*attempt)),
                            ("graphrt.retry.delay_ms".to_owned(), AttrValue::from(*delay_ms)),
                            ("error.message".to_owned(), AttrValue::Str(truncate(error, 1024))),
                        ],
                    );
                }
            }
            Event::NodeSucceeded { exec_id, outputs, branch, usage, metadata, elapsed_ms, .. } => {
                let Some(mut open) = self.nodes.remove(exec_id) else { return };
                if let Some(branch) = branch {
                    open.span.set("graphrt.node.branch", branch.as_str());
                }
                if open.span.attr("gen_ai.request.model").is_none() {
                    if let Some(model) = metadata.get("model").and_then(Value::as_str) {
                        open.span.set("gen_ai.request.model", model);
                    }
                }
                if let Some(usage) = usage {
                    self.usage_attributes(&mut open.span, usage);
                }
                if self.capture_content && !outputs.is_empty() {
                    let text = serde_json::to_string(outputs).unwrap_or_default();
                    open.span.set("graphrt.node.outputs", truncate(&text, CONTENT_LIMIT));
                }
                self.finish_node(open, "succeeded", *elapsed_ms, now);
            }
            Event::NodeFailed { exec_id, error, error_type, handled, outputs, elapsed_ms, .. } => {
                let Some(mut open) = self.nodes.remove(exec_id) else { return };
                open.span.set("error.type", error_type.as_str());
                open.span.set("graphrt.node.handled", *handled);
                open.span.add_event(
                    "exception",
                    now,
                    vec![
                        ("exception.type".to_owned(), AttrValue::from(error_type.as_str())),
                        ("exception.message".to_owned(), AttrValue::Str(truncate(error, 1024))),
                    ],
                );
                open.span.error(error);
                if self.capture_content && !outputs.is_empty() {
                    let text = serde_json::to_string(outputs).unwrap_or_default();
                    open.span.set("graphrt.node.outputs", truncate(&text, CONTENT_LIMIT));
                }
                let status = if *handled { "handled_error" } else { "failed" };
                self.finish_node(open, status, *elapsed_ms, now);
            }
            Event::NodeSkipped { frame, node_id } => {
                let root = self.ensure_root(now);
                let mut attrs = vec![("graphrt.node.id".to_owned(), AttrValue::from(node_id.as_str()))];
                if !frame.is_empty() {
                    attrs.push(("graphrt.frame".to_owned(), AttrValue::from(frame.as_ref())));
                }
                root.add_event("node_skipped", now, attrs);
            }
            Event::NodePaused { exec_id, reason, .. } => {
                let Some(mut open) = self.nodes.remove(exec_id) else { return };
                open.span.set("graphrt.node.paused", true);
                open.span.set("graphrt.pause.kind", reason.kind.as_str());
                let elapsed = now.saturating_sub(open.span.start_unix_nano) / 1_000_000;
                self.finish_node(open, "paused", elapsed, now);
            }
            Event::VariableUpdated { node_id, selector, .. } => {
                let exec = self.nodes.values_mut().find(|n| &n.node_id == node_id);
                if let Some(open) = exec {
                    open.span.add_event(
                        "variable_updated",
                        now,
                        vec![("graphrt.variable".to_owned(), AttrValue::Str(selector.to_string()))],
                    );
                }
            }
            Event::RunSucceeded { usage, handled_errors, elapsed_ms, .. } => {
                let root = self.ensure_root(now);
                root.set("graphrt.run.status", "succeeded");
                root.set("graphrt.run.handled_errors", *handled_errors);
                set_usage(root, usage);
                self.finish_run("succeeded", *elapsed_ms, now);
            }
            Event::RunFailed { error, error_type, usage, elapsed_ms } => {
                let root = self.ensure_root(now);
                root.set("graphrt.run.status", "failed");
                root.set("error.type", error_type.as_str());
                root.error(error);
                set_usage(root, usage);
                self.finish_run("failed", *elapsed_ms, now);
            }
            Event::RunAborted { reason, elapsed_ms } => {
                let root = self.ensure_root(now);
                root.set("graphrt.run.status", "aborted");
                root.error(&format!("aborted: {reason}"));
                self.finish_run("aborted", *elapsed_ms, now);
            }
            Event::RunPaused { reasons, snapshot } => {
                let root = self.ensure_root(now);
                root.set("graphrt.run.status", "paused");
                root.set("graphrt.run.paused", true);
                root.set("graphrt.pause.kinds", pause_kinds(reasons));
                let elapsed = snapshot.elapsed_ms;
                self.finish_run("paused", elapsed, now);
            }
            _ => {}
        }
    }

    fn ensure_root(&mut self, now: u64) -> &mut Span {
        let trace_id = self.trace_id;
        let parent = self.parent.map(|p| p.span_id);
        let name = format!("workflow {}", self.workflow_name);
        let graph = self.graph.clone();
        let workflow_name = self.workflow_name.clone();
        self.root.get_or_insert_with(|| {
            let mut span = Span::new(trace_id, parent, name, now);
            span.kind = SpanKind::Server;
            span.set("graphrt.workflow.name", workflow_name);
            if let Some(graph) = graph {
                span.set("graphrt.workflow.fingerprint", graph.fingerprint());
            }
            span
        })
    }

    fn is_container(&self, node_id: &str) -> bool {
        match &self.graph {
            Some(graph) => graph.node_by_id(node_id).is_some_and(|n| n.role == graphrt_core::NodeRole::Container),
            None => true,
        }
    }

    /// Parent span for a node in `frame`, creating container body spans as needed.
    fn parent_for_frame(&mut self, frame: &str, now: u64) -> SpanId {
        if frame.is_empty() {
            return self.ensure_root(now).span_id;
        }
        if let Some(span) = self.frames.get(frame) {
            return span.span_id;
        }
        let (parent_frame, segment) = match frame.rsplit_once('/') {
            Some((p, s)) => (p.to_owned(), s),
            None => (String::new(), frame),
        };
        let (owner, index) = match segment.split_once('[') {
            Some((owner, rest)) => (owner.to_owned(), rest.trim_end_matches(']').to_owned()),
            None => (segment.to_owned(), String::new()),
        };
        let parent = match self.containers.get(&(parent_frame.clone(), owner.clone())) {
            Some(id) => *id,
            None => self.parent_for_frame(&parent_frame, now),
        };
        let kind = self
            .graph
            .as_ref()
            .and_then(|g| g.node_by_id(&owner).map(|n| n.kind.clone()))
            .unwrap_or_else(|| "frame".to_owned());
        let mut span = Span::new(self.trace_id, Some(parent), format!("{kind} {segment}"), now);
        span.set("graphrt.frame", frame);
        span.set("graphrt.container.id", owner.as_str());
        if let Ok(i) = index.parse::<u64>() {
            span.set("graphrt.frame.index", i);
        }
        let id = span.span_id;
        self.frames.insert(frame.to_owned(), span);
        id
    }

    fn model_attributes(&self, span: &mut Span, node_id: &str, kind: &str) {
        if !MODEL_KINDS.contains(&kind) {
            return;
        }
        span.set("gen_ai.operation.name", if kind == "system-one" { "classify" } else { "chat" });
        let model =
            self.graph.as_ref().and_then(|g| g.node_by_id(node_id)).and_then(|n| n.doc.config.get("model").cloned());
        match model {
            Some(serde_json::Value::Object(m)) => {
                if let Some(name) = m.get("name").and_then(serde_json::Value::as_str) {
                    span.set("gen_ai.request.model", name);
                }
                if let Some(provider) = m.get("provider").and_then(serde_json::Value::as_str) {
                    span.set("gen_ai.system", provider);
                }
            }
            Some(serde_json::Value::String(name)) => span.set("gen_ai.request.model", name),
            _ if kind == "system-one" => span.set("gen_ai.system", "typesafe"),
            _ => {}
        }
    }

    fn usage_attributes(&self, span: &mut Span, usage: &Usage) {
        set_usage(span, usage);
        if let Some(metrics) = &self.metrics {
            let model = match span.attr("gen_ai.request.model") {
                Some(AttrValue::Str(m)) => m.clone(),
                _ => String::new(),
            };
            metrics.record_tokens(&model, usage.prompt_tokens, usage.completion_tokens);
        }
    }

    fn finish_node(&mut self, mut open: OpenNode, status: &str, elapsed_ms: u64, now: u64) {
        open.span.end_unix_nano = now;
        open.span.set("graphrt.node.status", status);
        open.span.set("graphrt.node.elapsed_ms", elapsed_ms);
        if open.chunks > 0 {
            open.span.set("graphrt.node.chunks", open.chunks);
        }
        if let Some(metrics) = &self.metrics {
            metrics.record_node(&open.kind, status, elapsed_ms);
        }
        // Container body spans of this node end with it.
        let prefix = if open.frame.is_empty() {
            format!("{}[", open.node_id)
        } else {
            format!("{}/{}[", open.frame, open.node_id)
        };
        let children: Vec<String> = self.frames.keys().filter(|f| f.starts_with(&prefix)).cloned().collect();
        for frame in children {
            if let Some(mut span) = self.frames.remove(&frame) {
                span.end_unix_nano = now;
                self.emit(span);
            }
        }
        self.emit(open.span);
    }

    fn finish_run(&mut self, status: &str, elapsed_ms: u64, now: u64) {
        // Close whatever is still open (dropped nodes after an abort or failure).
        let open: Vec<String> = self.nodes.keys().cloned().collect();
        for exec in open {
            if let Some(mut node) = self.nodes.remove(&exec) {
                node.span.set("graphrt.node.interrupted", true);
                let elapsed = now.saturating_sub(node.span.start_unix_nano) / 1_000_000;
                self.finish_node(node, "interrupted", elapsed, now);
            }
        }
        let frames: Vec<String> = self.frames.keys().cloned().collect();
        for frame in frames {
            if let Some(mut span) = self.frames.remove(&frame) {
                span.end_unix_nano = now;
                self.emit(span);
            }
        }
        if let Some(mut root) = self.root.take() {
            root.end_unix_nano = now;
            root.set("graphrt.run.elapsed_ms", elapsed_ms);
            self.emit(root);
        }
        if let Some(metrics) = &self.metrics {
            metrics.record_run(&self.workflow_name, status, elapsed_ms);
        }
        self.finished = true;
    }

    fn emit(&self, span: Span) {
        if self.sampled {
            self.exporter.export(span);
        }
    }

    /// Close the run as abandoned (stream dropped before its terminal event).
    pub fn abandon(&mut self) {
        if self.finished || self.root.is_none() {
            self.finished = true;
            return;
        }
        let now = now_unix_nanos();
        let elapsed = self.root.as_ref().map(|r| now.saturating_sub(r.start_unix_nano) / 1_000_000).unwrap_or(0);
        if let Some(root) = self.root.as_mut() {
            root.set("graphrt.run.status", "abandoned");
            root.error("run ended without a terminal event (consumer dropped the stream)");
        }
        self.finish_run("abandoned", elapsed, now);
    }
}

impl Drop for EventTracer {
    fn drop(&mut self) {
        self.abandon();
    }
}

fn set_usage(span: &mut Span, usage: &Usage) {
    if usage.is_zero() {
        return;
    }
    span.set("gen_ai.usage.input_tokens", usage.prompt_tokens);
    span.set("gen_ai.usage.output_tokens", usage.completion_tokens);
}

fn pause_kinds(reasons: &[PauseReason]) -> String {
    let mut kinds: Vec<&str> = reasons.iter().map(|r| r.kind.as_str()).collect();
    kinds.dedup();
    kinds.join(",")
}

fn json_text(value: &Value) -> String {
    truncate(&serde_json::to_string(value).unwrap_or_default(), CONTENT_LIMIT)
}
