//! Configuration and prompt assembly shared by the model-calling nodes.

use std::sync::Arc;

use graphrt_core::event::Usage;
use graphrt_core::template::{Part, Template};
use graphrt_core::value::{FileKind, Value};
use graphrt_core::{NodeContext, NodeError, Selector, VariablePool};
use serde::Deserialize;
use serde_json::Value as Json;

use crate::registry::ModelRegistry;
use crate::types::{ChatDelta, ChatModel, ChatRequest, ChatResponse, ContentPart, Message, Role, collect};

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

impl ModelConfig {
    /// Build a request for this model, applying completion parameters. Parameter
    /// values that are templates (`"{{#start.temp#}}"`) are resolved per run;
    /// the configuration itself is never modified.
    pub fn request(&self, messages: Vec<Message>, pool: &VariablePool) -> ChatRequest {
        let mut req = ChatRequest::new(self.name.clone(), messages);
        for (key, raw) in &self.completion_params {
            if IGNORED_PARAMS.contains(&key.as_str()) {
                continue;
            }
            let value = resolve_param(raw, pool);
            match key.as_str() {
                "temperature" => req.temperature = value.as_f64(),
                "top_p" => req.top_p = value.as_f64(),
                "max_tokens" | "max_completion_tokens" => {
                    req.max_tokens = value.as_u64().and_then(|n| u32::try_from(n).ok());
                }
                "stop" => {
                    req.stop = match value {
                        Json::String(s) if !s.is_empty() => vec![s],
                        Json::Array(items) => items.iter().filter_map(|i| i.as_str().map(str::to_owned)).collect(),
                        _ => Vec::new(),
                    };
                }
                _ => {
                    req.extra.insert(key.clone(), value);
                }
            }
        }
        req
    }
}

/// Resolve a completion parameter: templates are rendered, numeric/boolean strings coerced.
fn resolve_param(raw: &Json, pool: &VariablePool) -> Json {
    let Json::String(s) = raw else { return raw.clone() };
    let resolved = if s.contains("{{#") { Template::parse(s).resolve(pool) } else { Value::from(s.as_str()) };
    match resolved {
        Value::String(text) => {
            let t = text.trim();
            if let Ok(i) = t.parse::<i64>() {
                Json::from(i)
            } else if let Ok(f) = t.parse::<f64>() {
                Json::from(f)
            } else if t == "true" || t == "false" {
                Json::Bool(t == "true")
            } else {
                Json::String(text.to_string())
            }
        }
        other => other.to_json(),
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
    /// Image parts for the configured files (default selector `sys.files`).
    pub fn image_parts(&self, pool: &VariablePool) -> Vec<ContentPart> {
        if !self.enabled {
            return Vec::new();
        }
        let configs = self.configs.clone().unwrap_or_default();
        let selector = configs
            .variable_selector
            .or_else(|| self.variable_selector.clone())
            .unwrap_or_else(|| Selector::of("sys", "files"));
        let detail = configs.detail.or_else(|| self.detail.clone());
        let files: Vec<Value> = match pool.get(&selector) {
            Some(Value::Array(items)) => items.iter().cloned().collect(),
            Some(v @ Value::File(_)) => vec![v],
            _ => Vec::new(),
        };
        files
            .iter()
            .filter_map(|v| v.as_file())
            .filter(|f| f.kind == FileKind::Image || f.mime_type.as_deref().is_some_and(|m| m.starts_with("image/")))
            .filter_map(|f| f.url.clone())
            .map(|url| ContentPart::Image { url, detail: detail.clone() })
            .collect()
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
/// `{role, content}` objects, oldest first. With `window.enabled`, only the last
/// `window.size` turns (two messages per turn) are kept. The current query is
/// appended as a user message, rendered from `query_prompt_template` (default
/// `{{#sys.query#}}`).
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
    pub fn history(&self, pool: &VariablePool) -> Vec<Message> {
        let selector = self.variable_selector.clone().unwrap_or_else(|| Selector::of("sys", "history"));
        let Some(Value::Array(items)) = pool.get(&selector) else { return Vec::new() };
        let mut messages: Vec<Message> = items
            .iter()
            .filter_map(|item| {
                let obj = item.as_object()?;
                let role = match obj.get("role").and_then(Value::as_str)? {
                    "user" | "human" => Role::User,
                    "assistant" | "ai" => Role::Assistant,
                    "system" => Role::System,
                    _ => return None,
                };
                let content = obj.get("content").map(Value::to_text).unwrap_or_default();
                Some(Message { role, ..Message::user(content) })
            })
            .collect();
        if let Some(window) = self.window.as_ref().filter(|w| w.enabled) {
            let keep = window.size.unwrap_or(10).saturating_mul(2);
            if messages.len() > keep {
                messages.drain(..messages.len() - keep);
            }
        }
        messages
    }

    pub fn query(&self, pool: &VariablePool) -> Option<String> {
        let text = match &self.query_prompt_template {
            Some(t) => t.render(pool),
            None => pool.get(&Selector::of("sys", "query"))?.to_text(),
        };
        if text.is_empty() { None } else { Some(text) }
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
) -> Vec<Message> {
    let rendered: Vec<Message> = template
        .messages()
        .into_iter()
        .map(|m| Message { role: m.role, ..Message::user(render_with_context(&m.text, pool, context)) })
        .collect();
    let split = rendered.iter().take_while(|m| m.role == Role::System).count();
    let mut messages: Vec<Message> = rendered.iter().take(split).cloned().collect();
    if let Some(mem) = memory {
        messages.extend(mem.history(pool));
    }
    messages.extend(rendered.into_iter().skip(split));
    if let Some(query) = memory.and_then(|m| m.query(pool)) {
        messages.push(Message::user(query));
    }
    if !images.is_empty() {
        match messages.iter_mut().rev().find(|m| m.role == Role::User) {
            Some(last_user) => last_user.content.extend(images),
            None => messages.push(Message { content: images, ..Message::user("") }),
        }
    }
    messages
}

/// Look up the configured provider.
pub fn model_for(ctx: &NodeContext, config: &ModelConfig) -> Result<Arc<dyn ChatModel>, NodeError> {
    let registry = ctx.require_service::<ModelRegistry>("model registry (graphrt_llm::ModelRegistry)")?;
    Ok(registry.get(&config.provider)?)
}

/// Run a request, optionally streaming answer and reasoning text as node chunks.
pub async fn invoke(
    ctx: &NodeContext,
    model: &dyn ChatModel,
    request: ChatRequest,
    stream_chunks: bool,
) -> Result<ChatResponse, NodeError> {
    let stream = model.chat(request).await?;
    let response = collect(stream, |delta| {
        if !stream_chunks {
            return;
        }
        match delta {
            ChatDelta::Text(t) if !t.is_empty() => ctx.chunk("text", t.clone()),
            ChatDelta::Reasoning(t) if !t.is_empty() => ctx.chunk("reasoning", t.clone()),
            _ => {}
        }
    })
    .await?;
    Ok(response)
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
    while let Some(start) = rest.find("<think>") {
        answer.push_str(&rest[..start]);
        let after = &rest[start + "<think>".len()..];
        match after.find("</think>") {
            Some(end) => {
                reasoning.push_str(after[..end].trim());
                rest = &after[end + "</think>".len()..];
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

/// Extract the first JSON object or array from model output, tolerating code
/// fences, think tags and surrounding prose. Brace matching ignores braces
/// inside string literals.
pub fn extract_json(text: &str) -> Option<Json> {
    let (answer, _) = split_think_tags(text);
    let trimmed = answer.trim();
    if let Ok(v) = serde_json::from_str::<Json>(trimmed) {
        if v.is_object() || v.is_array() {
            return Some(v);
        }
    }
    let bytes = trimmed.as_bytes();
    let mut search_from = 0;
    while let Some(offset) = trimmed.get(search_from..)?.find(['{', '[']) {
        let start = search_from + offset;
        if let Some(end) = matching_close(bytes, start) {
            if let Some(candidate) = trimmed.get(start..=end) {
                if let Ok(v) = serde_json::from_str::<Json>(candidate) {
                    return Some(v);
                }
            }
        }
        search_from = start + 1;
    }
    None
}

fn matching_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
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
                depth -= 1;
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
    }

    #[test]
    fn think_tags() {
        let (a, r) = split_think_tags("<think>plan</think>\nanswer");
        assert_eq!(a, "answer");
        assert_eq!(r, "plan");
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

    #[test]
    fn params_resolve_without_mutation() {
        let mut pool = VariablePool::new();
        pool.insert("start", "t", Value::from("0.3"));
        let cfg: ModelConfig = serde_json::from_value(serde_json::json!({
            "provider": "p", "name": "m",
            "completion_params": {"temperature": "{{#start.t#}}", "max_tokens": 20, "stop": "x", "seed": "7", "mode": "chat"}
        }))
        .unwrap();
        let req = cfg.request(vec![], &pool);
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.max_tokens, Some(20));
        assert_eq!(req.stop, vec!["x".to_owned()]);
        assert_eq!(req.extra.get("seed"), Some(&Json::from(7)));
        assert!(!req.extra.contains_key("mode"));
        assert_eq!(cfg.completion_params["temperature"], "{{#start.t#}}");
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
        let msgs = build_messages(&template, &pool, None, Some(&memory), vision.image_parts(&pool));
        let texts: Vec<String> = msgs.iter().map(Message::text).collect();
        assert_eq!(texts, vec!["sys", "old3", "old4", "tmpl", "now"]);
        assert!(matches!(msgs[4].content.last(), Some(ContentPart::Image { .. })));
    }
}
