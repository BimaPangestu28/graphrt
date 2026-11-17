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
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One piece of message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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

impl ToolCall {
    /// Arguments parsed as JSON. Invalid or empty text yields an empty object.
    pub fn arguments_json(&self) -> serde_json::Value {
        if self.arguments.trim().is_empty() {
            return serde_json::Value::Object(serde_json::Map::new());
        }
        serde_json::from_str(&self.arguments).unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
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

/// How the model may use tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
}

/// Why generation stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Collect a delta stream. `on_delta` sees every delta as it arrives (used for
/// streaming chunks to the run's event stream).
///
/// Tool-call fragments are grouped by their `index`, so interleaved parallel
/// calls are assembled correctly.
pub async fn collect(mut stream: ChatStream, mut on_delta: impl FnMut(&ChatDelta)) -> Result<ChatResponse, LlmError> {
    let mut response = ChatResponse::default();
    let mut calls: BTreeMap<usize, PartialCall> = BTreeMap::new();
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        on_delta(&delta);
        match delta {
            ChatDelta::Text(t) => response.text.push_str(&t),
            ChatDelta::Reasoning(t) => response.reasoning.push_str(&t),
            ChatDelta::ToolCall { index, id, name, arguments } => {
                let call = calls.entry(index).or_default();
                if let Some(id) = id.filter(|s| !s.is_empty()) {
                    call.id = Some(id);
                }
                if let Some(name) = name.filter(|s| !s.is_empty()) {
                    call.name = name;
                }
                call.arguments.push_str(&arguments);
            }
            ChatDelta::Usage(u) => response.usage = u,
            ChatDelta::Finish(r) => response.finish_reason = Some(r),
            ChatDelta::ProviderState(s) => response.provider_state = Some(s),
        }
    }
    response.tool_calls = calls
        .into_iter()
        .filter(|(_, c)| !c.name.is_empty())
        .map(|(index, c)| ToolCall {
            id: c.id.unwrap_or_else(|| format!("call_{index}")),
            name: c.name,
            arguments: c.arguments,
        })
        .collect();
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
        assert_eq!(r.tool_calls[1].arguments_json()["url"], "x");
        assert_eq!(r.usage.total_tokens, 7);
        assert_eq!(r.finish_reason, Some(FinishReason::ToolCalls));
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
