//! Configuration and prompt assembly shared by the model-calling nodes.

use std::sync::{Arc, LazyLock};

use graphrt_core::event::Usage;
use graphrt_core::template::{Part, Template};
use graphrt_core::value::{FileKind, Value};
use graphrt_core::{NodeContext, NodeError, Selector, VariablePool};
use serde::Deserialize;
use serde_json::Value as Json;

use crate::registry::ModelRegistry;
use crate::types::{
    ChatDelta, ChatModel, ChatRequest, ChatResponse, ContentPart, FinishReason, Message, RESERVED_EXTRA_KEYS, Role,
    collect,
};

/// Literal placeholder replaced by the configured context value.
pub const CONTEXT_PLACEHOLDER: &str = "{{#context#}}";

/// `model: {provider, name, completion_params}`.
#[derive(Clone, Debug, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub name: String,
    #[serde(default)]
    pub completion_params: serde_json::Map<String, Json>,
}

/// Parameters never forwarded to providers (UI-only settings).
const IGNORED_PARAMS: &[&str] = &["mode"];

/// A resolved completion parameter.
enum Param {
    /// Not set (explicit `null`, or an empty literal string).
    Unset,
    Value(Json),
}

impl ModelConfig {
    /// Build a request for this model, applying completion parameters. Parameter
    /// values that are templates (`"{{#start.temp#}}"`) are resolved per run;
    /// the configuration itself is never modified.
    ///
    /// A parameter whose value has the wrong type (e.g. `temperature: "hot"`),
    /// or a template that resolves to nothing, is an input error naming the
    /// parameter instead of being dropped silently.
    pub fn request(&self, messages: Vec<Message>, pool: &VariablePool) -> Result<ChatRequest, NodeError> {
        let mut req = ChatRequest::new(self.name.clone(), messages);
        for (key, raw) in &self.completion_params {
            if IGNORED_PARAMS.contains(&key.as_str()) {
                continue;
            }
            let value = match resolve_param(key, raw, pool)? {
                Param::Unset => continue,
                Param::Value(v) => v,
            };
            let bad = |expected: &str| {
                NodeError::invalid_input(format!("completion parameter `{key}` must be {expected}, got {value}"))
            };
            match key.as_str() {
                "temperature" => req.temperature = Some(value.as_f64().ok_or_else(|| bad("a number"))?),
                "top_p" => req.top_p = Some(value.as_f64().ok_or_else(|| bad("a number"))?),
                "max_tokens" | "max_completion_tokens" => {
                    let n = value
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| bad("a non-negative integer that fits in 32 bits"))?;
                    req.max_tokens = Some(n);
                }
                "stop" => {
                    req.stop = match &value {
                        Json::String(s) => vec![s.clone()],
                        Json::Array(items) => items
                            .iter()
                            .map(|i| i.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                            .ok_or_else(|| bad("a string or a list of strings"))?,
                        _ => return Err(bad("a string or a list of strings")),
                    };
                }
                reserved if RESERVED_EXTRA_KEYS.contains(&reserved) => {
                    return Err(NodeError::invalid_input(format!(
                        "completion parameter `{key}` is managed by the node and cannot be set"
                    )));
                }
                _ => {
                    req.extra.insert(key.clone(), value);
                }
            }
        }
        Ok(req)
    }

    /// Selectors referenced by templated completion parameters.
    pub fn selectors(&self) -> Vec<Selector> {
        self.completion_params
            .values()
            .filter_map(Json::as_str)
            .filter(|s| s.contains("{{#"))
            .flat_map(|s| Template::parse(s).selectors().cloned().collect::<Vec<_>>())
            .collect()
    }
}

/// Resolve a completion parameter: templates are rendered, numeric/boolean strings coerced.
fn resolve_param(key: &str, raw: &Json, pool: &VariablePool) -> Result<Param, NodeError> {
    let text = match raw {
        Json::Null => return Ok(Param::Unset),
        Json::String(s) => s,
        other => return Ok(Param::Value(other.clone())),
    };
    let is_template = text.contains("{{#");
    let resolved = if is_template { Template::parse(text).resolve(pool) } else { Value::from(text.as_str()) };
    match resolved {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                if is_template {
                    return Err(NodeError::invalid_input(format!(
                        "completion parameter `{key}` resolved to an empty value (`{text}`)"
                    )));
                }
                return Ok(Param::Unset);
            }
            Ok(Param::Value(if let Ok(i) = t.parse::<i64>() {
                Json::from(i)
            } else if let Ok(f) = t.parse::<f64>() {
                Json::from(f)
            } else if t == "true" || t == "false" {
                Json::Bool(t == "true")
            } else {
                Json::String(s.to_string())
            }))
        }
        Value::Null => Err(NodeError::invalid_input(format!(
            "completion parameter `{key}` references a missing variable (`{text}`)"
        ))),
        other => Ok(Param::Value(other.to_json())),
    }
}

/// One prompt message template.
#[derive(Clone, Debug, Deserialize)]
pub struct PromptMessage {
    #[serde(default = "user_role")]
    pub role: Role,
    #[serde(default)]
    pub text: Template,
}

fn user_role() -> Role {
    Role::User
}

/// `prompt_template`: a list of chat messages, or a single completion prompt.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum PromptTemplate {
    Chat(Vec<PromptMessage>),
    Completion(PromptMessage),
}

impl Default for PromptTemplate {
    fn default() -> Self {
        PromptTemplate::Chat(Vec::new())
    }
}

impl PromptTemplate {
    pub fn messages(&self) -> Vec<&PromptMessage> {
        match self {
            PromptTemplate::Chat(m) => m.iter().collect(),
            PromptTemplate::Completion(m) => vec![m],
        }
    }

    /// Selectors referenced by the message templates.
    pub fn selectors(&self) -> Vec<Selector> {
        self.messages().iter().flat_map(|m| m.text.selectors().cloned()).collect()
    }
}

/// `context: {enabled, variable_selector}`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ContextConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub variable_selector: Option<Selector>,
}

impl ContextConfig {
    /// Context text, if enabled and present.
    pub fn resolve(&self, pool: &VariablePool) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let value = pool.get(self.variable_selector.as_ref()?)?;
        Some(context_text(&value))
    }

    pub fn selectors(&self) -> Vec<Selector> {
        if !self.enabled {
            return Vec::new();
        }
        self.variable_selector.iter().cloned().collect()
    }
}

/// Retrieved-document lists render as their `content` fields; other values as text.
fn context_text(value: &Value) -> String {
    match value {
        Value::Array(items) => items
            .iter()
            .map(|item| match item.as_object().and_then(|o| o.get("content")) {
                Some(content) => content.to_text(),
                None => item.to_text(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_text(),
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct VisionDetail {
    #[serde(default)]
    pub variable_selector: Option<Selector>,
    #[serde(default)]
    pub detail: Option<String>,
}

/// `vision: {enabled, configs: {variable_selector, detail}}` (or the fields inline).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct VisionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub configs: Option<VisionDetail>,
    #[serde(default)]
    pub variable_selector: Option<Selector>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl VisionConfig {
    fn selector(&self) -> Selector {
        self.configs
            .as_ref()
            .and_then(|c| c.variable_selector.clone())
            .or_else(|| self.variable_selector.clone())
            .unwrap_or_else(|| Selector::of("sys", "files"))
    }

    /// Image parts for the configured files (default selector `sys.files`).
    ///
    /// Non-image files are ignored. An image without a URL (e.g. a host
    /// storage reference that was not resolved) is an input error rather than
    /// being dropped silently.
    pub fn image_parts(&self, pool: &VariablePool) -> Result<Vec<ContentPart>, NodeError> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        let detail = self.configs.as_ref().and_then(|c| c.detail.clone()).or_else(|| self.detail.clone());
        let files: Vec<Value> = match pool.get(&self.selector()) {
            Some(Value::Array(items)) => items.iter().cloned().collect(),
            Some(v @ Value::File(_)) => vec![v],
            _ => Vec::new(),
        };
        let mut parts = Vec::new();
        for file in files.iter().filter_map(Value::as_file) {
            let is_image =
                file.kind == FileKind::Image || file.mime_type.as_deref().is_some_and(|m| m.starts_with("image/"));
            if !is_image {
                continue;
            }
            let url = file.url.clone().filter(|u| !u.is_empty()).ok_or_else(|| {
                NodeError::invalid_input(format!(
                    "image `{}` has no URL; resolve host files to URLs before sending them to a model",
                    file.name
                ))
            })?;
            parts.push(ContentPart::Image { url, detail: detail.clone() });
        }
        Ok(parts)
    }

    pub fn selectors(&self) -> Vec<Selector> {
        if self.enabled { vec![self.selector()] } else { Vec::new() }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WindowConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub size: Option<usize>,
}

/// Conversation memory.
///
/// History is read from `variable_selector` (default `sys.history`): an array of
/// `{role, content}` objects (or a JSON string encoding one), oldest first.
/// Roles are `user`/`human`, `assistant`/`ai` and `system`. With
/// `window.enabled`, only the last `window.size` turns (two messages per turn)
/// are kept. The current query is appended as a user message, rendered from
/// `query_prompt_template` (default `{{#sys.query#}}`). Without an explicit
/// `query_prompt_template`, the query is not appended when a prompt template
/// message already references `sys.query` (it would otherwise be sent twice).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct MemoryConfig {
    #[serde(default)]
    pub window: Option<WindowConfig>,
    #[serde(default)]
    pub variable_selector: Option<Selector>,
    #[serde(default)]
    pub query_prompt_template: Option<Template>,
}

impl MemoryConfig {
    fn history_selector(&self) -> Selector {
        self.variable_selector.clone().unwrap_or_else(|| Selector::of("sys", "history"))
    }

    /// Conversation history. A missing history is empty; a history of the
    /// wrong shape is an input error.
    pub fn history(&self, pool: &VariablePool) -> Result<Vec<Message>, NodeError> {
        let selector = self.history_selector();
        let items: Vec<Value> = match pool.get(&selector) {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(Value::Array(items)) => items.iter().cloned().collect(),
            Some(Value::String(text)) if text.trim().is_empty() => return Ok(Vec::new()),
            Some(Value::String(text)) => match serde_json::from_str::<Json>(&text) {
                Ok(Json::Array(items)) => items.into_iter().map(Value::from_json).collect(),
                _ => {
                    return Err(NodeError::invalid_input(format!(
                        "memory history `{selector}` is a string that is not a JSON array"
                    )));
                }
            },
            Some(other) => {
                return Err(NodeError::invalid_input(format!(
                    "memory history `{selector}` must be an array of {{role, content}} objects, got {}",
                    other.type_name()
                )));
            }
        };
        let mut messages = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let bad = |why: &str| NodeError::invalid_input(format!("memory history item #{i}: {why}"));
            let obj = item.as_object().ok_or_else(|| bad("expected a {role, content} object"))?;
            let role = match obj.get("role").and_then(Value::as_str) {
                Some("user" | "human") => Role::User,
                Some("assistant" | "ai") => Role::Assistant,
                Some("system") => Role::System,
                Some(other) => return Err(bad(&format!("unsupported role `{other}`"))),
                None => return Err(bad("missing `role`")),
            };
            let content = obj.get("content").map(Value::to_text).unwrap_or_default();
            messages.push(Message { role, ..Message::user(content) });
        }
        if let Some(window) = self.window.as_ref().filter(|w| w.enabled) {
            let keep = window.size.unwrap_or(10).saturating_mul(2);
            if messages.len() > keep {
                messages.drain(..messages.len() - keep);
            }
        }
        Ok(messages)
    }

    pub fn query(&self, pool: &VariablePool) -> Option<String> {
        let text = match &self.query_prompt_template {
            Some(t) => t.render(pool),
            None => pool.get(&Selector::of("sys", "query"))?.to_text(),
        };
        if text.is_empty() { None } else { Some(text) }
    }

    /// Selectors read by the memory (history and query).
    pub fn selectors(&self) -> Vec<Selector> {
        let mut out = vec![self.history_selector()];
        match &self.query_prompt_template {
            Some(t) => out.extend(t.selectors().cloned()),
            None => out.push(Selector::of("sys", "query")),
        }
        out
    }
}

/// Render a template, substituting the literal context placeholder. Context and
/// variable values are inserted as data and never parsed again, so content such
/// as `{{#env.KEY#}}` inside retrieved text stays literal.
pub fn render_with_context(template: &Template, pool: &VariablePool, context: Option<&str>) -> String {
    let mut out = String::new();
    for part in template.parts() {
        match part {
            Part::Text(t) => {
                if t.contains(CONTEXT_PLACEHOLDER) {
                    out.push_str(&t.replace(CONTEXT_PLACEHOLDER, context.unwrap_or_default()));
                } else {
                    out.push_str(t);
                }
            }
            Part::Var(sel) => {
                if let Some(v) = pool.get(sel) {
                    out.push_str(&v.to_text());
                }
            }
        }
    }
    out
}

/// Assemble the message list: leading system messages, history, remaining
/// template messages, the memory query, then vision images on the last user message.
pub fn build_messages(
    template: &PromptTemplate,
    pool: &VariablePool,
    context: Option<&str>,
    memory: Option<&MemoryConfig>,
    images: Vec<ContentPart>,
) -> Result<Vec<Message>, NodeError> {
    let rendered: Vec<Message> = template
        .messages()
        .into_iter()
        .map(|m| Message { role: m.role, ..Message::user(render_with_context(&m.text, pool, context)) })
        .collect();
    let split = rendered.iter().take_while(|m| m.role == Role::System).count();
    let mut messages: Vec<Message> = rendered.iter().take(split).cloned().collect();
    if let Some(mem) = memory {
        messages.extend(mem.history(pool)?);
    }
    messages.extend(rendered.into_iter().skip(split));
    // The current query is appended after the history unless the prompt template already
    // contains it (a message referencing `sys.query`); otherwise it would be sent twice.
    // An explicit `query_prompt_template` is always appended.
    let template_has_query = template.messages().iter().any(|m| m.text.selectors().any(is_sys_query));
    if let Some(mem) = memory {
        if mem.query_prompt_template.is_some() || !template_has_query {
            if let Some(query) = mem.query(pool) {
                messages.push(Message::user(query));
            }
        }
    }
    if !images.is_empty() {
        match messages.iter_mut().rev().find(|m| m.role == Role::User) {
            Some(last_user) => last_user.content.extend(images),
            None => messages.push(Message { content: images, ..Message::user("") }),
        }
    }
    Ok(messages)
}

fn is_sys_query(selector: &Selector) -> bool {
    selector.namespace() == "sys" && selector.name() == "query"
}

/// Look up the configured provider.
pub fn model_for(ctx: &NodeContext, config: &ModelConfig) -> Result<Arc<dyn ChatModel>, NodeError> {
    let registry = ctx.require_service::<ModelRegistry>("model registry (graphrt_llm::ModelRegistry)")?;
    Ok(registry.get(&config.provider)?)
}

/// What a model call streams to the run's event stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamMode {
    /// Nothing is streamed.
    Off,
    /// Answer text on field `text`, reasoning on field `reasoning`, as produced.
    Raw,
    /// Like [`StreamMode::Raw`], but `<think>...</think>` sections inside the
    /// answer are routed to `reasoning`, so the streamed `text` matches the
    /// node's separated `text` output.
    SeparateThink,
}

/// Run a request, streaming chunks according to `mode`.
pub async fn invoke(
    ctx: &NodeContext,
    model: &dyn ChatModel,
    request: ChatRequest,
    mode: StreamMode,
) -> Result<ChatResponse, NodeError> {
    let stream = model.chat(request).await?;
    let mut splitter = ThinkSplitter::default();
    let response = collect(stream, |delta| match (mode, delta) {
        (StreamMode::Off, _) => {}
        (StreamMode::Raw, ChatDelta::Text(t)) if !t.is_empty() => ctx.chunk_shared(StreamField::Text.shared(), t),
        (StreamMode::SeparateThink, ChatDelta::Text(t)) => {
            for (field, piece) in splitter.push(&t) {
                ctx.chunk_shared(field.shared(), piece);
            }
        }
        (_, ChatDelta::Reasoning(t)) if !t.is_empty() => ctx.chunk_shared(StreamField::Reasoning.shared(), t),
        _ => {}
    })
    .await?;
    if mode == StreamMode::SeparateThink {
        for (field, piece) in splitter.finish() {
            ctx.chunk_shared(field.shared(), piece);
        }
    }
    Ok(response)
}

/// Finish reasons that mean the model produced a broken turn (worth a retry).
const MALFORMED_FINISH: &[&str] = &["malformed_function_call", "unexpected_tool_call", "too_many_tool_calls"];

/// What a node should do with a response's finish reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishCheck {
    /// A normal end (stop, length, tool calls, or none reported).
    Normal,
    /// An unrecognized provider reason: keep the answer but report it.
    Unknown(String),
}

/// Turn finish reasons that mean "no usable answer" into node errors: a content
/// filter block is fatal (`content_filter`), a malformed turn is retryable
/// (`model_decode`). Unknown reasons are returned for the caller to report.
pub fn check_finish(response: &ChatResponse) -> Result<FinishCheck, NodeError> {
    match &response.finish_reason {
        None | Some(FinishReason::Stop | FinishReason::Length | FinishReason::ToolCalls) => Ok(FinishCheck::Normal),
        Some(FinishReason::ContentFilter) => Err(NodeError::fatal(
            "content_filter",
            "the provider's safety filter blocked the response; no answer was produced",
        )),
        Some(FinishReason::Other(reason)) if MALFORMED_FINISH.contains(&reason.as_str()) => {
            Err(NodeError::new("model_decode", format!("the model produced a malformed turn ({reason})")))
        }
        Some(FinishReason::Other(reason)) => Ok(FinishCheck::Unknown(reason.clone())),
        #[allow(unreachable_patterns)]
        Some(other) => Ok(FinishCheck::Unknown(other.as_str().to_owned())),
    }
}

/// Output field a streamed piece belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamField {
    Text,
    Reasoning,
}

static TEXT_FIELD: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("text"));
static REASONING_FIELD: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("reasoning"));

impl StreamField {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamField::Text => "text",
            StreamField::Reasoning => "reasoning",
        }
    }

    /// The field name as a shared string for [`NodeContext::chunk_shared`]:
    /// one allocation per process instead of one per chunk.
    pub fn shared(self) -> &'static Arc<str> {
        match self {
            StreamField::Text => &TEXT_FIELD,
            StreamField::Reasoning => &REASONING_FIELD,
        }
    }
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Splits streamed answer text into answer and `<think>` reasoning pieces,
/// handling tags split across chunks. Leading whitespace of the answer is
/// dropped, matching [`split_think_tags`].
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    /// Text that may be the start of a tag and is held back until decided.
    pending: String,
    in_think: bool,
    answer_started: bool,
}

impl ThinkSplitter {
    /// Feed a chunk; returns the pieces that can be emitted now.
    pub fn push(&mut self, chunk: &str) -> Vec<(StreamField, String)> {
        self.pending.push_str(chunk);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { THINK_CLOSE } else { THINK_OPEN };
            if let Some(pos) = self.pending.find(tag) {
                let before: String = self.pending.drain(..pos).collect();
                self.emit(&mut out, before);
                self.pending.drain(..tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            // Hold back the longest suffix that could still become `tag`.
            let keep = longest_tag_prefix_suffix(&self.pending, tag);
            let emit_len = self.pending.len() - keep;
            let ready: String = self.pending.drain(..emit_len).collect();
            self.emit(&mut out, ready);
            break;
        }
        out
    }

    /// Flush held-back text at the end of the stream.
    pub fn finish(&mut self) -> Vec<(StreamField, String)> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.pending);
        self.emit(&mut out, rest);
        out
    }

    fn emit(&mut self, out: &mut Vec<(StreamField, String)>, mut text: String) {
        if self.in_think {
            if !text.is_empty() {
                out.push((StreamField::Reasoning, text));
            }
            return;
        }
        if !self.answer_started {
            let trimmed = text.trim_start();
            if trimmed.is_empty() {
                return;
            }
            text = trimmed.to_owned();
            self.answer_started = true;
        }
        if !text.is_empty() {
            out.push((StreamField::Text, text));
        }
    }
}

/// Length of the longest suffix of `text` that is a proper prefix of `tag`.
fn longest_tag_prefix_suffix(text: &str, tag: &str) -> usize {
    (1..tag.len()).rev().find(|&n| tag.get(..n).is_some_and(|prefix| text.ends_with(prefix))).unwrap_or(0)
}

/// Usage as an output object.
pub fn usage_value(usage: &Usage) -> Value {
    Value::from_json(serde_json::json!({
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "total_tokens": usage.total_tokens,
    }))
}

/// Remove `<think>...</think>` sections, returning `(answer, reasoning)`.
pub fn split_think_tags(text: &str) -> (String, String) {
    let mut answer = String::new();
    let mut reasoning = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(THINK_OPEN) {
        answer.push_str(&rest[..start]);
        let after = &rest[start + THINK_OPEN.len()..];
        match after.find(THINK_CLOSE) {
            Some(end) => {
                reasoning.push_str(after[..end].trim());
                rest = &after[end + THINK_CLOSE.len()..];
            }
            None => {
                reasoning.push_str(after.trim());
                rest = "";
            }
        }
    }
    answer.push_str(rest);
    (answer.trim_start().to_owned(), reasoning)
}

/// Maximum number of candidate positions [`extract_json`] tries. Each attempt
/// scans at most the remaining text once, so total work is linear in the
/// input size (times this constant).
const MAX_JSON_CANDIDATES: usize = 32;

/// Extract the first JSON object or array from model output, tolerating code
/// fences, think tags and surrounding prose. Brace matching ignores braces
/// inside string literals.
pub fn extract_json(text: &str) -> Option<Json> {
    extract_json_where(text, |v| v.is_object() || v.is_array())
}

/// Like [`extract_json`], returning the first candidate accepted by `accept`.
pub fn extract_json_where(text: &str, accept: impl Fn(&Json) -> bool) -> Option<Json> {
    let (answer, _) = split_think_tags(text);
    let trimmed = answer.trim();
    if let Ok(v) = serde_json::from_str::<Json>(trimmed) {
        if accept(&v) {
            return Some(v);
        }
    }
    let bytes = trimmed.as_bytes();
    let mut search_from = 0;
    let mut attempts = 0;
    while let Some(offset) = trimmed.get(search_from..)?.find(['{', '[']) {
        attempts += 1;
        if attempts > MAX_JSON_CANDIDATES {
            return None;
        }
        let start = search_from + offset;
        if let Some(candidate) = matching_close(bytes, start).and_then(|end| trimmed.get(start..=end)) {
            if let Ok(v) = serde_json::from_str::<Json>(candidate) {
                if accept(&v) {
                    return Some(v);
                }
            }
        }
        search_from = start + 1;
    }
    None
}

fn matching_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphrt_core::value::File;

    #[test]
    fn json_extraction() {
        assert_eq!(extract_json("```json\n{\"a\":1}\n```").unwrap()["a"], 1);
        assert_eq!(extract_json("<think>{x}</think>Sure: {\"s\":\"}{\",\"b\":2} done").unwrap()["b"], 2);
        assert!(extract_json("no json here").is_none());
        let v = extract_json_where("see [1], then {\"a\": 2}", Json::is_object).unwrap();
        assert_eq!(v["a"], 2);
    }

    #[test]
    fn json_extraction_is_bounded_on_hostile_input() {
        let hostile = "{".repeat(200_000);
        let started = std::time::Instant::now();
        assert!(extract_json(&hostile).is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn think_tags() {
        let (a, r) = split_think_tags("<think>plan</think>\nanswer");
        assert_eq!(a, "answer");
        assert_eq!(r, "plan");
    }

    fn split_stream(chunks: &[&str]) -> (String, String) {
        let mut s = ThinkSplitter::default();
        let mut pieces = Vec::new();
        for c in chunks {
            pieces.extend(s.push(c));
        }
        pieces.extend(s.finish());
        let mut text = String::new();
        let mut reasoning = String::new();
        for (field, piece) in pieces {
            match field {
                StreamField::Text => text.push_str(&piece),
                StreamField::Reasoning => reasoning.push_str(&piece),
            }
        }
        (text, reasoning)
    }

    #[test]
    fn think_splitter_handles_tags_split_across_chunks() {
        let (text, reasoning) = split_stream(&["<thi", "nk>pl", "an</th", "ink>\n", "Hel", "lo <", "b>"]);
        assert_eq!(text, "Hello <b>");
        assert_eq!(reasoning, "plan");
        let whole = "<think>plan</think>\nHello <b>";
        assert_eq!(text, split_think_tags(whole).0);
        let (text, reasoning) = split_stream(&["no tags at all"]);
        assert_eq!((text.as_str(), reasoning.as_str()), ("no tags at all", ""));
        let (text, _) = split_stream(&["a <", "/think> b"]);
        assert_eq!(text, "a </think> b");
    }

    #[test]
    fn context_is_not_reparsed() {
        let mut pool = VariablePool::new();
        pool.insert("env", "KEY", Value::Secret(Arc::from("s3cret")));
        pool.insert("sys", "query", Value::from("q"));
        let t = Template::parse("ctx: {{#context#}} q: {{#sys.query#}}");
        let out = render_with_context(&t, &pool, Some("evil {{#env.KEY#}}"));
        assert_eq!(out, "ctx: evil {{#env.KEY#}} q: q");
        let t = Template::parse("{{#env.KEY#}}");
        assert!(!render_with_context(&t, &pool, None).contains("s3cret"));
    }

    fn model_config(params: Json) -> ModelConfig {
        serde_json::from_value(serde_json::json!({"provider": "p", "name": "m", "completion_params": params})).unwrap()
    }

    #[test]
    fn params_resolve_without_mutation() {
        let mut pool = VariablePool::new();
        pool.insert("start", "t", Value::from("0.3"));
        let cfg = model_config(serde_json::json!(
            {"temperature": "{{#start.t#}}", "max_tokens": 20, "stop": "x", "seed": "7", "mode": "chat", "top_p": null}
        ));
        let req = cfg.request(vec![], &pool).unwrap();
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.max_tokens, Some(20));
        assert_eq!(req.top_p, None);
        assert_eq!(req.stop, vec!["x".to_owned()]);
        assert_eq!(req.extra.get("seed"), Some(&Json::from(7)));
        assert!(!req.extra.contains_key("mode"));
        assert_eq!(cfg.completion_params["temperature"], "{{#start.t#}}");
        assert_eq!(cfg.selectors(), vec![Selector::of("start", "t")]);
    }

    #[test]
    fn invalid_params_are_errors() {
        let pool = VariablePool::new();
        for params in [
            serde_json::json!({"temperature": "hot"}),
            serde_json::json!({"max_tokens": -1}),
            serde_json::json!({"max_tokens": 1e12}),
            serde_json::json!({"stop": [1, 2]}),
            serde_json::json!({"temperature": "{{#start.missing#}}"}),
            serde_json::json!({"stream": false}),
        ] {
            let err = model_config(params.clone()).request(vec![], &pool).unwrap_err();
            assert_eq!(err.kind, "invalid_input", "{params}");
            assert!(!err.retryable);
        }
    }

    #[test]
    fn messages_with_memory_and_vision() {
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("now"));
        pool.insert(
            "sys",
            "history",
            Value::from_json(serde_json::json!([
                {"role": "user", "content": "old1"}, {"role": "assistant", "content": "old2"},
                {"role": "user", "content": "old3"}, {"role": "assistant", "content": "old4"}
            ])),
        );
        let image = File {
            marker: true,
            name: "a.png".into(),
            kind: FileKind::Image,
            url: Some("https://x/a.png".into()),
            ..File::default()
        };
        pool.insert("sys", "files", Value::array([Value::from(image)]));
        let template: PromptTemplate = serde_json::from_value(serde_json::json!([
            {"role": "system", "text": "sys"}, {"role": "user", "text": "tmpl"}
        ]))
        .unwrap();
        let memory: MemoryConfig =
            serde_json::from_value(serde_json::json!({"window": {"enabled": true, "size": 1}})).unwrap();
        let vision = VisionConfig { enabled: true, ..VisionConfig::default() };
        let msgs = build_messages(&template, &pool, None, Some(&memory), vision.image_parts(&pool).unwrap()).unwrap();
        let texts: Vec<String> = msgs.iter().map(Message::text).collect();
        assert_eq!(texts, vec!["sys", "old3", "old4", "tmpl", "now"]);
        assert!(matches!(msgs[4].content.last(), Some(ContentPart::Image { .. })));
    }

    #[test]
    fn memory_does_not_duplicate_a_templated_query() {
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("now"));
        pool.insert("sys", "history", Value::from_json(serde_json::json!([{"role": "user", "content": "old"}])));
        let memory = MemoryConfig::default();
        let with_query: PromptTemplate = serde_json::from_value(serde_json::json!([
            {"role": "system", "text": "sys"}, {"role": "user", "text": "Q: {{#sys.query#}}"}
        ]))
        .unwrap();
        let msgs = build_messages(&with_query, &pool, None, Some(&memory), Vec::new()).unwrap();
        let texts: Vec<String> = msgs.iter().map(Message::text).collect();
        assert_eq!(texts, vec!["sys", "old", "Q: now"], "the query is sent once, after the history");

        let without_query: PromptTemplate =
            serde_json::from_value(serde_json::json!([{"role": "system", "text": "sys"}])).unwrap();
        let msgs = build_messages(&without_query, &pool, None, Some(&memory), Vec::new()).unwrap();
        let texts: Vec<String> = msgs.iter().map(Message::text).collect();
        assert_eq!(texts, vec!["sys", "old", "now"], "memory appends the query when the template lacks it");

        let explicit: MemoryConfig =
            serde_json::from_value(serde_json::json!({"query_prompt_template": "again: {{#sys.query#}}"})).unwrap();
        let msgs = build_messages(&with_query, &pool, None, Some(&explicit), Vec::new()).unwrap();
        assert_eq!(msgs.last().map(Message::text).as_deref(), Some("again: now"), "explicit query template wins");
    }

    #[test]
    fn history_shapes() {
        let memory = MemoryConfig::default();
        let mut pool = VariablePool::new();
        pool.insert("sys", "history", Value::from(r#"[{"role": "human", "content": "hi"}]"#));
        assert_eq!(memory.history(&pool).unwrap().len(), 1, "JSON-string history is parsed");
        pool.insert("sys", "history", Value::from_json(serde_json::json!([{"role": "tool", "content": "x"}])));
        assert!(memory.history(&pool).is_err());
        pool.insert("sys", "history", Value::Int(3));
        assert!(memory.history(&pool).is_err());
        assert!(memory.history(&VariablePool::new()).unwrap().is_empty());
    }

    #[test]
    fn image_without_url_is_an_error() {
        let mut pool = VariablePool::new();
        let image = File { marker: true, name: "a.png".into(), kind: FileKind::Image, ..File::default() };
        pool.insert("sys", "files", Value::array([Value::from(image)]));
        let vision = VisionConfig { enabled: true, ..VisionConfig::default() };
        assert!(vision.image_parts(&pool).is_err());
    }
}
