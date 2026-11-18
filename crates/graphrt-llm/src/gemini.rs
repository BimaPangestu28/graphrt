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
use crate::http::{Decoded, SseDecoder, send_request, sse_stream};
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
    client: crate::http::LazyClient,
}

impl fmt::Debug for GeminiModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiModel").field("config", &self.config).finish_non_exhaustive()
    }
}

impl GeminiModel {
    pub fn new(config: GeminiConfig) -> Result<Self, LlmError> {
        let client = crate::http::LazyClient::new(config.connect_timeout);
        Ok(Self { config, client })
    }

    fn endpoint(base_url: &str, model: &str) -> String {
        let model = model.strip_prefix("models/").unwrap_or(model);
        format!("{}/models/{}:streamGenerateContent?alt=sse", base_url.trim_end_matches('/'), encode(model))
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
            let (body, warnings) = build_body_with_warnings(&request)?;
            let mut retried = false;
            loop {
                let credential = self.credential().await?;
                // A credential may move the request to a proxy (thin-proxy pattern).
                let endpoint = crate::http::resolve_endpoint(&self.config.base_url, credential.base_url.as_deref())?;
                let url = Self::endpoint(&endpoint.base_url, &request.model);
                let mut builder = self.client.get()?.post(&url).json(&body);
                builder = match self.config.auth {
                    GeminiAuth::ApiKey => builder.header("x-goog-api-key", credential.secret.as_str()),
                    GeminiAuth::Bearer => builder.bearer_auth(&credential.secret),
                };
                if endpoint.send_static {
                    for (k, v) in &self.config.headers {
                        builder = builder.header(k.as_str(), v.as_str());
                    }
                }
                for (k, v) in &credential.headers {
                    builder = builder.header(k.as_str(), v.as_str());
                }
                match send_request(builder, classify).await {
                    Ok(response) => {
                        let stream = sse_stream(response, GeminiDecoder::default());
                        if warnings.is_empty() {
                            return Ok(stream);
                        }
                        let head: Vec<Result<ChatDelta, LlmError>> =
                            warnings.iter().cloned().map(|w| Ok(ChatDelta::Warning(w))).collect();
                        return Ok(Box::pin(futures::StreamExt::chain(futures::stream::iter(head), stream)));
                    }
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

/// Prefix of tool-call ids synthesized by the decoder when Gemini sends none.
const SYNTHETIC_ID_PREFIX: &str = "gemini_call_";

/// Gemini-supplied call id (sent back so parallel calls of the same function
/// map unambiguously); synthesized ids are not sent.
fn provider_call_id(id: &str) -> Option<&str> {
    (!id.is_empty() && !id.starts_with(SYNTHETIC_ID_PREFIX)).then_some(id)
}

/// Build the JSON request body.
#[cfg(test)]
pub(crate) fn build_body(req: &ChatRequest) -> Result<Json, LlmError> {
    build_body_with_warnings(req).map(|(body, _)| body)
}

/// Build the JSON request body, also returning conversion warnings.
pub(crate) fn build_body_with_warnings(req: &ChatRequest) -> Result<(Json, Vec<String>), LlmError> {
    check_extra(req)?;
    if req.extra.get("candidateCount").and_then(Json::as_u64).is_some_and(|n| n > 1) {
        return Err(LlmError::Config(
            "`candidateCount` > 1 is not supported: only one candidate is streamed back".into(),
        ));
    }
    let mut warnings: Vec<String> = Vec::new();
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
                    let mut call = json!({"name": c.name, "args": args});
                    if let (Some(id), Some(obj)) = (provider_call_id(&c.id), call.as_object_mut()) {
                        obj.insert("id".into(), json!(id));
                    }
                    parts.push(json!({ "functionCall": call }));
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
                let mut response = json!({"name": name, "response": function_response(&m.text())});
                if let (Some(id), Some(obj)) = (provider_call_id(id), response.as_object_mut()) {
                    obj.insert("id".into(), json!(id));
                }
                push("user", vec![json!({ "functionResponse": response })]);
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
        let mut declarations: Vec<Json> = Vec::with_capacity(req.tools.len());
        for t in &req.tools {
            let mut decl = json!({"name": t.name, "description": t.description});
            let has_params = t.parameters.get("properties").and_then(Json::as_object).is_some_and(|p| !p.is_empty());
            if has_params {
                let converted = to_gemini_schema(&t.parameters)
                    .map_err(|e| LlmError::Config(format!("tool `{}` parameters: {e}", t.name)))?;
                warnings.extend(converted.dropped.iter().map(|d| format!("tool `{}` parameters {d}", t.name)));
                if let Some(obj) = decl.as_object_mut() {
                    obj.insert("parameters".into(), converted.schema);
                }
            } else if t.parameters.as_object().is_some_and(|o| o.keys().any(|k| k != "type" && k != "$schema")) {
                // e.g. a free-form `additionalProperties` map: Gemini needs declared properties.
                warnings.push(format!(
                    "tool `{}`: parameters without `properties` cannot be declared to Gemini; \
                     the model can only call it with no arguments",
                    t.name
                ));
            }
            declarations.push(decl);
        }
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
            let converted = to_gemini_schema(schema).map_err(|e| LlmError::Config(format!("response schema: {e}")))?;
            warnings.extend(converted.dropped.iter().map(|d| format!("response schema {d}")));
            generation.insert("responseSchema".into(), converted.schema);
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
    Ok((Json::Object(body), warnings))
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

/// Keywords that carry no constraint for the model and are dropped silently.
const IGNORED_KEYWORDS: &[&str] = &["$schema", "$id", "$defs", "definitions", "$comment", "examples", "default"];

/// A converted schema plus the constraints that Gemini cannot express.
#[derive(Clone, Debug, PartialEq)]
pub struct GeminiSchema {
    pub schema: Json,
    /// `path: keyword` entries for constraints that were dropped. They are
    /// reported as warnings so callers know the model output is less constrained
    /// than the original schema.
    pub dropped: Vec<String>,
}

/// Convert a JSON Schema into Gemini's OpenAPI-style schema subset: upper-case
/// types, `nullable` instead of `"null"` types, local `$ref`s inlined, `const`
/// turned into a one-value `enum`. Constraints Gemini cannot express are dropped
/// and listed in [`GeminiSchema::dropped`]. Unresolvable `$ref`s, recursion
/// deeper than the nesting limit and non-object schemas are errors — they would
/// otherwise silently become "accept anything".
pub fn to_gemini_schema(schema: &Json) -> Result<GeminiSchema, LlmError> {
    let mut converter = SchemaConverter { root: schema, dropped: Vec::new() };
    let schema = converter.convert(schema, "$", 0)?;
    Ok(GeminiSchema { schema, dropped: converter.dropped })
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

struct SchemaConverter<'a> {
    root: &'a Json,
    dropped: Vec<String>,
}

impl SchemaConverter<'_> {
    fn drop(&mut self, path: &str, what: impl std::fmt::Display) {
        self.dropped.push(format!("{path}: {what}"));
    }

    fn convert(&mut self, schema: &Json, path: &str, depth: usize) -> Result<Json, LlmError> {
        if depth > MAX_SCHEMA_DEPTH {
            return Err(LlmError::Config(format!(
                "JSON Schema at `{path}` nests deeper than {MAX_SCHEMA_DEPTH} levels (recursive `$ref`?); \
                 Gemini schemas cannot express recursion"
            )));
        }
        let obj = match schema {
            Json::Object(obj) => obj,
            Json::Bool(true) => return Ok(json!({})),
            other => {
                return Err(LlmError::Config(format!("JSON Schema at `{path}` must be an object, got {other}")));
            }
        };
        if let Some(reference) = obj.get("$ref").and_then(Json::as_str) {
            let root = self.root;
            let target = resolve_ref(root, reference).ok_or_else(|| {
                LlmError::Config(format!("JSON Schema at `{path}`: cannot resolve `$ref` `{reference}`"))
            })?;
            return self.convert(target, path, depth + 1);
        }
        let mut out = Map::new();
        let mut nullable = obj.get("nullable").and_then(Json::as_bool).unwrap_or(false);
        for key in obj.keys() {
            let known = matches!(
                key.as_str(),
                "type"
                    | "nullable"
                    | "anyOf"
                    | "oneOf"
                    | "format"
                    | "enum"
                    | "const"
                    | "properties"
                    | "required"
                    | "items"
                    | "$ref"
            ) || COPIED_KEYWORDS.contains(&key.as_str())
                || IGNORED_KEYWORDS.contains(&key.as_str());
            if !known {
                self.drop(path, format!("unsupported keyword `{key}`"));
            }
        }
        match obj.get("type") {
            Some(Json::String(t)) => match gemini_type(t) {
                Some(g) => {
                    out.insert("type".into(), json!(g));
                }
                None if t == "null" => nullable = true,
                None => self.drop(path, format!("unknown type `{t}`")),
            },
            Some(Json::Array(types)) => {
                let names: Vec<&str> = types.iter().filter_map(Json::as_str).collect();
                nullable |= names.contains(&"null");
                let non_null: Vec<&str> = names.iter().copied().filter(|t| *t != "null").collect();
                if let (true, Some(first)) = (non_null.len() > 1, non_null.first()) {
                    self.drop(path, format!("type union {non_null:?} (only `{first}` kept)"));
                }
                if let Some(g) = non_null.iter().find_map(|t| gemini_type(t)) {
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
                if variants_key == "oneOf" && non_null.len() > 1 {
                    self.drop(path, "`oneOf` exclusivity (converted to `anyOf`)");
                }
                if let [only] = non_null.as_slice() {
                    // `anyOf: [X, {type: null}]` is just a nullable X.
                    if let Json::Object(inner) = self.convert(only, path, depth + 1)? {
                        for (k, v) in inner {
                            out.entry(k).or_insert(v);
                        }
                    }
                } else if !non_null.is_empty() {
                    let mut converted = Vec::with_capacity(non_null.len());
                    for (i, v) in non_null.iter().enumerate() {
                        converted.push(self.convert(v, &format!("{path}.{variants_key}[{i}]"), depth + 1)?);
                    }
                    out.insert("anyOf".into(), Json::Array(converted));
                }
            }
        }
        if let Some(format) = obj.get("format").and_then(Json::as_str) {
            if SUPPORTED_FORMATS.contains(&format) {
                out.insert("format".into(), json!(format));
            } else {
                self.drop(path, format!("format `{format}`"));
            }
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
            } else {
                self.drop(path, "`enum` with non-string values (Gemini enums are strings only)");
            }
        } else if let Some(value) = obj.get("const") {
            match value {
                Json::String(value) => {
                    out.insert("enum".into(), json!([value]));
                    out.entry("type").or_insert_with(|| json!("STRING"));
                }
                other => self.drop(path, format!("non-string `const` {other}")),
            }
        }
        if let Some(props) = obj.get("properties").and_then(Json::as_object) {
            let mut converted = Map::new();
            for (k, v) in props {
                converted.insert(k.clone(), self.convert(v, &format!("{path}.{k}"), depth + 1)?);
            }
            if let Some(required) = obj.get("required").and_then(Json::as_array) {
                let mut kept = Vec::new();
                for r in required {
                    match r.as_str() {
                        Some(name) if converted.contains_key(name) => kept.push(r.clone()),
                        _ => self.drop(path, format!("`required` entry {r} that is not a declared property")),
                    }
                }
                if !kept.is_empty() {
                    out.insert("required".into(), Json::Array(kept));
                }
            }
            out.insert("properties".into(), Json::Object(converted));
            out.entry("type").or_insert_with(|| json!("OBJECT"));
        } else if obj.contains_key("required") {
            self.drop(path, "`required` without `properties`");
        }
        if let Some(items) = obj.get("items") {
            let converted = self.convert(items, &format!("{path}[]"), depth + 1)?;
            out.insert("items".into(), converted);
            out.entry("type").or_insert_with(|| json!("ARRAY"));
        }
        if nullable {
            out.insert("nullable".into(), json!(true));
        }
        Ok(Json::Object(out))
    }
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
                    let name = call
                        .get("name")
                        .and_then(Json::as_str)
                        .filter(|n| !n.is_empty())
                        .ok_or_else(|| LlmError::Decode("Gemini returned a functionCall without a name".into()))?
                        .to_owned();
                    let id = call
                        .get("id")
                        .and_then(Json::as_str)
                        .filter(|id| !id.is_empty())
                        .map_or_else(|| format!("{SYNTHETIC_ID_PREFIX}{index}"), str::to_owned);
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
        // Keep reading after `finishReason`: a later chunk may still carry the final
        // `usageMetadata`. The body's end completes the response.
        Ok(Decoded::deltas(out))
    }

    fn end_of_stream(&mut self) -> Result<Vec<ChatDelta>, LlmError> {
        if self.finished { Ok(self.state().into_iter().collect()) } else { Err(crate::http::truncated()) }
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
        }
        // The body ended: a response is complete only if a finish reason was seen.
        match decoder.end_of_stream() {
            Ok(tail) => {
                deltas.extend(tail);
                Ok((deltas, true))
            }
            Err(_) => Ok((deltas, false)),
        }
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
        let converted = to_gemini_schema(&schema).unwrap();
        let g = &converted.schema;
        // Dropped constraints are reported, not silently lost.
        let dropped = converted.dropped.join("\n");
        assert!(dropped.contains("additionalProperties"), "{dropped}");
        assert!(dropped.contains("format `uri`"), "{dropped}");
        assert!(dropped.contains("ghost"), "{dropped}");
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
        // Recursive references cannot be expressed: an error instead of an
        // accept-anything `{}`.
        let recursive = json!({"$defs": {"n": {"type": "object", "properties": {"next": {"$ref": "#/$defs/n"}}}}, "$ref": "#/$defs/n"});
        assert!(matches!(to_gemini_schema(&recursive), Err(LlmError::Config(_))));
        let dangling = json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/missing"}}});
        assert!(matches!(to_gemini_schema(&dangling), Err(LlmError::Config(_))));
        // Integer enums are dropped with a warning, not silently.
        let ints = to_gemini_schema(&json!({"type": "integer", "enum": [1, 2]})).unwrap();
        assert!(ints.schema.get("enum").is_none());
        assert_eq!(ints.dropped.len(), 1);
    }

    #[test]
    fn warnings_reach_the_response_and_candidate_count_is_rejected() {
        let mut req = ChatRequest::new("m", vec![Message::user("x")]);
        req.response_format = Some(ResponseFormat::JsonSchema {
            name: "s".into(),
            schema: json!({"type": "object", "properties": {"a": {"type": "string", "format": "email"}}}),
            strict: false,
        });
        let (_, warnings) = build_body_with_warnings(&req).unwrap();
        assert!(warnings.iter().any(|w| w.contains("email")), "{warnings:?}");
        req.extra.insert("candidateCount".into(), json!(2));
        assert!(matches!(build_body_with_warnings(&req), Err(LlmError::Config(_))));
    }

    #[test]
    fn provider_call_ids_are_sent_back() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![
            ToolCall { id: "abc".into(), name: "f".into(), arguments: "{}".into() },
            ToolCall { id: "gemini_call_1".into(), name: "f".into(), arguments: "{}".into() },
        ];
        let req = ChatRequest::new(
            "m",
            vec![Message::user("q"), assistant, Message::tool("abc", "1"), Message::tool("gemini_call_1", "2")],
        );
        let body = build_body(&req).unwrap();
        let contents = body["contents"].as_array().unwrap();
        let calls = contents[1]["parts"].as_array().unwrap();
        assert_eq!(calls[0]["functionCall"]["id"], "abc");
        assert!(calls[1]["functionCall"].get("id").is_none());
        let responses = contents[2]["parts"].as_array().unwrap();
        assert_eq!(responses[0]["functionResponse"]["id"], "abc");
        assert!(responses[1]["functionResponse"].get("id").is_none());
    }

    #[test]
    fn function_call_without_name_is_an_error() {
        let fixture = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"args\":{}}}]}}]}\n\n";
        assert!(matches!(decode_all(fixture), Err(LlmError::Decode(_))));
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
            GeminiModel::endpoint(GEMINI_BASE_URL, "models/gemini-2.5-flash"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert!(!format!("{model:?}").contains("AIza-secret"));
    }
}
