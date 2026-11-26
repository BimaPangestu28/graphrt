//! `agent` node: a tool-calling loop.
//!
//! Each iteration calls the model with the available tools, runs the tool
//! calls it requests (concurrently when `parallel_tool_calls` is on) and feeds
//! the results back, until the model answers without calling tools. On the
//! last allowed iteration the tools stay declared (providers require that when
//! the history contains tool calls) but the model may not call them
//! (`tool_choice: none`), so the run always ends with an answer.
//!
//! Streaming: only the final answer is streamed on field `text`, so the
//! streamed text always equals the `text` output. Turns that may still call
//! tools are buffered; text the model writes alongside tool calls is reported
//! as an `agent_thought` progress event instead.
//!
//! Tool-call arguments that are not a JSON object are not executed; the model
//! receives an error result explaining the problem so it can retry the call.
//!
//! Tools come from the [`ToolRegistry`] service and are referenced as
//! `provider/name` (or `provider/*` for every tool of a provider). Tools listed
//! in `require_approval` pause the run before they execute; other calls of the
//! same turn run immediately. The host resumes with `{"approved": true|false}`
//! for every pending call, or per call with `{"approvals": {"<call id>": true}}`,
//! optionally with a `reason`. Calls without a decision (including when the
//! resume input is missing or not understood) keep the run paused.
//!
//! Progress events: `agent_step`, `agent_thought`, `tool_call`, `tool_result`,
//! `tool_denied`. Outputs: `text`, `tool_calls`
//! (`[{name, provider, tool, arguments, result, is_error}]`), `ui` (the UI blocks of
//! successful tool results, see [`graphrt_core::ui`]; never sent to the model), `iterations`,
//! `stopped_reason` (`completed`, `max_iterations` or `length`), `usage`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::event::Usage;
use graphrt_core::node::{ConfigError, NodeOutcome, NodeResult, PauseRequest, parse_config};
use graphrt_core::template::Template;
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolRuntime, ToolSpec};
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, NodeError, NodeOutput, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::common::{
    FinishCheck, MemoryConfig, ModelConfig, StreamMode, VisionConfig, check_finish, invoke, model_for, usage_value,
};
use crate::types::{FinishReason, Message, Role, ToolCall, ToolChoice};

/// Upper bound for `max_iterations`.
pub const MAX_ITERATIONS_LIMIT: u32 = 100;

/// Reference to a tool: `"provider/name"` or `{provider, name}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ToolRef {
    Path(String),
    Pair { provider: String, name: String },
}

impl ToolRef {
    fn split(&self) -> Option<(String, String)> {
        match self {
            ToolRef::Path(p) => {
                let (provider, name) = p.split_once('/')?;
                if provider.is_empty() || name.is_empty() {
                    return None;
                }
                Some((provider.to_owned(), name.to_owned()))
            }
            ToolRef::Pair { provider, name } if !provider.is_empty() && !name.is_empty() => {
                Some((provider.clone(), name.clone()))
            }
            ToolRef::Pair { .. } => None,
        }
    }
}

fn default_query() -> Template {
    Template::parse("{{#sys.query#}}")
}

fn default_max_iterations() -> u32 {
    10
}

fn default_true() -> bool {
    true
}

fn default_max_tool_output() -> usize {
    32_000
}

/// Configuration of the `agent` node.
#[derive(Clone, Debug, Deserialize)]
pub struct AgentConfig {
    pub model: ModelConfig,
    /// System prompt.
    #[serde(default)]
    pub instruction: Option<Template>,
    /// User message (default `{{#sys.query#}}`).
    #[serde(default = "default_query")]
    pub query: Template,
    #[serde(default)]
    pub tools: Vec<ToolRef>,
    /// Maximum model calls. Default 10.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Tool calls beyond this number in one turn are rejected with an error result.
    #[serde(default)]
    pub max_tool_calls_per_turn: Option<usize>,
    #[serde(default = "default_true")]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub memory: Option<MemoryConfig>,
    #[serde(default)]
    pub vision: VisionConfig,
    /// Tools that need host approval: `provider/name`, a tool name, or `*` for all.
    #[serde(default)]
    pub require_approval: Vec<String>,
    /// Tool results longer than this (in characters) are truncated. Default 32 000.
    #[serde(default = "default_max_tool_output")]
    pub max_tool_output_chars: usize,
}

/// A tool bound for one run.
#[derive(Clone, Debug)]
struct BoundTool {
    /// Name exposed to the model (unique, provider-safe characters).
    exposed: String,
    provider: String,
    name: String,
    runtime: Arc<dyn ToolRuntime>,
    spec: ToolSpec,
}

/// One executed (or rejected) tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ToolLogEntry {
    id: String,
    name: String,
    provider: String,
    tool: String,
    /// Parsed arguments, or the raw text when the model sent malformed JSON.
    arguments: Json,
    result: String,
    is_error: bool,
    /// UI blocks in the tool's structured result (see [`graphrt_core::ui`]), for the host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ui: Vec<Json>,
}

/// Persistable loop state, used to pause for tool approval.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct AgentState {
    messages: Vec<Message>,
    iteration: u32,
    log: Vec<ToolLogEntry>,
    usage: Usage,
    /// Calls awaiting approval.
    pending: Vec<ToolCall>,
}

/// Why the loop ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StopReason {
    /// The model answered without calling tools.
    Completed,
    /// The iteration limit forced a final answer without tools.
    MaxIterations,
    /// The final answer was cut off by the output token limit.
    Length,
    /// The provider ended the turn with a reason graphrt does not recognize.
    Unrecognized(String),
}

impl StopReason {
    fn as_str(&self) -> String {
        match self {
            StopReason::Completed => "completed".into(),
            StopReason::MaxIterations => "max_iterations".into(),
            StopReason::Length => "length".into(),
            StopReason::Unrecognized(reason) => format!("finish:{reason}"),
        }
    }
}

/// The `agent` node.
#[derive(Debug)]
pub struct AgentNode {
    config: AgentConfig,
    refs: Vec<(String, String)>,
}

impl AgentNode {
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let config: AgentConfig = parse_config(doc)?;
        if config.max_iterations == 0 || config.max_iterations > MAX_ITERATIONS_LIMIT {
            return Err(ConfigError::new(doc, format!("max_iterations must be between 1 and {MAX_ITERATIONS_LIMIT}")));
        }
        let refs = config
            .tools
            .iter()
            .map(|r| {
                r.split()
                    .ok_or_else(|| ConfigError::new(doc, format!("invalid tool reference {r:?}; use `provider/name`")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { config, refs })
    }

    /// Resolve tool references against the registry, assigning unique exposed names.
    async fn bind_tools(&self, ctx: &NodeContext) -> Result<Vec<BoundTool>, NodeError> {
        if self.refs.is_empty() {
            return Ok(Vec::new());
        }
        let registry = ctx.require_service::<ToolRegistry>("tool registry (graphrt_core::tool::ToolRegistry)")?;
        let mut listings: HashMap<String, Vec<ToolSpec>> = HashMap::new();
        let mut bound: Vec<BoundTool> = Vec::new();
        let mut used: HashSet<String> = HashSet::new();
        for (provider, name) in &self.refs {
            let runtime = registry
                .get(provider)
                .cloned()
                .ok_or_else(|| NodeError::fatal("tool_not_found", format!("unknown tool provider `{provider}`")))?;
            if !listings.contains_key(provider) {
                let specs = runtime.list().await.map_err(|e| NodeError::new("tool", e.to_string()))?;
                listings.insert(provider.clone(), specs);
            }
            let specs = listings.get(provider).map(Vec::as_slice).unwrap_or_default();
            let selected: Vec<&ToolSpec> = if name == "*" {
                specs.iter().collect()
            } else {
                let spec = specs.iter().find(|s| &s.name == name).ok_or_else(|| {
                    NodeError::fatal("tool_not_found", format!("provider `{provider}` has no tool `{name}`"))
                })?;
                vec![spec]
            };
            for spec in selected {
                if bound.iter().any(|b| &b.provider == provider && b.name == spec.name) {
                    continue;
                }
                let exposed = unique_name(&spec.name, provider, &mut used);
                bound.push(BoundTool {
                    exposed: exposed.clone(),
                    provider: provider.clone(),
                    name: spec.name.clone(),
                    runtime: Arc::clone(&runtime),
                    spec: ToolSpec {
                        name: exposed,
                        description: spec.description.clone(),
                        parameters: spec.parameters.clone(),
                    },
                });
            }
        }
        Ok(bound)
    }

    fn needs_approval(&self, tool: &BoundTool) -> bool {
        self.config.require_approval.iter().any(|r| {
            r == "*" || *r == tool.exposed || *r == tool.name || *r == format!("{}/{}", tool.provider, tool.name)
        })
    }

    fn call_needs_approval(&self, tools: &[BoundTool], call: &ToolCall) -> bool {
        tools.iter().any(|t| t.exposed == call.name && self.needs_approval(t))
    }

    fn initial_state(&self, ctx: &NodeContext) -> Result<AgentState, NodeError> {
        let cfg = &self.config;
        let pool = &ctx.pool;
        let mut messages = Vec::new();
        if let Some(instruction) = &cfg.instruction {
            let text = instruction.render(pool);
            if !text.trim().is_empty() {
                messages.push(Message::system(text));
            }
        }
        if let Some(memory) = &cfg.memory {
            messages.extend(memory.history(pool)?);
        }
        let query = cfg.query.render(pool);
        let images = cfg.vision.image_parts(pool)?;
        if query.trim().is_empty() && images.is_empty() {
            if messages.iter().all(|m| m.role == Role::System) {
                return Err(NodeError::invalid_input("agent query is empty"));
            }
        } else {
            let mut user = Message::user(query);
            user.content.extend(images);
            messages.push(user);
        }
        Ok(AgentState { messages, ..AgentState::default() })
    }

    /// Apply the host's decisions to the pending calls. Returns the calls that
    /// still have no decision.
    async fn apply_decisions(
        &self,
        ctx: &NodeContext,
        state: &mut AgentState,
        tools: &[BoundTool],
        input: Option<&Value>,
    ) -> Vec<ToolCall> {
        let decisions = Approvals::from_input(input);
        let mut approved = Vec::new();
        let mut undecided = Vec::new();
        for call in std::mem::take(&mut state.pending) {
            match decisions.decision(&call.id) {
                Some(true) => approved.push(call),
                Some(false) => self.deny(ctx, state, tools, &call, decisions.reason.as_deref()),
                None => undecided.push(call),
            }
        }
        self.run_calls(ctx, state, tools, approved).await;
        undecided
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        let cfg = &self.config;
        let tools = self.bind_tools(&ctx).await?;
        let model = model_for(&ctx, &cfg.model)?;

        let mut state = match &ctx.resume {
            Some(resume) => {
                let mut state: AgentState = serde_json::from_value(resume.state.clone())
                    .map_err(|e| NodeError::fatal("invalid_resume_state", e.to_string()))?;
                let undecided = self.apply_decisions(&ctx, &mut state, &tools, resume.input.as_ref()).await;
                if !undecided.is_empty() {
                    state.pending = undecided;
                    return self.pause(&state);
                }
                state
            }
            None => self.initial_state(&ctx)?,
        };

        let (final_text, stop) = loop {
            state.iteration += 1;
            let final_turn = state.iteration >= cfg.max_iterations;
            let offer_tools = !tools.is_empty();
            let can_call_tools = offer_tools && !final_turn;
            let mut request = cfg.model.request(state.messages.clone(), &ctx.pool)?;
            if offer_tools {
                // Tools stay declared on the final turn: the history may contain tool calls,
                // which providers only accept alongside tool definitions.
                request.tools = tools.iter().map(|t| t.spec.clone()).collect();
                request.tool_choice = Some(if can_call_tools { ToolChoice::Auto } else { ToolChoice::None });
            }
            ctx.progress(
                "agent_step",
                Value::from_json(json!({"iteration": state.iteration, "tools_enabled": can_call_tools})),
            );

            // A turn that may call tools is buffered: its text is only the answer if no tools are called.
            let mode = if can_call_tools { StreamMode::Off } else { StreamMode::Raw };
            let response = invoke(&ctx, model.as_ref(), request, mode).await?;
            state.usage.add(&response.usage);
            // A content-filtered or malformed turn is an error, never an empty "completed" answer.
            let finish = check_finish(&response)?;

            if !can_call_tools || response.tool_calls.is_empty() {
                if can_call_tools && !response.text.is_empty() {
                    ctx.chunk("text", response.text.clone());
                }
                let stop = if response.finish_reason == Some(FinishReason::Length) {
                    StopReason::Length
                } else if let FinishCheck::Unknown(reason) = finish {
                    StopReason::Unrecognized(reason)
                } else if offer_tools && final_turn {
                    StopReason::MaxIterations
                } else {
                    StopReason::Completed
                };
                state.messages.push(Message::assistant(response.text.clone()));
                break (response.text, stop);
            }

            if !response.text.trim().is_empty() {
                ctx.progress(
                    "agent_thought",
                    Value::from_json(json!({"iteration": state.iteration, "text": response.text})),
                );
            }
            // Malformed arguments are never replayed to the provider: the history keeps `{}`
            // and the tool result explains the problem to the model.
            let mut assistant = response.to_message();
            for call in &mut assistant.tool_calls {
                if call.arguments_json().is_err() {
                    call.arguments = "{}".to_owned();
                }
            }
            state.messages.push(assistant);

            let mut calls = response.tool_calls;
            if let Some(limit) = cfg.max_tool_calls_per_turn {
                if calls.len() > limit {
                    for call in calls.split_off(limit) {
                        self.record(
                            &mut state,
                            &tools,
                            &call,
                            ToolOutput::error("rejected: too many tool calls in one turn"),
                        );
                    }
                }
            }
            let mut runnable = Vec::with_capacity(calls.len());
            for call in calls {
                match call.arguments_json() {
                    Ok(_) => runnable.push(call),
                    Err(e) => {
                        let message = format!("error: {e}. Call the tool again with a valid JSON object.");
                        self.record(&mut state, &tools, &call, ToolOutput::error(message));
                    }
                }
            }
            let (gated, free): (Vec<ToolCall>, Vec<ToolCall>) =
                runnable.into_iter().partition(|c| self.call_needs_approval(&tools, c));
            self.run_calls(&ctx, &mut state, &tools, free).await;
            if !gated.is_empty() {
                state.pending = gated;
                return self.pause(&state);
            }
        };

        let log: Vec<Value> = state
            .log
            .iter()
            .map(|e| {
                Value::from_json(json!({
                    "name": e.name, "provider": e.provider, "tool": e.tool,
                    "arguments": e.arguments, "result": e.result, "is_error": e.is_error,
                }))
            })
            .collect();
        let ui: Vec<Value> = state.log.iter().flat_map(|e| e.ui.iter().cloned().map(Value::from_json)).collect();
        Ok(NodeOutput::new()
            .with("text", final_text)
            .with("tool_calls", Value::from(log))
            .with("ui", Value::from(ui))
            .with("iterations", i64::from(state.iteration))
            .with("stopped_reason", stop.as_str())
            .with("usage", usage_value(&state.usage))
            .usage(state.usage)
            .into())
    }

    fn pause(&self, state: &AgentState) -> NodeResult {
        let calls: Vec<Json> =
            state.pending.iter().map(|c| json!({"id": c.id, "name": c.name, "arguments": log_arguments(c)})).collect();
        let saved = serde_json::to_value(state)
            .map_err(|e| NodeError::fatal("agent_state", format!("cannot save agent state: {e}")))?;
        Ok(NodeOutcome::Paused(PauseRequest {
            kind: "tool_approval".to_owned(),
            data: Value::from_json(json!({"calls": calls})),
            state: saved,
        }))
    }

    fn deny(
        &self,
        ctx: &NodeContext,
        state: &mut AgentState,
        tools: &[BoundTool],
        call: &ToolCall,
        reason: Option<&str>,
    ) {
        let text = match reason {
            Some(r) if !r.is_empty() => format!("The user did not approve this tool call: {r}"),
            _ => "The user did not approve this tool call.".to_owned(),
        };
        ctx.progress("tool_denied", Value::from_json(json!({"id": call.id, "name": call.name})));
        self.record(state, tools, call, ToolOutput::error(text));
    }

    /// Append a tool result to the conversation and the log.
    fn record(&self, state: &mut AgentState, tools: &[BoundTool], call: &ToolCall, output: ToolOutput) {
        let bound = tools.iter().find(|t| t.exposed == call.name);
        let text = truncate(&output.text, self.config.max_tool_output_chars);
        state.messages.push(Message::tool(call.id.clone(), text.clone()));
        // A failed tool shows nothing: its data is no answer the person should see.
        let ui = if output.is_error {
            Vec::new()
        } else {
            graphrt_core::ui::collect_ui_blocks(&output.data).iter().map(Value::to_json).collect()
        };
        state.log.push(ToolLogEntry {
            id: call.id.clone(),
            name: call.name.clone(),
            provider: bound.map(|b| b.provider.clone()).unwrap_or_default(),
            tool: bound.map(|b| b.name.clone()).unwrap_or_default(),
            arguments: log_arguments(call),
            result: text,
            is_error: output.is_error,
            ui,
        });
    }

    async fn run_calls(&self, ctx: &NodeContext, state: &mut AgentState, tools: &[BoundTool], calls: Vec<ToolCall>) {
        if calls.is_empty() {
            return;
        }
        for call in &calls {
            ctx.progress(
                "tool_call",
                Value::from_json(json!({"id": call.id, "name": call.name, "arguments": log_arguments(call)})),
            );
        }
        let outputs: Vec<ToolOutput> = if self.config.parallel_tool_calls {
            futures::future::join_all(calls.iter().map(|c| call_tool(tools, c))).await
        } else {
            let mut out = Vec::with_capacity(calls.len());
            for c in &calls {
                out.push(call_tool(tools, c).await);
            }
            out
        };
        for (call, output) in calls.iter().zip(outputs) {
            ctx.progress(
                "tool_result",
                Value::from_json(json!({"id": call.id, "name": call.name, "is_error": output.is_error})),
            );
            self.record(state, tools, call, output);
        }
    }
}

/// Arguments as shown in logs and approval requests: the parsed object, or the
/// raw text when it is not valid JSON (so nothing is hidden from the reader).
fn log_arguments(call: &ToolCall) -> Json {
    match call.arguments_json() {
        Ok(map) => Json::Object(map),
        Err(_) => Json::String(call.arguments.clone()),
    }
}

async fn call_tool(tools: &[BoundTool], call: &ToolCall) -> ToolOutput {
    let Some(tool) = tools.iter().find(|t| t.exposed == call.name) else {
        return ToolOutput::error(format!("error: unknown tool `{}`", call.name));
    };
    let arguments = match call.arguments_json() {
        Ok(map) => Json::Object(map),
        Err(e) => return ToolOutput::error(format!("error: {e}")),
    };
    match tool.runtime.call(&tool.name, arguments).await {
        Ok(output) => output,
        Err(e) => ToolOutput::error(format!("error: {e}")),
    }
}

/// Host decision for paused tool calls.
#[derive(Debug, Default)]
struct Approvals {
    all: Option<bool>,
    per_call: HashMap<String, bool>,
    reason: Option<String>,
}

impl Approvals {
    /// Recognized forms: `true`/`false`, `{"approved": bool}`,
    /// `{"approvals": {"<call id>": bool}}` (both may be combined, per-call wins),
    /// plus an optional `reason`. Anything else decides nothing.
    fn from_input(input: Option<&Value>) -> Self {
        let mut out = Self::default();
        match input {
            Some(Value::Bool(b)) => out.all = Some(*b),
            Some(Value::Object(map)) => {
                out.all = map.get("approved").and_then(Value::as_bool);
                if let Some(Value::Object(per)) = map.get("approvals") {
                    for (id, v) in per.iter() {
                        if let Some(b) = v.as_bool() {
                            out.per_call.insert(id.clone(), b);
                        }
                    }
                }
                out.reason = map.get("reason").and_then(Value::as_str).map(str::to_owned);
            }
            _ => {}
        }
        out
    }

    /// `Some(true)` approved, `Some(false)` denied, `None` no decision yet.
    fn decision(&self, id: &str) -> Option<bool> {
        self.per_call.get(id).copied().or(self.all)
    }
}

/// Model-safe tool name: `[A-Za-z0-9_-]{1,64}`, unique within the run.
fn unique_name(name: &str, provider: &str, used: &mut HashSet<String>) -> String {
    let clean = |s: &str| -> String {
        let mut out: String = s
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .take(64)
            .collect();
        if out.is_empty() {
            out.push_str("tool");
        }
        out
    };
    let mut candidate = clean(name);
    if used.contains(&candidate) {
        candidate = clean(&format!("{provider}_{name}"));
    }
    let base = candidate.clone();
    let mut n = 2;
    while used.contains(&candidate) {
        let suffix = format!("_{n}");
        let keep = 64usize.saturating_sub(suffix.len());
        candidate = format!("{}{suffix}", base.chars().take(keep).collect::<String>());
        n += 1;
    }
    used.insert(candidate.clone());
    candidate
}

fn truncate(text: &str, max_chars: usize) -> String {
    if max_chars == 0 || text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n[truncated]");
    out
}

impl Node for AgentNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }

    fn inputs(&self) -> Vec<Selector> {
        let cfg = &self.config;
        let mut out: Vec<Selector> = cfg.query.selectors().cloned().collect();
        if let Some(i) = &cfg.instruction {
            out.extend(i.selectors().cloned());
        }
        if let Some(memory) = &cfg.memory {
            out.extend(memory.selectors());
        }
        out.extend(cfg.vision.selectors());
        out.extend(cfg.model.selectors());
        out
    }
}

/// Factory for the node registry.
pub fn factory(doc: &NodeDoc) -> Result<Arc<dyn Node>, ConfigError> {
    Ok(Arc::new(AgentNode::from_doc(doc)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockModel;
    use crate::registry::ModelRegistry;
    use crate::types::Role;
    use graphrt_core::event::Event;
    use graphrt_core::node::Resume;
    use graphrt_core::tool::ToolError;
    use graphrt_core::{Services, VariablePool};
    use std::sync::Mutex;

    /// Tools: `add {a, b}` and `fail {}`.
    #[derive(Debug, Default)]
    struct MathTools {
        calls: Mutex<Vec<String>>,
    }

    impl ToolRuntime for MathTools {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
            Box::pin(async {
                Ok(vec![
                    ToolSpec {
                        name: "add".into(),
                        description: "add numbers".into(),
                        parameters: json!({"type": "object"}),
                    },
                    ToolSpec {
                        name: "fail".into(),
                        description: "always fails".into(),
                        parameters: json!({"type": "object"}),
                    },
                ])
            })
        }

        fn call<'a>(&'a self, name: &'a str, arguments: Json) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            self.calls.lock().unwrap().push(name.to_owned());
            Box::pin(async move {
                match name {
                    "add" => {
                        let a = arguments["a"].as_i64().unwrap_or(0);
                        let b = arguments["b"].as_i64().unwrap_or(0);
                        Ok(ToolOutput::text((a + b).to_string()))
                    }
                    _ => Err(ToolError { tool: name.to_owned(), message: "broken".into() }),
                }
            })
        }
    }

    /// Tools: `chart {}` (a UI block with its text), `broken_chart {}` (an error with a block).
    #[derive(Debug, Default)]
    struct ChartTools;

    impl ToolRuntime for ChartTools {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
            let spec = |name: &str| ToolSpec { name: name.into(), description: "d".into(), parameters: json!({}) };
            Box::pin(async move { Ok(vec![spec("chart"), spec("broken_chart")]) })
        }

        fn call<'a>(&'a self, name: &'a str, _arguments: Json) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async move {
                let block = Value::from_json(json!({"$ui": "chart", "version": 1, "type": "bar"}));
                Ok(match name {
                    "chart" => ToolOutput { text: "Chart shown".into(), data: block, is_error: false },
                    _ => ToolOutput { text: "bad chart".into(), data: block, is_error: true },
                })
            })
        }
    }

    struct Setup {
        model: Arc<MockModel>,
        tools: Arc<MathTools>,
        events: Arc<Mutex<Vec<Event>>>,
    }

    fn setup(responses: Vec<Vec<crate::ChatDelta>>) -> Setup {
        Setup {
            model: Arc::new(MockModel::new(responses)),
            tools: Arc::new(MathTools::default()),
            events: Arc::default(),
        }
    }

    fn ctx(s: &Setup) -> NodeContext {
        let mut tool_registry = ToolRegistry::new();
        tool_registry.add("math", Arc::clone(&s.tools) as Arc<dyn ToolRuntime>);
        tool_registry.add("charts", Arc::new(ChartTools) as Arc<dyn ToolRuntime>);
        let services = Services::new()
            .with(ModelRegistry::new().with("openai", Arc::clone(&s.model) as Arc<dyn crate::ChatModel>))
            .with(tool_registry);
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("what is 2+3?"));
        let events = Arc::clone(&s.events);
        NodeContext::for_test("agent", pool)
            .with_services(Arc::new(services))
            .with_sink(Arc::new(move |e| events.lock().unwrap().push(e)))
    }

    fn agent(extra: Json) -> AgentNode {
        let mut config = json!({
            "model": {"provider": "openai", "name": "gpt"},
            "instruction": "You are a calculator.",
            "tools": ["math/*"],
        });
        if let (Some(c), Json::Object(e)) = (config.as_object_mut(), extra) {
            c.extend(e);
        }
        AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap()
    }

    fn completed(r: NodeResult) -> NodeOutput {
        match r.unwrap() {
            NodeOutcome::Completed(o) => o,
            NodeOutcome::Paused(p) => panic!("unexpected pause: {p:?}"),
        }
    }

    #[tokio::test]
    async fn ui_blocks_of_tool_results_become_the_ui_output_but_never_reach_the_model() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "chart", "{}"), ("c2", "broken_chart", "{}")]),
            MockModel::text("Here is your chart."),
        ]);
        let out = completed(agent(json!({"tools": ["charts/*"]})).run(ctx(&s)).await);
        let ui = out.outputs["ui"].as_array().unwrap().to_vec();
        assert_eq!(ui.len(), 1, "only the successful tool's block: {ui:?}");
        assert_eq!(graphrt_core::ui::ui_block_kind(&ui[0]), Some("chart"));
        let log = out.outputs["tool_calls"].to_json();
        assert!(!log.to_string().contains("$ui"), "the log does not repeat the blocks: {log}");
        let second = &s.model.requests()[1].messages;
        assert!(!format!("{second:?}").contains("$ui"), "blocks are not sent to the model");
    }

    #[tokio::test]
    async fn loops_until_answer() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":3}"#), ("c2", "fail", "{}")]),
            MockModel::text("The answer is 5."),
        ]);
        let out = completed(agent(json!({})).run(ctx(&s)).await);
        assert_eq!(out.outputs["text"], Value::from("The answer is 5."));
        assert_eq!(out.outputs["iterations"], Value::Int(2));
        let log = out.outputs["tool_calls"].as_array().unwrap().to_vec();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].get_path(&["result"]), Some(Value::from("5")));
        assert_eq!(log[1].get_path(&["is_error"]), Some(Value::Bool(true)));
        assert_eq!(out.usage.unwrap().total_tokens, 4);

        let requests = s.model.requests();
        assert_eq!(requests[0].tools.len(), 2);
        let second = &requests[1].messages;
        assert_eq!(second[0].role, Role::System);
        assert_eq!(second[2].tool_calls.len(), 2);
        assert_eq!(second[3].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(second[3].text(), "5");
        assert!(second[4].text().contains("broken"));

        let progress: Vec<String> = s
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::NodeProgress { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(progress, ["agent_step", "tool_call", "tool_call", "tool_result", "tool_result", "agent_step"]);
    }

    #[tokio::test]
    async fn last_iteration_keeps_tools_but_forbids_calls() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "add", r#"{"a":1,"b":1}"#)]), MockModel::text("2")]);
        let out = completed(agent(json!({"max_iterations": 2})).run(ctx(&s)).await);
        assert_eq!(out.outputs["text"], Value::from("2"));
        assert_eq!(out.outputs["stopped_reason"], Value::from("max_iterations"));
        let requests = s.model.requests();
        assert_eq!(requests[0].tool_choice, Some(ToolChoice::Auto));
        assert!(!requests[1].tools.is_empty(), "tool definitions stay for the history's tool calls");
        assert_eq!(requests[1].tool_choice, Some(ToolChoice::None));
    }

    #[tokio::test]
    async fn only_the_final_answer_is_streamed() {
        let mut first = vec![crate::ChatDelta::Text("Let me check.".into())];
        first.extend(MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":3}"#)]));
        let s = setup(vec![first, MockModel::text("It is 5.")]);
        let out = completed(agent(json!({})).run(ctx(&s)).await);
        assert_eq!(out.outputs["stopped_reason"], Value::from("completed"));
        let events = s.events.lock().unwrap();
        let streamed: String = events
            .iter()
            .filter_map(|e| match e {
                Event::NodeChunk { field, chunk, .. } if field == "text" => Some(chunk.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "It is 5.");
        assert_eq!(out.outputs["text"], Value::from(streamed.as_str()));
        let thought = events.iter().find_map(|e| match e {
            Event::NodeProgress { name, data, .. } if name == "agent_thought" => data.get_path(&["text"]),
            _ => None,
        });
        assert_eq!(thought, Some(Value::from("Let me check.")));
    }

    #[tokio::test]
    async fn malformed_arguments_are_reported_back_to_the_model() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"#)]), MockModel::text("sorry")]);
        let out = completed(agent(json!({})).run(ctx(&s)).await);
        assert!(s.tools.calls.lock().unwrap().is_empty(), "malformed calls never run");
        let log = out.outputs["tool_calls"].as_array().unwrap().to_vec();
        assert_eq!(log[0].get_path(&["arguments"]), Some(Value::from(r#"{"a":2,"#)));
        assert_eq!(log[0].get_path(&["is_error"]), Some(Value::Bool(true)));
        let second = &s.model.requests()[1];
        assert_eq!(second.messages[2].tool_calls[0].arguments, "{}", "history never replays malformed JSON");
        assert!(second.messages[3].text().contains("invalid JSON arguments"));
    }

    #[tokio::test]
    async fn per_turn_limit_rejects_extra_calls() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "add", r#"{"a":1,"b":1}"#), ("c2", "add", r#"{"a":2,"b":2}"#)]),
            MockModel::text("ok"),
        ]);
        let out =
            completed(agent(json!({"max_tool_calls_per_turn": 1, "parallel_tool_calls": false})).run(ctx(&s)).await);
        assert_eq!(s.tools.calls.lock().unwrap().len(), 1);
        let log = out.outputs["tool_calls"].as_array().unwrap().to_vec();
        assert_eq!(log[0].get_path(&["is_error"]), Some(Value::Bool(true)));
    }

    #[tokio::test]
    async fn approval_pauses_and_resumes() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":2}"#)]), MockModel::text("4")]);
        let node = agent(json!({"tools": [{"provider": "math", "name": "add"}], "require_approval": ["math/add"]}));
        let paused = match node.run(ctx(&s)).await.unwrap() {
            NodeOutcome::Paused(p) => p,
            NodeOutcome::Completed(_) => panic!("expected pause"),
        };
        assert_eq!(paused.kind, "tool_approval");
        assert_eq!(paused.data.get_path(&["calls", "0", "name"]), Some(Value::from("add")));
        assert!(s.tools.calls.lock().unwrap().is_empty(), "nothing runs before approval");

        // The saved state survives serialization (snapshots store it as JSON).
        let state: Json = serde_json::from_str(&paused.state.to_string()).unwrap();
        let resumed =
            ctx(&s).with_resume(Some(Resume { state, input: Some(Value::from_json(json!({"approved": true}))) }));
        let out = completed(node.run(resumed).await);
        assert_eq!(out.outputs["text"], Value::from("4"));
        assert_eq!(s.tools.calls.lock().unwrap().as_slice(), ["add"]);
    }

    #[tokio::test]
    async fn denied_calls_are_reported_to_the_model() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "add", "{}")]), MockModel::text("fine")]);
        let node = agent(json!({"require_approval": ["*"]}));
        let NodeOutcome::Paused(p) = node.run(ctx(&s)).await.unwrap() else { panic!("expected pause") };
        let input = Value::from_json(json!({"approvals": {"c1": false}, "reason": "not now"}));
        let out = completed(node.run(ctx(&s).with_resume(Some(Resume { state: p.state, input: Some(input) }))).await);
        assert_eq!(out.outputs["text"], Value::from("fine"));
        assert!(s.tools.calls.lock().unwrap().is_empty());
        let last_request = s.model.requests().pop().unwrap();
        assert!(last_request.messages.iter().any(|m| m.text().contains("not now")));
    }

    #[tokio::test]
    async fn only_gated_calls_wait_for_approval() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "add", r#"{"a":1,"b":1}"#), ("c2", "fail", "{}")]),
            MockModel::text("done"),
        ]);
        let node = agent(json!({"require_approval": ["math/fail"]}));
        let NodeOutcome::Paused(p) = node.run(ctx(&s)).await.unwrap() else { panic!("expected pause") };
        assert_eq!(s.tools.calls.lock().unwrap().as_slice(), ["add"], "ungated calls run immediately");
        assert_eq!(p.data.get_path(&["calls", "0", "id"]), Some(Value::from("c2")));
        assert!(p.data.get_path(&["calls", "1"]).is_none());

        // No decision (or one that is not understood) keeps the run paused.
        for input in [None, Some(Value::from("yes")), Some(Value::from_json(json!({"approved": "true"})))] {
            let resumed = ctx(&s).with_resume(Some(Resume { state: p.state.clone(), input }));
            assert!(matches!(node.run(resumed).await.unwrap(), NodeOutcome::Paused(_)));
        }
        assert_eq!(s.tools.calls.lock().unwrap().len(), 1);

        let input = Value::from_json(json!({"approvals": {"c2": true}}));
        let out = completed(node.run(ctx(&s).with_resume(Some(Resume { state: p.state, input: Some(input) }))).await);
        assert_eq!(out.outputs["text"], Value::from("done"));
        assert_eq!(s.tools.calls.lock().unwrap().as_slice(), ["add", "fail"]);
    }

    #[tokio::test]
    async fn per_call_decisions_can_arrive_in_steps() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "add", r#"{"a":1,"b":1}"#), ("c2", "add", r#"{"a":2,"b":2}"#)]),
            MockModel::text("ok"),
        ]);
        let node = agent(json!({"require_approval": ["*"], "parallel_tool_calls": false}));
        let NodeOutcome::Paused(p) = node.run(ctx(&s)).await.unwrap() else { panic!("expected pause") };
        let input = Value::from_json(json!({"approvals": {"c1": true}}));
        let NodeOutcome::Paused(p2) =
            node.run(ctx(&s).with_resume(Some(Resume { state: p.state, input: Some(input) }))).await.unwrap()
        else {
            panic!("c2 is still undecided")
        };
        assert_eq!(s.tools.calls.lock().unwrap().len(), 1);
        assert_eq!(p2.data.get_path(&["calls", "0", "id"]), Some(Value::from("c2")));
        let input = Value::from_json(json!({"approved": false}));
        let out = completed(node.run(ctx(&s).with_resume(Some(Resume { state: p2.state, input: Some(input) }))).await);
        assert_eq!(out.outputs["text"], Value::from("ok"));
        assert_eq!(s.tools.calls.lock().unwrap().len(), 1, "c2 was denied");
    }

    #[test]
    fn inputs_cover_templates_and_memory() {
        let node = agent(json!({"query": "{{#start.q#}}", "instruction": "{{#env.persona#}}", "memory": {}}));
        let inputs: Vec<String> = node.inputs().iter().map(ToString::to_string).collect();
        for expected in ["start.q", "env.persona", "sys.history", "sys.query"] {
            assert!(inputs.iter().any(|i| i == expected), "missing {expected} in {inputs:?}");
        }
    }

    #[tokio::test]
    async fn unknown_provider_is_fatal() {
        let s = setup(vec![]);
        let err = agent(json!({"tools": ["nope/x"]})).run(ctx(&s)).await.unwrap_err();
        assert!(!err.retryable);
        assert_eq!(err.kind, "tool_not_found");
    }

    #[test]
    fn config_validation_and_names() {
        let bad = NodeDoc::new("a", "agent", json!({"model": {"provider": "p", "name": "m"}, "tools": ["no-slash"]}));
        assert!(AgentNode::from_doc(&bad).is_err());
        let bad = NodeDoc::new("a", "agent", json!({"model": {"provider": "p", "name": "m"}, "max_iterations": 0}));
        assert!(AgentNode::from_doc(&bad).is_err());
        let mut used = HashSet::new();
        assert_eq!(unique_name("search", "web", &mut used), "search");
        assert_eq!(unique_name("search", "docs", &mut used), "docs_search");
        assert_eq!(unique_name("search", "docs", &mut used), "docs_search_2");
        assert_eq!(unique_name("a.b/c", "p", &mut used), "a_b_c");
    }
}
