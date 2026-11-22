//! `agent` node: a tool-calling loop.
//!
//! Each iteration calls the model with the available tools, runs the tool
//! calls it requests (concurrently when `parallel_tool_calls` is on) and feeds
//! the results back, until the model answers without calling tools. On the
//! last allowed iteration the tools stay declared (providers require that when
//! the history contains tool calls) but the model may not call them
//! (`tool_choice: none`), so the run always ends with an answer.
//!
//! Streaming: every turn streams its answer text on field `text` token by
//! token (reasoning on `reasoning`), including turns that may still call
//! tools, so the final answer reaches the client as it is generated.
//! Tool-call fragments are never streamed as text. Once a turn produces its
//! first tool-call fragment, the rest of that turn's text is no longer
//! streamed. Text a turn streamed before its tool calls (a *preamble* such as
//! "Let me look that up.") cannot be taken back, so it stays part of the
//! answer: the `text` output is every streamed preamble followed by the final
//! answer, separated by a blank line (streamed as its own `"\n\n"` chunk),
//! and the streamed text always equals the `text` output. The full text of a
//! turn that called tools is also reported as an `agent_thought` progress
//! event (`streamed: true` when part of it was streamed).
//!
//! `stream_tool_turns: false` restores buffered tool turns: only the final
//! answer is streamed (as one chunk when the turn could still have called
//! tools) and preambles are reported only as `agent_thought`, never in `text`.
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
//! Tool results may carry images (see `docs/tool-images.md`): they are checked
//! against the limits of [`check_images`] (invalid ones are dropped with a
//! note to the model) and follow the result text in the `tool` message.
//! `keep_tool_images` (default 3, `null` for all) bounds how many image-bearing
//! results keep their images.
//!
//! `tool_context` sends per-run values (e.g. the end user's JWT) to the tools of
//! a provider beside their arguments, as a [`ToolCallContext`]. The model never
//! sees them, they are never recorded, and they are only rendered (with secrets
//! exposed) for providers that [accept secrets](ToolRuntime::accepts_secrets);
//! any other provider fails the node. The values are registered as run secrets
//! and scrubbed from tool results and logged arguments.
//!
//! Progress events: `agent_step`, `agent_thought`, `tool_call`, `tool_result`,
//! `tool_denied`. Outputs: `text`, `tool_calls`
//! (`[{name, provider, tool, arguments, result, is_error, images}]`), `iterations`,
//! `stopped_reason` (`completed`, `max_iterations` or `length`), `usage`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::event::Usage;
use graphrt_core::node::{ConfigError, NodeOutcome, NodeResult, PauseRequest, parse_config};
use graphrt_core::redact::redact_secrets;
use graphrt_core::template::Template;
use graphrt_core::tool::{ToolCallContext, ToolImage, ToolOutput, ToolRegistry, ToolRuntime, ToolSpec, check_images};
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, NodeError, NodeOutput, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::common::{
    FinishCheck, MemoryConfig, ModelConfig, StreamField, StreamMode, VisionConfig, check_finish, invoke, model_for,
    usage_value,
};
use crate::types::{
    ChatDelta, ChatModel, ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role, ToolCall, ToolChoice,
    collect,
};

/// Streamed between a preamble and the text that follows it in a later turn.
const PREAMBLE_SEPARATOR: &str = "\n\n";

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

fn default_keep_tool_images() -> Option<usize> {
    Some(3)
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
    /// Keep the images of only the last N image-bearing tool results in the
    /// conversation (older ones become a text placeholder). Default 3, since
    /// images are resent on every turn; `null` keeps every image.
    #[serde(default = "default_keep_tool_images")]
    pub keep_tool_images: Option<usize>,
    /// Stream the text of turns that may still call tools as it is generated
    /// (see the module docs). Default `true`; `false` buffers those turns.
    #[serde(default = "default_true")]
    pub stream_tool_turns: bool,
    /// Per provider, named values sent with every call to that provider's tools
    /// (`{provider: {name: template}}`), outside the arguments. See the module docs.
    /// Kept as raw JSON (parsed by [`AgentNode::from_doc`]) so no extra serde
    /// code is generated for a rarely used field.
    #[serde(default)]
    pub tool_context: serde_json::Map<String, Json>,
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
    /// Rendered `tool_context` of the provider, shared by its tools.
    context: Option<Arc<ToolCallContext>>,
}

impl BoundTool {
    fn context(&self) -> &ToolCallContext {
        self.context.as_deref().unwrap_or(ToolCallContext::empty())
    }
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
    /// Images sent to the model with the result.
    #[serde(default)]
    images: usize,
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
    /// Answer text streamed so far (preambles of tool-calling turns).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    answer: String,
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
    /// Parsed `tool_context`.
    tool_context: Vec<ProviderContext>,
}

/// One provider's `tool_context`: provider name and `(name, template)` pairs.
type ProviderContext = (String, Vec<(String, Template)>);

/// Parse `tool_context`: every provider must appear in `tools`, every value
/// must be a non-empty name mapped to a template string.
fn parse_tool_context(
    doc: &NodeDoc,
    raw: &serde_json::Map<String, Json>,
    refs: &[(String, String)],
) -> Result<Vec<ProviderContext>, ConfigError> {
    let mut parsed = Vec::with_capacity(raw.len());
    for (provider, values) in raw {
        if !refs.iter().any(|(p, _)| p == provider) {
            return Err(ConfigError::new(
                doc,
                format!("tool_context provider `{provider}` is not used by any entry of `tools`"),
            ));
        }
        let Json::Object(values) = values else {
            return Err(ConfigError::new(doc, format!("tool_context `{provider}` must map names to templates")));
        };
        let mut templates = Vec::with_capacity(values.len());
        for (name, value) in values {
            let (false, Json::String(text)) = (name.is_empty(), value) else {
                return Err(ConfigError::new(
                    doc,
                    format!("tool_context `{provider}.{name}` must be a non-empty name with a template string"),
                ));
            };
            templates.push((name.clone(), Template::parse(text)));
        }
        parsed.push((provider.clone(), templates));
    }
    Ok(parsed)
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
        let tool_context = parse_tool_context(doc, &config.tool_context, &refs)?;
        Ok(Self { config, refs, tool_context })
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
                    context: None,
                });
            }
        }
        self.attach_tool_context(ctx, &mut bound)?;
        Ok(bound)
    }

    /// Render `tool_context` (secrets exposed) for each configured provider and
    /// share it with that provider's bound tools. Fails when a provider does not
    /// accept secrets: silently dropping the values would hide a misconfiguration.
    fn attach_tool_context(&self, ctx: &NodeContext, bound: &mut [BoundTool]) -> Result<(), NodeError> {
        for (provider, templates) in &self.tool_context {
            let Some(first) = bound.iter().find(|b| &b.provider == provider) else { continue };
            if !first.runtime.accepts_secrets() {
                return Err(NodeError::fatal(
                    "tool_context_denied",
                    format!(
                        "tool provider `{provider}` does not accept a tool context (a plugin needs \
                         `permissions.secrets` and must be trusted, e.g. `--trust-plugin {provider}`)"
                    ),
                ));
            }
            let mut context = ToolCallContext::default();
            for (name, template) in templates {
                let value = template.render_exposed(&ctx.pool);
                // A missing variable renders empty: leave the name out so the tool can refuse.
                if value.is_empty() {
                    continue;
                }
                ctx.observe_secret(&value);
                context.secrets.insert(name.clone(), value);
            }
            let context = Arc::new(context);
            for tool in bound.iter_mut().filter(|b| &b.provider == provider) {
                tool.context = Some(Arc::clone(&context));
            }
        }
        Ok(())
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
                    return self.pause(&state, &tools);
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
            if let Some(keep) = cfg.keep_tool_images {
                prune_tool_images(&mut state.messages, keep);
            }
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

            let plan = TurnPlan {
                live: cfg.stream_tool_turns || !can_call_tools,
                stop_at_tool_call: can_call_tools,
                separate: !state.answer.is_empty(),
            };
            let (response, mut streamed) = stream_turn(&ctx, model.as_ref(), request, plan).await?;
            state.usage.add(&response.usage);
            // A content-filtered or malformed turn is an error, never an empty "completed" answer.
            let finish = check_finish(&response)?;

            if !can_call_tools || response.tool_calls.is_empty() {
                if !plan.live && !response.text.is_empty() {
                    ctx.chunk_shared(StreamField::Text.shared(), response.text.clone());
                    streamed = response.text.len();
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
                break (final_answer(std::mem::take(&mut state.answer), response.text, streamed), stop);
            }

            if !response.text.trim().is_empty() {
                ctx.progress(
                    "agent_thought",
                    Value::from_json(
                        json!({"iteration": state.iteration, "text": response.text, "streamed": streamed > 0}),
                    ),
                );
            }
            commit_preamble(&mut state.answer, &response.text, streamed);
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
            if let Some(limit) = cfg.max_tool_calls_per_turn
                && calls.len() > limit
            {
                for call in calls.split_off(limit) {
                    self.record(
                        &mut state,
                        &tools,
                        &call,
                        ToolOutput::error("rejected: too many tool calls in one turn"),
                    );
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
                return self.pause(&state, &tools);
            }
        };

        let log: Vec<Value> = state
            .log
            .iter()
            .map(|e| {
                Value::from_json(json!({
                    "name": e.name, "provider": e.provider, "tool": e.tool,
                    "arguments": e.arguments, "result": e.result, "is_error": e.is_error, "images": e.images,
                }))
            })
            .collect();
        Ok(NodeOutput::new()
            .with("text", final_text)
            .with("tool_calls", Value::from(log))
            .with("iterations", i64::from(state.iteration))
            .with("stopped_reason", stop.as_str())
            .with("usage", usage_value(&state.usage))
            .usage(state.usage)
            .into())
    }

    fn pause(&self, state: &AgentState, tools: &[BoundTool]) -> NodeResult {
        let calls: Vec<Json> = state
            .pending
            .iter()
            .map(|c| json!({"id": c.id, "name": c.name, "arguments": log_arguments(c, tools)}))
            .collect();
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

    /// A tool result's text as the model sees it: the tool's context masked,
    /// then cut to `max_tool_output_chars`.
    ///
    /// A tool may echo its context (e.g. an upstream error repeating the JWT):
    /// the model, the outputs and the progress events must never see it. Masking
    /// comes first so a cut through the secret cannot leave its prefix behind.
    fn visible_result_text(&self, tools: &[BoundTool], call: &ToolCall, text: &str) -> String {
        let bound = tools.iter().find(|t| t.exposed == call.name);
        let scrubbed = bound.map_or(Cow::Borrowed(text), |b| scrub_context(text, b.context()));
        truncate(&scrubbed, self.config.max_tool_output_chars)
    }

    /// Append a tool result to the conversation and the log.
    fn record(&self, state: &mut AgentState, tools: &[BoundTool], call: &ToolCall, output: ToolOutput) {
        let bound = tools.iter().find(|t| t.exposed == call.name);
        let mut text = self.visible_result_text(tools, call, &output.text);
        let images = match check_images(&output.images) {
            Ok(()) => output.images,
            Err(e) => {
                let note = format!("[images dropped: {e}]");
                text = if text.is_empty() { note } else { format!("{text}\n{note}") };
                Vec::new()
            }
        };
        let image_count = images.len();
        state.messages.push(Message::tool_with_images(
            call.id.clone(),
            text.clone(),
            images.iter().map(ToolImage::to_url),
        ));
        state.log.push(ToolLogEntry {
            id: call.id.clone(),
            name: call.name.clone(),
            provider: bound.map(|b| b.provider.clone()).unwrap_or_default(),
            tool: bound.map(|b| b.name.clone()).unwrap_or_default(),
            arguments: log_arguments(call, tools),
            result: text,
            is_error: output.is_error,
            images: image_count,
        });
    }

    async fn run_calls(&self, ctx: &NodeContext, state: &mut AgentState, tools: &[BoundTool], calls: Vec<ToolCall>) {
        if calls.is_empty() {
            return;
        }
        for call in &calls {
            ctx.progress(
                "tool_call",
                Value::from_json(json!({"id": call.id, "name": call.name, "arguments": log_arguments(call, tools)})),
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
        let capture = ctx.captures_content();
        for (call, output) in calls.iter().zip(outputs) {
            let mut data =
                json!({"id": call.id, "name": call.name, "is_error": output.is_error, "images": output.images.len()});
            if capture && let Some(fields) = data.as_object_mut() {
                // Same text the model and the `tool_calls` output see.
                let result = self.visible_result_text(tools, call, &output.text);
                fields.insert("result".to_owned(), Json::String(result));
            }
            ctx.progress("tool_result", Value::from_json(data));
            self.record(state, tools, call, output);
        }
    }
}

/// How one model turn streams.
#[derive(Clone, Copy, Debug)]
struct TurnPlan {
    /// Stream text and reasoning as they arrive (otherwise nothing is streamed).
    live: bool,
    /// Stop streaming text at the turn's first tool-call fragment.
    stop_at_tool_call: bool,
    /// Earlier turns streamed text: send [`PREAMBLE_SEPARATOR`] before this turn's first text.
    separate: bool,
}

/// Run one turn. Returns the response and how many leading bytes of its text
/// were streamed on field `text` (always whole deltas, so a char boundary).
/// Deltas are moved into chunks, never copied.
async fn stream_turn(
    ctx: &NodeContext,
    model: &dyn ChatModel,
    request: ChatRequest,
    plan: TurnPlan,
) -> Result<(ChatResponse, usize), NodeError> {
    if !plan.live {
        return Ok((invoke(ctx, model, request, StreamMode::Off).await?, 0));
    }
    let stream = model.chat(request).await?;
    let mut streamed = 0;
    let mut text_closed = false;
    let mut separator_pending = plan.separate;
    let response = collect(stream, |delta| match delta {
        ChatDelta::ToolCall { .. } => text_closed |= plan.stop_at_tool_call,
        ChatDelta::Text(text) if !text.is_empty() && !text_closed => {
            if std::mem::take(&mut separator_pending) {
                ctx.chunk_shared(StreamField::Text.shared(), PREAMBLE_SEPARATOR);
            }
            streamed += text.len();
            ctx.chunk_shared(StreamField::Text.shared(), text);
        }
        ChatDelta::Reasoning(text) if !text.is_empty() => ctx.chunk_shared(StreamField::Reasoning.shared(), text),
        _ => {}
    })
    .await?;
    Ok((response, streamed))
}

/// Append the streamed part of a tool-calling turn's text to the answer,
/// mirroring what [`stream_turn`] sent (separator included).
fn commit_preamble(answer: &mut String, text: &str, streamed: usize) {
    let Some(preamble) = text.get(..streamed).filter(|p| !p.is_empty()) else { return };
    if !answer.is_empty() {
        answer.push_str(PREAMBLE_SEPARATOR);
    }
    answer.push_str(preamble);
}

/// The `text` output: streamed preambles, then the final turn's text (which
/// is always streamed whole). Without preambles the final text is moved as is.
fn final_answer(mut answer: String, text: String, streamed: usize) -> String {
    if answer.is_empty() {
        return text;
    }
    commit_preamble(&mut answer, &text, streamed);
    answer
}

/// Replace the images of all but the last `keep` image-bearing tool results
/// with a text placeholder, so a screenshot loop does not resend every
/// screenshot on every turn.
fn prune_tool_images(messages: &mut [Message], keep: usize) {
    let mut seen = 0;
    for message in messages.iter_mut().rev().filter(|m| m.role == Role::Tool) {
        let count = message.images().count();
        if count == 0 {
            continue;
        }
        seen += 1;
        if seen <= keep {
            continue;
        }
        let note = if count == 1 {
            "[1 image removed from history]".to_owned()
        } else {
            format!("[{count} images removed from history]")
        };
        let text = message.text();
        let text = if text.is_empty() { note } else { format!("{text}\n{note}") };
        message.content = vec![ContentPart::Text { text }];
    }
}

/// Arguments as shown in logs and approval requests: the parsed object, or the
/// raw text when it is not valid JSON (so nothing is hidden from the reader),
/// with the tool's context values masked.
fn log_arguments(call: &ToolCall, tools: &[BoundTool]) -> Json {
    let mut arguments = match call.arguments_json() {
        Ok(map) => Json::Object(map),
        Err(_) => Json::String(call.arguments.clone()),
    };
    if let Some(tool) = tools.iter().find(|t| t.exposed == call.name)
        && !tool.context().is_empty()
    {
        scrub_json(&mut arguments, tool.context());
    }
    arguments
}

/// `text` with every value of `context` masked. Borrows when there is nothing to mask.
fn scrub_context<'a>(text: &'a str, context: &ToolCallContext) -> Cow<'a, str> {
    if context.is_empty() {
        return Cow::Borrowed(text);
    }
    let values: Vec<&str> = context.secrets.values().map(String::as_str).collect();
    redact_secrets(text, &values)
}

/// Mask the values of `context` in every string of `value`.
fn scrub_json(value: &mut Json, context: &ToolCallContext) {
    match value {
        Json::String(text) => {
            if let Cow::Owned(scrubbed) = scrub_context(text, context) {
                *text = scrubbed;
            }
        }
        Json::Array(items) => items.iter_mut().for_each(|item| scrub_json(item, context)),
        Json::Object(map) => map.values_mut().for_each(|item| scrub_json(item, context)),
        _ => {}
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
    match tool.runtime.call(&tool.name, arguments, tool.context()).await {
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
        out.extend(self.tool_context.iter().flat_map(|(_, t)| t).flat_map(|(_, t)| t.selectors().cloned()));
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

        fn call<'a>(
            &'a self,
            name: &'a str,
            arguments: Json,
            _context: &'a ToolCallContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
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

    /// Tools: `screenshot {}` (one PNG), `bad_image {}` (an unsupported type).
    #[derive(Debug, Default)]
    struct ScreenTools;

    impl ToolRuntime for ScreenTools {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
            let spec = |name: &str| ToolSpec { name: name.into(), description: "d".into(), parameters: json!({}) };
            Box::pin(async move { Ok(vec![spec("screenshot"), spec("bad_image")]) })
        }

        fn call<'a>(
            &'a self,
            name: &'a str,
            _arguments: Json,
            _context: &'a ToolCallContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async move {
                Ok(match name {
                    "screenshot" => ToolOutput::text("800x600").with_image(ToolImage::png("aGVsbG8=")),
                    _ => ToolOutput::text("odd")
                        .with_image(ToolImage::Base64 { media_type: "image/bmp".into(), data_base64: "aGVs".into() }),
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
        tool_registry.add("screen", Arc::new(ScreenTools) as Arc<dyn ToolRuntime>);
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

    const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.user-token";

    /// Tool `whoami {}` that echoes its context (as a careless upstream error would).
    #[derive(Debug)]
    struct VaultTools {
        accepts: bool,
        contexts: Mutex<Vec<ToolCallContext>>,
    }

    impl VaultTools {
        fn new(accepts: bool) -> Arc<Self> {
            Arc::new(Self { accepts, contexts: Mutex::default() })
        }
    }

    impl ToolRuntime for VaultTools {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
            let spec = ToolSpec { name: "whoami".into(), description: "d".into(), parameters: json!({}) };
            Box::pin(async move { Ok(vec![spec]) })
        }

        fn call<'a>(
            &'a self,
            _name: &'a str,
            _arguments: Json,
            context: &'a ToolCallContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            self.contexts.lock().unwrap().push(context.clone());
            let token = context.secret("user_jwt").unwrap_or("none").to_owned();
            Box::pin(async move { Ok(ToolOutput::text(format!("rejected token {token}"))) })
        }

        fn accepts_secrets(&self) -> bool {
            self.accepts
        }
    }

    fn vault_ctx(s: &Setup, vault: &Arc<VaultTools>) -> NodeContext {
        let mut registry = ToolRegistry::new();
        registry.add("vault", Arc::clone(vault) as Arc<dyn ToolRuntime>);
        let services = Services::new()
            .with(ModelRegistry::new().with("openai", Arc::clone(&s.model) as Arc<dyn crate::ChatModel>))
            .with(registry);
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("who am I?"));
        pool.insert("start", "user_jwt", Value::Secret(Arc::from(JWT)));
        let events = Arc::clone(&s.events);
        NodeContext::for_test("agent", pool)
            .with_services(Arc::new(services))
            .with_sink(Arc::new(move |e| events.lock().unwrap().push(e)))
    }

    fn vault_agent(extra: Json) -> AgentNode {
        let mut config = json!({
            "model": {"provider": "openai", "name": "gpt"},
            "tools": ["vault/whoami"],
            "tool_context": {"vault": {"user_jwt": "{{#start.user_jwt#}}", "missing": "{{#start.nope#}}"}},
        });
        if let (Some(c), Json::Object(e)) = (config.as_object_mut(), extra) {
            c.extend(e);
        }
        AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap()
    }

    #[tokio::test]
    async fn tool_context_reaches_the_tool_but_nothing_else() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "whoami", &format!(r#"{{"echo":"{JWT}"}}"#))]),
            MockModel::text("done"),
        ]);
        let vault = VaultTools::new(true);
        let secrets = Arc::new(graphrt_core::redact::SecretSet::new());
        let ctx = vault_ctx(&s, &vault).with_secret_values(Arc::clone(&secrets));
        let node = vault_agent(json!({}));
        let out = completed(node.run(ctx).await);

        let contexts = vault.contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].secret("user_jwt"), Some(JWT));
        assert_eq!(contexts[0].secret("missing"), None, "empty renders are left out");

        // Not in the model's history, the outputs or any event.
        let requests = s.model.requests();
        assert_eq!(requests[1].messages.last().unwrap().text(), "rejected token ******");
        // (The model's own arguments stay in its history as it wrote them.)
        let tool_messages: Vec<_> = requests[1].messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert!(!format!("{tool_messages:?}").contains(JWT));
        assert!(!serde_json::to_string(&out.outputs).unwrap().contains(JWT));
        let events = serde_json::to_string(&*s.events.lock().unwrap()).unwrap();
        assert!(!events.contains(JWT), "{events}");
        // Registered with the run so later node errors are scrubbed too.
        assert_eq!(secrets.scrub(&format!("bad {JWT}")), "bad ******");
        assert!(node.inputs().iter().any(|sel| sel.to_string() == "start.user_jwt"));
    }

    #[tokio::test]
    async fn captured_tool_results_never_carry_the_tool_context() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "whoami", "{}")]), MockModel::text("done")]);
        let vault = VaultTools::new(true);
        let base = vault_ctx(&s, &vault);
        let services = (*base.services).clone().with(graphrt_core::node::CaptureContent);
        completed(vault_agent(json!({})).run(base.with_services(Arc::new(services))).await);
        assert_eq!(tool_results(&s)[0].get_path(&["result"]), Some(Value::from("rejected token ******")));
        let events = serde_json::to_string(&*s.events.lock().unwrap()).unwrap();
        assert!(!events.contains(JWT), "{events}");
    }

    #[tokio::test]
    async fn tool_results_are_masked_before_they_are_cut() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "whoami", "{}")]), MockModel::text("done")]);
        let vault = VaultTools::new(true);
        let base = vault_ctx(&s, &vault);
        let services = (*base.services).clone().with(graphrt_core::node::CaptureContent);
        // The cut falls inside the echoed token: masking afterwards would miss its prefix.
        let limit = "rejected token ".len() + 12;
        let node = vault_agent(json!({"max_tool_output_chars": limit}));
        completed(node.run(base.with_services(Arc::new(services))).await);
        let token_prefix = &JWT[..12];
        let events = serde_json::to_string(&*s.events.lock().unwrap()).unwrap();
        assert!(!events.contains(token_prefix), "{events}");
        let requests = s.model.requests();
        assert!(!format!("{:?}", requests[1].messages).contains(token_prefix));
    }

    #[tokio::test]
    async fn tool_context_is_sent_again_after_approval() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "whoami", "{}")]), MockModel::text("done")]);
        let vault = VaultTools::new(true);
        let node = vault_agent(json!({"require_approval": ["*"]}));
        let paused = match node.run(vault_ctx(&s, &vault)).await.unwrap() {
            NodeOutcome::Paused(p) => p,
            NodeOutcome::Completed(_) => panic!("expected a pause"),
        };
        assert!(!paused.state.to_string().contains(JWT), "the snapshot state never holds the context");
        let resume = Resume { state: paused.state, input: Some(Value::Bool(true)) };
        completed(node.run(vault_ctx(&s, &vault).with_resume(Some(resume))).await);
        assert_eq!(vault.contexts.lock().unwrap()[0].secret("user_jwt"), Some(JWT));
    }

    #[tokio::test]
    async fn tool_context_needs_a_provider_that_accepts_secrets() {
        let s = setup(vec![MockModel::text("unused")]);
        let vault = VaultTools::new(false);
        let error = vault_agent(json!({})).run(vault_ctx(&s, &vault)).await.unwrap_err();
        assert_eq!(error.kind, "tool_context_denied");
        assert!(vault.contexts.lock().unwrap().is_empty());
    }

    #[test]
    fn tool_context_providers_must_be_used() {
        let config = json!({
            "model": {"provider": "openai", "name": "gpt"},
            "tools": ["math/add"],
            "tool_context": {"vault": {"user_jwt": "x"}},
        });
        let error = AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap_err();
        assert!(error.to_string().contains("`vault` is not used"), "{error}");
        for bad in [json!({"math": {"jwt": 1}}), json!({"math": {"": "x"}}), json!({"math": "x"})] {
            let config =
                json!({"model": {"provider": "openai", "name": "gpt"}, "tools": ["math/add"], "tool_context": bad});
            assert!(AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).is_err());
        }
    }

    fn tool_results(s: &Setup) -> Vec<Value> {
        let events = s.events.lock().unwrap();
        let results = events.iter().filter_map(|e| match e {
            Event::NodeProgress { name, data, .. } if name == "tool_result" => Some(data.clone()),
            _ => None,
        });
        results.collect()
    }

    #[tokio::test]
    async fn tool_results_carry_text_only_when_the_host_captures_content() {
        let responses = || vec![MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":3}"#)]), MockModel::text("5")];
        let s = setup(responses());
        completed(agent(json!({})).run(ctx(&s)).await);
        assert_eq!(tool_results(&s)[0].get_path(&["result"]), None);

        let s = setup(responses());
        let base = ctx(&s);
        let services = (*base.services).clone().with(graphrt_core::node::CaptureContent);
        completed(agent(json!({})).run(base.with_services(Arc::new(services))).await);
        assert_eq!(tool_results(&s)[0].get_path(&["result"]), Some(Value::from("5")));
    }

    #[tokio::test]
    async fn tool_images_reach_the_model() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "screenshot", "{}")]), MockModel::text("a login form")]);
        let out = completed(agent(json!({"tools": ["screen/*"]})).run(ctx(&s)).await);
        let requests = s.model.requests();
        let result = &requests[1].messages[3];
        assert_eq!(result.role, Role::Tool);
        assert_eq!(result.text(), "800x600");
        assert_eq!(
            result.images().cloned().collect::<Vec<_>>(),
            vec![ContentPart::Image { url: "data:image/png;base64,aGVsbG8=".into(), detail: None }]
        );
        let log = out.outputs["tool_calls"].as_array().unwrap().to_vec();
        assert_eq!(log[0].get_path(&["images"]), Some(Value::Int(1)));
        let reported = s.events.lock().unwrap().iter().find_map(|e| match e {
            Event::NodeProgress { name, data, .. } if name == "tool_result" => data.get_path(&["images"]),
            _ => None,
        });
        assert_eq!(reported, Some(Value::Int(1)));
    }

    #[tokio::test]
    async fn invalid_tool_images_are_dropped_with_a_note() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "bad_image", "{}")]), MockModel::text("ok")]);
        completed(agent(json!({"tools": ["screen/*"]})).run(ctx(&s)).await);
        let result = &s.model.requests()[1].messages[3];
        assert_eq!(result.images().count(), 0);
        assert!(
            result.text().starts_with("odd\n[images dropped: image #0: image media type `image/bmp`"),
            "{}",
            result.text()
        );
    }

    #[tokio::test]
    async fn keep_tool_images_prunes_older_screenshots() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "screenshot", "{}")]),
            MockModel::tool_calls(&[("c2", "screenshot", "{}")]),
            MockModel::tool_calls(&[("c3", "screenshot", "{}")]),
            MockModel::text("done"),
        ]);
        completed(agent(json!({"tools": ["screen/*"], "keep_tool_images": 2})).run(ctx(&s)).await);
        let last = s.model.requests().last().cloned().unwrap();
        let results: Vec<&Message> = last.messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(results.iter().map(|m| m.images().count()).collect::<Vec<_>>(), [0, 1, 1]);
        assert_eq!(results[0].text(), "800x600\n[1 image removed from history]");
        // The second request still had every image (only two existed).
        let second = &s.model.requests()[2];
        assert_eq!(second.messages.iter().map(|m| m.images().count()).sum::<usize>(), 2);
    }

    fn four_screenshots() -> Vec<Vec<crate::ChatDelta>> {
        vec![
            MockModel::tool_calls(&[("c1", "screenshot", "{}")]),
            MockModel::tool_calls(&[("c2", "screenshot", "{}")]),
            MockModel::tool_calls(&[("c3", "screenshot", "{}")]),
            MockModel::tool_calls(&[("c4", "screenshot", "{}")]),
            MockModel::text("done"),
        ]
    }

    fn images_per_tool_result(s: &Setup) -> Vec<usize> {
        let last = s.model.requests().last().cloned().unwrap();
        last.messages.iter().filter(|m| m.role == Role::Tool).map(|m| m.images().count()).collect()
    }

    #[tokio::test]
    async fn keep_tool_images_defaults_to_the_last_three() {
        let s = setup(four_screenshots());
        completed(agent(json!({"tools": ["screen/*"]})).run(ctx(&s)).await);
        assert_eq!(images_per_tool_result(&s), [0, 1, 1, 1]);
    }

    #[tokio::test]
    async fn keep_tool_images_null_keeps_every_image() {
        let s = setup(four_screenshots());
        completed(agent(json!({"tools": ["screen/*"], "keep_tool_images": null})).run(ctx(&s)).await);
        assert_eq!(images_per_tool_result(&s), [1, 1, 1, 1]);
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

    fn text_chunks(s: &Setup) -> Vec<String> {
        s.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::NodeChunk { field, chunk, .. } if field.as_ref() == "text" => Some(chunk.clone()),
                _ => None,
            })
            .collect()
    }

    fn thought(s: &Setup) -> Option<Value> {
        s.events.lock().unwrap().iter().find_map(|e| match e {
            Event::NodeProgress { name, data, .. } if name == "agent_thought" => Some(data.clone()),
            _ => None,
        })
    }

    fn tokens(pieces: &[&str]) -> Vec<crate::ChatDelta> {
        let mut out: Vec<crate::ChatDelta> = pieces.iter().map(|p| crate::ChatDelta::Text((*p).into())).collect();
        out.push(crate::ChatDelta::Finish(FinishReason::Stop));
        out
    }

    #[tokio::test]
    async fn final_answer_streams_token_by_token() {
        let s =
            setup(vec![MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":3}"#)]), tokens(&["It", " is", " 5", "."])]);
        let out = completed(agent(json!({})).run(ctx(&s)).await);
        // One chunk per provider delta, on a turn that was still allowed to call tools.
        assert_eq!(text_chunks(&s), ["It", " is", " 5", "."]);
        assert_eq!(out.outputs["text"], Value::from("It is 5."));
        assert_eq!(s.model.requests()[1].tool_choice, Some(ToolChoice::Auto));
    }

    #[tokio::test]
    async fn preamble_before_a_tool_call_is_streamed_and_kept_in_the_answer() {
        let mut first = tokens(&["Let me", " check."]);
        first.pop();
        first.extend(MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":3}"#)]));
        let s = setup(vec![first, tokens(&["It is", " 5."])]);
        let out = completed(agent(json!({})).run(ctx(&s)).await);
        let chunks = text_chunks(&s);
        assert_eq!(chunks, ["Let me", " check.", "\n\n", "It is", " 5."]);
        assert_eq!(out.outputs["text"], Value::from(chunks.concat().as_str()));
        let thought = thought(&s).unwrap();
        assert_eq!(thought.get_path(&["text"]), Some(Value::from("Let me check.")));
        assert_eq!(thought.get_path(&["streamed"]), Some(Value::Bool(true)));
        // The history keeps each turn separate: the preamble belongs to the tool-call turn.
        let second = &s.model.requests()[1].messages;
        assert_eq!(second[2].text(), "Let me check.");
        assert_eq!(second[2].tool_calls.len(), 1);
    }

    #[tokio::test]
    async fn tool_call_fragments_and_later_text_are_never_streamed() {
        use crate::ChatDelta::{Finish, Text, ToolCall};
        let first = vec![
            Text("A".into()),
            ToolCall { index: 0, id: Some("c1".into()), name: Some("add".into()), arguments: r#"{"a":"#.into() },
            Text("B".into()),
            ToolCall { index: 0, id: None, name: None, arguments: "1}".into() },
            Finish(FinishReason::ToolCalls),
        ];
        let s = setup(vec![first, MockModel::text("done")]);
        let out = completed(agent(json!({})).run(ctx(&s)).await);
        assert_eq!(text_chunks(&s), ["A", "\n\n", "done"]);
        assert_eq!(out.outputs["text"], Value::from("A\n\ndone"));
        assert_eq!(thought(&s).unwrap().get_path(&["text"]), Some(Value::from("AB")));
        assert_eq!(s.tools.calls.lock().unwrap().as_slice(), ["add"]);
    }

    #[tokio::test]
    async fn stream_tool_turns_false_buffers_tool_turns() {
        let mut first = vec![crate::ChatDelta::Text("Let me check.".into())];
        first.extend(MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":3}"#)]));
        let s = setup(vec![first, tokens(&["It is", " 5."])]);
        let out = completed(agent(json!({"stream_tool_turns": false})).run(ctx(&s)).await);
        assert_eq!(out.outputs["stopped_reason"], Value::from("completed"));
        assert_eq!(text_chunks(&s), ["It is 5."]);
        assert_eq!(out.outputs["text"], Value::from("It is 5."));
        let thought = thought(&s).unwrap();
        assert_eq!(thought.get_path(&["text"]), Some(Value::from("Let me check.")));
        assert_eq!(thought.get_path(&["streamed"]), Some(Value::Bool(false)));
    }

    #[tokio::test]
    async fn forced_final_turn_streams_token_by_token() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "add", r#"{"a":1,"b":1}"#)]), tokens(&["=", "2"])]);
        let out = completed(agent(json!({"max_iterations": 2, "stream_tool_turns": false})).run(ctx(&s)).await);
        assert_eq!(text_chunks(&s), ["=", "2"]);
        assert_eq!(out.outputs["text"], Value::from("=2"));
    }

    #[tokio::test]
    async fn preamble_survives_a_tool_approval_pause() {
        let mut first = vec![crate::ChatDelta::Text("Checking.".into())];
        first.extend(MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":2}"#)]));
        let s = setup(vec![first, MockModel::text("4")]);
        let node = agent(json!({"require_approval": ["add"]}));
        let NodeOutcome::Paused(paused) = node.run(ctx(&s)).await.unwrap() else { panic!("expected pause") };
        let state: Json = serde_json::from_str(&paused.state.to_string()).unwrap();
        let resumed =
            ctx(&s).with_resume(Some(Resume { state, input: Some(Value::from_json(json!({"approved": true}))) }));
        let out = completed(node.run(resumed).await);
        assert_eq!(out.outputs["text"], Value::from("Checking.\n\n4"));
        assert_eq!(text_chunks(&s), ["Checking.", "\n\n", "4"]);
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
