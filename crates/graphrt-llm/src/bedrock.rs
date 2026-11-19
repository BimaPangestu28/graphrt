//! AWS Bedrock provider (Converse API, `converse-stream`).
//!
//! Authentication, in order of precedence:
//! 1. a [`CredentialSource`] (its secret is sent as a Bedrock API key bearer token);
//! 2. [`BedrockAuth::ApiKey`] (Bedrock API key, `Authorization: Bearer`, e.g.
//!    from `AWS_BEARER_TOKEN_BEDROCK`);
//! 3. [`BedrockAuth::SigV4`] (access key, secret key, optional session token),
//!    signed with AWS Signature Version 4.
//!
//! Only explicit credentials are supported: there is no instance-metadata,
//! SSO or profile-file credential chain.
//!
//! Mapping notes:
//! - Images must be inline (`data:` URLs); Bedrock does not fetch image URLs.
//! - JSON output is requested through a system instruction (Converse has no
//!   provider-neutral JSON mode).
//! - [`ToolChoice::None`] keeps the tool definitions (required when the history
//!   contains tool calls) but sends no `toolChoice`, which means "auto": Converse
//!   has no way to forbid tool use.
//! - [`ChatRequest::extra`] keys `guardrailConfig`, `performanceConfig`,
//!   `promptVariables`, `requestMetadata` and `additionalModelRequestFields` go
//!   to the request top level; any other key is merged into
//!   `additionalModelRequestFields` (e.g. `thinking` for Claude models).
//! - Signed reasoning blocks are returned as provider state and replayed on the
//!   next turn, as required for extended thinking combined with tool use.

mod eventstream;
mod sigv4;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::event::Usage;
use serde_json::{Map, Value as Json, json};

pub use self::sigv4::AwsCredentials;
use crate::credentials::CredentialSource;
use crate::error::LlmError;
use crate::http::{byte_stream, send_request};
use crate::types::{
    ChatDelta, ChatModel, ChatRequest, ChatStream, ContentPart, FinishReason, ResponseFormat, Role, ToolChoice,
    check_extra, tool_images_intro,
};

/// `extra` keys placed at the request top level.
const TOP_LEVEL_EXTRA: &[&str] =
    &["guardrailConfig", "performanceConfig", "promptVariables", "requestMetadata", "additionalModelRequestFields"];

/// Key under which reasoning blocks are stored in the message provider state.
const STATE_KEY: &str = "bedrock_reasoning";

/// Static Bedrock credentials.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BedrockAuth {
    /// AWS access keys, signed with SigV4.
    SigV4(AwsCredentials),
    /// A Bedrock API key (bearer token).
    ApiKey(String),
}

impl fmt::Debug for BedrockAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BedrockAuth::SigV4(creds) => f.debug_tuple("SigV4").field(creds).finish(),
            BedrockAuth::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
        }
    }
}

/// Configuration of a Bedrock runtime endpoint.
#[derive(Clone)]
pub struct BedrockConfig {
    /// AWS region, e.g. `us-east-1`.
    pub region: String,
    /// Endpoint override (VPC endpoints, proxies, tests). Defaults to
    /// `https://bedrock-runtime.<region>.amazonaws.com`.
    pub endpoint: Option<String>,
    pub auth: Option<BedrockAuth>,
    /// Dynamic credentials (bearer API keys); when set they replace `auth`. On
    /// HTTP 401/403 the source is invalidated and the request retried once.
    pub credentials: Option<Arc<dyn CredentialSource>>,
    /// Provider name passed to the credential source.
    pub provider_name: String,
    /// Extra headers (not signed).
    pub headers: Vec<(String, String)>,
    pub connect_timeout: Duration,
}

impl fmt::Debug for BedrockConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockConfig")
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("auth", &self.auth)
            .field("credentials", &self.credentials)
            .field("provider_name", &self.provider_name)
            .field("headers", &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl BedrockConfig {
    pub fn new(region: impl Into<String>, auth: BedrockAuth) -> Self {
        Self {
            region: region.into(),
            endpoint: None,
            auth: Some(auth),
            credentials: None,
            provider_name: "bedrock".to_owned(),
            headers: Vec::new(),
            connect_timeout: Duration::from_secs(10),
        }
    }

    /// Use a credential source (bearer API keys) instead of static credentials.
    pub fn with_credentials(mut self, source: Arc<dyn CredentialSource>) -> Self {
        self.credentials = Some(source);
        self
    }

    fn base_url(&self) -> String {
        match &self.endpoint {
            Some(e) => e.trim_end_matches('/').to_owned(),
            None => format!("https://bedrock-runtime.{}.amazonaws.com", self.region),
        }
    }
}

/// Bedrock chat model endpoint.
#[derive(Clone)]
pub struct BedrockModel {
    config: BedrockConfig,
    client: crate::http::LazyClient,
}

impl fmt::Debug for BedrockModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockModel").field("config", &self.config).finish_non_exhaustive()
    }
}

impl BedrockModel {
    pub fn new(config: BedrockConfig) -> Result<Self, LlmError> {
        let valid_region = !config.region.is_empty()
            && config.region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid_region {
            return Err(LlmError::Config(format!("invalid AWS region `{}`", config.region)));
        }
        let client = crate::http::LazyClient::new(config.connect_timeout);
        Ok(Self { config, client })
    }

    /// Build a signed (or bearer-authenticated) request for `body` to `path`.
    async fn request(&self, path: &str, body: &[u8]) -> Result<reqwest::RequestBuilder, LlmError> {
        let parse = |base: &str| {
            reqwest::Url::parse(&format!("{}{path}", base.trim_end_matches('/')))
                .map_err(|e| LlmError::Config(format!("invalid Bedrock endpoint: {e}")))
        };
        let configured = self.config.base_url();
        if let Some(source) = &self.config.credentials {
            // Bearer credentials may carry a proxy base URL (thin-proxy pattern).
            let credential = source.credential(&self.config.provider_name).await?;
            let endpoint = crate::http::resolve_endpoint(&configured, credential.base_url.as_deref())?;
            let url = parse(&endpoint.base_url)?;
            let mut builder =
                self.client.get()?.post(url).header("content-type", "application/json").body(body.to_vec());
            builder = builder.bearer_auth(&credential.secret);
            for (k, v) in &credential.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            if endpoint.send_static {
                for (k, v) in &self.config.headers {
                    builder = builder.header(k.as_str(), v.as_str());
                }
            }
            return Ok(builder);
        }
        let url = parse(&configured)?;
        let mut builder =
            self.client.get()?.post(url.clone()).header("content-type", "application/json").body(body.to_vec());
        {
            match &self.config.auth {
                Some(BedrockAuth::ApiKey(key)) => builder = builder.bearer_auth(key),
                Some(BedrockAuth::SigV4(creds)) => {
                    let host = match (url.host_str(), url.port()) {
                        (Some(h), Some(p)) => format!("{h}:{p}"),
                        (Some(h), None) => h.to_owned(),
                        (None, _) => return Err(LlmError::Config("Bedrock endpoint has no host".into())),
                    };
                    // A signature dated 1970 would only produce a confusing "signature
                    // expired" from AWS; fail clearly instead.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .map_err(|_| {
                        LlmError::Config("system clock is before 1970; cannot sign AWS requests".into())
                    })?;
                    let date = sigv4::amz_date(now);
                    let payload_hash = sigv4::sha256_hex(body);
                    let mut headers: Vec<(&str, &str)> = vec![
                        ("content-type", "application/json"),
                        ("host", &host),
                        ("x-amz-content-sha256", &payload_hash),
                        ("x-amz-date", &date),
                    ];
                    if let Some(token) = &creds.session_token {
                        headers.push(("x-amz-security-token", token));
                    }
                    let signing = sigv4::SigningRequest {
                        method: "POST",
                        path: url.path(),
                        query: &[],
                        headers: &headers,
                        payload_hash: &payload_hash,
                        region: &self.config.region,
                        service: "bedrock",
                        amz_date: &date,
                        double_encode_path: true,
                    };
                    let authorization = sigv4::authorization(creds, &signing);
                    builder = builder
                        .header("x-amz-content-sha256", payload_hash.as_str())
                        .header("x-amz-date", date.as_str())
                        .header("authorization", authorization);
                    if let Some(token) = &creds.session_token {
                        builder = builder.header("x-amz-security-token", token.as_str());
                    }
                }
                None => {
                    return Err(LlmError::Config(format!(
                        "provider `{}` has no credentials",
                        self.config.provider_name
                    )));
                }
            }
        }
        for (k, v) in &self.config.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        Ok(builder)
    }
}

impl ChatModel for BedrockModel {
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        Box::pin(async move {
            let body = build_body(&request)?;
            let body =
                serde_json::to_vec(&body).map_err(|e| LlmError::Config(format!("cannot encode request: {e}")))?;
            let path = format!("/model/{}/converse-stream", sigv4::uri_encode(&request.model, false));
            let mut retried = false;
            loop {
                let builder = self.request(&path, &body).await?;
                match send_request(builder, classify).await {
                    Ok(response) => return Ok(event_stream(response)),
                    Err(LlmError::Auth(_)) if !retried && self.config.credentials.is_some() => {
                        if let Some(source) = &self.config.credentials {
                            source.invalidate(&self.config.provider_name);
                        }
                        retried = true;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
    }
}

/// Classify an HTTP error response (`x-amzn-errortype` style bodies carry `message`).
fn classify(status: u16, body: &str, retry_after_ms: Option<u64>) -> LlmError {
    let lowered = body.to_ascii_lowercase();
    if status == 400 && (lowered.contains("input is too long") || lowered.contains("too many input tokens")) {
        return LlmError::ContextLength(LlmError::from_status(400, body, None).to_string());
    }
    match status {
        424 => LlmError::Server { status, message: message_of(body) },
        _ => LlmError::from_status(status, body, retry_after_ms),
    }
}

fn message_of(body: &str) -> String {
    serde_json::from_str::<Json>(body)
        .ok()
        .and_then(|v| v.get("message").or_else(|| v.get("Message")).and_then(Json::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(1000).collect())
}

/// Image `format` from a media type.
fn image_format(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpeg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn content_blocks(parts: &[ContentPart]) -> Result<Vec<Json>, LlmError> {
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            ContentPart::Text { text } if text.is_empty() => {}
            ContentPart::Text { text } => out.push(json!({"text": text})),
            ContentPart::Image { url, .. } => {
                let (mime, data) = url
                    .strip_prefix("data:")
                    .and_then(|rest| rest.split_once(','))
                    .and_then(|(meta, data)| meta.strip_suffix(";base64").map(|m| (m, data)))
                    .ok_or_else(|| {
                        LlmError::Config("Bedrock requires images as base64 `data:` URLs, not remote URLs".into())
                    })?;
                let format = image_format(mime)
                    .ok_or_else(|| LlmError::Config(format!("image type `{mime}` is not supported by Bedrock")))?;
                out.push(json!({"image": {"format": format, "source": {"bytes": data}}}));
            }
        }
    }
    Ok(out)
}

/// Whether `model` (an id, inference profile or ARN) accepts images inside
/// `toolResult` blocks: per the Bedrock reference, Anthropic Claude and Amazon
/// Nova models. Other models get the images in the user turn instead.
fn supports_tool_result_images(model: &str) -> bool {
    let lowered = model.to_ascii_lowercase();
    lowered.contains("anthropic.claude") || lowered.contains("amazon.nova")
}

/// Build the Converse request body.
pub(crate) fn build_body(req: &ChatRequest) -> Result<Json, LlmError> {
    check_extra(req)?;
    let mut system_parts: Vec<String> = Vec::new();
    let mut turns: Vec<(&'static str, Vec<Json>)> = Vec::new();
    let mut push = |role: &'static str, blocks: Vec<Json>| {
        if blocks.is_empty() {
            return;
        }
        match turns.last_mut() {
            Some((last, existing)) if *last == role => existing.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    };
    let native_images = supports_tool_result_images(&req.model);
    let mut call_names: HashMap<&str, &str> = HashMap::new();
    // Fallback for other models: tool images go after the run of tool results.
    let mut deferred: Vec<Json> = Vec::new();
    for m in &req.messages {
        if m.role != Role::Tool && !deferred.is_empty() {
            push("user", std::mem::take(&mut deferred));
        }
        match m.role {
            Role::System => system_parts.push(m.text()),
            Role::User => push("user", content_blocks(&m.content)?),
            Role::Assistant => {
                for c in &m.tool_calls {
                    call_names.insert(c.id.as_str(), c.name.as_str());
                }
                let mut blocks = Vec::new();
                if let Some(Json::Array(state)) = m.provider_state.as_ref().and_then(|s| s.get(STATE_KEY)) {
                    blocks.extend(state.iter().cloned());
                }
                blocks.extend(content_blocks(&m.content)?);
                for c in &m.tool_calls {
                    let input = c.arguments_json().map_err(|e| LlmError::Config(e.to_string()))?;
                    blocks.push(json!({"toolUse": {"toolUseId": c.id, "name": c.name, "input": input}}));
                }
                push("assistant", blocks);
            }
            Role::Tool => {
                let id = m
                    .tool_call_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| LlmError::Config("tool message without tool_call_id".into()))?;
                let text = m.text();
                let mut content = match serde_json::from_str::<Json>(&text) {
                    Ok(value @ Json::Object(_)) => vec![json!({"json": value})],
                    _ => vec![json!({"text": text})],
                };
                let images: Vec<ContentPart> = m.images().cloned().collect();
                if !images.is_empty() {
                    if native_images {
                        content.extend(content_blocks(&images)?);
                    } else {
                        deferred.push(json!({"text": tool_images_intro(call_names.get(id).copied(), id)}));
                        deferred.extend(content_blocks(&images)?);
                    }
                }
                push("user", vec![json!({"toolResult": {"toolUseId": id, "content": content}})]);
            }
        }
    }
    if !deferred.is_empty() {
        push("user", deferred);
    }
    match &req.response_format {
        Some(ResponseFormat::JsonObject) => {
            system_parts.push("Respond with a single valid JSON object and nothing else.".to_owned());
        }
        Some(ResponseFormat::JsonSchema { schema, .. }) => system_parts.push(format!(
            "Respond with a single valid JSON value matching this JSON Schema and nothing else:\n{schema}"
        )),
        _ => {}
    }

    let mut body = Map::new();
    let messages: Vec<Json> =
        turns.into_iter().map(|(role, content)| json!({"role": role, "content": content})).collect();
    body.insert("messages".into(), Json::Array(messages));
    let system: Vec<Json> = system_parts.iter().filter(|s| !s.is_empty()).map(|s| json!({"text": s})).collect();
    if !system.is_empty() {
        body.insert("system".into(), Json::Array(system));
    }
    let mut inference = Map::new();
    if let Some(n) = req.max_tokens {
        inference.insert("maxTokens".into(), json!(n));
    }
    if let Some(t) = req.temperature {
        inference.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        inference.insert("topP".into(), json!(p));
    }
    if !req.stop.is_empty() {
        inference.insert("stopSequences".into(), json!(req.stop));
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".into(), Json::Object(inference));
    }
    if !req.tools.is_empty() {
        let tools: Vec<Json> = req
            .tools
            .iter()
            .map(|t| {
                json!({"toolSpec": {"name": t.name, "description": t.description, "inputSchema": {"json": t.parameters}}})
            })
            .collect();
        let mut tool_config = json!({"tools": tools});
        let choice = match &req.tool_choice {
            Some(ToolChoice::Auto) => Some(json!({"auto": {}})),
            Some(ToolChoice::Required) => Some(json!({"any": {}})),
            Some(ToolChoice::Tool(name)) => Some(json!({"tool": {"name": name}})),
            _ => None,
        };
        if let Some(choice) = choice
            && let Some(obj) = tool_config.as_object_mut()
        {
            obj.insert("toolChoice".into(), choice);
        }
        body.insert("toolConfig".into(), tool_config);
    }
    let mut additional = Map::new();
    for (k, v) in &req.extra {
        if k == "additionalModelRequestFields" {
            match v {
                Json::Object(fields) => additional.extend(fields.clone()),
                other => {
                    return Err(LlmError::Config(format!(
                        "`additionalModelRequestFields` must be a JSON object, got {other}"
                    )));
                }
            }
        } else if TOP_LEVEL_EXTRA.contains(&k.as_str()) {
            body.insert(k.clone(), v.clone());
        } else {
            additional.insert(k.clone(), v.clone());
        }
    }
    if !additional.is_empty() {
        body.insert("additionalModelRequestFields".into(), Json::Object(additional));
    }
    Ok(Json::Object(body))
}

fn stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "guardrail_intervened" | "content_filtered" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_owned()),
    }
}

/// Map an exception message.
fn exception(kind: &str, payload: &[u8]) -> LlmError {
    let message = serde_json::from_slice::<Json>(payload)
        .ok()
        .and_then(|v| v.get("message").or_else(|| v.get("Message")).and_then(Json::as_str).map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(payload).chars().take(1000).collect());
    match kind {
        "throttlingException" => LlmError::RateLimit { message, retry_after_ms: None },
        "accessDeniedException" => LlmError::Auth(message),
        "validationException" if message.to_ascii_lowercase().contains("too long") => LlmError::ContextLength(message),
        "validationException" => LlmError::BadRequest(message),
        "serviceUnavailableException" => LlmError::Server { status: 503, message },
        "modelTimeoutException" => LlmError::Network(message),
        _ => LlmError::Server { status: 500, message: format!("{kind}: {message}") },
    }
}

#[derive(Debug)]
enum Block {
    ToolUse { call_index: usize },
    Reasoning { text: String, signature: String, redacted: Option<String> },
}

/// Turns Converse stream messages into deltas.
#[derive(Debug, Default)]
pub(crate) struct BedrockDecoder {
    blocks: HashMap<u64, Block>,
    calls: usize,
    reasoning: Vec<Json>,
    stopped: bool,
}

/// Result of decoding one message.
#[derive(Debug, Default)]
pub(crate) struct Step {
    pub deltas: Vec<ChatDelta>,
    pub done: bool,
}

fn unknown_block(what: &str, index: u64) -> LlmError {
    LlmError::Decode(format!("`{what}` delta for content block {index}, which is not an open block of that kind"))
}

impl BedrockDecoder {
    fn state(&mut self) -> Option<ChatDelta> {
        if self.reasoning.is_empty() {
            return None;
        }
        Some(ChatDelta::ProviderState(json!({ STATE_KEY: std::mem::take(&mut self.reasoning) })))
    }

    pub fn decode(&mut self, message: &eventstream::Message) -> Result<Step, LlmError> {
        let message_type = message.header(":message-type").unwrap_or("event");
        if message_type != "event" {
            let kind = message.header(":exception-type").or_else(|| message.header(":error-code")).unwrap_or("error");
            return Err(exception(kind, &message.payload));
        }
        let event = message.header(":event-type").unwrap_or_default();
        let v: Json = if message.payload.is_empty() {
            Json::Null
        } else {
            serde_json::from_slice(&message.payload)
                .map_err(|e| LlmError::Decode(format!("invalid `{event}` payload: {e}")))?
        };
        let block_index = v.get("contentBlockIndex").and_then(Json::as_u64);
        let needs_index = matches!(event, "contentBlockStart" | "contentBlockDelta" | "contentBlockStop");
        if needs_index && block_index.is_none() {
            return Err(LlmError::Decode(format!("`{event}` event without `contentBlockIndex`")));
        }
        let index = block_index.unwrap_or(0);
        let mut out = Vec::new();
        match event {
            "contentBlockStart" => {
                if let Some(tool) = v.get("start").and_then(|s| s.get("toolUse")) {
                    let call_index = self.calls;
                    self.calls += 1;
                    out.push(ChatDelta::ToolCall {
                        index: call_index,
                        id: tool.get("toolUseId").and_then(Json::as_str).map(str::to_owned),
                        name: tool.get("name").and_then(Json::as_str).map(str::to_owned),
                        arguments: String::new(),
                    });
                    self.blocks.insert(index, Block::ToolUse { call_index });
                }
            }
            "contentBlockDelta" => {
                let delta = v.get("delta").cloned().unwrap_or(Json::Null);
                if let Some(text) = delta.get("text").and_then(Json::as_str) {
                    out.push(ChatDelta::Text(text.to_owned()));
                } else if let Some(tool) = delta.get("toolUse") {
                    let Some(Block::ToolUse { call_index }) = self.blocks.get(&index) else {
                        return Err(unknown_block("toolUse", index));
                    };
                    let input = match tool.get("input") {
                        Some(Json::String(s)) => s.clone(),
                        None | Some(Json::Null) => String::new(),
                        Some(other) => {
                            return Err(LlmError::Decode(format!("toolUse `input` delta is not a string: {other}")));
                        }
                    };
                    out.push(ChatDelta::ToolCall { index: *call_index, id: None, name: None, arguments: input });
                } else if let Some(reasoning) = delta.get("reasoningContent") {
                    let entry = self.blocks.entry(index).or_insert_with(|| Block::Reasoning {
                        text: String::new(),
                        signature: String::new(),
                        redacted: None,
                    });
                    let Block::Reasoning { text, signature, redacted } = entry else {
                        return Err(unknown_block("reasoningContent", index));
                    };
                    if let Some(t) = reasoning.get("text").and_then(Json::as_str) {
                        text.push_str(t);
                        out.push(ChatDelta::Reasoning(t.to_owned()));
                    }
                    if let Some(s) = reasoning.get("signature").and_then(Json::as_str) {
                        signature.push_str(s);
                    }
                    if let Some(r) = reasoning.get("redactedContent").and_then(Json::as_str) {
                        redacted.get_or_insert_with(String::new).push_str(r);
                    }
                }
            }
            "contentBlockStop" => {
                if let Some(Block::Reasoning { text, signature, redacted }) = self.blocks.remove(&index) {
                    match redacted {
                        Some(data) => self.reasoning.push(json!({"reasoningContent": {"redactedContent": data}})),
                        // Unsigned reasoning cannot be replayed (Bedrock rejects the next
                        // turn), so it is streamed but not kept for replay.
                        None if signature.is_empty() => {}
                        None => self.reasoning.push(
                            json!({"reasoningContent": {"reasoningText": {"text": text, "signature": signature}}}),
                        ),
                    }
                }
            }
            "messageStop" => {
                match v.get("stopReason").and_then(Json::as_str) {
                    Some(reason) => out.push(ChatDelta::Finish(stop_reason(reason))),
                    None => {
                        out.push(ChatDelta::Warning("Bedrock messageStop without stopReason".into()));
                        out.push(ChatDelta::Finish(FinishReason::Other("unknown".into())));
                    }
                }
                self.stopped = true;
            }
            "metadata" => {
                if let Some(usage) = v.get("usage") {
                    let get = |k: &str| usage.get(k).and_then(Json::as_u64).unwrap_or(0);
                    let (prompt, completion) = (get("inputTokens"), get("outputTokens"));
                    let total = match get("totalTokens") {
                        0 => prompt.saturating_add(completion),
                        t => t,
                    };
                    out.push(ChatDelta::Usage(Usage {
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        total_tokens: total,
                    }));
                }
                // Metadata is the last event of a response.
                out.extend(self.state());
                return Ok(Step { deltas: out, done: true });
            }
            _ => {} // messageStart and future event types
        }
        Ok(Step { deltas: out, done: false })
    }

    /// The body ended: fine after `messageStop` (metadata is optional), otherwise truncated.
    pub fn end_of_stream(&mut self) -> Result<Vec<ChatDelta>, LlmError> {
        if self.stopped { Ok(self.state().into_iter().collect()) } else { Err(crate::http::truncated()) }
    }
}

/// Stream the binary event-stream body.
fn event_stream(response: reqwest::Response) -> ChatStream {
    struct State {
        bytes: futures::stream::BoxStream<'static, Result<Vec<u8>, LlmError>>,
        frames: eventstream::Decoder,
        decoder: BedrockDecoder,
        pending: VecDeque<Result<ChatDelta, LlmError>>,
        finished: bool,
    }

    impl State {
        fn fail(&mut self, e: LlmError) {
            self.pending.push_back(Err(e));
            self.finished = true;
        }

        fn feed(&mut self, chunk: &[u8]) {
            let messages = match self.frames.push(chunk) {
                Ok(m) => m,
                Err(e) => return self.fail(e),
            };
            for message in messages {
                match self.decoder.decode(&message) {
                    Ok(step) => {
                        self.pending.extend(step.deltas.into_iter().map(Ok));
                        if step.done {
                            self.finished = true;
                            return;
                        }
                    }
                    Err(e) => return self.fail(e),
                }
            }
        }
    }

    let state = State {
        bytes: byte_stream(response),
        frames: eventstream::Decoder::default(),
        decoder: BedrockDecoder::default(),
        pending: VecDeque::new(),
        finished: false,
    };
    let stream = futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.pending.pop_front() {
                return Some((item, st));
            }
            if st.finished {
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => st.feed(&chunk),
                Some(Err(e)) => st.fail(e),
                None => {
                    if st.frames.has_partial() {
                        st.fail(crate::http::truncated());
                        continue;
                    }
                    match st.decoder.end_of_stream() {
                        Ok(deltas) => {
                            st.pending.extend(deltas.into_iter().map(Ok));
                            st.finished = true;
                        }
                        Err(e) => st.fail(e),
                    }
                }
            }
        }
    });
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::eventstream::encode;
    use super::*;
    use crate::types::{Message, ToolCall, collect};
    use graphrt_core::tool::ToolSpec;

    fn event(kind: &str, payload: Json) -> Vec<u8> {
        encode(
            &[(":event-type", kind), (":content-type", "application/json"), (":message-type", "event")],
            payload.to_string().as_bytes(),
        )
    }

    fn decode_all(bytes: &[u8]) -> Result<(Vec<ChatDelta>, bool), LlmError> {
        let mut frames = eventstream::Decoder::default();
        let mut decoder = BedrockDecoder::default();
        let mut out = Vec::new();
        for m in frames.push(bytes)? {
            let step = decoder.decode(&m)?;
            out.extend(step.deltas);
            if step.done {
                return Ok((out, true));
            }
        }
        Ok((out, false))
    }

    #[test]
    fn body_shape() {
        let mut assistant = Message::assistant("checking");
        assistant.tool_calls = vec![ToolCall { id: "tu1".into(), name: "get".into(), arguments: "{\"a\":1}".into() }];
        assistant.provider_state =
            Some(json!({STATE_KEY: [{"reasoningContent": {"reasoningText": {"text": "t", "signature": "s"}}}]}));
        let req = ChatRequest::new(
            "anthropic.claude-sonnet-4-5-v1:0",
            vec![
                Message::system("be brief"),
                Message {
                    role: Role::User,
                    content: vec![
                        ContentPart::Text { text: "hi".into() },
                        ContentPart::Image { url: "data:image/png;base64,AAAA".into(), detail: None },
                    ],
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_state: None,
                },
                assistant,
                Message::tool("tu1", "{\"v\":2}"),
                Message::tool("tu2", "plain"),
            ],
        )
        .with_tools(vec![ToolSpec {
            name: "get".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        }])
        .with_tool_choice(ToolChoice::Required)
        .with_max_tokens(256)
        .with_extra("thinking", json!({"type": "enabled", "budget_tokens": 1024}))
        .with_extra("guardrailConfig", json!({"guardrailIdentifier": "g"}));
        let body = build_body(&req).unwrap();
        assert_eq!(body["system"][0]["text"], "be brief");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["content"][1]["image"]["format"], "png");
        assert_eq!(msgs[0]["content"][1]["image"]["source"]["bytes"], "AAAA");
        assert_eq!(msgs[1]["content"][0]["reasoningContent"]["reasoningText"]["signature"], "s");
        assert_eq!(msgs[1]["content"][2]["toolUse"]["input"]["a"], 1);
        assert_eq!(msgs[2]["content"][0]["toolResult"]["content"][0]["json"]["v"], 2);
        assert_eq!(msgs[2]["content"][1]["toolResult"]["content"][0]["text"], "plain");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 256);
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"], "object");
        assert!(body["toolConfig"]["toolChoice"]["any"].is_object());
        assert_eq!(body["additionalModelRequestFields"]["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["guardrailConfig"]["guardrailIdentifier"], "g");
    }

    fn screenshot_history(model: &str) -> ChatRequest {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![ToolCall { id: "tu1".into(), name: "screenshot".into(), arguments: "{}".into() }];
        ChatRequest::new(
            model,
            vec![
                Message::user("look"),
                assistant,
                Message::tool_with_images("tu1", "1280x800", ["data:image/jpeg;base64,AAAA".to_owned()]),
            ],
        )
    }

    #[test]
    fn claude_and_nova_get_images_inside_the_tool_result() {
        for model in ["us.anthropic.claude-sonnet-4-5-v1:0", "amazon.nova-pro-v1:0"] {
            let body = build_body(&screenshot_history(model)).unwrap();
            assert_eq!(
                body["messages"][2],
                json!({"role": "user", "content": [{"toolResult": {"toolUseId": "tu1", "content": [
                    {"text": "1280x800"},
                    {"image": {"format": "jpeg", "source": {"bytes": "AAAA"}}}
                ]}}]}),
                "{model}"
            );
        }
    }

    #[test]
    fn other_models_get_images_after_the_tool_results() {
        let body = build_body(&screenshot_history("meta.llama3-2-90b-instruct-v1:0")).unwrap();
        assert_eq!(
            body["messages"][2],
            json!({"role": "user", "content": [
                {"toolResult": {"toolUseId": "tu1", "content": [{"text": "1280x800"}]}},
                {"text": "Images returned by tool `screenshot` (call tu1):"},
                {"image": {"format": "jpeg", "source": {"bytes": "AAAA"}}}
            ]})
        );
    }

    #[test]
    fn remote_images_are_rejected() {
        let req = ChatRequest::new(
            "m",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::Image { url: "https://x/cat.png".into(), detail: None }],
                tool_calls: vec![],
                tool_call_id: None,
                provider_state: None,
            }],
        );
        assert!(matches!(build_body(&req), Err(LlmError::Config(_))));
    }

    fn fixture() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(event("messageStart", json!({"role": "assistant"})));
        bytes.extend(event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "think"}}}),
        ));
        bytes.extend(event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sig"}}}),
        ));
        bytes.extend(event("contentBlockStop", json!({"contentBlockIndex": 0})));
        bytes.extend(event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"text": "Hel"}})));
        bytes.extend(event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"text": "lo"}})));
        bytes.extend(event("contentBlockStop", json!({"contentBlockIndex": 1})));
        bytes.extend(event(
            "contentBlockStart",
            json!({"contentBlockIndex": 2, "start": {"toolUse": {"toolUseId": "tu_1", "name": "get"}}}),
        ));
        bytes.extend(event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "{\"a\":"}}}),
        ));
        bytes
            .extend(event("contentBlockDelta", json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "2}"}}})));
        bytes.extend(event("contentBlockStop", json!({"contentBlockIndex": 2})));
        bytes.extend(event("messageStop", json!({"stopReason": "tool_use"})));
        bytes.extend(event(
            "metadata",
            json!({"usage": {"inputTokens": 10, "outputTokens": 20, "totalTokens": 30}, "metrics": {"latencyMs": 5}}),
        ));
        bytes
    }

    #[tokio::test]
    async fn decode_recorded_stream() {
        let (deltas, done) = decode_all(&fixture()).unwrap();
        assert!(done);
        let deltas: Vec<Result<ChatDelta, LlmError>> = deltas.into_iter().map(Ok).collect();
        let r = collect(Box::pin(futures::stream::iter(deltas)), |_| {}).await.unwrap();
        assert_eq!(r.text, "Hello");
        assert_eq!(r.reasoning, "think");
        assert_eq!(
            r.tool_calls,
            vec![ToolCall { id: "tu_1".into(), name: "get".into(), arguments: "{\"a\":2}".into() }]
        );
        assert_eq!(r.usage, Usage { prompt_tokens: 10, completion_tokens: 20, total_tokens: 30 });
        assert_eq!(r.finish_reason, Some(FinishReason::ToolCalls));
        let state = r.provider_state.clone().unwrap();
        assert_eq!(state[STATE_KEY][0]["reasoningContent"]["reasoningText"]["signature"], "sig");
        let body = build_body(&ChatRequest::new("m", vec![r.to_message()])).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["reasoningContent"]["reasoningText"]["text"], "think");
    }

    #[test]
    fn exceptions_and_truncation() {
        let throttle = encode(
            &[(":message-type", "exception"), (":exception-type", "throttlingException")],
            br#"{"message":"slow down"}"#,
        );
        assert!(matches!(decode_all(&throttle), Err(LlmError::RateLimit { .. })));
        let validation = encode(
            &[(":message-type", "exception"), (":exception-type", "validationException")],
            br#"{"message":"Input is too long for requested model."}"#,
        );
        assert!(matches!(decode_all(&validation), Err(LlmError::ContextLength(_))));
        let stray = event("contentBlockDelta", json!({"contentBlockIndex": 7, "delta": {"toolUse": {"input": "{}"}}}));
        assert!(matches!(decode_all(&stray), Err(LlmError::Decode(_))));

        let mut decoder = BedrockDecoder::default();
        assert!(decoder.end_of_stream().unwrap_err().is_retryable());
        decoder.stopped = true;
        assert!(decoder.end_of_stream().unwrap().is_empty());
    }

    #[test]
    fn http_error_classification() {
        assert!(matches!(classify(403, r#"{"message":"denied"}"#, None), LlmError::Auth(_)));
        assert!(matches!(
            classify(429, r#"{"message":"Too many requests"}"#, Some(1000)),
            LlmError::RateLimit { retry_after_ms: Some(1000), .. }
        ));
        assert!(matches!(classify(400, r#"{"message":"Input is too long"}"#, None), LlmError::ContextLength(_)));
        assert!(matches!(classify(424, r#"{"message":"model failed"}"#, None), LlmError::Server { status: 424, .. }));
    }

    #[test]
    fn config_validation_and_redaction() {
        assert!(BedrockModel::new(BedrockConfig::new("us-east-1/../x", BedrockAuth::ApiKey("k".into()))).is_err());
        let model =
            BedrockModel::new(BedrockConfig::new("eu-west-1", BedrockAuth::ApiKey("bedrock-secret".into()))).unwrap();
        assert!(!format!("{model:?}").contains("bedrock-secret"));
        assert_eq!(model.config.base_url(), "https://bedrock-runtime.eu-west-1.amazonaws.com");
    }

    #[tokio::test]
    async fn sigv4_request_headers() {
        let creds = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "secret".into(),
            session_token: Some("token".into()),
        };
        let model = BedrockModel::new(BedrockConfig::new("us-east-1", BedrockAuth::SigV4(creds))).unwrap();
        let req = model.request("/model/a%3A0/converse-stream", b"{}").await.unwrap().build().unwrap();
        let auth = req.headers().get("authorization").unwrap().to_str().unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"), "{auth}");
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token"));
        assert_eq!(req.headers().get("x-amz-security-token").unwrap(), "token");
    }

    #[tokio::test]
    async fn credential_base_url_is_honored_and_static_headers_stay_home() {
        use crate::credentials::{Credential, StaticCredential};
        let source = StaticCredential::from_credential(
            Credential::new("proxy-token").base_url("https://proxy.example.com/bedrock"),
        );
        let mut config =
            BedrockConfig::new("us-east-1", BedrockAuth::ApiKey("unused".into())).with_credentials(Arc::new(source));
        config.headers = vec![("x-gateway-secret".into(), "s".into())];
        let model = BedrockModel::new(config).unwrap();
        let req = model.request("/model/m/converse-stream", b"{}").await.unwrap().build().unwrap();
        assert_eq!(req.url().as_str(), "https://proxy.example.com/bedrock/model/m/converse-stream");
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer proxy-token");
        assert!(req.headers().get("x-gateway-secret").is_none());
    }

    #[test]
    fn non_object_additional_fields_are_rejected() {
        let mut req = ChatRequest::new("m", vec![Message::user("x")]);
        req.extra.insert("additionalModelRequestFields".into(), json!("{\"top_k\": 5}"));
        assert!(matches!(build_body(&req), Err(LlmError::Config(_))));
    }

    #[test]
    fn unsigned_reasoning_is_not_replayed_and_missing_stop_reason_is_flagged() {
        let mut bytes = Vec::new();
        bytes.extend(event("messageStart", json!({"role": "assistant"})));
        bytes.extend(event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "think"}}}),
        ));
        bytes.extend(event("contentBlockStop", json!({"contentBlockIndex": 0})));
        bytes.extend(event("messageStop", json!({})));
        bytes.extend(event("metadata", json!({"usage": {"inputTokens": 1, "outputTokens": 1}})));
        let (deltas, _) = decode_all(&bytes).unwrap();
        assert!(!deltas.iter().any(|d| matches!(d, ChatDelta::ProviderState(_))));
        assert!(deltas.iter().any(|d| matches!(d, ChatDelta::Warning(_))));
        assert!(deltas.iter().any(|d| matches!(d, ChatDelta::Finish(FinishReason::Other(r)) if r == "unknown")));
    }

    #[test]
    fn malformed_tool_input_and_missing_block_index_are_errors() {
        let mut bytes = event(
            "contentBlockStart",
            json!({"contentBlockIndex": 0, "start": {"toolUse": {"toolUseId": "t", "name": "f"}}}),
        );
        bytes.extend(event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"toolUse": {"input": {"a": 1}}}}),
        ));
        assert!(matches!(decode_all(&bytes), Err(LlmError::Decode(_))));
        let bytes = event("contentBlockDelta", json!({"delta": {"text": "x"}}));
        assert!(matches!(decode_all(&bytes), Err(LlmError::Decode(_))));
    }
}
