//! Google Gemini provider (`streamGenerateContent`, server-sent events).
//!
//! Works with the Gemini API (`generativelanguage.googleapis.com`, API key sent
//! in the `x-goog-api-key` header, never in the URL) and, by overriding
//! [`GeminiConfig::base_url`] and using [`GeminiAuth::Bearer`], with Vertex AI
//! (`https://<region>-aiplatform.googleapis.com/v1/projects/<p>/locations/<region>/publishers/google`),
//! where the bearer token usually comes from a [`CredentialSource`].
//!
//! Mapping notes:
//! - Gemini function calls carry no ids. The provider synthesizes stable ids
//!   (`gemini_call_<n>`, or the id Gemini sends when it does) and maps tool
//!   results back to function names through the conversation history.
//! - Thinking parts (`thought: true`) stream as reasoning. Parts carrying a
//!   `thoughtSignature` are returned as provider state and replayed verbatim on
//!   the next turn, as required for multi-turn function calling with thinking.
//! - [`ChatRequest::extra`] keys `safetySettings`, `cachedContent` and `labels`
//!   go to the request top level; every other key goes into `generationConfig`
//!   (for example `thinkingConfig`, `seed`, `responseModalities`).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::event::Usage;
use serde_json::{Map, Value as Json, json};

use crate::credentials::{Credential, CredentialSource};
use crate::error::LlmError;
use crate::http::{Decoded, SseDecoder, build_client, send_request, sse_stream};
use crate::sse::SseEvent;
use crate::types::{
    ChatDelta, ChatModel, ChatRequest, ChatStream, ContentPart, FinishReason, ResponseFormat, Role, ToolChoice,
    check_extra,
};

/// Default Gemini API base URL.
pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Key under which Gemini parts are stored in [`crate::types::Message::provider_state`].
const STATE_KEY: &str = "gemini_parts";

/// `extra` keys placed at the request top level instead of `generationConfig`.
const TOP_LEVEL_EXTRA: &[&str] = &["safetySettings", "cachedContent", "labels"];

/// How the credential is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeminiAuth {
    /// `x-goog-api-key: <key>` (Gemini API).
    #[default]
    ApiKey,
    /// `Authorization: Bearer <token>` (Vertex AI, OAuth access tokens).
    Bearer,
}

/// Configuration of a Gemini endpoint.
#[derive(Clone)]
pub struct GeminiConfig {
    /// Base URL; requests go to `<base_url>/models/<model>:streamGenerateContent?alt=sse`.
    pub base_url: String,
    /// Static API key or token.
    pub api_key: Option<String>,
    pub auth: GeminiAuth,
    /// Dynamic credentials; when set they replace `api_key`. On HTTP 401/403 the
    /// source is invalidated and the request retried once.
    pub credentials: Option<Arc<dyn CredentialSource>>,
    /// Provider name passed to the credential source.
    pub provider_name: String,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
    pub connect_timeout: Duration,
}

impl fmt::Debug for GeminiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("auth", &self.auth)
            .field("credentials", &self.credentials)
            .field("provider_name", &self.provider_name)
            .field("headers", &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            base_url: GEMINI_BASE_URL.to_owned(),
            api_key: None,
            auth: GeminiAuth::ApiKey,
            credentials: None,
            provider_name: "gemini".to_owned(),
            headers: Vec::new(),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

impl GeminiConfig {
    /// Gemini API with a static API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: Some(api_key.into()), ..Self::default() }
    }

    /// Use a credential source instead of a static key.
    pub fn with_credentials(mut self, source: Arc<dyn CredentialSource>) -> Self {
        self.credentials = Some(source);
        self
    }
}

/// Gemini chat model endpoint.
#[derive(Clone)]
pub struct GeminiModel {
    config: GeminiConfig,
    client: reqwest::Client,
}

impl fmt::Debug for GeminiModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiModel").field("config", &self.config).finish_non_exhaustive()
    }
}

impl GeminiModel {
    pub fn new(config: GeminiConfig) -> Result<Self, LlmError> {
        let client = build_client(config.connect_timeout)?;
        Ok(Self { config, client })
    }

    fn endpoint(&self, model: &str) -> String {
        let model = model.strip_prefix("models/").unwrap_or(model);
        format!("{}/models/{}:streamGenerateContent?alt=sse", self.config.base_url.trim_end_matches('/'), encode(model))
    }

    async fn credential(&self) -> Result<Credential, LlmError> {
        if let Some(source) = &self.config.credentials {
            return source.credential(&self.config.provider_name).await;
        }
        match &self.config.api_key {
            Some(key) => Ok(Credential::new(key.clone())),
            None => Err(LlmError::Config(format!("provider `{}` has no API key", self.config.provider_name))),
        }
    }
}

impl ChatModel for GeminiModel {
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        Box::pin(async move {
            let body = build_body(&request)?;
            let url = self.endpoint(&request.model);
            let mut retried = false;
            loop {
                let credential = self.credential().await?;
                let mut builder = self.client.post(&url).json(&body);
                builder = match self.config.auth {
                    GeminiAuth::ApiKey => builder.header("x-goog-api-key", credential.secret.as_str()),
                    GeminiAuth::Bearer => builder.bearer_auth(&credential.secret),
                };
                for (k, v) in self.config.headers.iter().chain(credential.headers.iter()) {
                    builder = builder.header(k.as_str(), v.as_str());
                }
                match send_request(builder, classify).await {
                    Ok(response) => return Ok(sse_stream(response, GeminiDecoder::default())),
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

/// Percent-encode a path segment.
fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Classify an HTTP error response, reading Google's `RetryInfo` delay.
fn classify(status: u16, body: &str, retry_after_ms: Option<u64>) -> LlmError {
    let retry = retry_after_ms.or_else(|| retry_delay_ms(body));
    if status == 400 && (body.contains("API_KEY_INVALID") || body.contains("API key not valid")) {
        return LlmError::Auth("API key not valid".into());
    }
    LlmError::from_status(status, body, retry)
}

/// `error.details[].retryDelay` (e.g. `"17s"`, `"0.5s"`) in milliseconds.
fn retry_delay_ms(body: &str) -> Option<u64> {
    let v: Json = serde_json::from_str(body).ok()?;
    let details = v.get("error")?.get("details")?.as_array()?;
    details.iter().find_map(|d| d.get("retryDelay").and_then(Json::as_str).and_then(parse_duration_ms))
}

fn parse_duration_ms(text: &str) -> Option<u64> {
    let seconds: f64 = text.trim().strip_suffix('s')?.parse().ok()?;
    if seconds.is_finite() && seconds >= 0.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((seconds * 1000.0) as u64)
    } else {
        None
    }
}

/// Split a `data:<mime>;base64,<data>` URL.
fn data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    Some((mime, data))
}

/// Guess an image media type from a URL path.
fn guess_mime(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".heic") {
        "image/heic"
    } else if path.ends_with(".pdf") {
        "application/pdf"
    } else {
        "image/jpeg"
    }
}

fn content_parts(parts: &[ContentPart]) -> Result<Vec<Json>, LlmError> {
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            ContentPart::Text { text } if text.is_empty() => {}
            ContentPart::Text { text } => out.push(json!({"text": text})),
            ContentPart::Image { url, .. } => {
                if let Some((mime, data)) = data_url(url) {
                    out.push(json!({"inlineData": {"mimeType": mime, "data": data}}));
                } else if url.starts_with("data:") {
                    return Err(LlmError::Config("image data URLs must be base64 encoded".into()));
                } else {
                    out.push(json!({"fileData": {"mimeType": guess_mime(url), "fileUri": url}}));
                }
            }
        }
    }
    Ok(out)
}

/// Function response payload: a JSON object result is sent as-is, anything else
/// is wrapped as `{"result": ...}`.
fn function_response(text: &str) -> Json {
    match serde_json::from_str::<Json>(text) {
        Ok(Json::Object(map)) => Json::Object(map),
        Ok(other) => json!({"result": other}),
        Err(_) => json!({"result": text}),
    }
}

/// Build the JSON request body.
pub(crate) fn build_body(req: &ChatRequest) -> Result<Json, LlmError> {
    check_extra(req)?;
    let mut system_parts: Vec<String> = Vec::new();
    let mut turns: Vec<(&'static str, Vec<Json>)> = Vec::new();
    let mut push = |role: &'static str, parts: Vec<Json>| {
        if parts.is_empty() {
            return;
        }
        match turns.last_mut() {
            Some((last, existing)) if *last == role => existing.extend(parts),
            _ => turns.push((role, parts)),
        }
    };
    // Tool call id -> function name, from earlier assistant messages.
    let mut call_names: HashMap<&str, &str> = HashMap::new();
    for m in &req.messages {
        match m.role {
            Role::System => system_parts.push(m.text()),
            Role::User => push("user", content_parts(&m.content)?),
            Role::Assistant => {
                for c in &m.tool_calls {
                    call_names.insert(c.id.as_str(), c.name.as_str());
                }
                let replay = m
                    .provider_state
                    .as_ref()
                    .and_then(|s| s.get(STATE_KEY))
                    .and_then(Json::as_array)
                    .filter(|parts| !parts.is_empty());
                if let Some(parts) = replay {
                    push("model", parts.clone());
                    continue;
                }
                let mut parts = content_parts(&m.content)?;
                for c in &m.tool_calls {
                    let args = c.arguments_json().map_err(|e| LlmError::Config(e.to_string()))?;
                    parts.push(json!({"functionCall": {"name": c.name, "args": args}}));
                }
                push("model", parts);
            }
            Role::Tool => {
                let id = m
                    .tool_call_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| LlmError::Config("tool message without tool_call_id".into()))?;
                let name = call_names.get(id).copied().ok_or_else(|| {
                    LlmError::Config(format!("tool result `{id}` does not answer any earlier tool call"))
                })?;
                push(
                    "user",
                    vec![json!({"functionResponse": {"name": name, "response": function_response(&m.text())}})],
                );
            }
        }
    }

    let mut body = Map::new();
    let system: Vec<&str> = system_parts.iter().map(String::as_str).filter(|s| !s.is_empty()).collect();
    if !system.is_empty() {
        body.insert("systemInstruction".into(), json!({"parts": [{"text": system.join("\n\n")}]}));
    }
    let contents: Vec<Json> = turns.into_iter().map(|(role, parts)| json!({"role": role, "parts": parts})).collect();
    body.insert("contents".into(), Json::Array(contents));

    if !req.tools.is_empty() {
        let declarations: Vec<Json> = req
            .tools
            .iter()
            .map(|t| {
                let mut decl = json!({"name": t.name, "description": t.description});
                let has_params =
                    t.parameters.get("properties").and_then(Json::as_object).is_some_and(|p| !p.is_empty());
                if has_params {
                    if let Some(obj) = decl.as_object_mut() {
                        obj.insert("parameters".into(), to_gemini_schema(&t.parameters));
                    }
                }
                decl
            })
            .collect();
        body.insert("tools".into(), json!([{"functionDeclarations": declarations}]));
    }
    if let Some(choice) = &req.tool_choice {
        let config = match choice {
            ToolChoice::Auto => json!({"mode": "AUTO"}),
            ToolChoice::None => json!({"mode": "NONE"}),
            ToolChoice::Required => json!({"mode": "ANY"}),
            ToolChoice::Tool(name) => json!({"mode": "ANY", "allowedFunctionNames": [name]}),
        };
        body.insert("toolConfig".into(), json!({"functionCallingConfig": config}));
    }

    let mut generation = Map::new();
    if let Some(t) = req.temperature {
        generation.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        generation.insert("topP".into(), json!(p));
    }
    if let Some(n) = req.max_tokens {
        generation.insert("maxOutputTokens".into(), json!(n));
    }
    if !req.stop.is_empty() {
        generation.insert("stopSequences".into(), json!(req.stop));
    }
    match &req.response_format {
        Some(ResponseFormat::JsonObject) => {
            generation.insert("responseMimeType".into(), json!("application/json"));
        }
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            generation.insert("responseMimeType".into(), json!("application/json"));
            generation.insert("responseSchema".into(), to_gemini_schema(schema));
        }
        _ => {}
    }
    for (k, v) in &req.extra {
        if TOP_LEVEL_EXTRA.contains(&k.as_str()) {
            body.insert(k.clone(), v.clone());
        } else {
            generation.insert(k.clone(), v.clone());
        }
    }
    if !generation.is_empty() {
        body.insert("generationConfig".into(), Json::Object(generation));
    }
    Ok(Json::Object(body))
}

/// String formats Gemini accepts; others are rejected by the API and are dropped.
const SUPPORTED_FORMATS: &[&str] = &["date-time", "enum", "int32", "int64", "float", "double"];

/// Keywords copied verbatim when present.
const COPIED_KEYWORDS: &[&str] = &[
    "title",
    "description",
    "pattern",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "minProperties",
    "maxProperties",
    "propertyOrdering",
];

/// Nesting limit for schema conversion (guards against recursive `$ref`s).
const MAX_SCHEMA_DEPTH: usize = 24;

/// Convert a JSON Schema into Gemini's OpenAPI-style schema subset: upper-case
/// types, `nullable` instead of `"null"` types, local `$ref`s inlined,
/// `const` turned into a one-value `enum`, and unsupported keywords
/// (`additionalProperties`, `$schema`, unknown formats, ...) dropped.
pub fn to_gemini_schema(schema: &Json) -> Json {
    convert_schema(schema, schema, 0)
}

fn resolve_ref<'a>(root: &'a Json, reference: &str) -> Option<&'a Json> {
    let path = reference.strip_prefix("#/")?;
    let mut current = root;
    for segment in path.split('/') {
        current = current.get(segment.replace("~1", "/").replace("~0", "~"))?;
    }
    Some(current)
}

fn gemini_type(t: &str) -> Option<&'static str> {
    Some(match t {
        "string" => "STRING",
        "number" => "NUMBER",
        "integer" => "INTEGER",
        "boolean" => "BOOLEAN",
        "array" => "ARRAY",
        "object" => "OBJECT",
        _ => return None,
    })
}

fn convert_schema(schema: &Json, root: &Json, depth: usize) -> Json {
    let Some(obj) = schema.as_object() else {
        return json!({});
    };
    if depth > MAX_SCHEMA_DEPTH {
        return json!({});
    }
    if let Some(target) = obj.get("$ref").and_then(Json::as_str).and_then(|r| resolve_ref(root, r)) {
        return convert_schema(target, root, depth + 1);
    }
    let mut out = Map::new();
    let mut nullable = obj.get("nullable").and_then(Json::as_bool).unwrap_or(false);
    match obj.get("type") {
        Some(Json::String(t)) => {
            if let Some(g) = gemini_type(t) {
                out.insert("type".into(), json!(g));
            }
        }
        Some(Json::Array(types)) => {
            let names: Vec<&str> = types.iter().filter_map(Json::as_str).collect();
            nullable |= names.contains(&"null");
            if let Some(g) = names.iter().filter(|t| **t != "null").find_map(|t| gemini_type(t)) {
                out.insert("type".into(), json!(g));
            }
        }
        _ => {}
    }
    for variants_key in ["anyOf", "oneOf"] {
        if let Some(variants) = obj.get(variants_key).and_then(Json::as_array) {
            let non_null: Vec<&Json> =
                variants.iter().filter(|v| v.get("type").and_then(Json::as_str) != Some("null")).collect();
            nullable |= non_null.len() < variants.len();
            if let [only] = non_null.as_slice() {
                // `anyOf: [X, {type: null}]` is just a nullable X.
                if let Json::Object(inner) = convert_schema(only, root, depth + 1) {
                    for (k, v) in inner {
                        out.entry(k).or_insert(v);
                    }
                }
            } else if !non_null.is_empty() {
                let converted: Vec<Json> = non_null.iter().map(|v| convert_schema(v, root, depth + 1)).collect();
                out.insert("anyOf".into(), Json::Array(converted));
            }
        }
    }
    if let Some(format) = obj.get("format").and_then(Json::as_str).filter(|f| SUPPORTED_FORMATS.contains(f)) {
        out.insert("format".into(), json!(format));
    }
    for key in COPIED_KEYWORDS {
        if let Some(v) = obj.get(*key) {
            out.insert((*key).to_owned(), v.clone());
        }
    }
    if let Some(Json::Array(values)) = obj.get("enum") {
        if values.iter().all(Json::is_string) {
            out.insert("enum".into(), Json::Array(values.clone()));
            out.entry("type").or_insert_with(|| json!("STRING"));
        }
    } else if let Some(Json::String(value)) = obj.get("const") {
        out.insert("enum".into(), json!([value]));
        out.entry("type").or_insert_with(|| json!("STRING"));
    }
    if let Some(props) = obj.get("properties").and_then(Json::as_object) {
        let converted: Map<String, Json> =
            props.iter().map(|(k, v)| (k.clone(), convert_schema(v, root, depth + 1))).collect();
        if let Some(required) = obj.get("required").and_then(Json::as_array) {
            let required: Vec<Json> = required
                .iter()
                .filter(|r| r.as_str().is_some_and(|name| converted.contains_key(name)))
                .cloned()
                .collect();
            if !required.is_empty() {
                out.insert("required".into(), Json::Array(required));
            }
        }
        out.insert("properties".into(), Json::Object(converted));
        out.entry("type").or_insert_with(|| json!("OBJECT"));
    }
    if let Some(items) = obj.get("items") {
        out.insert("items".into(), convert_schema(items, root, depth + 1));
        out.entry("type").or_insert_with(|| json!("ARRAY"));
    }
    if nullable {
        out.insert("nullable".into(), json!(true));
    }
    Json::Object(out)
}

fn finish_reason(reason: &str, had_calls: bool) -> FinishReason {
    match reason {
        "STOP" if had_calls => FinishReason::ToolCalls,
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY"
        | "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_ascii_lowercase()),
    }
}

/// Map an in-stream `{"error": {...}}` object.
fn stream_error(err: &Json) -> LlmError {
    let message = err.get("message").and_then(Json::as_str).unwrap_or("stream error").to_owned();
    let code = err.get("code").and_then(Json::as_u64).and_then(|c| u16::try_from(c).ok()).unwrap_or(500);
    match err.get("status").and_then(Json::as_str).unwrap_or_default() {
        "RESOURCE_EXHAUSTED" => LlmError::RateLimit { message, retry_after_ms: None },
        "UNAUTHENTICATED" | "PERMISSION_DENIED" => LlmError::Auth(message),
        "UNAVAILABLE" | "INTERNAL" | "DEADLINE_EXCEEDED" => LlmError::Server { status: code.max(500), message },
        _ if code >= 500 => LlmError::Server { status: code, message },
        _ => LlmError::BadRequest(message),
    }
}

/// Decodes `streamGenerateContent` SSE chunks.
#[derive(Debug, Default)]
pub(crate) struct GeminiDecoder {
    calls: usize,
    finished: bool,
    /// Model parts kept for replay (text merged; signed parts verbatim).
    parts: Vec<Json>,
    signed: bool,
}

impl GeminiDecoder {
    fn record(&mut self, part: &Json) {
        let has_signature = part.get("thoughtSignature").is_some();
        self.signed |= has_signature;
        let is_thought = part.get("thought").and_then(Json::as_bool).unwrap_or(false);
        if is_thought && !has_signature {
            return;
        }
        if !has_signature && part.get("functionCall").is_none() {
            if let Some(text) = part.get("text").and_then(Json::as_str) {
                let last_plain = self
                    .parts
                    .last_mut()
                    .filter(|p| p.get("thoughtSignature").is_none() && p.get("thought").is_none())
                    .and_then(|p| p.get_mut("text"));
                if let Some(Json::String(existing)) = last_plain {
                    existing.push_str(text);
                    return;
                }
            }
        }
        self.parts.push(part.clone());
    }

    fn state(&mut self) -> Option<ChatDelta> {
        if !self.signed || self.parts.is_empty() {
            return None;
        }
        Some(ChatDelta::ProviderState(json!({ STATE_KEY: std::mem::take(&mut self.parts) })))
    }
}

impl SseDecoder for GeminiDecoder {
    fn decode(&mut self, event: SseEvent) -> Result<Decoded, LlmError> {
        if event.data.trim().is_empty() {
            return Ok(Decoded::default());
        }
        let v: Json =
            serde_json::from_str(&event.data).map_err(|e| LlmError::Decode(format!("{e}: {}", event.data)))?;
        if let Some(err) = v.get("error") {
            return Err(stream_error(err));
        }
        let mut out = Vec::new();
        let candidate = v.get("candidates").and_then(Json::as_array).and_then(|c| c.first());
        if candidate.is_none() {
            if let Some(reason) = v.get("promptFeedback").and_then(|f| f.get("blockReason")).and_then(Json::as_str) {
                return Err(LlmError::BadRequest(format!(
                    "prompt blocked by the provider's safety filters ({reason})"
                )));
            }
        }
        if let Some(candidate) = candidate {
            let parts = candidate.get("content").and_then(|c| c.get("parts")).and_then(Json::as_array);
            for part in parts.into_iter().flatten() {
                self.record(part);
                if let Some(call) = part.get("functionCall") {
                    let index = self.calls;
                    self.calls += 1;
                    let name = call.get("name").and_then(Json::as_str).unwrap_or_default().to_owned();
                    let id = call
                        .get("id")
                        .and_then(Json::as_str)
                        .filter(|id| !id.is_empty())
                        .map_or_else(|| format!("gemini_call_{index}"), str::to_owned);
                    let arguments = match call.get("args") {
                        Some(args @ Json::Object(_)) => args.to_string(),
                        None | Some(Json::Null) => "{}".to_owned(),
                        Some(other) => other.to_string(),
                    };
                    out.push(ChatDelta::ToolCall { index, id: Some(id), name: Some(name), arguments });
                    continue;
                }
                let Some(text) = part.get("text").and_then(Json::as_str).filter(|t| !t.is_empty()) else {
                    continue;
                };
                if part.get("thought").and_then(Json::as_bool).unwrap_or(false) {
                    out.push(ChatDelta::Reasoning(text.to_owned()));
                } else {
                    out.push(ChatDelta::Text(text.to_owned()));
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Json::as_str) {
                out.push(ChatDelta::Finish(finish_reason(reason, self.calls > 0)));
                self.finished = true;
            }
        }
        if let Some(usage) = v.get("usageMetadata") {
            let get = |k: &str| usage.get(k).and_then(Json::as_u64).unwrap_or(0);
            let prompt = get("promptTokenCount");
            let completion = get("candidatesTokenCount").saturating_add(get("thoughtsTokenCount"));
            let total = match get("totalTokenCount") {
                0 => prompt.saturating_add(completion),
                t => t,
            };
            out.push(ChatDelta::Usage(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
            }));
        }
        if self.finished {
            out.extend(self.state());
            return Ok(Decoded::done(out));
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

    fn decode_all(fixture: &str) -> Result<(Vec<ChatDelta>, bool), LlmError> {
        let mut parser = SseParser::new();
        let mut decoder = GeminiDecoder::default();
        let mut deltas = Vec::new();
        for ev in parser.push(fixture.as_bytes()).unwrap() {
            let d = decoder.decode(ev)?;
            deltas.extend(d.deltas);
            if d.done {
                return Ok((deltas, true));
            }
        }
        Ok((deltas, false))
    }

    #[test]
    fn body_shape() {
        let mut assistant = Message::assistant("checking");
        assistant.tool_calls =
            vec![ToolCall { id: "c1".into(), name: "weather".into(), arguments: "{\"city\":\"Oslo\"}".into() }];
        let mut req = ChatRequest::new(
            "gemini-2.5-flash",
            vec![
                Message::system("be brief"),
                Message {
                    role: Role::User,
                    content: vec![
                        ContentPart::Text { text: "hi".into() },
                        ContentPart::Image { url: "data:image/png;base64,AAAA".into(), detail: None },
                        ContentPart::Image { url: "https://x.test/cat.webp".into(), detail: None },
                    ],
                    tool_calls: vec![],
                    tool_call_id: None,
                    provider_state: None,
                },
                assistant,
                Message::tool("c1", "{\"temp\": 3}"),
            ],
        )
        .with_tools(vec![ToolSpec {
            name: "weather".into(),
            description: "d".into(),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}),
        }])
        .with_tool_choice(ToolChoice::Tool("weather".into()))
        .with_temperature(0.2)
        .with_max_tokens(100)
        .with_stop(vec!["END".into()])
        .with_extra("thinkingConfig", json!({"thinkingBudget": 0}))
        .with_extra("safetySettings", json!([]));
        req.response_format = Some(ResponseFormat::JsonObject);
        let body = build_body(&req).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["parts"][1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(contents[0]["parts"][2]["fileData"]["mimeType"], "image/webp");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][1]["functionCall"]["args"]["city"], "Oslo");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "weather");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["temp"], 3);
        let decl = &body["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["parameters"]["type"], "OBJECT");
        assert!(decl["parameters"].get("additionalProperties").is_none());
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0], "weather");
        let gen_cfg = &body["generationConfig"];
        assert_eq!(gen_cfg["maxOutputTokens"], 100);
        assert_eq!(gen_cfg["stopSequences"][0], "END");
        assert_eq!(gen_cfg["responseMimeType"], "application/json");
        assert_eq!(gen_cfg["thinkingConfig"]["thinkingBudget"], 0);
        assert!(body["safetySettings"].is_array());
    }

    #[test]
    fn tool_result_for_unknown_call_is_an_error() {
        let req = ChatRequest::new("m", vec![Message::user("x"), Message::tool("nope", "r")]);
        assert!(matches!(build_body(&req), Err(LlmError::Config(_))));
    }

    #[test]
    fn schema_conversion() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "$defs": {"city": {"type": "string", "format": "uri", "description": "City"}},
            "properties": {
                "city": {"$ref": "#/$defs/city"},
                "days": {"type": ["integer", "null"], "minimum": 1},
                "unit": {"enum": ["c", "f"]},
                "kind": {"const": "forecast"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "when": {"type": "string", "format": "date-time"},
                "maybe": {"anyOf": [{"type": "number"}, {"type": "null"}]}
            },
            "required": ["city", "ghost"]
        });
        let g = to_gemini_schema(&schema);
        assert_eq!(g["type"], "OBJECT");
        assert!(g.get("$schema").is_none() && g.get("additionalProperties").is_none());
        let p = &g["properties"];
        assert_eq!(p["city"]["type"], "STRING");
        assert!(p["city"].get("format").is_none(), "unsupported formats are dropped");
        assert_eq!(p["city"]["description"], "City");
        assert_eq!(p["days"]["type"], "INTEGER");
        assert_eq!(p["days"]["nullable"], true);
        assert_eq!(p["unit"]["enum"], json!(["c", "f"]));
        assert_eq!(p["kind"]["enum"], json!(["forecast"]));
        assert_eq!(p["tags"]["items"]["type"], "STRING");
        assert_eq!(p["when"]["format"], "date-time");
        assert_eq!(p["maybe"]["type"], "NUMBER");
        assert_eq!(p["maybe"]["nullable"], true);
        assert_eq!(g["required"], json!(["city"]));
        // Recursive references terminate.
        let recursive = json!({"$defs": {"n": {"type": "object", "properties": {"next": {"$ref": "#/$defs/n"}}}}, "$ref": "#/$defs/n"});
        assert_eq!(to_gemini_schema(&recursive)["type"], "OBJECT");
    }

    const FIXTURE: &str = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"plan\",\"thought\":true}]}}]}\r\n\r\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hel\"}]}}],\"usageMetadata\":{\"promptTokenCount\":5}}\r\n\r\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"lo\"},{\"functionCall\":{\"name\":\"weather\",\"args\":{\"city\":\"Oslo\"}},\"thoughtSignature\":\"sig1\"},{\"functionCall\":{\"name\":\"time\",\"args\":{}}}]},\"finishReason\":\"STOP\"}],",
        "\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":7,\"thoughtsTokenCount\":3,\"totalTokenCount\":15}}\r\n\r\n",
    );

    #[tokio::test]
    async fn decode_recorded_stream_and_replay_signatures() {
        let (deltas, done) = decode_all(FIXTURE).unwrap();
        assert!(done);
        let deltas: Vec<Result<ChatDelta, LlmError>> = deltas.into_iter().map(Ok).collect();
        let r = collect(Box::pin(futures::stream::iter(deltas)), |_| {}).await.unwrap();
        assert_eq!(r.text, "Hello");
        assert_eq!(r.reasoning, "plan");
        assert_eq!(r.tool_calls.len(), 2);
        assert_eq!(r.tool_calls[0].id, "gemini_call_0");
        assert_eq!(r.tool_calls[0].arguments_json().unwrap()["city"], "Oslo");
        assert_eq!(r.tool_calls[1].name, "time");
        assert_eq!(r.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(r.usage, Usage { prompt_tokens: 5, completion_tokens: 10, total_tokens: 15 });
        let state = r.provider_state.clone().unwrap();
        let parts = state[STATE_KEY].as_array().unwrap();
        assert_eq!(parts[0]["text"], "Hello");
        assert_eq!(parts[1]["thoughtSignature"], "sig1");

        // Next turn: the signed parts are replayed and tool results map back by id.
        let next = ChatRequest::new(
            "m",
            vec![
                Message::user("q"),
                r.to_message(),
                Message::tool("gemini_call_0", "sunny"),
                Message::tool("gemini_call_1", "{\"t\":\"noon\"}"),
            ],
        );
        let body = build_body(&next).unwrap();
        assert_eq!(body["contents"][1]["parts"][1]["thoughtSignature"], "sig1");
        let responses = body["contents"][2]["parts"].as_array().unwrap();
        assert_eq!(responses[0]["functionResponse"]["name"], "weather");
        assert_eq!(responses[0]["functionResponse"]["response"]["result"], "sunny");
        assert_eq!(responses[1]["functionResponse"]["response"]["t"], "noon");
    }

    #[test]
    fn stream_without_finish_reason_is_truncated() {
        let cut = FIXTURE
            .split("data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"lo\"")
            .next()
            .unwrap();
        let (_, done) = decode_all(cut).unwrap();
        assert!(!done);
        let err = GeminiDecoder::default().end_of_stream().unwrap_err();
        assert!(err.is_retryable());
    }

    #[test]
    fn safety_and_errors() {
        let blocked = "data: {\"promptFeedback\":{\"blockReason\":\"SAFETY\"}}\n\n";
        let err = decode_all(blocked).unwrap_err();
        assert!(matches!(err, LlmError::BadRequest(ref m) if m.contains("SAFETY")), "{err:?}");
        let filtered = "data: {\"candidates\":[{\"finishReason\":\"SAFETY\"}]}\n\n";
        let (deltas, done) = decode_all(filtered).unwrap();
        assert!(done);
        assert_eq!(deltas, vec![ChatDelta::Finish(FinishReason::ContentFilter)]);
        let in_stream = "data: {\"error\":{\"code\":429,\"status\":\"RESOURCE_EXHAUSTED\",\"message\":\"quota\"}}\n\n";
        assert!(matches!(decode_all(in_stream), Err(LlmError::RateLimit { .. })));
    }

    #[test]
    fn http_error_classification() {
        let body = r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED",
            "details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"17s"}]}}"#;
        assert_eq!(
            classify(429, body, None),
            LlmError::RateLimit { message: "Quota exceeded".into(), retry_after_ms: Some(17_000) }
        );
        let bad_key = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}"#;
        assert!(matches!(classify(400, bad_key, None), LlmError::Auth(_)));
        assert!(matches!(classify(403, r#"{"error":{"message":"denied"}}"#, None), LlmError::Auth(_)));
        assert!(matches!(classify(400, r#"{"error":{"message":"bad"}}"#, None), LlmError::BadRequest(_)));
        assert_eq!(parse_duration_ms("0.5s"), Some(500));
    }

    #[test]
    fn endpoint_and_debug() {
        let model = GeminiModel::new(GeminiConfig::new("AIza-secret")).unwrap();
        assert_eq!(
            model.endpoint("models/gemini-2.5-flash"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert!(!format!("{model:?}").contains("AIza-secret"));
    }
}
