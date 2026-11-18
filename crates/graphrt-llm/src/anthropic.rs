//! Anthropic Messages API provider (streaming, tool use, extended thinking).
//!
//! Extended thinking is enabled through [`ChatRequest::extra`], e.g.
//! `{"thinking": {"type": "enabled", "budget_tokens": 2048}}`. Signed thinking
//! blocks are returned as provider state and replayed on the next turn, which
//! the API requires when thinking is combined with tool use.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::event::Usage;
use serde_json::{Map, Value as Json, json};

use crate::credentials::{Credential, CredentialSource};
use crate::error::LlmError;
use crate::http::{Decoded, SseDecoder, send_sse};
use crate::sse::SseEvent;
use crate::types::{
    ChatDelta, ChatModel, ChatRequest, ChatStream, ContentPart, FinishReason, ResponseFormat, Role, ToolChoice,
    check_extra,
};

/// API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// `max_tokens` used when the request does not set one (the API requires it).
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Configuration of the Anthropic endpoint.
#[derive(Clone)]
pub struct AnthropicConfig {
    /// Base URL without `/v1/messages`.
    pub base_url: String,
    pub api_key: Option<String>,
    /// Extra headers (e.g. `anthropic-beta`).
    pub headers: Vec<(String, String)>,
    pub connect_timeout: Duration,
    /// Per-request credential source. When set it replaces `api_key`; its headers are
    /// appended and its `base_url`, if any, overrides `base_url`. A 401/403 response
    /// invalidates the credential and the request is retried once.
    pub credentials: Option<Arc<dyn CredentialSource>>,
    /// Provider name passed to the credential source (default `anthropic`).
    pub credential_name: String,
}

impl fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("headers", &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("credentials", &self.credentials.as_ref().map(|_| "<source>"))
            .field("credential_name", &self.credential_name)
            .finish_non_exhaustive()
    }
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_owned(),
            api_key: None,
            headers: Vec::new(),
            connect_timeout: Duration::from_secs(10),
            credentials: None,
            credential_name: "anthropic".to_owned(),
        }
    }
}

impl AnthropicConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: Some(api_key.into()), ..Self::default() }
    }

    /// Ask `source` for a credential before every request (see [`crate::credentials`]).
    pub fn with_credentials(mut self, source: Arc<dyn CredentialSource>) -> Self {
        self.credentials = Some(source);
        self
    }

    /// Provider name passed to the credential source.
    pub fn credential_name(mut self, name: impl Into<String>) -> Self {
        self.credential_name = name.into();
        self
    }
}

/// Anthropic chat model endpoint.
#[derive(Clone)]
pub struct AnthropicModel {
    config: AnthropicConfig,
    client: crate::http::LazyClient,
}

impl fmt::Debug for AnthropicModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicModel").field("config", &self.config).finish_non_exhaustive()
    }
}

impl AnthropicModel {
    pub fn new(config: AnthropicConfig) -> Result<Self, LlmError> {
        let client = crate::http::LazyClient::new(config.connect_timeout);
        Ok(Self { config, client })
    }
}

impl AnthropicModel {
    fn request(&self, body: &Json, credential: Option<&Credential>) -> Result<reqwest::RequestBuilder, LlmError> {
        let endpoint =
            crate::http::resolve_endpoint(&self.config.base_url, credential.and_then(|c| c.base_url.as_deref()))?;
        let url = format!("{}/v1/messages", endpoint.base_url.trim_end_matches('/'));
        let mut builder = self.client.get()?.post(url).json(body).header("anthropic-version", ANTHROPIC_VERSION);
        let key = credential.map(|c| c.secret.as_str()).or(self.config.api_key.as_deref());
        if let Some(key) = key {
            builder = builder.header("x-api-key", key);
        }
        if endpoint.send_static {
            for (k, v) in &self.config.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
        }
        if let Some(credential) = credential {
            for (k, v) in &credential.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
        }
        Ok(builder)
    }
}

impl ChatModel for AnthropicModel {
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        Box::pin(async move {
            let body = build_body(&request)?;
            let name = self.config.credential_name.as_str();
            let mut retried = false;
            loop {
                let credential = match &self.config.credentials {
                    Some(source) => Some(source.credential(name).await?),
                    None => None,
                };
                let result = send_sse(self.request(&body, credential.as_ref())?, AnthropicDecoder::default()).await;
                match (result, &self.config.credentials) {
                    // The credential was rejected: drop it and retry once with a fresh one.
                    (Err(LlmError::Auth(_)), Some(source)) if !retried => {
                        source.invalidate(name);
                        retried = true;
                    }
                    (result, _) => return result,
                }
            }
        })
    }
}

/// Convert an image URL into an Anthropic image source (`url` or inline `base64`).
fn image_source(url: &str) -> Json {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            if let Some(media_type) = meta.strip_suffix(";base64") {
                return json!({"type": "base64", "media_type": media_type, "data": data});
            }
        }
    }
    json!({"type": "url", "url": url})
}

fn content_blocks(parts: &[ContentPart]) -> Vec<Json> {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } if text.is_empty() => None,
            ContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
            ContentPart::Image { url, .. } => Some(json!({"type": "image", "source": image_source(url)})),
        })
        .collect()
}

/// Build the JSON request body.
pub(crate) fn build_body(req: &ChatRequest) -> Result<Json, LlmError> {
    check_extra(req)?;
    let mut system_parts: Vec<String> = Vec::new();
    // (role, blocks); consecutive messages with the same role are merged, as the API requires alternation.
    let mut turns: Vec<(&'static str, Vec<Json>)> = Vec::new();
    let mut push = |role: &'static str, blocks: Vec<Json>| {
        if blocks.is_empty() {
            return;
        }
        match turns.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => last_blocks.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    };
    for m in &req.messages {
        match m.role {
            Role::System => system_parts.push(m.text()),
            Role::User => push("user", content_blocks(&m.content)),
            Role::Tool => {
                let id = m
                    .tool_call_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| LlmError::Config("tool message without tool_call_id".into()))?;
                push("user", vec![json!({"type": "tool_result", "tool_use_id": id, "content": m.text()})]);
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                if let Some(Json::Array(state)) = &m.provider_state {
                    blocks.extend(state.iter().cloned());
                }
                blocks.extend(content_blocks(&m.content));
                for c in &m.tool_calls {
                    // The API needs the input as an object; a malformed call must not be replayed as `{}`.
                    let input = c.arguments_json().map_err(|e| LlmError::Config(e.to_string()))?;
                    blocks.push(json!({"type": "tool_use", "id": c.id, "name": c.name, "input": input}));
                }
                push("assistant", blocks);
            }
        }
    }

    match &req.response_format {
        Some(ResponseFormat::JsonObject) => {
            system_parts.push("Respond with a single valid JSON object and nothing else.".to_owned());
        }
        Some(ResponseFormat::JsonSchema { schema, .. }) => system_parts.push(format!(
            "Respond with a single valid JSON value matching this JSON Schema and nothing else:\n{schema}"
        )),
        None | Some(ResponseFormat::Text) => {}
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("max_tokens".into(), json!(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)));
    body.insert("stream".into(), json!(true));
    let system: Vec<&String> = system_parts.iter().filter(|s| !s.is_empty()).collect();
    if !system.is_empty() {
        let joined: Vec<&str> = system.iter().map(|s| s.as_str()).collect();
        body.insert("system".into(), json!(joined.join("\n\n")));
    }
    let messages: Vec<Json> =
        turns.into_iter().map(|(role, blocks)| json!({"role": role, "content": blocks})).collect();
    body.insert("messages".into(), Json::Array(messages));
    if !req.tools.is_empty() {
        let tools: Vec<Json> = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
            .collect();
        body.insert("tools".into(), Json::Array(tools));
    }
    if let Some(choice) = &req.tool_choice {
        let v = match choice {
            ToolChoice::Auto => json!({"type": "auto"}),
            ToolChoice::None => json!({"type": "none"}),
            ToolChoice::Required => json!({"type": "any"}),
            ToolChoice::Tool(name) => json!({"type": "tool", "name": name}),
        };
        body.insert("tool_choice".into(), v);
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if !req.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(req.stop));
    }
    for (k, v) in &req.extra {
        body.insert(k.clone(), v.clone());
    }
    Ok(Json::Object(body))
}

/// Kind of an open content block.
#[derive(Debug)]
enum Block {
    Text,
    ToolUse { call_index: usize },
    Thinking { thinking: String, signature: String },
    RedactedThinking { data: String },
    Other,
}

/// Decodes Messages API stream events.
#[derive(Debug, Default)]
pub(crate) struct AnthropicDecoder {
    blocks: HashMap<u64, Block>,
    tool_calls: usize,
    usage: Usage,
    /// Completed thinking blocks, replayed as provider state.
    thinking: Vec<Json>,
}

fn unknown_block(delta_type: &str, index: u64) -> LlmError {
    LlmError::Decode(format!("`{delta_type}` for content block {index}, which is not an open block of that kind"))
}

fn stop_reason(s: &str) -> FinishReason {
    match s {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_owned()),
    }
}

impl AnthropicDecoder {
    fn usage_delta(&mut self, usage: &Json) -> ChatDelta {
        if let Some(n) = usage.get("input_tokens").and_then(Json::as_u64) {
            self.usage.prompt_tokens = n;
        }
        if let Some(n) = usage.get("output_tokens").and_then(Json::as_u64) {
            self.usage.completion_tokens = n;
        }
        self.usage.total_tokens = self.usage.prompt_tokens.saturating_add(self.usage.completion_tokens);
        ChatDelta::Usage(self.usage)
    }
}

impl SseDecoder for AnthropicDecoder {
    fn decode(&mut self, event: SseEvent) -> Result<Decoded, LlmError> {
        if event.data.trim().is_empty() {
            return Ok(Decoded::default());
        }
        let v: Json =
            serde_json::from_str(&event.data).map_err(|e| LlmError::Decode(format!("{e}: {}", event.data)))?;
        let kind = v.get("type").and_then(Json::as_str).unwrap_or_default();
        let index = v.get("index").and_then(Json::as_u64).unwrap_or(0);
        let mut out = Vec::new();
        match kind {
            "message_start" => {
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    out.push(self.usage_delta(u));
                }
            }
            "content_block_start" => {
                let block = v.get("content_block").cloned().unwrap_or(Json::Null);
                let block_type = block.get("type").and_then(Json::as_str).unwrap_or_default();
                let state = match block_type {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(Json::as_str).filter(|t| !t.is_empty()) {
                            out.push(ChatDelta::Text(t.to_owned()));
                        }
                        Block::Text
                    }
                    "tool_use" => {
                        let call_index = self.tool_calls;
                        self.tool_calls += 1;
                        // Some servers send the full input up front; it is normally `{}` followed by deltas.
                        let initial = block
                            .get("input")
                            .filter(|i| i.as_object().is_some_and(|o| !o.is_empty()))
                            .map(Json::to_string)
                            .unwrap_or_default();
                        out.push(ChatDelta::ToolCall {
                            index: call_index,
                            id: block.get("id").and_then(Json::as_str).map(str::to_owned),
                            name: block.get("name").and_then(Json::as_str).map(str::to_owned),
                            arguments: initial,
                        });
                        Block::ToolUse { call_index }
                    }
                    "thinking" => Block::Thinking {
                        thinking: block.get("thinking").and_then(Json::as_str).unwrap_or_default().to_owned(),
                        signature: block.get("signature").and_then(Json::as_str).unwrap_or_default().to_owned(),
                    },
                    "redacted_thinking" => Block::RedactedThinking {
                        data: block.get("data").and_then(Json::as_str).unwrap_or_default().to_owned(),
                    },
                    _ => Block::Other,
                };
                self.blocks.insert(index, state);
            }
            "content_block_delta" => {
                let delta = v.get("delta").cloned().unwrap_or(Json::Null);
                let delta_type = delta.get("type").and_then(Json::as_str).unwrap_or_default();
                let field = |name: &str| delta.get(name).and_then(Json::as_str).unwrap_or_default().to_owned();
                match (delta_type, self.blocks.get_mut(&index)) {
                    ("text_delta", _) => out.push(ChatDelta::Text(field("text"))),
                    ("input_json_delta", Some(Block::ToolUse { call_index })) => out.push(ChatDelta::ToolCall {
                        index: *call_index,
                        id: None,
                        name: None,
                        arguments: field("partial_json"),
                    }),
                    ("input_json_delta", _) => return Err(unknown_block(delta_type, index)),
                    ("thinking_delta", Some(Block::Thinking { thinking, .. })) => {
                        let t = field("thinking");
                        thinking.push_str(&t);
                        out.push(ChatDelta::Reasoning(t));
                    }
                    ("thinking_delta", _) => out.push(ChatDelta::Reasoning(field("thinking"))),
                    ("signature_delta", Some(Block::Thinking { signature, .. })) => {
                        signature.push_str(&field("signature"))
                    }
                    // A lost signature would make the next turn fail; report it now.
                    ("signature_delta", _) => return Err(unknown_block(delta_type, index)),
                    _ => {} // citations and future delta types carry nothing we use
                }
            }
            "content_block_stop" => match self.blocks.remove(&index) {
                Some(Block::Thinking { thinking, signature }) => {
                    self.thinking.push(json!({"type": "thinking", "thinking": thinking, "signature": signature}));
                }
                Some(Block::RedactedThinking { data }) => {
                    self.thinking.push(json!({"type": "redacted_thinking", "data": data}));
                }
                _ => {}
            },
            "message_delta" => {
                if let Some(reason) = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(Json::as_str) {
                    out.push(ChatDelta::Finish(stop_reason(reason)));
                }
                if let Some(u) = v.get("usage") {
                    out.push(self.usage_delta(u));
                }
            }
            "message_stop" => {
                if !self.thinking.is_empty() {
                    out.push(ChatDelta::ProviderState(Json::Array(std::mem::take(&mut self.thinking))));
                }
                return Ok(Decoded::done(out));
            }
            "error" => {
                let err = v.get("error").cloned().unwrap_or(Json::Null);
                let message = err.get("message").and_then(Json::as_str).unwrap_or("stream error").to_owned();
                let err_type = err.get("type").and_then(Json::as_str).unwrap_or_default();
                return Err(match err_type {
                    "overloaded_error" | "api_error" => LlmError::Server { status: 529, message },
                    "rate_limit_error" => LlmError::RateLimit { message, retry_after_ms: None },
                    "authentication_error" | "permission_error" => LlmError::Auth(message),
                    _ => LlmError::BadRequest(message),
                });
            }
            _ => {} // ping and future event types
        }
        Ok(Decoded::deltas(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;
    use crate::types::{Message, ToolCall, collect};
    use graphrt_core::tool::ToolSpec;

    #[test]
    fn body_shape() {
        let mut assistant = Message::assistant("let me check");
        assistant.tool_calls = vec![ToolCall { id: "tu1".into(), name: "get".into(), arguments: "{\"a\":1}".into() }];
        assistant.provider_state = Some(json!([{"type": "thinking", "thinking": "t", "signature": "s"}]));
        let mut req = ChatRequest::new(
            "claude-sonnet",
            vec![
                Message::system("be brief"),
                Message::user("hi"),
                Message {
                    role: Role::User,
                    content: vec![ContentPart::Image { url: "data:image/png;base64,AAAA".into(), detail: None }],
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_state: None,
                },
                assistant,
                Message::tool("tu1", "42"),
                Message::tool("tu2", "43"),
            ],
        );
        req.tools =
            vec![ToolSpec { name: "get".into(), description: "d".into(), parameters: json!({"type": "object"}) }];
        req.tool_choice = Some(ToolChoice::Required);
        req.stop = vec!["END".into()];
        req.response_format = Some(ResponseFormat::JsonObject);
        let body = build_body(&req).unwrap();
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert!(body["system"].as_str().unwrap().starts_with("be brief"));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "consecutive user turns and tool results are merged");
        assert_eq!(msgs[0]["content"][1]["source"]["type"], "base64");
        assert_eq!(msgs[0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(msgs[1]["content"][0]["type"], "thinking");
        assert_eq!(msgs[1]["content"][2]["input"]["a"], 1);
        assert_eq!(msgs[2]["content"][1]["tool_use_id"], "tu2");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["tool_choice"]["type"], "any");
        assert_eq!(body["stop_sequences"][0], "END");
    }

    const FIXTURE: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"get\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"2}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":20}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    #[tokio::test]
    async fn decode_recorded_stream() {
        let (deltas, done) = decode_all(FIXTURE);
        assert!(done);
        let deltas: Vec<Result<ChatDelta, LlmError>> = deltas.into_iter().map(Ok).collect();
        let r = collect(Box::pin(futures::stream::iter(deltas)), |_| {}).await.unwrap();
        assert_eq!(r.text, "Hi");
        assert_eq!(r.reasoning, "hmm");
        assert_eq!(
            r.tool_calls,
            vec![ToolCall { id: "tu_1".into(), name: "get".into(), arguments: "{\"a\":2}".into() }]
        );
        assert_eq!(r.usage, Usage { prompt_tokens: 10, completion_tokens: 20, total_tokens: 30 });
        assert_eq!(r.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(r.provider_state, Some(json!([{"type": "thinking", "thinking": "hmm", "signature": "sig"}])));
        // The provider state is replayed into the next request.
        let body = build_body(&ChatRequest::new("m", vec![r.to_message()])).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["signature"], "sig");
    }

    /// Decode a fixture; returns the deltas and whether `message_stop` was seen.
    fn decode_all(fixture: &str) -> (Vec<ChatDelta>, bool) {
        let mut parser = SseParser::new();
        let mut decoder = AnthropicDecoder::default();
        let mut deltas = Vec::new();
        for ev in parser.push(fixture.as_bytes()).unwrap() {
            let d = decoder.decode(ev).unwrap();
            deltas.extend(d.deltas);
            if d.done {
                return (deltas, true);
            }
        }
        (deltas, false)
    }

    #[test]
    fn stream_without_message_stop_is_truncated() {
        let cut = FIXTURE.split("event: message_delta").next().unwrap();
        let (_, done) = decode_all(cut);
        assert!(!done);
        let mut decoder = AnthropicDecoder::default();
        let err = decoder.end_of_stream().unwrap_err();
        assert!(matches!(err, LlmError::Network(_)) && err.is_retryable());
    }

    #[test]
    fn deltas_for_unknown_blocks_are_errors() {
        let mut d = AnthropicDecoder::default();
        let ev = SseEvent {
            data: "{\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}".into(),
            ..SseEvent::default()
        };
        assert!(matches!(d.decode(ev), Err(LlmError::Decode(_))));
    }

    #[test]
    fn malformed_tool_arguments_are_not_replayed_as_empty_input() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![ToolCall { id: "t".into(), name: "get".into(), arguments: "{\"a\":".into() }];
        let req = ChatRequest::new("m", vec![Message::user("x"), assistant, Message::tool("t", "r")]);
        assert!(matches!(build_body(&req), Err(LlmError::Config(_))));
    }

    #[test]
    fn final_turn_keeps_tools_with_choice_none() {
        let req = ChatRequest::new("m", vec![Message::user("x")])
            .with_tools(vec![ToolSpec { name: "t".into(), description: "d".into(), parameters: json!({}) }])
            .with_tool_choice(ToolChoice::None);
        let body = build_body(&req).unwrap();
        assert_eq!(body["tool_choice"]["type"], "none");
        assert_eq!(body["tools"][0]["name"], "t");
    }

    #[test]
    fn error_event() {
        let mut d = AnthropicDecoder::default();
        let ev = SseEvent {
            data: "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}".into(),
            ..SseEvent::default()
        };
        let err = d.decode(ev).unwrap_err();
        assert!(err.is_retryable());
    }
}
