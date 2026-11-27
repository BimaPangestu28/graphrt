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
//! `skills` attaches installed skills (see [`graphrt_core::skill`]): the system
//! prompt gets one line per skill, and the model loads a skill's instructions
//! with the `load_skill` tool when a task needs them. Tools the owner granted
//! to a skill are offered to the model only after that skill was loaded in the
//! run, and wait for approval unless the grant says otherwise. An agent
//! without `skills` does none of this. The code is behind the cargo feature
//! `skills`: without it, a node that lists skills is a configuration error.
//!
//! `learning` adds the approved lessons of one agent, and of the run's user,
//! to the system prompt (see [`graphrt_core::learning`]). The node only reads
//! lessons. Nothing is learned during a run. The code is behind the cargo
//! feature `learning`: without it, a node with `learning` is a configuration
//! error.
//!
//! `context_window` keeps the conversation of a long loop inside the model's
//! context window (see [`crate::context`]): by default it acts when the
//! provider refuses a turn for its size, trims the conversation and sends the
//! turn once more. A tool call is never parted from its result. The code is
//! behind the cargo feature `context`: without it nothing is trimmed, and a
//! node that sets `context_window` to anything but `off` is a configuration
//! error.
//!
//! Progress events: `agent_step`, `agent_thought`, `tool_call`, `tool_result`,
//! `tool_denied`, `skill_tool_unavailable`, `lessons_applied`, `context_trimmed`. Outputs: `text`, `tool_calls`
//! (`[{name, provider, tool, arguments, result, is_error, images}]`), `ui` (the UI blocks of
//! successful tool results, see [`graphrt_core::ui`]; never sent to the model), `files` (feature
//! `tool-files`: the files successful tool results returned, see
//! `graphrt_core::tool::collect_files`; never sent to the model), `context`
//! (feature `context`, only when the guard removed something:
//! `{removed_messages, summarized}`), `iterations`,
//! `stopped_reason` (`completed`, `max_iterations` or `length`), `usage`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::event::Usage;
#[cfg(feature = "learning")]
use graphrt_core::learning::{LessonScope, Lessons};
use graphrt_core::node::{ConfigError, NodeOutcome, NodeResult, PauseRequest, parse_config};
use graphrt_core::redact::redact_secrets;
#[cfg(feature = "skills")]
use graphrt_core::skill::{LOAD_SKILL_TOOL, SKILL_PROVIDER, SkillToolGrant, Skills};
use graphrt_core::template::Template;
#[cfg(feature = "tool-files")]
use graphrt_core::tool::collect_files;
use graphrt_core::tool::{ToolCallContext, ToolImage, ToolOutput, ToolRegistry, ToolRuntime, ToolSpec, check_images};
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, NodeError, NodeOutput, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

#[cfg(feature = "context")]
use crate::context::{ContextState, ContextWindow};

use super::common::{
    FinishCheck, MemoryConfig, ModelConfig, StreamField, StreamMode, VisionConfig, check_finish, invoke, model_for,
    usage_value,
};
use super::tool_call_repair;
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
    /// Installed skills this agent may load (see the module docs). Needs the
    /// cargo feature `skills` and a `graphrt_core::skill::Skills` service.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Read approved lessons into the system prompt: `{agent, user}` (see the
    /// module docs). Needs the cargo feature `learning` and a
    /// `graphrt_core::learning::Lessons` service. Kept as raw JSON, like
    /// `tool_context`, so builds without the feature carry no code for it.
    #[serde(default)]
    pub learning: Option<Json>,
    /// What happens when the conversation outgrows the model's context
    /// window: `auto` (the default), `summarize`, `off` or a mapping (see
    /// [`crate::context`]). Needs the cargo feature `context`. Kept as raw
    /// JSON, like `learning`.
    #[serde(default)]
    pub context_window: Option<Json>,
}

/// `learning` of the `agent` node: whose lessons a run reads.
#[cfg(feature = "learning")]
#[derive(Clone, Debug)]
struct Learning {
    /// The agent the lessons belong to.
    agent: String,
    /// The user the run is for (a template, for example `{{#sys.user_id#}}`).
    /// Lessons of that user are added to the lessons of the agent as a whole.
    /// Without it, or when it renders empty, only the agent's lessons are read.
    user: Option<Template>,
}

/// Parse `learning`: `{agent: <name>, user?: <template>}`. Any other key is an
/// error, so a misspelled `user` cannot silently widen what a run reads.
#[cfg(feature = "learning")]
fn parse_learning(doc: &NodeDoc, raw: Option<&Json>) -> Result<Option<Learning>, ConfigError> {
    let Some(raw) = raw.filter(|raw| !raw.is_null()) else { return Ok(None) };
    let invalid = |why: &str| ConfigError::new(doc, format!("learning: {why}"));
    let fields = raw.as_object().ok_or_else(|| invalid("expected `{agent, user}`"))?;
    if let Some(unknown) = fields.keys().find(|key| !matches!(key.as_str(), "agent" | "user")) {
        return Err(invalid(&format!("unknown field `{unknown}`")));
    }
    let agent = fields.get("agent").and_then(Json::as_str).filter(|agent| !agent.is_empty());
    let agent = agent.ok_or_else(|| invalid("`agent` must be the name of the agent the lessons belong to"))?;
    let user = match fields.get("user") {
        None | Some(Json::Null) => None,
        Some(Json::String(template)) => Some(Template::parse(template)),
        Some(_) => return Err(invalid("`user` must be a template string")),
    };
    Ok(Some(Learning { agent: agent.to_owned(), user }))
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
    /// Skills that grant this tool. Empty for the agent's own tools. Otherwise
    /// the tool is offered only once one of these skills was loaded in the run.
    #[cfg(feature = "skills")]
    gates: Vec<String>,
    /// A skill grant that asks first: every call waits for approval.
    #[cfg(feature = "skills")]
    ask: bool,
    /// One of the skill tools themselves (never set for a registry provider,
    /// whatever its name), so only they can mark a skill as loaded.
    #[cfg(feature = "skills")]
    skill_tool: bool,
}

impl BoundTool {
    fn context(&self) -> &ToolCallContext {
        self.context.as_deref().unwrap_or(ToolCallContext::empty())
    }

    /// The same tool was selected a second time.
    ///
    /// A tool the agent lists itself stays as the agent configured it. A tool
    /// granted by several skills opens with any of them and asks first if any
    /// of the grants does.
    #[cfg_attr(not(feature = "skills"), allow(clippy::unused_self, clippy::needless_pass_by_ref_mut))]
    fn bound_again(&mut self, origin: Origin<'_>) {
        #[cfg(feature = "skills")]
        if let (false, Origin::Grant(grant)) = (self.gates.is_empty(), origin) {
            self.gates.push(grant.skill.clone());
            self.ask |= grant.ask;
        }
        #[cfg(not(feature = "skills"))]
        let _ = origin;
    }

    /// Whether the model may see and call this tool given the skills loaded so far.
    #[cfg(feature = "skills")]
    fn offered(&self, state: &AgentState) -> bool {
        self.gates.is_empty() || self.gates.iter().any(|gate| state.loaded_skills.contains(gate))
    }

    /// Without skill support every bound tool is one of the agent's own.
    #[cfg(not(feature = "skills"))]
    #[allow(clippy::unused_self)]
    fn offered(&self, _state: &AgentState) -> bool {
        true
    }

    /// Whether a skill grant makes every call of this tool wait for approval.
    #[cfg(feature = "skills")]
    fn asks_first(&self) -> bool {
        self.ask
    }

    #[cfg(not(feature = "skills"))]
    #[allow(clippy::unused_self)]
    fn asks_first(&self) -> bool {
        false
    }
}

/// Where a tool being bound comes from.
#[derive(Clone, Copy)]
enum Origin<'a> {
    /// Listed in the agent's own `tools`.
    Agent,
    /// One of the skill tools (`load_skill`, `read_skill_file`).
    #[cfg(feature = "skills")]
    SkillTool,
    /// Granted to a skill by the owner.
    #[cfg(feature = "skills")]
    Grant(&'a SkillToolGrant),
    /// Keeps the lifetime in use in builds without skills.
    #[cfg(not(feature = "skills"))]
    #[allow(dead_code)]
    Unused(std::marker::PhantomData<&'a ()>),
}

/// Collects the tools of one run, giving each a unique exposed name.
#[derive(Default)]
struct Binder {
    listings: HashMap<String, Vec<ToolSpec>>,
    bound: Vec<BoundTool>,
    used: HashSet<String>,
}

impl Binder {
    /// Bind `name` (or `*`) of a registry provider, listing the provider once.
    async fn bind_registered(
        &mut self,
        provider: &str,
        name: &str,
        runtime: &Arc<dyn ToolRuntime>,
        origin: Origin<'_>,
    ) -> Result<(), NodeError> {
        // The listing is taken out while it is read, so binding can borrow `self`.
        let specs = match self.listings.remove(provider) {
            Some(specs) => specs,
            None => runtime.list().await.map_err(|e| NodeError::new("tool", e.to_string()))?,
        };
        let result = self.bind_specs(provider, name, &specs, runtime, origin);
        self.listings.insert(provider.to_owned(), specs);
        result
    }

    fn bind_specs(
        &mut self,
        provider: &str,
        name: &str,
        specs: &[ToolSpec],
        runtime: &Arc<dyn ToolRuntime>,
        origin: Origin<'_>,
    ) -> Result<(), NodeError> {
        for spec in select_specs(specs, provider, name)? {
            if let Some(existing) = self.bound.iter_mut().find(|b| b.provider == provider && b.name == spec.name) {
                existing.bound_again(origin);
                continue;
            }
            let exposed = unique_name(&spec.name, provider, &mut self.used);
            #[cfg(feature = "skills")]
            let (gates, ask) = match origin {
                Origin::Grant(grant) => (vec![grant.skill.clone()], grant.ask),
                Origin::Agent | Origin::SkillTool => (Vec::new(), false),
            };
            self.bound.push(BoundTool {
                exposed: exposed.clone(),
                provider: provider.to_owned(),
                name: spec.name.clone(),
                runtime: Arc::clone(runtime),
                spec: ToolSpec {
                    name: exposed,
                    description: spec.description.clone(),
                    parameters: spec.parameters.clone(),
                },
                context: None,
                #[cfg(feature = "skills")]
                gates,
                #[cfg(feature = "skills")]
                ask,
                #[cfg(feature = "skills")]
                skill_tool: matches!(origin, Origin::SkillTool),
            });
        }
        Ok(())
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
    /// UI blocks in the tool's structured result (see [`graphrt_core::ui`]), for the host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ui: Vec<Json>,
    /// Files in the tool's structured result (see [`collect_files`]), for the host.
    #[cfg(feature = "tool-files")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    files: Vec<Json>,
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
    /// Tools already asked once to be called again with their required arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    reasked: Vec<String>,
    /// Answer text streamed so far (preambles of tool-calling turns).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    answer: String,
    /// Skills loaded in this run: their granted tools are offered to the model.
    #[cfg(feature = "skills")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    loaded_skills: Vec<String>,
    /// This run reads learned notes: a skill it loads is told they still apply.
    #[cfg(all(feature = "skills", feature = "learning"))]
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    has_notes: bool,
    /// What the context guard learned and removed in this run.
    #[cfg(feature = "context")]
    #[serde(default, skip_serializing_if = "ContextState::is_empty")]
    context: ContextState,
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
    /// Parsed `learning`.
    #[cfg(feature = "learning")]
    learning: Option<Learning>,
    /// Parsed `context_window`.
    #[cfg(feature = "context")]
    context: ContextWindow,
}

/// Whether `context_window` asks for the guard, which this build does not have.
#[cfg(not(feature = "context"))]
pub(crate) fn asks_for_context_guard(raw: Option<&Json>) -> bool {
    !matches!(raw, None | Some(Json::Null | Json::Bool(false))) && raw.and_then(Json::as_str) != Some("off")
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
        // Without the feature the field is still read, so that a workflow that
        // relies on skills fails here instead of running without them.
        #[cfg(not(feature = "skills"))]
        if !config.skills.is_empty() {
            return Err(ConfigError::new(doc, "`skills` needs a build with skill support (cargo feature `skills`)"));
        }
        #[cfg(not(feature = "learning"))]
        if config.learning.is_some() {
            return Err(ConfigError::new(
                doc,
                "`learning` needs a build with self learning support (cargo feature `learning`)",
            ));
        }
        #[cfg(feature = "learning")]
        let learning = parse_learning(doc, config.learning.as_ref())?;
        #[cfg(not(feature = "context"))]
        if asks_for_context_guard(config.context_window.as_ref()) {
            return Err(ConfigError::new(
                doc,
                "`context_window` needs a build with the context guard (cargo feature `context`)",
            ));
        }
        #[cfg(feature = "context")]
        let context = ContextWindow::parse(doc, config.context_window.as_ref())?;
        Ok(Self {
            config,
            refs,
            tool_context,
            #[cfg(feature = "learning")]
            learning,
            #[cfg(feature = "context")]
            context,
        })
    }

    /// Resolve tool references against the registry and attach the agent's
    /// skills, assigning unique exposed names. Returns the tools and the skill
    /// catalog for the system prompt (empty without skills).
    async fn bind_tools(&self, ctx: &NodeContext) -> Result<(Vec<BoundTool>, String), NodeError> {
        if self.refs.is_empty() && self.config.skills.is_empty() {
            return Ok((Vec::new(), String::new()));
        }
        let mut binder = Binder::default();
        if !self.refs.is_empty() {
            let registry = ctx.require_service::<ToolRegistry>("tool registry (graphrt_core::tool::ToolRegistry)")?;
            for (provider, name) in &self.refs {
                let runtime = registry
                    .get(provider)
                    .cloned()
                    .ok_or_else(|| NodeError::fatal("tool_not_found", format!("unknown tool provider `{provider}`")))?;
                binder.bind_registered(provider, name, &runtime, Origin::Agent).await?;
            }
        }
        #[cfg(feature = "skills")]
        let catalog = self.attach_skills(ctx, &mut binder).await?;
        #[cfg(not(feature = "skills"))]
        let catalog = String::new();
        let mut bound = binder.bound;
        self.attach_tool_context(ctx, &mut bound)?;
        Ok((bound, catalog))
    }

    /// Attach the skills of `skills`: bind the skill tools and the tools the
    /// owner granted to each skill. Returns the catalog for the system prompt.
    ///
    /// A granted tool that is not available in this host (its plugin was
    /// removed, say) is left out and reported as `skill_tool_unavailable`: the
    /// skill still loads, it just cannot use that tool.
    #[cfg(feature = "skills")]
    async fn attach_skills(&self, ctx: &NodeContext, binder: &mut Binder) -> Result<String, NodeError> {
        if self.config.skills.is_empty() {
            return Ok(String::new());
        }
        let skills = ctx.require_service::<Skills>("skill source (graphrt_core::skill::Skills)")?;
        let attached =
            skills.0.attach(&self.config.skills).await.map_err(|e| NodeError::fatal(e.kind, e.to_string()))?;
        if attached.is_empty() {
            return Ok(String::new());
        }
        if binder.bound.iter().any(|b| b.provider == SKILL_PROVIDER) {
            return Err(NodeError::fatal(
                "skill_provider_conflict",
                format!("an agent that uses skills cannot also list tools of a provider named `{SKILL_PROVIDER}`"),
            ));
        }
        let specs = attached.runtime.list().await.map_err(|e| NodeError::new("tool", e.to_string()))?;
        binder.bind_specs(SKILL_PROVIDER, "*", &specs, &attached.runtime, Origin::SkillTool)?;
        let registry = ctx.service::<ToolRegistry>();
        for grant in &attached.grants {
            let bound = match registry.and_then(|r| r.get(&grant.provider)) {
                Some(runtime) => {
                    binder.bind_registered(&grant.provider, &grant.tool, runtime, Origin::Grant(grant)).await.is_ok()
                }
                None => false,
            };
            if !bound {
                ctx.progress(
                    "skill_tool_unavailable",
                    Value::from_json(json!({
                        "skill": grant.skill, "tool": format!("{}/{}", grant.provider, grant.tool),
                    })),
                );
            }
        }
        Ok(attached.catalog)
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
        tool.asks_first()
            || self.config.require_approval.iter().any(|r| {
                r == "*" || *r == tool.exposed || *r == tool.name || *r == format!("{}/{}", tool.provider, tool.name)
            })
    }

    fn call_needs_approval(&self, tools: &[BoundTool], call: &ToolCall) -> bool {
        bound_tool(tools, call).is_some_and(|t| self.needs_approval(t))
    }

    /// The approved lessons this run reads, as a section for the system
    /// prompt (empty without `learning` or without lessons).
    #[cfg(feature = "learning")]
    async fn lesson_notes(&self, ctx: &NodeContext) -> Result<String, NodeError> {
        let Some(learning) = &self.learning else { return Ok(String::new()) };
        let lessons = ctx.require_service::<Lessons>("lesson source (graphrt_core::learning::Lessons)")?;
        let user = learning.user.as_ref().map(|user| user.render(&ctx.pool)).filter(|user| !user.trim().is_empty());
        let scope = LessonScope { agent: learning.agent.clone(), user };
        let notes = lessons.0.notes(&scope).await.map_err(|e| NodeError::fatal(e.kind, e.to_string()))?;
        if !notes.is_empty() {
            ctx.progress("lessons_applied", Value::from_json(json!({"agent": scope.agent, "ids": notes.ids})));
        }
        Ok(notes.text)
    }

    /// The conversation a run starts with. Each of `sections` (the skill
    /// catalog, the learned notes) follows the instruction in the system
    /// message. Without an instruction they are the whole system message.
    /// `images` are the vision inputs of the user message.
    fn initial_state(
        &self,
        ctx: &NodeContext,
        sections: &[&str],
        images: Vec<ContentPart>,
    ) -> Result<AgentState, NodeError> {
        let cfg = &self.config;
        let pool = &ctx.pool;
        let mut messages = Vec::new();
        let mut system = cfg.instruction.as_ref().map(|instruction| instruction.render(pool)).unwrap_or_default();
        for section in sections.iter().filter(|section| !section.is_empty()) {
            if !system.trim().is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(section);
        }
        if !system.trim().is_empty() {
            messages.push(Message::system(system));
        }
        if let Some(memory) = &cfg.memory {
            messages.extend(memory.history(pool)?);
        }
        let query = cfg.query.render(pool);
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
        let (tools, skill_catalog) = self.bind_tools(&ctx).await?;
        let model = model_for(&ctx, &self.config.model)?;
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
            None => {
                #[cfg(feature = "learning")]
                let notes = self.lesson_notes(&ctx).await?;
                #[cfg(not(feature = "learning"))]
                let notes = String::new();
                #[cfg(feature = "vision-preparer")]
                let images = self.config.vision.prepared_image_parts(&ctx).await?;
                #[cfg(not(feature = "vision-preparer"))]
                let images = self.config.vision.image_parts(&ctx.pool)?;
                #[allow(unused_mut)]
                let mut state = self.initial_state(&ctx, &[&skill_catalog, &notes], images)?;
                #[cfg(all(feature = "skills", feature = "learning"))]
                {
                    state.has_notes = !notes.is_empty();
                }
                state
            }
        };
        let (final_text, stop) = loop {
            state.iteration += 1;
            let offer =
                TurnTools { offered: !tools.is_empty(), final_turn: state.iteration >= self.config.max_iterations };
            let mut turn = self.model_turn(&ctx, model.as_ref(), &mut state, &tools, offer).await?;
            if offer.callable() && turn.response.tool_calls.is_empty() {
                read_calls_written_as_text(&mut turn, &state, &tools);
            }
            if !offer.callable() || turn.response.tool_calls.is_empty() {
                break self.answer_turn(&ctx, &mut state, turn, offer);
            }
            self.tool_turn(&ctx, &mut state, &tools, turn).await;
            if !state.pending.is_empty() {
                return self.pause(&state, &tools);
            }
        };
        Ok(agent_outputs(state, final_text, &stop))
    }

    /// Ask the model for the next turn (streaming per `stream_tool_turns`) and
    /// add its usage to the state.
    async fn model_turn(
        &self,
        ctx: &NodeContext,
        model: &dyn ChatModel,
        state: &mut AgentState,
        tools: &[BoundTool],
        offer: TurnTools,
    ) -> Result<Turn, NodeError> {
        let cfg = &self.config;
        if let Some(keep) = cfg.keep_tool_images {
            prune_tool_images(&mut state.messages, keep);
        }
        // Tools stay declared on the final turn: the history may contain tool calls,
        // which providers only accept alongside tool definitions.
        // A tool granted through a skill appears once that skill was loaded.
        let specs: Vec<ToolSpec> = if offer.offered {
            tools.iter().filter(|t| t.offered(state)).map(|t| t.spec.clone()).collect()
        } else {
            Vec::new()
        };
        let request = |messages: Vec<Message>, tools: Vec<ToolSpec>| -> Result<ChatRequest, NodeError> {
            let mut request = cfg.model.request(messages, &ctx.pool)?;
            if offer.offered {
                request.tools = tools;
                request.tool_choice = Some(if offer.callable() { ToolChoice::Auto } else { ToolChoice::None });
            }
            Ok(request)
        };
        ctx.progress(
            "agent_step",
            Value::from_json(json!({"iteration": state.iteration, "tools_enabled": offer.callable()})),
        );
        let plan = TurnPlan {
            live: cfg.stream_tool_turns || !offer.callable(),
            stop_at_tool_call: offer.callable(),
            separate: !state.answer.is_empty(),
        };
        #[cfg(not(feature = "context"))]
        let (response, streamed) = stream_turn(ctx, model, request(state.messages.clone(), specs)?, plan).await?;
        #[cfg(feature = "context")]
        let (response, streamed) = {
            // The fields of the state the guard reads and changes, borrowed apart.
            let AgentState { messages, context, usage, log, .. } = state;
            let shown = pinned_results(log, messages);
            let pinned: Vec<(&str, &str)> = shown.iter().map(|(id, text)| (*id, text.as_str())).collect();
            let scope = crate::context::Scope { pinned: &pinned, tail: 0 };
            let mut guard = self.context.session(ctx, model, &cfg.model, scope);
            loop {
                let estimate = guard.fit(context, messages, &specs, usage).await?;
                let turn = request(context.request_messages(messages), specs.clone())?;
                match stream_turn(ctx, model, turn, plan).await {
                    Ok(done) => {
                        guard.observe(context, estimate, &done.0.usage);
                        break done;
                    }
                    // A turn the provider refused for its size is trimmed and sent
                    // once more. Any other error, and a second refusal, end the turn.
                    Err(error) => guard.refused(context, messages, &specs, error, usage).await?,
                }
            }
        };
        state.usage.add(&response.usage);
        // A content-filtered or malformed turn is an error, never an empty "completed" answer.
        let finish = check_finish(&response)?;
        Ok(Turn { response, streamed, finish, live: plan.live })
    }

    /// A turn without tool calls ends the loop: its text completes the answer.
    fn answer_turn(
        &self,
        ctx: &NodeContext,
        state: &mut AgentState,
        mut turn: Turn,
        offer: TurnTools,
    ) -> (String, StopReason) {
        let response = turn.response;
        if !turn.live && !response.text.is_empty() {
            ctx.chunk_shared(StreamField::Text.shared(), response.text.clone());
            turn.streamed = response.text.len();
        }
        let stop = if response.finish_reason == Some(FinishReason::Length) {
            StopReason::Length
        } else if let FinishCheck::Unknown(reason) = turn.finish {
            StopReason::Unrecognized(reason)
        } else if offer.offered && offer.final_turn {
            StopReason::MaxIterations
        } else {
            StopReason::Completed
        };
        state.messages.push(Message::assistant(response.text.clone()));
        (final_answer(std::mem::take(&mut state.answer), response.text, turn.streamed), stop)
    }

    /// A turn with tool calls: keep its preamble, then reject, run or hold
    /// (in `state.pending`, for approval) each call.
    async fn tool_turn(&self, ctx: &NodeContext, state: &mut AgentState, tools: &[BoundTool], turn: Turn) {
        let response = turn.response;
        if !response.text.trim().is_empty() {
            ctx.progress(
                "agent_thought",
                Value::from_json(
                    json!({"iteration": state.iteration, "text": response.text, "streamed": turn.streamed > 0}),
                ),
            );
        }
        commit_preamble(&mut state.answer, &response.text, turn.streamed);
        state.messages.push(replayable_message(&response));
        let runnable = self.runnable_calls(state, tools, response.tool_calls);
        let (gated, free): (Vec<ToolCall>, Vec<ToolCall>) =
            runnable.into_iter().partition(|c| self.call_needs_approval(tools, c));
        self.run_calls(ctx, state, tools, free).await;
        state.pending = gated;
    }

    /// The calls that can run. Calls past `max_tool_calls_per_turn` and calls
    /// with malformed arguments get an error result for the model instead.
    fn runnable_calls(&self, state: &mut AgentState, tools: &[BoundTool], mut calls: Vec<ToolCall>) -> Vec<ToolCall> {
        if let Some(limit) = self.config.max_tool_calls_per_turn
            && calls.len() > limit
        {
            for call in calls.split_off(limit) {
                self.record(state, tools, &call, ToolOutput::error("rejected: too many tool calls in one turn"));
            }
        }
        let mut runnable = Vec::with_capacity(calls.len());
        for call in calls {
            // Never offered in this run so far: the model was not shown this
            // tool, so the call did not come from its tool list.
            if bound_tool(tools, &call).is_some_and(|tool| !tool.offered(state)) {
                let message = "error: this tool belongs to a skill that is not loaded. Load the skill first.";
                self.record(state, tools, &call, ToolOutput::error(message));
                continue;
            }
            match call.arguments_json() {
                Ok(arguments) => {
                    let mut call = call;
                    let Some(tool) = bound_tool(tools, &call) else {
                        runnable.push(call);
                        continue;
                    };
                    // Arguments in the type the tool's schema asks for ("400" for an integer).
                    let fitted = tool_call_repair::fit_to_schema(Json::Object(arguments), &tool.spec.parameters);
                    call.arguments = fitted.to_string();
                    let missing = match &fitted {
                        Json::Object(fitted) => tool_call_repair::missing_required(fitted, &tool.spec.parameters),
                        _ => Vec::new(),
                    };
                    // A required argument left out: the model is asked once to call again with it.
                    if !missing.is_empty() && !state.reasked.contains(&call.name) {
                        state.reasked.push(call.name.clone());
                        let message = format!(
                            "error: this call leaves out {} it needs: {}. Call `{}` again with every required argument.",
                            if missing.len() == 1 { "an argument" } else { "arguments" },
                            missing.join(", "),
                            call.name
                        );
                        self.record(state, tools, &call, ToolOutput::error(message));
                        continue;
                    }
                    runnable.push(call);
                }
                Err(e) => {
                    let message = format!("error: {e}. Call the tool again with a valid JSON object.");
                    self.record(state, tools, &call, ToolOutput::error(message));
                }
            }
        }
        runnable
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

    /// A tool result as the model, the `tool_calls` output and the captured
    /// `tool_result` progress all see it: the tool's context masked, cut to
    /// `max_tool_output_chars`, and a note in place of images that fail
    /// [`check_images`].
    ///
    /// A tool may echo its context (e.g. an upstream error repeating the JWT):
    /// the model, the outputs and the progress events must never see it. Masking
    /// comes first so a cut through the secret cannot leave its prefix behind.
    /// Every consumer takes the text from here, so none can skip the masking.
    fn visible_result(&self, tools: &[BoundTool], call: &ToolCall, output: ToolOutput) -> VisibleResult {
        let bound = bound_tool(tools, call);
        let scrubbed = bound.map_or(Cow::Borrowed(output.text.as_str()), |b| scrub_context(&output.text, b.context()));
        let mut text = truncate(&scrubbed, self.config.max_tool_output_chars);
        let images = match check_images(&output.images) {
            Ok(()) => output.images,
            Err(e) => {
                let note = format!("[images dropped: {e}]");
                text = if text.is_empty() { note } else { format!("{text}\n{note}") };
                Vec::new()
            }
        };
        // A failed tool shows nothing: its data is no answer the person should see.
        let ui = if output.is_error {
            Vec::new()
        } else {
            graphrt_core::ui::collect_ui_blocks(&output.data).iter().map(Value::to_json).collect()
        };
        VisibleResult {
            text,
            images,
            ui,
            #[cfg(feature = "tool-files")]
            files: returned_files(&output.data, output.is_error),
            is_error: output.is_error,
        }
    }

    /// Append a tool result to the conversation and the log.
    fn record(&self, state: &mut AgentState, tools: &[BoundTool], call: &ToolCall, output: ToolOutput) {
        let result = self.visible_result(tools, call, output);
        push_result(state, tools, call, result);
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
        let outputs = self.call_tools(tools, &calls).await;
        let capture = ctx.captures_content();
        for (call, output) in calls.iter().zip(outputs) {
            // The tool's own image count, including images the result drops.
            let offered_images = output.images.len();
            let result = self.visible_result(tools, call, output);
            let mut data =
                json!({"id": call.id, "name": call.name, "is_error": result.is_error, "images": offered_images});
            if capture && let Some(fields) = data.as_object_mut() {
                fields.insert("result".to_owned(), Json::String(result.text.clone()));
            }
            ctx.progress("tool_result", Value::from_json(data));
            push_result(state, tools, call, result);
        }
    }

    /// Run `calls` concurrently or one after another, per `parallel_tool_calls`.
    async fn call_tools(&self, tools: &[BoundTool], calls: &[ToolCall]) -> Vec<ToolOutput> {
        if self.config.parallel_tool_calls {
            return futures::future::join_all(calls.iter().map(|c| call_tool(tools, c))).await;
        }
        let mut outputs = Vec::with_capacity(calls.len());
        for call in calls {
            outputs.push(call_tool(tools, call).await);
        }
        outputs
    }
}

/// A tool result after [`AgentNode::visible_result`]: safe to show to the model and the host.
struct VisibleResult {
    text: String,
    images: Vec<ToolImage>,
    /// UI blocks the tool returned in its data; never sent to the model.
    ui: Vec<Json>,
    /// Files the tool returned in its data; never sent to the model.
    #[cfg(feature = "tool-files")]
    files: Vec<Json>,
    is_error: bool,
}

/// Append a visible tool result to the conversation and the log.
fn push_result(state: &mut AgentState, tools: &[BoundTool], call: &ToolCall, result: VisibleResult) {
    let bound = bound_tool(tools, call);
    #[allow(unused_mut)]
    let mut shown = result.text.clone();
    #[cfg(feature = "skills")]
    if !result.is_error
        && let Some(skill) = loaded_skill(bound, call)
    {
        // The skill is the newest text the model reads, and a model then tends to
        // follow it alone. The notes in the system message are named again.
        #[cfg(feature = "learning")]
        if state.has_notes {
            shown.push_str(NOTES_STILL_APPLY);
        }
        if !state.loaded_skills.contains(&skill) {
            state.loaded_skills.push(skill);
        }
    }
    state.messages.push(Message::tool_with_images(call.id.clone(), shown, result.images.iter().map(ToolImage::to_url)));
    state.log.push(ToolLogEntry {
        id: call.id.clone(),
        name: call.name.clone(),
        provider: bound.map(|b| b.provider.clone()).unwrap_or_default(),
        tool: bound.map(|b| b.name.clone()).unwrap_or_default(),
        arguments: log_arguments(call, tools),
        result: result.text,
        is_error: result.is_error,
        images: result.images.len(),
        ui: result.ui,
        #[cfg(feature = "tool-files")]
        files: result.files,
    });
}

/// Follows the content of a loaded skill when the run also reads learned notes.
/// Without it a model that has just read a skill follows the skill alone: measured
/// with a small hosted model, an approved note was followed in 3 of 12 runs, and in
/// 12 of 12 with this sentence, the skill's own steps kept in both.
#[cfg(all(feature = "skills", feature = "learning"))]
const NOTES_STILL_APPLY: &str = "\nThis skill adds to your instructions and does not replace them: the learned notes in \
your instructions still apply. Keep every step of the skill, including how it says to end the reply.";

/// The skill a call loads: the `name` argument of the skill tool `load_skill`.
/// Only the skill tools bound by [`AgentNode::attach_skills`] count, never a
/// registry provider that happens to use the same names.
#[cfg(feature = "skills")]
fn loaded_skill(bound: Option<&BoundTool>, call: &ToolCall) -> Option<String> {
    bound.filter(|tool| tool.skill_tool && tool.name == LOAD_SKILL_TOOL)?;
    let arguments = call.arguments_json().ok()?;
    arguments.get("name").and_then(Json::as_str).map(str::to_owned)
}

/// Whether a turn offers tools, and whether it is the last one the iteration limit allows.
#[derive(Clone, Copy, Debug)]
struct TurnTools {
    offered: bool,
    final_turn: bool,
}

impl TurnTools {
    /// The model may call tools: they are offered and this is not the forced final turn.
    fn callable(self) -> bool {
        self.offered && !self.final_turn
    }
}

/// One model turn: the response, how many leading bytes of its text were
/// already streamed, its finish check, and whether it streamed live.
struct Turn {
    response: ChatResponse,
    streamed: usize,
    finish: FinishCheck,
    live: bool,
}

/// The assistant message of a tool turn as it is kept in the history. Malformed
/// arguments are never replayed to the provider: the history keeps `{}` and
/// the tool result explains the problem to the model.
fn replayable_message(response: &ChatResponse) -> Message {
    let mut assistant = response.to_message();
    for call in &mut assistant.tool_calls {
        if call.arguments_json().is_err() {
            call.arguments = "{}".to_owned();
        }
    }
    assistant
}

/// The tool results the context guard must leave alone, as the id of the
/// call and the text the model was shown: the instructions of the skills
/// loaded in this run. Their tools stay on offer, so the model must keep the
/// text that says how to use them. A skill that was loaded twice is listed
/// once, where it was loaded last.
#[cfg(feature = "context")]
fn pinned_results<'a>(log: &'a [ToolLogEntry], messages: &[Message]) -> Vec<(&'a str, String)> {
    #[cfg(feature = "skills")]
    let pinned = {
        let mut pinned: Vec<(&str, String)> = Vec::new();
        let loads = log.iter().filter(|e| e.provider == SKILL_PROVIDER && e.tool == LOAD_SKILL_TOOL && !e.is_error);
        for entry in loads {
            pinned.retain(|(_, text)| shown_skill(text) != entry.result);
            pinned.push((&entry.id, shown_text(entry, messages)));
        }
        pinned
    };
    #[cfg(not(feature = "skills"))]
    let pinned = {
        let _ = (log, messages);
        Vec::new()
    };
    pinned
}

/// The text of a skill without the sentence the agent may add after it.
#[cfg(all(feature = "context", feature = "skills"))]
fn shown_skill(text: &str) -> &str {
    #[cfg(feature = "learning")]
    let text = text.strip_suffix(NOTES_STILL_APPLY).unwrap_or(text);
    text
}

/// What the model was shown for `entry`: its result, or its result with the
/// sentence [`push_result`] adds when learned notes apply.
#[cfg(all(feature = "context", feature = "skills"))]
fn shown_text(entry: &ToolLogEntry, messages: &[Message]) -> String {
    #[cfg(feature = "learning")]
    {
        let with_notes = format!("{}{NOTES_STILL_APPLY}", entry.result);
        let shown = messages.iter().any(|message| {
            message.role == Role::Tool
                && message.tool_call_id.as_deref() == Some(entry.id.as_str())
                && message.text() == with_notes
        });
        if shown {
            return with_notes;
        }
    }
    let _ = messages;
    entry.result.clone()
}

/// The node outputs of a finished loop.
fn agent_outputs(state: AgentState, final_text: String, stop: &StopReason) -> NodeOutcome {
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
    let ui: Vec<Value> = state.log.iter().flat_map(|e| e.ui.iter().cloned().map(Value::from_json)).collect();
    let output =
        NodeOutput::new().with("text", final_text).with("tool_calls", Value::from(log)).with("ui", Value::from(ui));
    #[cfg(feature = "tool-files")]
    let output = output.with("files", Value::array(produced_files(&state.log)));
    // Present only when the model was not sent everything.
    #[cfg(feature = "context")]
    let output = match state.context.report() {
        Some(report) => output.with("context", Value::from_json(report)),
        None => output,
    };
    output
        .with("iterations", i64::from(state.iteration))
        .with("stopped_reason", stop.as_str())
        .with("usage", usage_value(&state.usage))
        .usage(state.usage)
        .into()
}

/// The files in a tool's structured result, as JSON for the saved loop state.
/// A failed tool produced nothing the person should be handed.
#[cfg(feature = "tool-files")]
fn returned_files(data: &Value, is_error: bool) -> Vec<Json> {
    if is_error {
        return Vec::new();
    }
    collect_files(data).into_iter().map(|file| Value::File(file).to_json()).collect()
}

/// The files of every tool result, in call order. A file returned more than once
/// (written, then overwritten) appears once, where it was returned last.
#[cfg(feature = "tool-files")]
fn produced_files(log: &[ToolLogEntry]) -> Vec<Value> {
    // The log is this node's own state (never model or tool JSON), so its file objects are trusted.
    let files: Vec<Value> =
        log.iter().flat_map(|entry| entry.files.iter().cloned().map(Value::from_json_trusted)).collect();
    let identity =
        |value: &Value| value.as_file().map(|file| (file.reference.clone(), file.url.clone(), file.name.clone()));
    let mut unique: Vec<Value> = Vec::with_capacity(files.len());
    for file in files {
        unique.retain(|earlier| identity(earlier) != identity(&file));
        unique.push(file);
    }
    unique
}

/// The tools a `provider/name` reference selects from the provider's listing:
/// all of them for `*`, else the one named (an error when it is missing).
fn select_specs<'a>(specs: &'a [ToolSpec], provider: &str, name: &str) -> Result<Vec<&'a ToolSpec>, NodeError> {
    if name == "*" {
        return Ok(specs.iter().collect());
    }
    let spec = specs
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| NodeError::fatal("tool_not_found", format!("provider `{provider}` has no tool `{name}`")))?;
    Ok(vec![spec])
}

/// The bound tool a call names (by its exposed, possibly de-duplicated name).
fn bound_tool<'a>(tools: &'a [BoundTool], call: &ToolCall) -> Option<&'a BoundTool> {
    tools.iter().find(|t| t.exposed == call.name)
}

/// A turn without tool calls whose text holds calls written out as JSON (a small model's
/// `{"name": "resize", "parameters": {...}}`) gets those calls, when they name offered tools.
/// Text not yet streamed loses the written calls; text already shown stays as it was.
fn read_calls_written_as_text(turn: &mut Turn, state: &AgentState, tools: &[BoundTool]) {
    let offered: Vec<&str> =
        tools.iter().filter(|tool| tool.offered(state)).map(|tool| tool.exposed.as_str()).collect();
    let prefix = format!("text_call_{}_", state.iteration);
    let Some((calls, rest)) = tool_call_repair::calls_written_as_text(&turn.response.text, &offered, &prefix) else {
        return;
    };
    if turn.streamed == 0 {
        turn.response.text = rest;
    }
    turn.response.tool_calls = calls;
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
    if let Some(tool) = bound_tool(tools, call)
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
    let Some(tool) = bound_tool(tools, call) else {
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
        #[cfg(feature = "learning")]
        if let Some(user) = self.learning.as_ref().and_then(|learning| learning.user.as_ref()) {
            out.extend(user.selectors().cloned());
        }
        #[cfg(feature = "context")]
        out.extend(self.context.selectors());
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
            Box::pin(
                async move { Ok(vec![spec("screenshot"), spec("bad_image"), spec("chart"), spec("broken_chart")]) },
            )
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
                    "chart" => ToolOutput {
                        text: "Chart shown".into(),
                        data: Value::from_json(json!({"$ui": "chart", "version": 1, "type": "bar"})),
                        ..ToolOutput::default()
                    },
                    "broken_chart" => {
                        ToolOutput { data: Value::from_json(json!({"$ui": "chart"})), ..ToolOutput::error("bad chart") }
                    }
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
    async fn ui_blocks_of_tool_results_become_the_ui_output_but_never_reach_the_model() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "chart", "{}"), ("c2", "broken_chart", "{}")]),
            MockModel::text("Here is your chart."),
        ]);
        let out = completed(agent(json!({"tools": ["screen/*"]})).run(ctx(&s)).await);
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

    /// Tool `image_resize {path*, max_width* (integer), overwrite (boolean)}` that records
    /// the arguments it got.
    #[derive(Debug, Default)]
    struct ImageTools {
        arguments: Mutex<Vec<Json>>,
    }

    impl ToolRuntime for ImageTools {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
            Box::pin(async {
                Ok(vec![ToolSpec {
                    name: "image_resize".into(),
                    description: "resize a picture".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "max_width": {"type": "integer", "description": "Largest width in pixels"},
                            "overwrite": {"type": "boolean"}
                        },
                        "required": ["path", "max_width"]
                    }),
                }])
            })
        }

        fn call<'a>(
            &'a self,
            _name: &'a str,
            arguments: Json,
            _context: &'a ToolCallContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            self.arguments.lock().unwrap().push(arguments.clone());
            Box::pin(async move {
                Ok(match arguments["max_width"].as_i64() {
                    Some(width) => ToolOutput::text(format!("resized to {width} px")),
                    None => ToolOutput::error("invalid arguments: max_width must be an integer"),
                })
            })
        }
    }

    fn image_ctx(model: &Arc<MockModel>, tools: &Arc<ImageTools>) -> NodeContext {
        let mut tool_registry = ToolRegistry::new();
        tool_registry.add("image", Arc::clone(tools) as Arc<dyn ToolRuntime>);
        let services = Services::new()
            .with(ModelRegistry::new().with("openai", Arc::clone(model) as Arc<dyn crate::ChatModel>))
            .with(tool_registry);
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("make it 400 px wide"));
        NodeContext::for_test("agent", pool).with_services(Arc::new(services))
    }

    fn image_agent() -> AgentNode {
        let config = json!({"model": {"provider": "openai", "name": "gpt"}, "tools": ["image/*"]});
        AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap()
    }

    #[tokio::test]
    async fn arguments_are_put_into_the_types_the_tool_asks_for() {
        let model = Arc::new(MockModel::new(vec![
            MockModel::tool_calls(&[(
                "c1",
                "image_resize",
                r#"{"path": "a.png", "width": "400", "overwrite": "false"}"#,
            )]),
            MockModel::text("Done."),
        ]));
        let tools = Arc::new(ImageTools::default());

        let out = completed(image_agent().run(image_ctx(&model, &tools)).await);

        assert_eq!(out.outputs["text"], Value::from("Done."));
        assert_eq!(*tools.arguments.lock().unwrap(), [json!({"path": "a.png", "max_width": 400, "overwrite": false})]);
    }

    #[tokio::test]
    async fn a_call_written_as_text_is_made() {
        let written =
            "Sure.\n```json\n{\"name\": \"resize\", \"parameters\": {\"path\": \"a.png\", \"max_width\": 400}}\n```";
        let model = Arc::new(MockModel::new(vec![MockModel::text(written), MockModel::text("Resized to 400 px.")]));
        let tools = Arc::new(ImageTools::default());

        let out = completed(image_agent().run(image_ctx(&model, &tools)).await);

        assert_eq!(*tools.arguments.lock().unwrap(), [json!({"path": "a.png", "max_width": 400})]);
        assert_eq!(out.outputs["iterations"], Value::Int(2));
        let second = &model.requests()[1].messages;
        let assistant = second.iter().find(|message| !message.tool_calls.is_empty()).unwrap();
        assert_eq!(assistant.tool_calls[0].name, "image_resize");
        assert!(out.outputs["text"].as_str().unwrap().ends_with("Resized to 400 px."));
    }

    #[tokio::test]
    async fn a_call_without_a_required_argument_is_asked_again_once() {
        let model = Arc::new(MockModel::new(vec![
            MockModel::tool_calls(&[("c1", "image_resize", r#"{"path": "a.png"}"#)]),
            MockModel::tool_calls(&[("c2", "image_resize", r#"{"path": "a.png"}"#)]),
            MockModel::text("I could not resize it."),
        ]));
        let tools = Arc::new(ImageTools::default());

        let out = completed(image_agent().run(image_ctx(&model, &tools)).await);

        let log = out.outputs["tool_calls"].as_array().unwrap().to_vec();
        let first_result = log[0].get_path(&["result"]).unwrap();
        assert!(first_result.as_str().unwrap().contains("`max_width` (Largest width in pixels)"), "{first_result:?}");
        // Asked once; the second call reaches the tool, which answers for itself.
        assert_eq!(tools.arguments.lock().unwrap().len(), 1);
        assert!(log[1].get_path(&["result"]).unwrap().as_str().unwrap().contains("invalid arguments"));
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
    async fn captured_tool_result_text_matches_what_the_model_sees() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "bad_image", "{}")]), MockModel::text("ok")]);
        let base = ctx(&s);
        let services = (*base.services).clone().with(graphrt_core::node::CaptureContent);
        let out = completed(agent(json!({"tools": ["screen/*"]})).run(base.with_services(Arc::new(services))).await);
        let seen_by_model = s.model.requests()[1].messages[3].text();
        assert!(seen_by_model.contains("[images dropped:"), "{seen_by_model}");
        let reported = tool_results(&s)[0].get_path(&["result"]);
        assert_eq!(reported, Some(Value::from(seen_by_model.clone())), "progress event");
        let log = out.outputs["tool_calls"].as_array().unwrap().to_vec();
        assert_eq!(log[0].get_path(&["result"]), Some(Value::from(seen_by_model)), "tool_calls output");
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

    /// Tool `page {}`: a long result, like a fetched web page.
    #[cfg(feature = "context")]
    #[derive(Debug, Default)]
    struct PageTools {
        calls: Mutex<usize>,
    }

    #[cfg(feature = "context")]
    impl ToolRuntime for PageTools {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
            let spec = ToolSpec { name: "page".into(), description: "fetch a page".into(), parameters: json!({}) };
            Box::pin(async move { Ok(vec![spec]) })
        }

        fn call<'a>(
            &'a self,
            _name: &'a str,
            _arguments: Json,
            _context: &'a ToolCallContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            Box::pin(async move { Ok(ToolOutput::text(format!("page {n}: {}", "lorem ".repeat(3_500)))) })
        }
    }

    /// A context with the `page` and `math` tools and a model that fetches
    /// `pages` pages, one per turn, before the rest of its script.
    #[cfg(feature = "context")]
    fn page_ctx(pages: usize) -> (Setup, Arc<PageTools>, impl Fn() -> NodeContext) {
        let s = setup((1..=pages).map(|n| MockModel::tool_calls(&[(&format!("p{n}"), "page", "{}")])).collect());
        let page_tools = Arc::new(PageTools::default());
        let (model, tools, events) = (Arc::clone(&s.model), Arc::clone(&s.tools), Arc::clone(&s.events));
        let pages = Arc::clone(&page_tools);
        let ctx = move || {
            let mut tool_registry = ToolRegistry::new();
            tool_registry.add("math", Arc::clone(&tools) as Arc<dyn ToolRuntime>);
            tool_registry.add("web", Arc::clone(&pages) as Arc<dyn ToolRuntime>);
            let services = Services::new()
                .with(ModelRegistry::new().with("openai", Arc::clone(&model) as Arc<dyn crate::ChatModel>))
                .with(tool_registry);
            let mut pool = VariablePool::new();
            pool.insert("sys", "query", Value::from("read the pages"));
            let events = Arc::clone(&events);
            NodeContext::for_test("agent", pool)
                .with_services(Arc::new(services))
                .with_sink(Arc::new(move |e| events.lock().unwrap().push(e)))
        };
        (s, page_tools, ctx)
    }

    #[cfg(feature = "context")]
    fn page_agent(extra: Json) -> AgentNode {
        let mut config = json!({"tools": ["web/*", "math/*"], "max_iterations": 30});
        if let (Some(c), Json::Object(e)) = (config.as_object_mut(), extra) {
            c.extend(e);
        }
        agent(config)
    }

    /// Every tool result of a request has its call in the assistant message
    /// before its run of results, and every call has its result.
    #[cfg(feature = "context")]
    fn assert_request_pairs(messages: &[Message]) {
        let mut open: Vec<String> = Vec::new();
        for message in messages {
            if message.role == Role::Tool {
                let id = message.tool_call_id.clone().unwrap();
                assert!(open.contains(&id), "result {id} without its call");
                open.retain(|other| other != &id);
            } else {
                assert!(open.is_empty(), "calls without results: {open:?}");
                open = message.tool_calls.iter().map(|call| call.id.clone()).collect();
            }
        }
        assert!(open.is_empty(), "calls without results: {open:?}");
    }

    #[cfg(feature = "context")]
    fn progress_named(events: &Mutex<Vec<Event>>, wanted: &str) -> Vec<Json> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::NodeProgress { name, data, .. } if name == wanted => Some(data.to_json()),
                _ => None,
            })
            .collect()
    }

    #[cfg(feature = "context")]
    fn too_long(window: u64, sent: u64) -> crate::LlmError {
        crate::LlmError::ContextLength(format!(
            "This model's maximum context length is {window} tokens. However, your messages resulted in {sent} tokens."
        ))
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn a_turn_refused_for_its_size_is_trimmed_and_sent_again() {
        let (s, pages, ctx) = page_ctx(6);
        s.model.push_error(too_long(20_000, 36_500));
        s.model.push(MockModel::text("done"));
        let out = completed(page_agent(json!({})).run(ctx()).await);
        assert_eq!(out.outputs["text"], Value::from("done"));
        assert_eq!(out.outputs["stopped_reason"], Value::from("completed"));
        // The outputs say that the model was not sent everything.
        assert_eq!(
            out.outputs["context"].to_json(),
            json!({"removed_messages": 8, "placeholders": 0, "cut_results": 1, "summarized": false, "unsummarized_messages": 0})
        );
        assert_eq!(*pages.calls.lock().unwrap(), 6, "no tool ran twice");
        // Six tool turns, the refused turn and the same turn again.
        let requests = s.model.requests();
        assert_eq!(requests.len(), 8);
        let (refused, retried) = (&requests[6], &requests[7]);
        assert!(crate::context::estimate_messages(&retried.messages) < 15_000);
        assert!(
            crate::context::estimate_messages(&refused.messages) > 30_000,
            "nothing was trimmed before the provider refused"
        );
        assert_request_pairs(&retried.messages);
        assert_eq!(retried.messages[0], refused.messages[0], "the system message is unchanged");
        let user = retried.messages.iter().find(|m| m.role == Role::User).unwrap().text();
        assert!(user.ends_with("read the pages"), "the request stays");
        assert!(user.starts_with("[Context note, written by the runtime. 8 earlier message(s)"), "{user}");
        assert_eq!(retried.tools, refused.tools);
        let trims = progress_named(&s.events, "context_trimmed");
        assert_eq!(trims.len(), 1);
        assert_eq!(trims[0]["reason"], "provider_limit");
        // The `tool_calls` output still has every result in full.
        let log = out.outputs["tool_calls"].as_array().unwrap().to_vec();
        assert_eq!(log.len(), 6);
        assert!(log[0].get_path(&["result"]).unwrap().to_text().len() > 20_000);
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn a_second_refusal_fails_the_node_with_the_same_kind() {
        let (s, pages, ctx) = page_ctx(6);
        s.model.push_error(too_long(20_000, 36_500));
        s.model.push_error(too_long(20_000, 21_000));
        s.model.push(MockModel::text("never reached"));
        let error = page_agent(json!({})).run(ctx()).await.unwrap_err();
        assert_eq!(error.kind, "model_context_length");
        assert!(!error.retryable);
        assert!(error.message.contains("21000 tokens"), "the provider's own words: {}", error.message);
        assert!(error.message.contains("already trimmed the conversation and sent it again"), "{}", error.message);
        assert_eq!(s.model.requests().len(), 8, "sent again once, not in a loop");
        assert_eq!(*pages.calls.lock().unwrap(), 6);
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn off_fails_on_the_first_refusal_as_before() {
        let (s, _, ctx) = page_ctx(6);
        s.model.push_error(too_long(20_000, 36_500));
        s.model.push(MockModel::text("never reached"));
        let error = page_agent(json!({"context_window": "off"})).run(ctx()).await.unwrap_err();
        assert_eq!(error.kind, "model_context_length");
        assert_eq!(s.model.requests().len(), 7);
        assert!(progress_named(&s.events, "context_trimmed").is_empty());
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn a_run_that_fits_is_sent_exactly_as_before() {
        let (s, _, ctx) = page_ctx(3);
        s.model.push(MockModel::text("done"));
        let guarded = completed(page_agent(json!({})).run(ctx()).await);
        let with_guard = s.model.requests();
        let (s, _, ctx) = page_ctx(3);
        s.model.push(MockModel::text("done"));
        let plain = completed(page_agent(json!({"context_window": "off"})).run(ctx()).await);
        assert_eq!(with_guard, s.model.requests());
        assert_eq!(guarded.outputs, plain.outputs);
        assert!(!guarded.outputs.contains_key("context"), "nothing was trimmed, so there is nothing to say");
        assert!(progress_named(&s.events, "context_trimmed").is_empty());
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn a_set_limit_keeps_every_request_under_it() {
        let (s, pages, ctx) = page_ctx(12);
        s.model.push(MockModel::text("done"));
        let node = page_agent(json!({"context_window": {"max_tokens": 40_000}}));
        let out = completed(node.run(ctx()).await);
        assert_eq!(out.outputs["text"], Value::from("done"));
        assert_eq!(*pages.calls.lock().unwrap(), 12);
        for (turn, request) in s.model.requests().iter().enumerate() {
            let size =
                crate::context::estimate_messages(&request.messages) + crate::context::estimate_tools(&request.tools);
            assert!(size <= 40_000, "turn {turn}: {size}");
            assert_request_pairs(&request.messages);
        }
        // Old results became placeholders, their calls are still in the conversation.
        let last = s.model.requests().pop().unwrap();
        let placeholders =
            last.messages.iter().filter(|m| m.text().starts_with("[tool result removed to save context")).count();
        assert!(placeholders >= 3, "{placeholders}");
        assert!(last.messages.iter().any(|m| m.text() == "read the pages"), "nothing was removed, so no note");
        let trims = progress_named(&s.events, "context_trimmed");
        assert!(!trims.is_empty() && trims.len() < 6, "trimmed in large steps, not on every turn: {}", trims.len());
        assert!(trims.iter().all(|trim| trim["reason"] == "budget"));
        // The result says so too, not only the events.
        let Value::Object(context) = &out.outputs["context"] else { panic!("no context output: {:?}", out.outputs) };
        assert!(context["placeholders"].to_json().as_u64().unwrap() >= 3);
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn a_window_one_run_learned_is_used_by_the_next_run() {
        // One registry for both runs, as a host keeps it.
        let (s, pages, _) = page_ctx(6);
        let registry = ModelRegistry::new().with("openai", Arc::clone(&s.model) as Arc<dyn crate::ChatModel>);
        let (tools, events) = (Arc::clone(&s.tools), Arc::clone(&s.events));
        let ctx = move || {
            let mut tool_registry = ToolRegistry::new();
            tool_registry.add("math", Arc::clone(&tools) as Arc<dyn ToolRuntime>);
            tool_registry.add("web", Arc::clone(&pages) as Arc<dyn ToolRuntime>);
            let mut pool = VariablePool::new();
            pool.insert("sys", "query", Value::from("read the pages"));
            let events = Arc::clone(&events);
            NodeContext::for_test("agent", pool)
                .with_services(Arc::new(Services::new().with(registry.clone()).with(tool_registry)))
                .with_sink(Arc::new(move |e| events.lock().unwrap().push(e)))
        };
        s.model.push_error(too_long(20_000, 36_500));
        s.model.push(MockModel::text("done"));
        completed(page_agent(json!({})).run(ctx()).await);
        let first_run = s.model.requests().len();
        s.events.lock().unwrap().clear();
        // The second run reads as many pages, and no refusal is queued: it
        // trims before sending, from what the first run learned.
        for n in 1..=6 {
            s.model.push(MockModel::tool_calls(&[(&format!("q{n}"), "page", "{}")]));
        }
        s.model.push(MockModel::text("done again"));
        let out = completed(page_agent(json!({})).run(ctx()).await);
        assert_eq!(out.outputs["text"], Value::from("done again"));
        let requests = s.model.requests();
        assert_eq!(requests.len() - first_run, 7, "no request of the second run was refused");
        for request in &requests[first_run..] {
            let size =
                crate::context::estimate_messages(&request.messages) + crate::context::estimate_tools(&request.tools);
            assert!(size < 20_000, "{size}");
        }
        let trims = progress_named(&s.events, "context_trimmed");
        assert!(!trims.is_empty() && trims.iter().all(|trim| trim["reason"] == "budget"), "{trims:?}");
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn what_the_guard_learned_survives_a_pause() {
        let (s, _, ctx) = page_ctx(6);
        s.model.push_error(too_long(20_000, 36_500));
        s.model.push(MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":2}"#)]));
        s.model.push(MockModel::tool_calls(&[("p7", "page", "{}")]));
        s.model.push(MockModel::text("done"));
        let node = page_agent(json!({"require_approval": ["math/add"]}));
        let NodeOutcome::Paused(paused) = node.run(ctx()).await.unwrap() else { panic!("expected pause") };
        let state: Json = serde_json::from_str(&paused.state.to_string()).unwrap();
        assert_eq!(state["context"]["window"], 20_000);
        let trimmed =
            ["removed", "placeholders", "cut"].iter().map(|k| state["context"][k].as_u64().unwrap_or(0)).sum::<u64>();
        assert!(trimmed > 0, "the refusal trimmed something: {}", state["context"]);
        // The call that waits for approval was not trimmed away.
        let pending_id = paused.data.get_path(&["calls", "0", "id"]).unwrap().to_text();
        assert!(state["messages"].to_string().contains(&format!(r#""id":"{pending_id}""#)));

        let resume = Resume { state, input: Some(Value::from_json(json!({"approved": true}))) };
        let out = completed(node.run(ctx().with_resume(Some(resume))).await);
        assert_eq!(out.outputs["text"], Value::from("done"));
        assert_eq!(s.tools.calls.lock().unwrap().as_slice(), ["add"]);
        // After the resume the loop still knows the window: no request is refused or oversized.
        for request in s.model.requests().iter().skip(7) {
            assert!(crate::context::estimate_messages(&request.messages) <= 15_000);
            assert_request_pairs(&request.messages);
        }
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn the_note_is_added_to_each_request_once_and_never_stored() {
        let (s, _, ctx) = page_ctx(6);
        s.model.push_error(too_long(20_000, 36_500));
        s.model.push(MockModel::text("Pages one to four were about lorem."));
        // Two more tool turns after the trim, then a call that pauses the run.
        s.model.push(MockModel::tool_calls(&[("a1", "add", r#"{"a":1,"b":1}"#)]));
        s.model.push(MockModel::tool_calls(&[("a2", "add", r#"{"a":2,"b":2}"#)]));
        s.model.push(MockModel::tool_calls(&[("c1", "fail", "{}")]));
        let node = page_agent(json!({"require_approval": ["math/fail"], "context_window": "summarize"}));
        let NodeOutcome::Paused(paused) = node.run(ctx()).await.unwrap() else { panic!("expected pause") };
        // Every request after the trim carries the note exactly once, in front of the request.
        let requests = s.model.requests();
        assert_eq!(requests.len(), 11);
        for request in requests.iter().skip(8) {
            let text: String = request.messages.iter().map(Message::text).collect();
            assert_eq!(text.matches("[Context note").count(), 1);
            assert_eq!(text.matches("[End of context note]").count(), 1);
            let user = request.messages.iter().find(|m| m.role == Role::User).unwrap().text();
            assert!(user.contains("Pages one to four were about lorem.") && user.ends_with("read the pages"));
        }
        let state: Json = serde_json::from_str(&paused.state.to_string()).unwrap();
        // The summary and the count are in the saved state. The note itself is not in any stored message.
        assert_eq!(state["context"]["summary"], "Pages one to four were about lorem.");
        assert!(state["context"]["removed"].as_u64().unwrap() > 0);
        assert!(!state["messages"].to_string().contains("Context note"));

        let (s3, _, ctx3) = page_ctx(0);
        s3.model.push(MockModel::text("done"));
        let resume = Resume { state, input: Some(Value::from_json(json!({"approved": true}))) };
        let out = completed(node.run(ctx3().with_resume(Some(resume))).await);
        assert_eq!(out.outputs["text"], Value::from("done"));
        assert_eq!(out.outputs["context"].get_path(&["summarized"]), Some(Value::Bool(true)));
        // After the resume the summary is still what the model is told.
        let request = s3.model.requests().pop().unwrap();
        let user = request.messages.iter().find(|m| m.role == Role::User).unwrap().text();
        assert_eq!(user.matches("Pages one to four were about lorem.").count(), 1);
    }

    #[cfg(all(feature = "context", feature = "skills"))]
    #[test]
    fn the_instructions_of_loaded_skills_are_pinned() {
        let entry = |id: &str, provider: &str, tool: &str, result: &str, is_error: bool| ToolLogEntry {
            id: id.into(),
            name: tool.into(),
            provider: provider.into(),
            tool: tool.into(),
            arguments: json!({}),
            result: result.into(),
            is_error,
            images: 0,
            ui: Vec::new(),
            #[cfg(feature = "tool-files")]
            files: Vec::new(),
        };
        let log = vec![
            entry("call_0", SKILL_PROVIDER, LOAD_SKILL_TOOL, "refund rules", false),
            entry("call_0", SKILL_PROVIDER, LOAD_SKILL_TOOL, "no such skill", true),
            entry("call_0", "web", "page", "a page", false),
            entry("call_0", "web", LOAD_SKILL_TOOL, "not a skill", false),
            entry("call_1", SKILL_PROVIDER, LOAD_SKILL_TOOL, "shipping rules", false),
            // The same skill again: only where it was loaded last.
            entry("call_2", SKILL_PROVIDER, LOAD_SKILL_TOOL, "refund rules", false),
        ];
        let pinned = pinned_results(&log, &[]);
        assert_eq!(pinned, [("call_1", "shipping rules".to_owned()), ("call_2", "refund rules".to_owned())]);

        // With learned notes the model was shown a sentence more: that is the text to keep.
        #[cfg(feature = "learning")]
        {
            let messages = vec![
                Message::tool("call_1", "shipping rules"),
                Message::tool("call_2", format!("refund rules{NOTES_STILL_APPLY}")),
            ];
            let pinned = pinned_results(&log, &messages);
            assert_eq!(
                pinned,
                [("call_1", "shipping rules".to_owned()), ("call_2", format!("refund rules{NOTES_STILL_APPLY}"))]
            );
            // And the trim leaves exactly that message alone.
            let mut conversation = vec![Message::system("s"), Message::user("q")];
            for (id, text) in [("call_2", messages[1].text()), ("call_3", "r".repeat(40_000))] {
                conversation.push(Message {
                    tool_calls: vec![ToolCall { id: id.into(), name: "t".into(), arguments: "{}".into() }],
                    ..Message::assistant("")
                });
                conversation.push(Message::tool(id, text));
            }
            conversation.push(Message::user("next"));
            conversation.push(Message::assistant("ok"));
            let borrowed: Vec<(&str, &str)> = pinned.iter().map(|(id, text)| (*id, text.as_str())).collect();
            let scope = crate::context::Scope { pinned: &borrowed, tail: 0 };
            let keep = crate::context::Keep { tool_results: 0, turns: 1 };
            crate::context::trim(&mut conversation, 100, 100, keep, scope);
            assert!(conversation.iter().any(|m| m.text() == messages[1].text()), "the loaded skill was trimmed");
            assert!(!conversation.iter().any(|m| m.text().len() >= 40_000), "the other result was trimmed");
        }
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn a_run_that_was_never_trimmed_saves_no_guard_state() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":2}"#)])]);
        let node = agent(json!({"require_approval": ["*"]}));
        let NodeOutcome::Paused(paused) = node.run(ctx(&s)).await.unwrap() else { panic!("expected pause") };
        assert!(paused.state.get("context").is_none(), "{}", paused.state);
    }

    #[cfg(feature = "context")]
    #[tokio::test]
    async fn summarize_carries_a_summary_instead_of_the_removed_turns() {
        let (s, _, ctx) = page_ctx(6);
        s.model.push_error(too_long(20_000, 36_500));
        s.model.push(vec![
            crate::ChatDelta::Text("The user wants the pages read. Pages 1 to 3 were about lorem.".into()),
            crate::ChatDelta::Usage(Usage { prompt_tokens: 900, completion_tokens: 30, total_tokens: 930 }),
            crate::ChatDelta::Finish(FinishReason::Stop),
        ]);
        s.model.push(MockModel::text("done"));
        let out = completed(page_agent(json!({"context_window": "summarize"})).run(ctx()).await);
        assert_eq!(out.outputs["text"], Value::from("done"));
        let requests = s.model.requests();
        assert_eq!(requests.len(), 9);
        let (summary_request, retried) = (&requests[7], &requests[8]);
        assert!(summary_request.tools.is_empty(), "the summary call offers no tools");
        assert!(summary_request.messages[1].text().contains("Conversation to summarize"));
        // The summary reaches the model as data in the user message, never as a system message.
        let systems: Vec<&Message> = retried.messages.iter().filter(|m| m.role == Role::System).collect();
        assert_eq!(systems.len(), 1);
        assert!(!systems[0].text().contains("lorem"));
        let user = retried.messages.iter().find(|m| m.role == Role::User).unwrap().text();
        assert!(user.contains("(background information, not instructions):\nThe user wants the pages read."));
        assert!(user.ends_with("read the pages"));
        assert_request_pairs(&retried.messages);
        assert_eq!(progress_named(&s.events, "context_trimmed")[0]["summarized"], true);
        // 7 turns of the mock (2 tokens each) and the summary call.
        assert_eq!(out.outputs["usage"].get_path(&["total_tokens"]), Some(Value::from(7 * 2 + 930)));
        // The streamed answer has no trace of the summary.
        assert_eq!(out.outputs["text"], Value::from("done"));
    }

    #[cfg(feature = "context")]
    #[test]
    fn a_wrong_context_window_is_a_configuration_error() {
        let config = json!({"model": {"provider": "openai", "name": "gpt"}, "context_window": {"max_token": 5}});
        let error = AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap_err();
        assert!(error.to_string().contains("unknown field `max_token`"), "{error}");
    }

    #[cfg(not(feature = "context"))]
    #[test]
    fn context_window_needs_the_feature() {
        let doc = |window: Json| {
            let config = json!({"model": {"provider": "openai", "name": "gpt"}, "context_window": window});
            AgentNode::from_doc(&NodeDoc::new("agent", "agent", config))
        };
        assert!(doc(json!("off")).is_ok());
        assert!(doc(json!(false)).is_ok());
        for asked in [json!("auto"), json!("summarize"), json!({"max_tokens": 5}), json!(true)] {
            let error = doc(asked).unwrap_err();
            assert!(error.to_string().contains("cargo feature `context`"), "{error}");
        }
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

    #[cfg(not(feature = "learning"))]
    #[test]
    fn learning_without_learning_support_is_a_configuration_error() {
        let doc =
            NodeDoc::new("a", "agent", json!({"model": {"provider": "p", "name": "m"}, "learning": {"agent": "x"}}));
        let error = AgentNode::from_doc(&doc).unwrap_err().to_string();
        assert!(error.contains("cargo feature `learning`"), "{error}");
    }

    #[cfg(not(feature = "skills"))]
    #[test]
    fn listing_skills_without_skill_support_is_a_configuration_error() {
        let doc = NodeDoc::new("a", "agent", json!({"model": {"provider": "p", "name": "m"}, "skills": ["report"]}));
        let error = AgentNode::from_doc(&doc).unwrap_err().to_string();
        assert!(error.contains("cargo feature `skills`"), "{error}");
        let empty = NodeDoc::new("a", "agent", json!({"model": {"provider": "p", "name": "m"}, "skills": []}));
        assert!(AgentNode::from_doc(&empty).is_ok());
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

    /// Tools: `export {name}` (returns a file), `export_failed {}` (an error that carries one).
    #[cfg(feature = "tool-files")]
    #[derive(Debug, Default)]
    struct ExportTools;

    #[cfg(feature = "tool-files")]
    fn exported(name: &str) -> Value {
        Value::from(graphrt_core::value::File {
            marker: true,
            name: name.to_owned(),
            transfer: graphrt_core::value::FileTransfer::ToolFile,
            reference: Some(format!("workspace:outputs/{name}")),
            ..Default::default()
        })
    }

    #[cfg(feature = "tool-files")]
    impl ToolRuntime for ExportTools {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
            let spec = |name: &str| ToolSpec { name: name.into(), description: "d".into(), parameters: json!({}) };
            Box::pin(async move { Ok(vec![spec("export"), spec("export_failed")]) })
        }

        fn call<'a>(
            &'a self,
            name: &'a str,
            arguments: Json,
            _context: &'a ToolCallContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async move {
                let file = exported(arguments["name"].as_str().unwrap_or("out.csv"));
                let data = Value::object([("file", file)]);
                Ok(match name {
                    "export" => ToolOutput { text: "written".into(), data, ..ToolOutput::default() },
                    _ => ToolOutput { text: "disk full".into(), data, is_error: true, ..ToolOutput::default() },
                })
            })
        }
    }

    #[cfg(feature = "tool-files")]
    fn export_ctx(s: &Setup) -> NodeContext {
        let mut registry = ToolRegistry::new();
        registry.add("export", Arc::new(ExportTools) as Arc<dyn ToolRuntime>);
        let services = Services::new()
            .with(ModelRegistry::new().with("openai", Arc::clone(&s.model) as Arc<dyn crate::ChatModel>))
            .with(registry);
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("export the report"));
        NodeContext::for_test("agent", pool).with_services(Arc::new(services))
    }

    #[cfg(feature = "tool-files")]
    #[tokio::test]
    async fn files_returned_by_tools_become_the_files_output() {
        let s = setup(vec![
            MockModel::tool_calls(&[
                ("c1", "export", r#"{"name":"a.csv"}"#),
                ("c2", "export", r#"{"name":"b.md"}"#),
                ("c3", "export_failed", r#"{"name":"lost.txt"}"#),
            ]),
            // The same file again: it is listed once, in its last position.
            MockModel::tool_calls(&[("c4", "export", r#"{"name":"a.csv"}"#)]),
            MockModel::text("done"),
        ]);
        let out = completed(agent(json!({"tools": ["export/*"]})).run(export_ctx(&s)).await);
        let files = out.outputs["files"].as_array().unwrap().to_vec();
        let names: Vec<&str> = files.iter().map(|f| f.as_file().unwrap().name.as_str()).collect();
        assert_eq!(names, ["b.md", "a.csv"]);
        assert_eq!(files[0].as_file().unwrap().reference.as_deref(), Some("workspace:outputs/b.md"));
        // The model saw the tool's text only: no file object, no reference.
        let seen = serde_json::to_string(&s.model.requests()[1].messages).unwrap();
        assert!(seen.contains("written") && !seen.contains("workspace:"), "{seen}");
    }

    #[cfg(feature = "tool-files")]
    #[tokio::test]
    async fn an_agent_without_file_tools_has_an_empty_files_output() {
        let s = setup(vec![MockModel::tool_calls(&[("c1", "add", r#"{"a":2,"b":3}"#)]), MockModel::text("5")]);
        let out = completed(agent(json!({})).run(ctx(&s)).await);
        assert_eq!(out.outputs["files"], Value::array([]));
    }

    #[cfg(feature = "tool-files")]
    #[tokio::test]
    async fn files_survive_a_pause_for_approval() {
        let s = setup(vec![
            MockModel::tool_calls(&[("c1", "export", r#"{"name":"a.csv"}"#)]),
            MockModel::tool_calls(&[("c2", "export_failed", "{}")]),
            MockModel::text("done"),
        ]);
        let node = agent(json!({"tools": ["export/*"], "require_approval": ["export/export_failed"]}));
        let NodeOutcome::Paused(pause) = node.run(export_ctx(&s)).await.unwrap() else { panic!("expected a pause") };
        // The saved state is JSON: the file must come back as a file after resuming.
        let state = serde_json::from_str::<Json>(&serde_json::to_string(&pause.state).unwrap()).unwrap();
        let input = Some(Value::from_json(json!({"approved": true})));
        let resumed = export_ctx(&s).with_resume(Some(Resume { state, input }));
        let out = completed(node.run(resumed).await);
        let files = out.outputs["files"].as_array().unwrap().to_vec();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_file().unwrap().name, "a.csv");
    }
}
