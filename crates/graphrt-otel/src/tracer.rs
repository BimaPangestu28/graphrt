//! [`EventTracer`]: turns a run's [`Event`] stream into OpenTelemetry spans.
//!
//! Span tree (names follow the OpenTelemetry GenAI conventions, see
//! [`crate::semconv`]):
//!
//! ```text
//! invoke_workflow <workflow>          (run; root, or child of an upstream traceparent)
//! ├── <node title>                    (one per node execution, retries are span events)
//! ├── chat <model>                    (llm, question-classifier, parameter-extractor, system-one)
//! ├── invoke_agent <agent>            (agent)
//! │   └── execute_tool <tool>         (one per tool call)
//! ├── retrieval                       (knowledge-retrieval)
//! └── <iteration node>
//!     └── iteration <id>[<index>]     (one per container body run)
//!         └── <node title>
//! ```
//!
//! Content (run input and output, rendered messages, tool arguments and
//! results, retrieval queries and documents, node outputs) is recorded only
//! when [`EventTracer::capture`] enables it; values are masked (secrets
//! serialize as `******`) and capped in size.

use std::collections::HashMap;
use std::sync::Arc;

use graphrt_core::event::{Event, PauseReason, Usage};
use graphrt_core::graph::Graph;
use graphrt_core::value::Value;
use graphrt_core::variables::Variables;
use serde_json::Value as Json;

use crate::config::CaptureMode;
use crate::exporter::SpanExporter;
use crate::metrics::Metrics;
use crate::semconv::{self, gen_ai, general, graphrt as gr, langfuse, operation};
use crate::span::{AttrValue, Attributes, Span, SpanId, SpanKind, TraceId, TraceParent, now_unix_nanos, truncate};

/// Default maximum bytes of captured content per value.
pub const DEFAULT_CONTENT_LIMIT: usize = 4096;

/// Progress emitted by model nodes with their rendered request messages (only
/// when the host registers [`graphrt_core::node::CaptureContent`]).
pub const MODEL_REQUEST_PROGRESS: &str = "model_request";

/// How a node execution is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeRole {
    Plain,
    Chat,
    Agent,
    Retrieval,
}

impl NodeRole {
    fn of(kind: &str) -> Self {
        match kind {
            "llm" | "question-classifier" | "parameter-extractor" | "system-one" => NodeRole::Chat,
            "agent" => NodeRole::Agent,
            "knowledge-retrieval" => NodeRole::Retrieval,
            _ => NodeRole::Plain,
        }
    }
}

/// Caller context stamped on spans: session, user, tags and metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunContext {
    /// Session / conversation id (`session.id`, `langfuse.session.id`, `gen_ai.conversation.id`).
    pub session_id: Option<String>,
    /// End-user id (`user.id`, `langfuse.user.id`).
    pub user_id: Option<String>,
    /// Tags (`langfuse.trace.tags`, `graphrt.run.tags`).
    pub tags: Vec<String>,
    /// Metadata (`langfuse.trace.metadata.<key>`, `graphrt.run.metadata.<key>`).
    pub metadata: Vec<(String, String)>,
}

impl RunContext {
    /// Whether nothing is set.
    pub fn is_empty(&self) -> bool {
        self.session_id.is_none() && self.user_id.is_none() && self.tags.is_empty() && self.metadata.is_empty()
    }

    /// Attributes for every span.
    fn span_attributes(&self) -> Attributes {
        let mut out = Vec::new();
        if let Some(session) = &self.session_id {
            out.push((general::SESSION_ID.to_owned(), AttrValue::from(session.as_str())));
            out.push((langfuse::SESSION_ID.to_owned(), AttrValue::from(session.as_str())));
        }
        if let Some(user) = &self.user_id {
            out.push((general::USER_ID.to_owned(), AttrValue::from(user.as_str())));
            out.push((langfuse::USER_ID.to_owned(), AttrValue::from(user.as_str())));
        }
        if !self.tags.is_empty() {
            out.push((langfuse::TRACE_TAGS.to_owned(), string_array(&self.tags)));
        }
        for (key, value) in &self.metadata {
            out.push((format!("{}{key}", langfuse::TRACE_METADATA_PREFIX), AttrValue::from(value.as_str())));
        }
        out
    }
}

fn string_array(items: &[String]) -> AttrValue {
    AttrValue::Structured(Json::Array(items.iter().map(|t| Json::String(t.clone())).collect()))
}

/// Content capture settings plus the per-value size cap.
#[derive(Clone, Copy, Debug)]
struct Content {
    mode: CaptureMode,
    limit: usize,
}

impl Content {
    fn on(self) -> bool {
        self.mode.is_enabled()
    }

    /// Record `value` under `key`: as an attribute and/or on the pending
    /// content event, depending on the mode.
    fn put(self, span: &mut Span, pending: &mut Attributes, key: &str, value: AttrValue) {
        match (self.mode.on_span(), self.mode.on_event()) {
            (true, true) => {
                pending.push((key.to_owned(), value.clone()));
                span.set(key, value);
            }
            (true, false) => span.set(key, value),
            (false, true) => pending.push((key.to_owned(), value)),
            (false, false) => {}
        }
    }

    /// Langfuse reads input/output from attributes only.
    fn put_langfuse(self, span: &mut Span, key: &str, json: &Json) {
        if self.mode.on_span() {
            span.set(key, self.text(&json_string(json)));
        }
    }

    fn text(self, text: &str) -> AttrValue {
        AttrValue::Str(truncate(text, self.limit))
    }

    /// Structured value with its strings cut so their total stays within the limit.
    fn structured(self, mut json: Json) -> AttrValue {
        let mut budget = self.limit;
        cap_strings(&mut json, &mut budget);
        AttrValue::Structured(json)
    }
}

/// Truncate strings in `json` (depth first) so their combined length fits `budget`.
fn cap_strings(json: &mut Json, budget: &mut usize) {
    match json {
        Json::String(s) => {
            if s.len() > *budget {
                *s = truncate(s, *budget);
            }
            *budget = budget.saturating_sub(s.len());
        }
        Json::Array(items) => items.iter_mut().for_each(|item| cap_strings(item, budget)),
        Json::Object(map) => map.values_mut().for_each(|item| cap_strings(item, budget)),
        _ => {}
    }
}

fn json_string(json: &Json) -> String {
    match json {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[derive(Debug)]
struct OpenNode {
    span: Span,
    frame: String,
    node_id: Arc<str>,
    kind: Arc<str>,
    role: NodeRole,
    chunks: u64,
    /// Content waiting for the `CONTENT_EVENT` (event capture modes).
    content: Attributes,
}

#[derive(Debug)]
struct OpenTool {
    span: Span,
    content: Attributes,
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
    content: Content,
    trace_id: TraceId,
    sampled: bool,
    root: Option<Span>,
    root_id: Option<SpanId>,
    root_content: Attributes,
    context: RunContext,
    /// Attributes stamped on every span (from [`RunContext`]).
    common: Attributes,
    /// Host attributes stamped on every span ([`EventTracer::with_span_attributes`]).
    extra: Attributes,
    /// Host attributes of the root span ([`EventTracer::with_root_attributes`]).
    root_extra: Attributes,
    /// Root span id chosen by the host ([`EventTracer::with_root_span_id`]).
    preset_root_id: Option<SpanId>,
    /// Open node spans by execution id.
    nodes: HashMap<Arc<str>, OpenNode>,
    /// Open tool spans by `(execution id, tool call id)`.
    tools: HashMap<(Arc<str>, String), OpenTool>,
    /// Latest execution id per `(frame, node id)`, used to find container spans.
    by_node: HashMap<(String, Arc<str>), Arc<str>>,
    /// Container node span ids by `(frame, node id)` (kept after the node ends).
    containers: HashMap<(String, Arc<str>), SpanId>,
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
            content: Content { mode: CaptureMode::NoContent, limit: DEFAULT_CONTENT_LIMIT },
            trace_id: TraceId::random(),
            sampled: true,
            root: None,
            root_id: None,
            root_content: Vec::new(),
            context: RunContext::default(),
            common: Vec::new(),
            extra: Vec::new(),
            root_extra: Vec::new(),
            preset_root_id: None,
            nodes: HashMap::new(),
            tools: HashMap::new(),
            by_node: HashMap::new(),
            containers: HashMap::new(),
            frames: HashMap::new(),
            finished: false,
        }
    }

    /// Workflow name used for the root span (`invoke_workflow <name>`) and `gen_ai.agent.name`.
    pub fn workflow_name(mut self, name: impl Into<String>) -> Self {
        self.workflow_name = name.into();
        self
    }

    /// Graph of the run: adds the workflow fingerprint and model attributes
    /// (`gen_ai.request.model`, `gen_ai.provider.name`) from node configuration.
    pub fn with_graph(mut self, graph: Arc<Graph>) -> Self {
        if self.workflow_name == "workflow"
            && let Some(name) = graph.workflow().name.clone()
        {
            self.workflow_name = name;
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

    /// Stamp session, user, tags and metadata on the run's spans.
    pub fn with_context(mut self, context: RunContext) -> Self {
        self.common = context.span_attributes();
        self.context = context;
        self
    }

    /// Stamp these attributes on every span of the run (e.g. Langfuse
    /// `langfuse.experiment.*` attributes of an offline evaluation).
    pub fn with_span_attributes(mut self, attributes: Attributes) -> Self {
        self.extra.extend(attributes);
        self
    }

    /// Set these attributes on the run's root span.
    pub fn with_root_attributes(mut self, attributes: Attributes) -> Self {
        self.root_extra.extend(attributes);
        self
    }

    /// Use `span_id` for the run's root span, so a host can reference it in
    /// attributes before the run starts.
    pub fn with_root_span_id(mut self, span_id: SpanId) -> Self {
        self.preset_root_id = Some(span_id);
        self
    }

    /// Content capture policy (see [`CaptureMode`]). Off by default.
    pub fn capture(mut self, mode: CaptureMode) -> Self {
        self.content.mode = mode;
        self
    }

    /// `true` is [`CaptureMode::SpanOnly`], `false` [`CaptureMode::NoContent`].
    pub fn capture_content(self, capture: bool) -> Self {
        self.capture(if capture { CaptureMode::SpanOnly } else { CaptureMode::NoContent })
    }

    /// Maximum bytes of captured content per value (default [`DEFAULT_CONTENT_LIMIT`]).
    pub fn content_limit(mut self, bytes: usize) -> Self {
        self.content.limit = bytes;
        self
    }

    /// Trace id of this run.
    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    /// Span id of the run's root span, once the run started (kept after it ends).
    pub fn root_span_id(&self) -> Option<SpanId> {
        self.root_id
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
                let role = NodeRole::of(kind);
                let mut span = Span::new(self.trace_id, Some(parent), String::new(), now);
                span.set("graphrt.node.id", node_id.as_ref());
                span.set("graphrt.node.kind", kind.as_ref());
                span.set("graphrt.node.exec_id", exec_id.as_ref());
                span.set("graphrt.node.attempt", *attempt);
                if !frame.is_empty() {
                    span.set("graphrt.frame", frame.as_ref());
                }
                let label = if title.is_empty() { kind.as_ref() } else { title.as_ref() };
                span.name = self.genai_attributes(&mut span, role, node_id, kind, label);
                let key = (frame.to_string(), node_id.clone());
                self.by_node.insert(key.clone(), exec_id.clone());
                if self.is_container(node_id) {
                    self.containers.insert(key, span.span_id);
                }
                let open = OpenNode {
                    span,
                    frame: frame.to_string(),
                    node_id: node_id.clone(),
                    kind: kind.clone(),
                    role,
                    chunks: 0,
                    content: Vec::new(),
                };
                self.nodes.insert(exec_id.clone(), open);
            }
            Event::NodeChunk { exec_id, .. } => {
                if let Some(open) = self.nodes.get_mut(exec_id) {
                    open.chunks += 1;
                }
            }
            Event::NodeProgress { exec_id, name, data, .. } => self.on_progress(exec_id, name, data, now),
            Event::NodeRetry { exec_id, attempt, error, delay_ms, .. } => {
                if let Some(open) = self.nodes.get_mut(exec_id) {
                    open.span.add_event(
                        "retry",
                        now,
                        vec![
                            ("graphrt.retry.attempt".to_owned(), AttrValue::from(*attempt)),
                            ("graphrt.retry.delay_ms".to_owned(), AttrValue::from(*delay_ms)),
                            (general::ERROR_MESSAGE.to_owned(), AttrValue::Str(truncate(error, 1024))),
                        ],
                    );
                }
            }
            Event::NodeSucceeded { frame, exec_id, outputs, branch, usage, metadata, elapsed_ms, .. } => {
                let Some(mut open) = self.nodes.remove(exec_id) else { return };
                if let Some(branch) = branch {
                    open.span.set("graphrt.node.branch", branch.as_str());
                }
                if matches!(open.role, NodeRole::Chat | NodeRole::Agent) {
                    model_from_metadata(&mut open.span, open.role, metadata);
                }
                if let Some(usage) = usage {
                    self.usage_attributes(&mut open.span, usage);
                }
                if self.content.on() {
                    self.node_content(&mut open, outputs, metadata);
                    if frame.is_empty() && open.kind.as_ref() == "start" {
                        self.root_input(outputs, now);
                    }
                }
                if open.role == NodeRole::Chat
                    && let Some(reason) = outputs.get("finish_reason").and_then(Value::as_str)
                {
                    open.span.set(gen_ai::RESPONSE_FINISH_REASONS, string_array(&[reason.to_owned()]));
                }
                self.finish_node(open, "succeeded", *elapsed_ms, now);
            }
            Event::NodeFailed { exec_id, error, error_type, handled, outputs, elapsed_ms, .. } => {
                let Some(mut open) = self.nodes.remove(exec_id) else { return };
                open.span.set(general::ERROR_TYPE, error_type.as_str());
                open.span.set("graphrt.node.handled", *handled);
                open.span.add_event(
                    "exception",
                    now,
                    vec![
                        (general::EXCEPTION_TYPE.to_owned(), AttrValue::from(error_type.as_str())),
                        (general::EXCEPTION_MESSAGE.to_owned(), AttrValue::Str(truncate(error, 1024))),
                    ],
                );
                open.span.error(error);
                if self.content.on() && !outputs.is_empty() {
                    let value = self.content.text(&serde_json::to_string(outputs).unwrap_or_default());
                    self.content.put(&mut open.span, &mut open.content, "graphrt.node.outputs", value);
                }
                let status = if *handled { "handled_error" } else { "failed" };
                self.finish_node(open, status, *elapsed_ms, now);
            }
            Event::NodeSkipped { frame, node_id } => {
                let root = self.ensure_root(now);
                let mut attrs = vec![("graphrt.node.id".to_owned(), AttrValue::from(node_id.as_ref()))];
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
            Event::RunSucceeded { outputs, usage, handled_errors, elapsed_ms } => {
                if self.content.on() {
                    self.root_output(outputs, now);
                }
                let root = self.ensure_root(now);
                root.set(gr::RUN_STATUS, "succeeded");
                root.set("graphrt.run.handled_errors", *handled_errors);
                set_usage(root, usage);
                self.finish_run("succeeded", *elapsed_ms, now);
            }
            Event::RunFailed { error, error_type, usage, elapsed_ms } => {
                let root = self.ensure_root(now);
                root.set(gr::RUN_STATUS, "failed");
                root.set(general::ERROR_TYPE, error_type.as_str());
                root.error(error);
                set_usage(root, usage);
                self.finish_run("failed", *elapsed_ms, now);
            }
            Event::RunAborted { reason, elapsed_ms } => {
                let root = self.ensure_root(now);
                root.set(gr::RUN_STATUS, "aborted");
                root.error(&format!("aborted: {reason}"));
                self.finish_run("aborted", *elapsed_ms, now);
            }
            Event::RunPaused { reasons, snapshot } => {
                let root = self.ensure_root(now);
                root.set(gr::RUN_STATUS, "paused");
                root.set("graphrt.run.paused", true);
                root.set("graphrt.pause.kinds", pause_kinds(reasons));
                let elapsed = snapshot.elapsed_ms;
                self.finish_run("paused", elapsed, now);
            }
            _ => {}
        }
    }

    fn on_progress(&mut self, exec_id: &Arc<str>, name: &str, data: &Value, now: u64) {
        match name {
            MODEL_REQUEST_PROGRESS => self.model_request(exec_id, data),
            "tool_call" => self.tool_call(exec_id, data, now),
            "tool_result" => self.tool_result(exec_id, data, now),
            "tool_denied" => {
                self.tool_call(exec_id, data, now);
                self.close_tool(exec_id, data, Some(("tool_denied", "the user did not approve this tool call")), now);
            }
            _ => {
                let capture = self.content.on();
                if let Some(open) = self.nodes.get_mut(exec_id) {
                    let mut attrs = Vec::new();
                    if capture {
                        let text = truncate(&serde_json::to_string(data).unwrap_or_default(), self.content.limit);
                        attrs.push(("graphrt.progress.data".to_owned(), AttrValue::Str(text)));
                    }
                    open.span.add_event(name, now, attrs);
                }
            }
        }
    }

    /// Rendered request messages of a model node (content capture only).
    fn model_request(&mut self, exec_id: &Arc<str>, data: &Value) {
        let content = self.content;
        if !content.on() {
            return;
        }
        let Some(open) = self.nodes.get_mut(exec_id) else { return };
        let (system, messages) = semconv_messages(&data.to_json());
        if !system.is_empty() {
            let value = content.structured(Json::Array(system));
            content.put(&mut open.span, &mut open.content, gen_ai::SYSTEM_INSTRUCTIONS, value);
        }
        let messages = Json::Array(messages);
        content.put_langfuse(&mut open.span, langfuse::OBSERVATION_INPUT, &messages);
        let value = content.structured(messages);
        content.put(&mut open.span, &mut open.content, gen_ai::INPUT_MESSAGES, value);
    }

    fn tool_call(&mut self, exec_id: &Arc<str>, data: &Value, now: u64) {
        let Some(open) = self.nodes.get(exec_id) else { return };
        let call_id = field_str(data, "id").unwrap_or_default().to_owned();
        let tool = field_str(data, "name").unwrap_or_default();
        let mut span = Span::new(
            self.trace_id,
            Some(open.span.span_id),
            semconv::span_name(operation::EXECUTE_TOOL, Some(tool)),
            now,
        );
        span.set(gen_ai::OPERATION_NAME, operation::EXECUTE_TOOL);
        span.set(gen_ai::TOOL_NAME, tool);
        span.set(gen_ai::TOOL_TYPE, "function");
        if !call_id.is_empty() {
            span.set(gen_ai::TOOL_CALL_ID, call_id.as_str());
        }
        if let Some(AttrValue::Str(agent)) = open.span.attr(gen_ai::AGENT_NAME) {
            span.set(gen_ai::AGENT_NAME, agent.clone());
        }
        let mut content = Vec::new();
        if self.content.on()
            && let Some(arguments) = data.as_object().and_then(|m| m.get("arguments"))
        {
            let json = arguments.to_json();
            self.content.put_langfuse(&mut span, langfuse::OBSERVATION_INPUT, &json);
            let value = self.content.text(&json_string(&json));
            self.content.put(&mut span, &mut content, gen_ai::TOOL_CALL_ARGUMENTS, value);
        }
        self.tools.insert((exec_id.clone(), call_id), OpenTool { span, content });
    }

    fn tool_result(&mut self, exec_id: &Arc<str>, data: &Value, now: u64) {
        let is_error = data.as_object().and_then(|m| m.get("is_error")).and_then(Value::as_bool).unwrap_or(false);
        let error = is_error.then_some(("tool_error", "the tool reported an error"));
        self.close_tool(exec_id, data, error, now);
    }

    fn close_tool(&mut self, exec_id: &Arc<str>, data: &Value, error: Option<(&str, &str)>, now: u64) {
        let call_id = field_str(data, "id").unwrap_or_default().to_owned();
        let Some(mut tool) = self.tools.remove(&(exec_id.clone(), call_id)) else { return };
        if let Some((error_type, message)) = error {
            tool.span.set(general::ERROR_TYPE, error_type);
            tool.span.error(message);
        }
        if self.content.on()
            && let Some(result) = data.as_object().and_then(|m| m.get("result"))
        {
            let json = result.to_json();
            self.content.put_langfuse(&mut tool.span, langfuse::OBSERVATION_OUTPUT, &json);
            let value = self.content.text(&json_string(&json));
            self.content.put(&mut tool.span, &mut tool.content, gen_ai::TOOL_CALL_RESULT, value);
        }
        tool.span.end_unix_nano = now;
        self.emit_with_content(tool.span, tool.content, now);
    }

    /// Captured content of a finished node: outputs, output messages, retrieval documents.
    fn node_content(&self, open: &mut OpenNode, outputs: &Variables, metadata: &graphrt_core::IndexMap<String, Value>) {
        let content = self.content;
        if !outputs.is_empty() {
            let value = content.text(&serde_json::to_string(outputs).unwrap_or_default());
            content.put(&mut open.span, &mut open.content, "graphrt.node.outputs", value);
        }
        match open.role {
            NodeRole::Chat | NodeRole::Agent => {
                let Some(text) = outputs.get("text").and_then(Value::as_str) else { return };
                let reason = outputs.get("finish_reason").and_then(Value::as_str).unwrap_or("stop");
                let message = serde_json::json!([{
                    "role": "assistant",
                    "parts": [{"type": "text", "content": text}],
                    "finish_reason": reason,
                }]);
                content.put_langfuse(&mut open.span, langfuse::OBSERVATION_OUTPUT, &Json::String(text.to_owned()));
                let value = content.structured(message);
                content.put(&mut open.span, &mut open.content, gen_ai::OUTPUT_MESSAGES, value);
            }
            NodeRole::Retrieval => {
                if let Some(query) = metadata.get("query").and_then(Value::as_str) {
                    content.put_langfuse(&mut open.span, langfuse::OBSERVATION_INPUT, &Json::String(query.to_owned()));
                    let value = content.text(query);
                    content.put(&mut open.span, &mut open.content, gen_ai::RETRIEVAL_QUERY_TEXT, value);
                }
                let documents: Vec<Json> = outputs
                    .get("result")
                    .and_then(Value::as_array)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|hit| {
                        let id = hit.get("id").map(Value::to_json).unwrap_or(Json::Null);
                        let score = hit.get("score").and_then(Value::as_f64).unwrap_or(0.0);
                        serde_json::json!({"id": id, "score": score})
                    })
                    .collect();
                let documents = Json::Array(documents);
                content.put_langfuse(&mut open.span, langfuse::OBSERVATION_OUTPUT, &documents);
                let value = content.structured(documents);
                content.put(&mut open.span, &mut open.content, gen_ai::RETRIEVAL_DOCUMENTS, value);
            }
            NodeRole::Plain => {}
        }
    }

    /// The run input (the root start node's validated outputs) on the root span.
    fn root_input(&mut self, outputs: &Variables, now: u64) {
        let json = serde_json::to_value(outputs).unwrap_or(Json::Null);
        self.put_root_content(gr::RUN_INPUT, langfuse::OBSERVATION_INPUT, &json, now);
    }

    fn root_output(&mut self, outputs: &graphrt_core::IndexMap<String, Value>, now: u64) {
        let json = serde_json::to_value(outputs).unwrap_or(Json::Null);
        self.put_root_content(gr::RUN_OUTPUT, langfuse::OBSERVATION_OUTPUT, &json, now);
    }

    fn put_root_content(&mut self, key: &str, langfuse_key: &str, json: &Json, now: u64) {
        let content = self.content;
        let mut pending = std::mem::take(&mut self.root_content);
        let root = self.ensure_root(now);
        content.put_langfuse(root, langfuse_key, json);
        content.put(root, &mut pending, key, content.text(&json_string(json)));
        self.root_content = pending;
    }

    fn ensure_root(&mut self, now: u64) -> &mut Span {
        if self.root.is_none() {
            let parent = self.parent.map(|p| p.span_id);
            let name = semconv::span_name(operation::INVOKE_WORKFLOW, Some(&self.workflow_name));
            let mut span = Span::new(self.trace_id, parent, name, now);
            if let Some(id) = self.preset_root_id {
                span.span_id = id;
            }
            for (key, value) in &self.root_extra {
                span.set(key, value.clone());
            }
            // A run continuing a remote caller's trace serves that request.
            span.kind = if parent.is_some() { SpanKind::Server } else { SpanKind::Internal };
            span.set(gen_ai::OPERATION_NAME, operation::INVOKE_WORKFLOW);
            span.set(gen_ai::AGENT_NAME, self.workflow_name.as_str());
            span.set(gr::WORKFLOW_NAME, self.workflow_name.as_str());
            if let Some(graph) = &self.graph {
                span.set("graphrt.workflow.fingerprint", graph.fingerprint());
            }
            if !self.context.tags.is_empty() {
                span.set(gr::RUN_TAGS, string_array(&self.context.tags));
            }
            for (key, value) in &self.context.metadata {
                span.set(&format!("{}{key}", gr::RUN_METADATA_PREFIX), value.as_str());
            }
            self.root_id = Some(span.span_id);
            self.root = Some(span);
        }
        // Just ensured above; the fallback is never taken.
        self.root.get_or_insert_with(|| Span::new(TraceId([0; 16]), None, String::new(), now))
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
        let parent = match self.containers.get(&(parent_frame.clone(), Arc::from(owner.as_str()))) {
            Some(id) => *id,
            None => self.parent_for_frame(&parent_frame, now),
        };
        let kind = self
            .graph
            .as_ref()
            .and_then(|g| g.node_by_id(&owner).map(|n| n.kind.to_string()))
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

    /// GenAI attributes of a node span; returns the span name.
    fn genai_attributes(&self, span: &mut Span, role: NodeRole, node_id: &str, kind: &str, label: &str) -> String {
        if role == NodeRole::Plain {
            return label.to_owned();
        }
        span.set(gr::NODE_TITLE, label);
        let config = self.graph.as_ref().and_then(|g| g.node_by_id(node_id)).map(|n| &n.doc.config);
        match role {
            NodeRole::Retrieval => {
                span.set(gen_ai::OPERATION_NAME, operation::RETRIEVAL);
                retrieval_config(span, config);
                semconv::span_name(operation::RETRIEVAL, None)
            }
            NodeRole::Agent => {
                span.set(gen_ai::OPERATION_NAME, operation::INVOKE_AGENT);
                span.set(gen_ai::AGENT_NAME, label);
                model_config(span, config.and_then(|c| c.get("model")));
                semconv::span_name(operation::INVOKE_AGENT, Some(label))
            }
            _ => {
                span.set(gen_ai::OPERATION_NAME, operation::CHAT);
                match kind {
                    "system-one" => span.set(gr::OPERATION_KIND, "classify"),
                    "question-classifier" => span.set(gr::OPERATION_KIND, "classify"),
                    "parameter-extractor" => span.set(gr::OPERATION_KIND, "extract"),
                    _ => {}
                }
                model_config(span, config.and_then(|c| c.get("model")));
                if kind == "system-one" && span.attr(gen_ai::PROVIDER_NAME).is_none() {
                    set_provider(span, "typesafe");
                }
                semconv::span_name(operation::CHAT, model_name(span))
            }
        }
    }

    fn usage_attributes(&self, span: &mut Span, usage: &Usage) {
        set_usage(span, usage);
        if let Some(metrics) = &self.metrics {
            metrics.record_tokens(model_name(span).unwrap_or_default(), usage.prompt_tokens, usage.completion_tokens);
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
        if open.role == NodeRole::Agent {
            self.close_open_tools(&open, now);
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
        self.emit_with_content(open.span, open.content, now);
    }

    /// Tool calls still open when their agent node ends (paused, failed, interrupted).
    fn close_open_tools(&mut self, open: &OpenNode, now: u64) {
        let exec = open.span.attr("graphrt.node.exec_id").and_then(|v| match v {
            AttrValue::Str(s) => Some(s.as_str()),
            _ => None,
        });
        let Some(exec) = exec else { return };
        let keys: Vec<(Arc<str>, String)> = self.tools.keys().filter(|(e, _)| e.as_ref() == exec).cloned().collect();
        for key in keys {
            if let Some(mut tool) = self.tools.remove(&key) {
                tool.span.set("graphrt.tool.interrupted", true);
                tool.span.end_unix_nano = now;
                self.emit_with_content(tool.span, tool.content, now);
            }
        }
    }

    fn finish_run(&mut self, status: &str, elapsed_ms: u64, now: u64) {
        // Close whatever is still open (dropped nodes after an abort or failure).
        let open: Vec<Arc<str>> = self.nodes.keys().cloned().collect();
        for exec in open {
            if let Some(mut node) = self.nodes.remove(&exec) {
                node.span.set("graphrt.node.interrupted", true);
                let elapsed = now.saturating_sub(node.span.start_unix_nano) / 1_000_000;
                self.finish_node(node, "interrupted", elapsed, now);
            }
        }
        let tools: Vec<(Arc<str>, String)> = self.tools.keys().cloned().collect();
        for key in tools {
            if let Some(mut tool) = self.tools.remove(&key) {
                tool.span.end_unix_nano = now;
                self.emit_with_content(tool.span, tool.content, now);
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
            let content = std::mem::take(&mut self.root_content);
            self.emit_with_content(root, content, now);
        }
        if let Some(metrics) = &self.metrics {
            metrics.record_run(&self.workflow_name, status, elapsed_ms);
        }
        self.finished = true;
    }

    fn emit_with_content(&self, mut span: Span, content: Attributes, now: u64) {
        if !content.is_empty() {
            span.add_event(semconv::CONTENT_EVENT, now, content);
        }
        self.emit(span);
    }

    fn emit(&self, mut span: Span) {
        if !self.sampled {
            return;
        }
        if !self.common.is_empty() {
            span.attributes.extend(self.common.iter().cloned());
            if let Some(session) = &self.context.session_id
                && span.attr(gen_ai::OPERATION_NAME).is_some()
            {
                span.set(gen_ai::CONVERSATION_ID, session.as_str());
            }
        }
        if !self.extra.is_empty() {
            span.attributes.extend(self.extra.iter().cloned());
        }
        self.exporter.export(span);
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
            root.set(gr::RUN_STATUS, "abandoned");
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

fn field_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.as_object().and_then(|m| m.get(key)).and_then(Value::as_str)
}

fn model_name(span: &Span) -> Option<&str> {
    match span.attr(gen_ai::REQUEST_MODEL) {
        Some(AttrValue::Str(m)) => Some(m.as_str()),
        _ => None,
    }
}

/// `gen_ai.provider.name` plus the deprecated `gen_ai.system` (raw provider name).
fn set_provider(span: &mut Span, provider: &str) {
    span.set(gen_ai::PROVIDER_NAME, semconv::provider_name(provider));
    span.set(gen_ai::SYSTEM, provider);
}

/// Model and provider from a node's `model` configuration.
fn model_config(span: &mut Span, model: Option<&Json>) {
    match model {
        Some(Json::Object(m)) => {
            if let Some(name) = m.get("name").and_then(Json::as_str) {
                span.set(gen_ai::REQUEST_MODEL, name);
            }
            if let Some(provider) = m.get("provider").and_then(Json::as_str) {
                set_provider(span, provider);
            }
        }
        Some(Json::String(name)) => span.set(gen_ai::REQUEST_MODEL, name.as_str()),
        _ => {}
    }
}

/// Model and provider reported by the node, when configuration did not name them.
fn model_from_metadata(span: &mut Span, role: NodeRole, metadata: &graphrt_core::IndexMap<String, Value>) {
    if model_name(span).is_none()
        && let Some(model) = metadata.get("model").and_then(Value::as_str)
    {
        span.set(gen_ai::REQUEST_MODEL, model);
        if role == NodeRole::Chat {
            span.name = semconv::span_name(operation::CHAT, Some(model));
        }
    }
    if span.attr(gen_ai::PROVIDER_NAME).is_none()
        && let Some(provider) = metadata.get("provider").and_then(Value::as_str)
    {
        set_provider(span, provider);
    }
}

/// `gen_ai.retrieval.top_k` and `gen_ai.data_source.id` from a knowledge-retrieval config.
fn retrieval_config(span: &mut Span, config: Option<&serde_json::Map<String, Json>>) {
    let Some(config) = config else { return };
    let top_k = config
        .get("multiple_retrieval_config")
        .and_then(|m| m.get("top_k"))
        .and_then(Json::as_i64)
        .unwrap_or(4)
        .clamp(1, 100);
    span.set(gen_ai::RETRIEVAL_TOP_K, top_k);
    if let Some(Json::Array(ids)) = config.get("dataset_ids")
        && let [Json::String(id)] = ids.as_slice()
    {
        span.set(gen_ai::DATA_SOURCE_ID, id.as_str());
    }
}

/// Split graphrt chat messages (`{role, content: [{type, text|url}], tool_calls,
/// tool_call_id}`) into semconv system instructions and input messages
/// (`{role, parts: [...]}`).
fn semconv_messages(data: &Json) -> (Vec<Json>, Vec<Json>) {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    let items = data.get("messages").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default();
    for message in items {
        let role = message.get("role").and_then(Json::as_str).unwrap_or("user");
        let mut parts: Vec<Json> = message
            .get("content")
            .and_then(Json::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(semconv_part)
            .collect();
        if role == "system" {
            system.append(&mut parts);
            continue;
        }
        for call in message.get("tool_calls").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default() {
            parts.push(serde_json::json!({
                "type": "tool_call",
                "id": call.get("id").cloned().unwrap_or(Json::Null),
                "name": call.get("name").cloned().unwrap_or(Json::Null),
                "arguments": call.get("arguments").cloned().unwrap_or(Json::Null),
            }));
        }
        if role == "tool" {
            let response = parts.iter().filter_map(|p| p.get("content").and_then(Json::as_str)).collect::<String>();
            let id = message.get("tool_call_id").cloned().unwrap_or(Json::Null);
            parts = vec![serde_json::json!({"type": "tool_call_response", "id": id, "response": response})];
        }
        messages.push(serde_json::json!({"role": role, "parts": parts}));
    }
    (system, messages)
}

fn semconv_part(part: &Json) -> Json {
    match part.get("type").and_then(Json::as_str) {
        Some("image") => {
            let url = part.get("url").and_then(Json::as_str).unwrap_or_default();
            // Inline images are base64 payloads: keep only the media type.
            let uri = if url.starts_with("data:") { url.split(',').next().unwrap_or("data:") } else { url };
            serde_json::json!({"type": "uri", "modality": "image", "uri": uri})
        }
        _ => serde_json::json!({"type": "text", "content": part.get("text").cloned().unwrap_or(Json::Null)}),
    }
}

fn set_usage(span: &mut Span, usage: &Usage) {
    if usage.is_zero() {
        return;
    }
    span.set(gen_ai::USAGE_INPUT_TOKENS, usage.prompt_tokens);
    span.set(gen_ai::USAGE_OUTPUT_TOKENS, usage.completion_tokens);
}

fn pause_kinds(reasons: &[PauseReason]) -> String {
    let mut kinds: Vec<&str> = reasons.iter().map(|r| r.kind.as_str()).collect();
    kinds.dedup();
    kinds.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_content_is_capped_without_breaking_structure() {
        let content = Content { mode: CaptureMode::SpanOnly, limit: 10 };
        let AttrValue::Structured(json) = content.structured(serde_json::json!([{"a": "0123456789abc"}, {"b": "xyz"}]))
        else {
            panic!("not structured")
        };
        assert_eq!(json[0]["a"], "0123456789…");
        assert_eq!(json[1]["b"], "…");
    }

    #[test]
    fn messages_map_to_semconv_shape() {
        let data = serde_json::json!({"messages": [
            {"role": "system", "content": [{"type": "text", "text": "be brief"}]},
            {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image", "url": "data:image/png;base64,AAAA"}]},
            {"role": "assistant", "content": [], "tool_calls": [{"id": "c1", "name": "f", "arguments": "{}"}]},
            {"role": "tool", "content": [{"type": "text", "text": "42"}], "tool_call_id": "c1"},
        ]});
        let (system, messages) = semconv_messages(&data);
        assert_eq!(system, vec![serde_json::json!({"type": "text", "content": "be brief"})]);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["parts"][1]["uri"], "data:image/png;base64");
        assert_eq!(messages[1]["parts"][0]["type"], "tool_call");
        assert_eq!(
            messages[2]["parts"][0],
            serde_json::json!({"type": "tool_call_response", "id": "c1", "response": "42"})
        );
    }
}
