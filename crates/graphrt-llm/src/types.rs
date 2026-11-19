//! Provider-neutral chat types.
//!
//! Every provider translates [`ChatRequest`] into its wire format and its
//! streaming response into [`ChatDelta`]s. [`collect`] folds a delta stream
//! into a [`ChatResponse`].

use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;

use futures::future::BoxFuture;
use futures::{Stream, StreamExt};
use graphrt_core::event::Usage;
use graphrt_core::tool::ToolSpec;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

/// Author of a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One piece of message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentPart {
    Text {
        text: String,
    },
    /// An image by URL (`https://...` or a `data:<mime>;base64,...` URL).
    Image {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

/// A tool call requested by the model. `arguments` is the raw JSON text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Tool-call arguments that are not a JSON object.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tool call `{name}` has invalid JSON arguments: {reason}")]
pub struct InvalidToolArguments {
    /// Tool name of the offending call.
    pub name: String,
    pub reason: String,
}

impl ToolCall {
    /// Arguments parsed as a JSON object. Empty text means "no arguments" and
    /// yields an empty object; anything that is not a JSON object is an error
    /// (it usually means the model output was truncated or malformed).
    pub fn arguments_json(&self) -> Result<serde_json::Map<String, serde_json::Value>, InvalidToolArguments> {
        if self.arguments.trim().is_empty() {
            return Ok(serde_json::Map::new());
        }
        let invalid = |reason: String| InvalidToolArguments { name: self.name.clone(), reason };
        match serde_json::from_str::<serde_json::Value>(&self.arguments) {
            Ok(serde_json::Value::Object(map)) => Ok(map),
            Ok(other) => Err(invalid(format!("expected a JSON object, got {}", json_kind(&other)))),
            Err(e) => Err(invalid(e.to_string())),
        }
    }
}

fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// A chat message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: Vec<ContentPart>,
    /// Tool calls made by an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For `tool` messages: the call this message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Opaque provider state attached to an assistant message (e.g. signed
    /// reasoning blocks) that must be sent back verbatim on the next turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<serde_json::Value>,
}

impl Message {
    fn with_text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            provider_state: None,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::with_text(Role::System, text)
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::with_text(Role::User, text)
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::with_text(Role::Assistant, text)
    }

    /// A tool result answering `call_id`.
    pub fn tool(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        let mut m = Self::with_text(Role::Tool, text);
        m.tool_call_id = Some(call_id.into());
        m
    }

    /// A tool result answering `call_id` with images (`https://` or `data:`
    /// URLs) after the text. Providers that cannot put images in a tool
    /// result send them in a user turn right after the tool results.
    pub fn tool_with_images(
        call_id: impl Into<String>,
        text: impl Into<String>,
        image_urls: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut m = Self::tool(call_id, text);
        m.content.extend(image_urls.into_iter().map(|url| ContentPart::Image { url, detail: None }));
        m
    }

    /// Image parts, in order.
    pub fn images(&self) -> impl Iterator<Item = &ContentPart> {
        self.content.iter().filter(|p| matches!(p, ContentPart::Image { .. }))
    }

    /// Concatenated text parts.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.content {
            if let ContentPart::Text { text } = part {
                out.push_str(text);
            }
        }
        out
    }
}

/// Text of a tool message for providers whose tool results are text-only:
/// the message text plus a note saying how many images follow.
#[cfg(feature = "openai")]
pub(crate) fn tool_text_with_image_note(m: &Message) -> String {
    let text = m.text();
    let count = m.images().count();
    if count == 0 {
        return text;
    }
    let note = if count == 1 {
        "[1 image attached after the tool results]".to_owned()
    } else {
        format!("[{count} images attached after the tool results]")
    };
    if text.is_empty() { note } else { format!("{text}\n{note}") }
}

/// Text introducing the images of one tool result in the fallback user turn.
#[cfg(any(feature = "openai", feature = "gemini", feature = "bedrock"))]
pub(crate) fn tool_images_intro(name: Option<&str>, call_id: &str) -> String {
    match name {
        Some(name) => format!("Images returned by tool `{name}` (call {call_id}):"),
        None => format!("Images returned by tool call {call_id}:"),
    }
}

/// How the model may use tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolChoice {
    Auto,
    None,
    /// The model must call some tool.
    Required,
    /// The model must call this tool.
    Tool(String),
}

/// Requested output format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        #[serde(default)]
        strict: bool,
    },
}

/// A provider-neutral chat request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Provider-specific parameters merged into the request body as-is.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self { model: model.into(), messages, ..Self::default() }
    }

    /// Set the tools offered to the model.
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    /// Set how the model may use tools.
    pub fn with_tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_top_p(mut self, top_p: f64) -> Self {
        self.top_p = Some(top_p);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn with_stop(mut self, stop: Vec<String>) -> Self {
        self.stop = stop;
        self
    }

    pub fn with_response_format(mut self, format: ResponseFormat) -> Self {
        self.response_format = Some(format);
        self
    }

    /// Add a provider-specific body parameter.
    pub fn with_extra(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }
}

/// Body keys owned by the providers; [`ChatRequest::extra`] must not set them.
pub const RESERVED_EXTRA_KEYS: &[&str] = &["model", "messages", "stream", "stream_options", "tools", "tool_choice"];

/// Reject `extra` keys that would silently override provider-managed fields.
/// Custom [`ChatModel`] implementations should call this before building a body.
pub fn check_extra(req: &ChatRequest) -> Result<(), LlmError> {
    for key in req.extra.keys() {
        if RESERVED_EXTRA_KEYS.contains(&key.as_str()) {
            return Err(LlmError::Config(format!(
                "parameter `{key}` cannot be set through extra parameters; use the dedicated request field"
            )));
        }
    }
    Ok(())
}

/// Why generation stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Other(String),
}

impl FinishReason {
    /// Stable wire name.
    pub fn as_str(&self) -> &str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::Length => "length",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::ContentFilter => "content_filter",
            FinishReason::Other(s) => s,
        }
    }
}

/// One streamed increment.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ChatDelta {
    /// Visible answer text.
    Text(String),
    /// Reasoning / thinking text.
    Reasoning(String),
    /// Fragment of a tool call. Fragments sharing `index` belong to the same call;
    /// `id` and `name` usually arrive on the first fragment only.
    ToolCall {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// Cumulative usage snapshot; the last one wins.
    Usage(Usage),
    Finish(FinishReason),
    /// Opaque provider state for the assistant message; the last one wins.
    ProviderState(serde_json::Value),
    /// Something the provider adapter changed or dropped while translating the
    /// request (e.g. unsupported JSON Schema keywords). Surfaced in node metadata.
    Warning(String),
}

/// Stream of deltas returned by [`ChatModel::chat`].
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatDelta, LlmError>> + Send>>;

/// A chat-capable model endpoint.
pub trait ChatModel: Send + Sync + fmt::Debug {
    /// Start a streaming chat completion.
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>>;
}

/// A fully collected response.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatResponse {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: Option<FinishReason>,
    pub provider_state: Option<serde_json::Value>,
    /// Adapter warnings (see [`ChatDelta::Warning`]).
    pub warnings: Vec<String>,
}

impl ChatResponse {
    /// The assistant message to append to the conversation.
    pub fn to_message(&self) -> Message {
        let content =
            if self.text.is_empty() { Vec::new() } else { vec![ContentPart::Text { text: self.text.clone() }] };
        Message {
            role: Role::Assistant,
            content,
            tool_calls: self.tool_calls.clone(),
            tool_call_id: None,
            provider_state: self.provider_state.clone(),
        }
    }
}

#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Collect a delta stream. `on_delta` receives every delta as it arrives, by
/// value, after it has been added to the response (used for streaming chunks to
/// the run's event stream): a text delta can become a chunk without copying.
///
/// Tool-call fragments are grouped by their `index`, so interleaved parallel
/// calls are assembled correctly. A call that has an id or arguments but never
/// received a name is a decode error rather than being dropped.
///
/// Detecting a truncated stream is the transport's job: the built-in providers
/// fail with a retryable [`LlmError::Network`] when the connection closes
/// before the provider's end-of-stream marker.
///
/// ```
/// # use graphrt_llm::{ChatDelta, ChatStream, collect};
/// # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
/// # runtime.block_on(async {
/// let stream: ChatStream =
///     Box::pin(futures::stream::iter(["Hel", "lo"].map(|t| Ok(ChatDelta::Text(t.into())))));
/// let mut chunks = Vec::new();
/// let response = collect(stream, |delta| {
///     if let ChatDelta::Text(text) = delta {
///         chunks.push(text); // moved, not copied
///     }
/// })
/// .await?;
/// assert_eq!((response.text.as_str(), chunks.len()), ("Hello", 2));
/// # Ok::<(), graphrt_llm::LlmError>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub async fn collect(mut stream: ChatStream, mut on_delta: impl FnMut(ChatDelta)) -> Result<ChatResponse, LlmError> {
    let mut response = ChatResponse::default();
    let mut calls: BTreeMap<usize, PartialCall> = BTreeMap::new();
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        // Text is copied into the response and the delta handed on; the rare
        // non-text deltas are cloned instead.
        match &delta {
            ChatDelta::Text(t) => response.text.push_str(t),
            ChatDelta::Reasoning(t) => response.reasoning.push_str(t),
            ChatDelta::ToolCall { index, id, name, arguments } => {
                let call = calls.entry(*index).or_default();
                if let Some(id) = id.as_ref().filter(|s| !s.is_empty()) {
                    call.id = Some(id.clone());
                }
                if let Some(name) = name.as_ref().filter(|s| !s.is_empty()) {
                    call.name.clone_from(name);
                }
                call.arguments.push_str(arguments);
            }
            ChatDelta::Usage(u) => response.usage = *u,
            ChatDelta::Finish(r) => response.finish_reason = Some(r.clone()),
            ChatDelta::ProviderState(s) => response.provider_state = Some(s.clone()),
            ChatDelta::Warning(w) => response.warnings.push(w.clone()),
        }
        on_delta(delta);
    }
    let mut tool_calls = Vec::with_capacity(calls.len());
    for (index, call) in calls {
        if call.name.is_empty() {
            if call.id.is_none() && call.arguments.trim().is_empty() {
                // An empty fragment carries no information; nothing is lost.
                continue;
            }
            return Err(LlmError::Decode(format!("tool call #{index} has arguments or an id but no name")));
        }
        tool_calls.push(ToolCall {
            id: call.id.unwrap_or_else(|| format!("call_{index}")),
            name: call.name,
            arguments: call.arguments,
        });
    }
    response.tool_calls = tool_calls;
    if response.usage.total_tokens == 0 {
        response.usage.total_tokens = response.usage.prompt_tokens.saturating_add(response.usage.completion_tokens);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(deltas: Vec<ChatDelta>) -> ChatStream {
        Box::pin(futures::stream::iter(deltas.into_iter().map(Ok)))
    }

    #[tokio::test]
    async fn interleaved_parallel_tool_calls_assemble_by_index() {
        let deltas = vec![
            ChatDelta::ToolCall {
                index: 0,
                id: Some("a".into()),
                name: Some("search".into()),
                arguments: "{\"q\":".into(),
            },
            ChatDelta::ToolCall {
                index: 1,
                id: Some("b".into()),
                name: Some("fetch".into()),
                arguments: "{\"url\":".into(),
            },
            ChatDelta::ToolCall { index: 0, id: None, name: None, arguments: "\"rust\"}".into() },
            ChatDelta::ToolCall { index: 1, id: None, name: None, arguments: "\"x\"}".into() },
            ChatDelta::Usage(Usage { prompt_tokens: 3, completion_tokens: 4, total_tokens: 0 }),
            ChatDelta::Finish(FinishReason::ToolCalls),
        ];
        let r = collect(stream(deltas), |_| {}).await.unwrap();
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].arguments, "{\"q\":\"rust\"}");
        assert_eq!(r.tool_calls[1].id, "b");
        assert_eq!(r.tool_calls[1].arguments_json().unwrap()["url"], "x");
        assert_eq!(r.usage.total_tokens, 7);
        assert_eq!(r.finish_reason, Some(FinishReason::ToolCalls));
    }

    #[tokio::test]
    async fn nameless_tool_call_is_an_error() {
        let deltas = vec![ChatDelta::ToolCall { index: 0, id: Some("a".into()), name: None, arguments: "{}".into() }];
        let err = collect(stream(deltas), |_| {}).await.unwrap_err();
        assert!(matches!(err, LlmError::Decode(_)), "{err:?}");
        // An empty fragment is harmless.
        let deltas = vec![ChatDelta::ToolCall { index: 0, id: None, name: None, arguments: String::new() }];
        assert!(collect(stream(deltas), |_| {}).await.unwrap().tool_calls.is_empty());
    }

    #[test]
    fn arguments_json_rejects_malformed_input() {
        let call = |args: &str| ToolCall { id: "1".into(), name: "t".into(), arguments: args.into() };
        assert!(call("").arguments_json().unwrap().is_empty());
        assert_eq!(call("{\"a\":1}").arguments_json().unwrap()["a"], 1);
        assert!(call("{\"a\":").arguments_json().is_err());
        let err = call("[1]").arguments_json().unwrap_err();
        assert!(err.to_string().contains("an array"), "{err}");
    }

    #[test]
    fn reserved_extra_keys_are_rejected() {
        let req = ChatRequest::new("m", vec![]).with_extra("stream", serde_json::json!(false));
        assert!(matches!(check_extra(&req), Err(LlmError::Config(_))));
        let req = ChatRequest::new("m", vec![]).with_extra("seed", serde_json::json!(1)).with_temperature(0.1);
        assert!(check_extra(&req).is_ok());
        assert_eq!(req.temperature, Some(0.1));
    }

    #[cfg(feature = "openai")]
    #[test]
    fn tool_messages_with_images() {
        let m = Message::tool_with_images("c1", "shot", ["data:image/png;base64,AAAA".to_owned()]);
        assert_eq!(m.text(), "shot");
        assert_eq!(m.images().count(), 1);
        assert_eq!(tool_text_with_image_note(&m), "shot\n[1 image attached after the tool results]");
        let m = Message::tool_with_images("c1", "", vec!["https://x/a.png".to_owned(); 2]);
        assert_eq!(tool_text_with_image_note(&m), "[2 images attached after the tool results]");
        assert_eq!(tool_text_with_image_note(&Message::tool("c1", "plain")), "plain");
        assert_eq!(tool_images_intro(Some("screenshot"), "c1"), "Images returned by tool `screenshot` (call c1):");
    }

    #[tokio::test]
    async fn text_and_reasoning_accumulate() {
        let deltas =
            vec![ChatDelta::Reasoning("hm".into()), ChatDelta::Text("he".into()), ChatDelta::Text("llo".into())];
        let mut seen = 0;
        let r = collect(stream(deltas), |_| seen += 1).await.unwrap();
        assert_eq!(seen, 3);
        assert_eq!(r.text, "hello");
        assert_eq!(r.reasoning, "hm");
        assert!(r.tool_calls.is_empty());
    }
}
