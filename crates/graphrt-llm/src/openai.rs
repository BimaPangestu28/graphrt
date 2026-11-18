//! OpenAI-compatible chat completions provider.
//!
//! Works with any endpoint implementing `POST {base_url}/chat/completions` with
//! streaming: OpenAI, Azure OpenAI (set [`AuthStyle::Header`] with `api-key`
//! and an `api-version` query parameter), Ollama, vLLM, LM Studio, Groq,
//! OpenRouter, DeepSeek, Together, Mistral and others.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::event::Usage;
use serde_json::{Map, Value as Json, json};

use crate::credentials::{Credential, CredentialSource};
use crate::error::LlmError;
use crate::http::{Decoded, SseDecoder, send_sse, truncated};
use crate::sse::SseEvent;
use crate::types::{
    ChatDelta, ChatModel, ChatRequest, ChatStream, ContentPart, FinishReason, Message, ResponseFormat, Role,
    ToolChoice, check_extra,
};

/// How the API key is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// A custom header, e.g. `api-key` for Azure OpenAI.
    Header(String),
}

/// Configuration of an OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// Base URL without the `/chat/completions` suffix, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub auth: AuthStyle,
    /// Extra headers sent with every request.
    pub headers: Vec<(String, String)>,
    /// Extra query parameters (e.g. `api-version`).
    pub query: Vec<(String, String)>,
    /// Ask for a final usage chunk (`stream_options.include_usage`). Disable for
    /// endpoints that reject the field.
    pub include_usage: bool,
    /// Send `max_completion_tokens` instead of `max_tokens` (required by some newer models).
    pub use_max_completion_tokens: bool,
    pub connect_timeout: Duration,
    /// Per-request credential source. When set it replaces `api_key`; its headers are
    /// appended and its `base_url`, if any, overrides `base_url`. A 401/403 response
    /// invalidates the credential and the request is retried once.
    pub credentials: Option<Arc<dyn CredentialSource>>,
    /// Provider name passed to the credential source (default `openai`).
    pub credential_name: String,
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("auth", &self.auth)
            .field("headers", &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("include_usage", &self.include_usage)
            .field("credentials", &self.credentials.as_ref().map(|_| "<source>"))
            .field("credential_name", &self.credential_name)
            .finish_non_exhaustive()
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_owned(),
            api_key: None,
            auth: AuthStyle::Bearer,
            headers: Vec::new(),
            query: Vec::new(),
            include_usage: true,
            use_max_completion_tokens: false,
            connect_timeout: Duration::from_secs(10),
            credentials: None,
            credential_name: "openai".to_owned(),
        }
    }
}

impl OpenAiConfig {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self { base_url: base_url.into(), api_key, ..Self::default() }
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

/// An OpenAI-compatible chat model endpoint.
#[derive(Clone)]
pub struct OpenAiModel {
    config: OpenAiConfig,
    client: crate::http::LazyClient,
}

impl fmt::Debug for OpenAiModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiModel").field("config", &self.config).finish_non_exhaustive()
    }
}

impl OpenAiModel {
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        let client = crate::http::LazyClient::new(config.connect_timeout);
        Ok(Self { config, client })
    }

    fn endpoint(&self, base_url: &str, with_query: bool) -> String {
        let mut url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let query: &[(String, String)] = if with_query { &self.config.query } else { &[] };
        for (i, (k, v)) in query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(&percent_encode(k));
            url.push('=');
            url.push_str(&percent_encode(v));
        }
        url
    }
}

/// Percent-encode a query component (RFC 3986 unreserved characters pass through).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl OpenAiModel {
    fn request(&self, body: &Json, credential: Option<&Credential>) -> Result<reqwest::RequestBuilder, LlmError> {
        let endpoint =
            crate::http::resolve_endpoint(&self.config.base_url, credential.and_then(|c| c.base_url.as_deref()))?;
        let mut builder = self.client.get()?.post(self.endpoint(&endpoint.base_url, endpoint.send_static)).json(body);
        let key = credential.map(|c| c.secret.as_str()).or(self.config.api_key.as_deref());
        if let Some(key) = key {
            builder = match &self.config.auth {
                AuthStyle::Bearer => builder.bearer_auth(key),
                AuthStyle::Header(name) => builder.header(name.as_str(), key),
            };
        }
        // Operator headers (gateway keys, org ids) never follow a credential override
        // to another origin.
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

impl ChatModel for OpenAiModel {
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        Box::pin(async move {
            let body = build_body(&request, &self.config)?;
            let name = self.config.credential_name.as_str();
            let mut retried = false;
            loop {
                let credential = match &self.config.credentials {
                    Some(source) => Some(source.credential(name).await?),
                    None => None,
                };
                let result = send_sse(self.request(&body, credential.as_ref())?, OpenAiDecoder::default()).await;
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

fn content_json(parts: &[ContentPart]) -> Json {
    if let [ContentPart::Text { text }] = parts {
        return Json::String(text.clone());
    }
    let items: Vec<Json> = parts
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => json!({"type": "text", "text": text}),
            ContentPart::Image { url, detail } => {
                let mut image = Map::new();
                image.insert("url".into(), Json::String(url.clone()));
                if let Some(d) = detail {
                    image.insert("detail".into(), Json::String(d.clone()));
                }
                json!({"type": "image_url", "image_url": image})
            }
        })
        .collect();
    Json::Array(items)
}

fn message_json(m: &Message) -> Result<Json, LlmError> {
    Ok(match m.role {
        Role::System => json!({"role": "system", "content": content_json(&m.content)}),
        Role::User => json!({"role": "user", "content": content_json(&m.content)}),
        Role::Tool => {
            let id = m
                .tool_call_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| LlmError::Config("tool message without tool_call_id".into()))?;
            json!({"role": "tool", "tool_call_id": id, "content": m.text()})
        }
        Role::Assistant => {
            let mut obj = Map::new();
            obj.insert("role".into(), json!("assistant"));
            let text = m.text();
            obj.insert("content".into(), if text.is_empty() { Json::Null } else { Json::String(text) });
            if !m.tool_calls.is_empty() {
                let calls: Vec<Json> = m
                    .tool_calls
                    .iter()
                    .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}))
                    .collect();
                obj.insert("tool_calls".into(), Json::Array(calls));
            }
            Json::Object(obj)
        }
    })
}

/// Build the JSON request body.
pub(crate) fn build_body(req: &ChatRequest, config: &OpenAiConfig) -> Result<Json, LlmError> {
    check_extra(req)?;
    // Only the first choice is decoded; asking for more would silently discard them.
    if let Some(n) = req.extra.get("n") {
        if n.as_u64() != Some(1) {
            return Err(LlmError::Config(format!("`n` must be 1 (got {n}); only one choice is supported")));
        }
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    let messages = req.messages.iter().map(message_json).collect::<Result<Vec<_>, _>>()?;
    body.insert("messages".into(), Json::Array(messages));
    body.insert("stream".into(), json!(true));
    if config.include_usage {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Json> = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
        body.insert("tools".into(), Json::Array(tools));
    }
    if let Some(choice) = &req.tool_choice {
        let v = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Tool(name) => json!({"type": "function", "function": {"name": name}}),
        };
        body.insert("tool_choice".into(), v);
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if let Some(max) = req.max_tokens {
        let key = if config.use_max_completion_tokens { "max_completion_tokens" } else { "max_tokens" };
        body.insert(key.into(), json!(max));
    }
    if !req.stop.is_empty() {
        body.insert("stop".into(), json!(req.stop));
    }
    match &req.response_format {
        None | Some(ResponseFormat::Text) => {}
        Some(ResponseFormat::JsonObject) => {
            body.insert("response_format".into(), json!({"type": "json_object"}));
        }
        Some(ResponseFormat::JsonSchema { name, schema, strict }) => {
            body.insert(
                "response_format".into(),
                json!({"type": "json_schema", "json_schema": {"name": name, "schema": schema, "strict": strict}}),
            );
        }
    }
    for (k, v) in &req.extra {
        body.insert(k.clone(), v.clone());
    }
    Ok(Json::Object(body))
}

/// Decodes `chat.completion.chunk` events.
///
/// Tool-call fragments are keyed by their `index`. Some compatible servers omit
/// `index`; then a fragment with a new `id` starts a new call and a fragment
/// without `id` continues the most recent one.
#[derive(Debug, Default)]
pub(crate) struct OpenAiDecoder {
    /// Call index assigned to each call id seen so far.
    ids: HashMap<String, usize>,
    /// Index of the most recent call fragment.
    last: Option<usize>,
    /// Smallest index not used yet.
    next: usize,
    /// A `finish_reason` was received (some servers end without `[DONE]`).
    saw_finish: bool,
}

impl OpenAiDecoder {
    fn call_index(&mut self, explicit: Option<usize>, id: Option<&str>) -> usize {
        let index = match (explicit, id) {
            (Some(i), _) => i,
            (None, Some(id)) => match self.ids.get(id) {
                Some(&i) => i,
                None => self.next,
            },
            (None, None) => self.last.unwrap_or(self.next),
        };
        if let Some(id) = id {
            self.ids.entry(id.to_owned()).or_insert(index);
        }
        self.last = Some(index);
        self.next = self.next.max(index.saturating_add(1));
        index
    }
}

/// Classify an error object embedded in the stream.
fn stream_error(err: &Json) -> LlmError {
    let message = err.get("message").and_then(Json::as_str).map(str::to_owned).unwrap_or_else(|| err.to_string());
    let kind = err.get("type").and_then(Json::as_str).unwrap_or_default();
    let code = match err.get("code") {
        Some(Json::String(c)) => c.clone(),
        Some(Json::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let tag = format!("{kind} {code}").to_ascii_lowercase();
    if tag.contains("rate_limit") || code == "429" {
        LlmError::RateLimit { message, retry_after_ms: None }
    } else if tag.contains("context_length") {
        LlmError::ContextLength(message)
    } else if tag.contains("auth") || tag.contains("permission") || code == "401" || code == "403" {
        LlmError::Auth(message)
    } else if tag.contains("invalid_request") || code == "400" {
        LlmError::BadRequest(message)
    } else {
        LlmError::Server { status: 0, message }
    }
}

fn as_u64(v: Option<&Json>) -> u64 {
    v.and_then(Json::as_u64).unwrap_or(0)
}

fn finish_reason(s: &str) -> FinishReason {
    match s {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_owned()),
    }
}

impl SseDecoder for OpenAiDecoder {
    fn decode(&mut self, event: SseEvent) -> Result<Decoded, LlmError> {
        let data = event.data.trim();
        if data == "[DONE]" {
            if !self.saw_finish {
                return Err(truncated());
            }
            return Ok(Decoded::done(Vec::new()));
        }
        if data.is_empty() {
            return Ok(Decoded::default());
        }
        let chunk: Json = serde_json::from_str(data).map_err(|e| LlmError::Decode(format!("{e}: {data}")))?;
        if let Some(err) = chunk.get("error").filter(|e| !e.is_null()) {
            return Err(stream_error(err));
        }
        let mut deltas = Vec::new();
        if let Some(choice) = chunk.get("choices").and_then(Json::as_array).and_then(|c| c.first()) {
            if let Some(delta) = choice.get("delta") {
                // Reasoning is exposed as `reasoning_content` (DeepSeek, vLLM) or `reasoning`
                // (OpenRouter, Ollama). Some servers send both with the same text: use the first.
                let reasoning = ["reasoning_content", "reasoning"]
                    .iter()
                    .find_map(|key| delta.get(*key).and_then(Json::as_str).filter(|s| !s.is_empty()));
                if let Some(r) = reasoning {
                    deltas.push(ChatDelta::Reasoning(r.to_owned()));
                }
                if let Some(text) = delta.get("content").and_then(Json::as_str).filter(|s| !s.is_empty()) {
                    deltas.push(ChatDelta::Text(text.to_owned()));
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Json::as_array) {
                    for call in calls {
                        let explicit = call.get("index").and_then(Json::as_u64).and_then(|i| usize::try_from(i).ok());
                        let id = call.get("id").and_then(Json::as_str).filter(|s| !s.is_empty());
                        let index = self.call_index(explicit, id);
                        let function = call.get("function");
                        let arguments = match function.and_then(|f| f.get("arguments")) {
                            None | Some(Json::Null) => String::new(),
                            Some(Json::String(s)) => s.clone(),
                            // Some servers send the arguments as a JSON object instead of a string.
                            Some(other) => other.to_string(),
                        };
                        deltas.push(ChatDelta::ToolCall {
                            index,
                            id: id.map(str::to_owned),
                            name: function.and_then(|f| f.get("name")).and_then(Json::as_str).map(str::to_owned),
                            arguments,
                        });
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Json::as_str) {
                self.saw_finish = true;
                deltas.push(ChatDelta::Finish(finish_reason(reason)));
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            deltas.push(ChatDelta::Usage(Usage {
                prompt_tokens: as_u64(usage.get("prompt_tokens")),
                completion_tokens: as_u64(usage.get("completion_tokens")),
                total_tokens: as_u64(usage.get("total_tokens")),
            }));
        }
        Ok(Decoded::deltas(deltas))
    }

    /// Servers that omit `[DONE]` are accepted once a `finish_reason` arrived.
    fn end_of_stream(&mut self) -> Result<Vec<ChatDelta>, LlmError> {
        if self.saw_finish { Ok(Vec::new()) } else { Err(truncated()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;
    use crate::types::ToolCall;
    use graphrt_core::tool::ToolSpec;

    #[test]
    fn body_shape() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![ToolCall { id: "c1".into(), name: "search".into(), arguments: "{}".into() }];
        let mut req = ChatRequest::new(
            "gpt-4o-mini",
            vec![
                Message::system("sys"),
                Message {
                    role: Role::User,
                    content: vec![
                        ContentPart::Text { text: "look".into() },
                        ContentPart::Image { url: "https://x/y.png".into(), detail: Some("high".into()) },
                    ],
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_state: None,
                },
                assistant,
                Message::tool("c1", "result"),
            ],
        );
        req.tools =
            vec![ToolSpec { name: "search".into(), description: "d".into(), parameters: json!({"type": "object"}) }];
        req.tool_choice = Some(ToolChoice::Tool("search".into()));
        req.max_tokens = Some(10);
        req.response_format =
            Some(ResponseFormat::JsonSchema { name: "out".into(), schema: json!({"type": "object"}), strict: true });
        req.extra.insert("seed".into(), json!(7));
        let body = build_body(&req, &OpenAiConfig::default()).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["content"], "sys");
        assert_eq!(body["messages"][1]["content"][1]["image_url"]["detail"], "high");
        assert_eq!(body["messages"][2]["content"], Json::Null);
        assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["name"], "search");
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(body["tool_choice"]["function"]["name"], "search");
        assert_eq!(body["max_tokens"], 10);
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(body["seed"], 7);

        let cfg = OpenAiConfig { use_max_completion_tokens: true, include_usage: false, ..OpenAiConfig::default() };
        let body = build_body(&req, &cfg).unwrap();
        assert_eq!(body["max_completion_tokens"], 10);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn body_rejects_reserved_extra_n_and_missing_tool_id() {
        let req = ChatRequest::new("m", vec![Message::user("x")]).with_extra("stream", json!(false));
        assert!(matches!(build_body(&req, &OpenAiConfig::default()), Err(LlmError::Config(_))));
        let req = ChatRequest::new("m", vec![Message::user("x")]).with_extra("n", json!(2));
        assert!(matches!(build_body(&req, &OpenAiConfig::default()), Err(LlmError::Config(_))));
        let mut tool = Message::tool("x", "r");
        tool.tool_call_id = None;
        let req = ChatRequest::new("m", vec![tool]);
        assert!(matches!(build_body(&req, &OpenAiConfig::default()), Err(LlmError::Config(_))));
    }

    fn decode_all(fixture: &str) -> (Vec<ChatDelta>, Result<bool, LlmError>) {
        let mut parser = SseParser::new();
        let mut decoder = OpenAiDecoder::default();
        let mut deltas = Vec::new();
        for ev in parser.push(fixture.as_bytes()).unwrap() {
            match decoder.decode(ev) {
                Ok(d) => {
                    deltas.extend(d.deltas);
                    if d.done {
                        return (deltas, Ok(true));
                    }
                }
                Err(e) => return (deltas, Err(e)),
            }
        }
        match decoder.end_of_stream() {
            Ok(d) => {
                deltas.extend(d);
                (deltas, Ok(false))
            }
            Err(e) => (deltas, Err(e)),
        }
    }

    #[test]
    fn truncated_stream_is_a_retryable_error() {
        let (_, end) = decode_all("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n");
        let err = end.unwrap_err();
        assert!(matches!(err, LlmError::Network(_)));
        assert!(err.is_retryable());
        // `[DONE]` without any finish_reason is also incomplete.
        let (_, end) =
            decode_all("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n");
        assert!(end.is_err());
        // A finish_reason without `[DONE]` is accepted.
        let (deltas, end) = decode_all(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        assert!(!end.unwrap());
        assert_eq!(deltas.last(), Some(&ChatDelta::Finish(FinishReason::Stop)));
    }

    #[tokio::test]
    async fn tool_calls_without_index_and_object_arguments() {
        let fixture = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"a\",\"function\":{\"name\":\"one\",\"arguments\":{\"x\":1}}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"b\",\"function\":{\"name\":\"two\",\"arguments\":\"{\\\"y\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"2}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (deltas, end) = decode_all(fixture);
        assert!(end.unwrap());
        let r =
            crate::types::collect(Box::pin(futures::stream::iter(deltas.into_iter().map(Ok))), |_| {}).await.unwrap();
        assert_eq!(r.tool_calls.len(), 2, "{:?}", r.tool_calls);
        assert_eq!(r.tool_calls[0].arguments_json().unwrap()["x"], 1);
        assert_eq!(r.tool_calls[1].name, "two");
        assert_eq!(r.tool_calls[1].arguments_json().unwrap()["y"], 2);
    }

    #[test]
    fn duplicate_reasoning_fields_are_not_doubled() {
        let (deltas, _) = decode_all(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"r\",\"reasoning\":\"r\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        assert_eq!(deltas.iter().filter(|d| matches!(d, ChatDelta::Reasoning(_))).count(), 1);
    }

    #[test]
    fn decode_recorded_stream() {
        let fixture = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"think\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"a\\\":1}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        );
        let (deltas, done) = decode_all(fixture);
        assert!(done.unwrap());
        assert_eq!(deltas[0], ChatDelta::Reasoning("think".into()));
        assert_eq!(deltas[1], ChatDelta::Text("Hel".into()));
        assert!(matches!(&deltas[3], ChatDelta::ToolCall { index: 0, name: Some(n), .. } if n == "get"));
        assert!(matches!(&deltas[4], ChatDelta::ToolCall { arguments, .. } if arguments == "{\"a\":1}"));
        assert_eq!(deltas[5], ChatDelta::Finish(FinishReason::ToolCalls));
        assert_eq!(deltas[6], ChatDelta::Usage(Usage { prompt_tokens: 5, completion_tokens: 2, total_tokens: 7 }));
    }

    #[test]
    fn stream_error_object() {
        let mut d = OpenAiDecoder::default();
        let ev = SseEvent { data: "{\"error\":{\"message\":\"boom\"}}".into(), ..SseEvent::default() };
        assert!(matches!(d.decode(ev), Err(LlmError::Server { .. })));
        let ev = SseEvent {
            data: "{\"error\":{\"message\":\"slow\",\"type\":\"rate_limit_exceeded\"}}".into(),
            ..SseEvent::default()
        };
        assert!(matches!(d.decode(ev), Err(LlmError::RateLimit { .. })));
        let ev = SseEvent {
            data: "{\"error\":{\"message\":\"long\",\"code\":\"context_length_exceeded\"}}".into(),
            ..SseEvent::default()
        };
        assert!(matches!(d.decode(ev), Err(LlmError::ContextLength(_))));
    }

    #[test]
    fn debug_redacts_key() {
        let cfg = OpenAiConfig::new("https://x", Some("sk-secret".into()));
        assert!(!format!("{cfg:?}").contains("sk-secret"));
    }
}
