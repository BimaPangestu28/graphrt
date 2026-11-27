//! Native tools of the app: a [`ToolProvider`] implemented in Kotlin or Swift,
//! registered with [`crate::Runtime::register_tool_provider`], that `agent` and
//! `tool` nodes call as `<provider>/<tool>`.
//!
//! The runtime owns everything around the call, so an app only writes the tool:
//!
//! - the arguments are checked against the tool's JSON Schema (type, required
//!   properties, property types, enums) before the app is called; a call that
//!   does not match goes back to the model as an error result, so it can retry;
//! - every call has a time limit (`timeout_ms` of the tool, 30 s by default);
//!   when it passes, or the run is aborted, the call is cancelled: the Kotlin
//!   coroutine or the Swift task of `callTool` receives a cancellation;
//! - a [`ToolCallError::Failed`] thrown by the app becomes an error result the
//!   model sees; any other exception becomes the same, with the text of the
//!   exception, and never crashes the run;
//! - results are bounded (1 MiB of text, 1 MiB of JSON data).

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::Value;
use graphrt_core::tool::{ToolCallContext, ToolError, ToolOutput, ToolRuntime, ToolSpec};

use crate::GraphrtError;

/// Time limit of a call when the tool sets none.
pub const DEFAULT_TOOL_TIMEOUT_MS: u64 = 30_000;
/// Longest time limit a tool may set (10 minutes).
pub const MAX_TOOL_TIMEOUT_MS: u64 = 600_000;
/// Largest `text` and `data_json` of one result, in bytes.
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
/// Most tools one provider may declare.
pub const MAX_TOOLS_PER_PROVIDER: usize = 128;

/// A tool the app offers to agents.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct HostToolSpec {
    /// Name within the provider: letters, digits, `_` and `-`, at most 64 characters.
    pub name: String,
    /// What the tool does and when to use it, for the model.
    pub description: String,
    /// JSON Schema of the arguments object, e.g.
    /// `{"type":"object","properties":{"to":{"type":"string"}},"required":["to"]}`.
    pub parameters_json: String,
    /// Time limit of one call in milliseconds (default 30 000, at most 600 000).
    #[uniffi(default)]
    pub timeout_ms: Option<u64>,
}

/// One call of a tool, as the app receives it.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct HostToolCall {
    /// The provider the tool was registered under.
    pub provider: String,
    /// The tool's name.
    pub name: String,
    /// The arguments, a JSON object that matches the tool's schema.
    pub arguments_json: String,
}

/// What a tool returns.
#[derive(Clone, Debug, Default, PartialEq, uniffi::Record)]
pub struct HostToolResult {
    /// Text handed to the model.
    pub text: String,
    /// Structured result as JSON, if any. UI blocks (`{"$ui": "card", ...}`) in it
    /// reach the app as `RunEvent.Ui` events.
    #[uniffi(default)]
    pub data_json: Option<String>,
    /// The tool did not do what was asked; the model sees `text` as an error.
    #[uniffi(default)]
    pub is_error: bool,
}

/// An error a tool reports. The model receives `reason` and may try again.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[non_exhaustive]
pub enum ToolCallError {
    /// The tool could not do what was asked.
    #[error("{reason}")]
    Failed { reason: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for ToolCallError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed { reason: format!("the app's tool threw an exception: {}", e.reason) }
    }
}

/// Native tools implemented by the app.
///
/// `call_tool` is a `suspend fun` in Kotlin and an `async throws` function in
/// Swift. It is called from the runtime's threads, possibly for several calls
/// at once; it is cancelled when its time limit passes or the run is aborted.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait ToolProvider: Send + Sync {
    /// Run one tool call. Throw [`ToolCallError::Failed`] to tell the model why it failed.
    async fn call_tool(&self, call: HostToolCall) -> Result<HostToolResult, ToolCallError>;
}

/// A checked tool declaration.
#[derive(Debug, Clone)]
struct Declared {
    spec: ToolSpec,
    timeout: Duration,
}

/// The [`ToolRuntime`] of one registered provider.
pub(crate) struct HostTools {
    provider: String,
    tools: Vec<Declared>,
    callback: Arc<dyn ToolProvider>,
}

impl std::fmt::Debug for HostTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.tools.iter().map(|t| t.spec.name.as_str()).collect();
        f.debug_struct("HostTools").field("provider", &self.provider).field("tools", &names).finish()
    }
}

/// Whether `name` is a valid provider or tool name.
pub(crate) fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl HostTools {
    /// Check the declarations of `provider`.
    pub(crate) fn new(
        provider: &str,
        specs: Vec<HostToolSpec>,
        callback: Arc<dyn ToolProvider>,
    ) -> Result<Self, GraphrtError> {
        let invalid = |message: String| GraphrtError::InvalidInput { message };
        if !is_valid_name(provider) {
            return Err(invalid(format!("tool provider `{provider}`: use 1 to 64 letters, digits, `_` or `-`")));
        }
        if specs.is_empty() {
            return Err(invalid(format!("tool provider `{provider}` declares no tools")));
        }
        if specs.len() > MAX_TOOLS_PER_PROVIDER {
            return Err(invalid(format!(
                "tool provider `{provider}` declares {} tools; at most {MAX_TOOLS_PER_PROVIDER} are allowed",
                specs.len()
            )));
        }
        let mut tools: Vec<Declared> = Vec::with_capacity(specs.len());
        for spec in specs {
            let what = format!("tool `{provider}/{}`", spec.name);
            if !is_valid_name(&spec.name) {
                return Err(invalid(format!("{what}: use 1 to 64 letters, digits, `_` or `-` in the name")));
            }
            if tools.iter().any(|t| t.spec.name == spec.name) {
                return Err(invalid(format!("{what} is declared twice")));
            }
            let parameters: serde_json::Value = serde_json::from_str(&spec.parameters_json)
                .map_err(|e| invalid(format!("{what}: parameters_json is not JSON: {e}")))?;
            check_schema(&parameters).map_err(|e| invalid(format!("{what}: {e}")))?;
            let timeout_ms = spec.timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT_MS);
            if timeout_ms == 0 || timeout_ms > MAX_TOOL_TIMEOUT_MS {
                return Err(invalid(format!("{what}: timeout_ms must be between 1 and {MAX_TOOL_TIMEOUT_MS}")));
            }
            tools.push(Declared {
                spec: ToolSpec { name: spec.name, description: spec.description, parameters },
                timeout: Duration::from_millis(timeout_ms),
            });
        }
        Ok(Self { provider: provider.to_owned(), tools, callback })
    }

    async fn call_declared(&self, tool: &Declared, arguments: serde_json::Value) -> ToolOutput {
        if let Err(problem) = check_arguments(&tool.spec.parameters, &arguments) {
            return ToolOutput::error(format!(
                "invalid arguments: {problem}. Call the tool again with arguments that match its schema."
            ));
        }
        let call = HostToolCall {
            provider: self.provider.clone(),
            name: tool.spec.name.clone(),
            arguments_json: arguments.to_string(),
        };
        // Dropping the future (time limit, aborted run) cancels the app's coroutine or task.
        match tokio::time::timeout(tool.timeout, self.callback.call_tool(call)).await {
            Err(_) => ToolOutput::error(format!(
                "the tool did not answer within {} ms and was cancelled",
                tool.timeout.as_millis()
            )),
            Ok(Err(ToolCallError::Failed { reason })) => ToolOutput::error(bounded(reason)),
            Ok(Ok(result)) => output_of(result),
        }
    }
}

/// The text of a result, cut at [`MAX_RESULT_BYTES`] on a character boundary.
fn bounded(mut text: String) -> String {
    if text.len() > MAX_RESULT_BYTES {
        let mut end = MAX_RESULT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" [truncated]");
    }
    text
}

fn output_of(result: HostToolResult) -> ToolOutput {
    let data = match result.data_json.as_deref().map(str::trim) {
        None | Some("") => Value::Null,
        Some(json) if json.len() > MAX_RESULT_BYTES => {
            return ToolOutput::error(format!(
                "the tool returned {} bytes of data; at most {MAX_RESULT_BYTES} are allowed",
                json.len()
            ));
        }
        Some(json) => match serde_json::from_str::<serde_json::Value>(json) {
            Ok(value) => Value::from_json(value),
            Err(e) => return ToolOutput::error(format!("the tool returned data that is not JSON: {e}")),
        },
    };
    ToolOutput { text: bounded(result.text), data, is_error: result.is_error, images: Vec::new() }
}

impl ToolRuntime for HostTools {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        let specs = self.tools.iter().map(|t| t.spec.clone()).collect();
        Box::pin(async move { Ok(specs) })
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
        _context: &'a ToolCallContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let Some(tool) = self.tools.iter().find(|t| t.spec.name == name) else {
                return Err(ToolError {
                    tool: format!("{}/{name}", self.provider),
                    message: "no such tool in this provider".to_owned(),
                });
            };
            Ok(self.call_declared(tool, arguments).await)
        })
    }
}

/// The schema of a tool's arguments must describe an object.
fn check_schema(schema: &serde_json::Value) -> Result<(), String> {
    let fields = schema.as_object().ok_or("parameters_json must be a JSON Schema object")?;
    match fields.get("type").and_then(serde_json::Value::as_str) {
        Some("object") => {}
        _ => return Err("parameters_json must have `\"type\": \"object\"`".to_owned()),
    }
    if let Some(properties) = fields.get("properties")
        && !properties.is_object()
    {
        return Err("`properties` must be an object".to_owned());
    }
    if let Some(required) = fields.get("required") {
        let names = required.as_array().ok_or("`required` must be an array of property names")?;
        if names.iter().any(|n| !n.is_string()) {
            return Err("`required` must be an array of property names".to_owned());
        }
    }
    Ok(())
}

/// Most nesting [`check_value`] follows, so a hostile schema cannot recurse forever.
const MAX_SCHEMA_DEPTH: usize = 16;

/// Check `arguments` against the parts of JSON Schema tools use: `type`,
/// `properties`, `required`, `enum`, `items` and `additionalProperties: false`.
/// Other keywords are not checked (the app still receives valid JSON).
pub(crate) fn check_arguments(schema: &serde_json::Value, arguments: &serde_json::Value) -> Result<(), String> {
    check_value(schema, arguments, "arguments", 0)
}

fn type_matches(expected: &str, value: &serde_json::Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        _ => true,
    }
}

fn check_value(schema: &serde_json::Value, value: &serde_json::Value, path: &str, depth: usize) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    if depth > MAX_SCHEMA_DEPTH {
        return Ok(());
    }
    match schema.get("type") {
        Some(serde_json::Value::String(expected)) if !type_matches(expected, value) => {
            return Err(format!("`{path}` must be of type {expected}"));
        }
        Some(serde_json::Value::Array(options)) => {
            let allowed: Vec<&str> = options.iter().filter_map(serde_json::Value::as_str).collect();
            if !allowed.is_empty() && !allowed.iter().any(|t| type_matches(t, value)) {
                return Err(format!("`{path}` must be of type {}", allowed.join(" or ")));
            }
        }
        _ => {}
    }
    if let Some(serde_json::Value::Array(options)) = schema.get("enum")
        && !options.contains(value)
    {
        return Err(format!("`{path}` must be one of {}", serde_json::Value::Array(options.clone())));
    }
    if let Some(fields) = value.as_object() {
        if let Some(serde_json::Value::Array(required)) = schema.get("required") {
            for name in required.iter().filter_map(serde_json::Value::as_str) {
                if !fields.contains_key(name) {
                    return Err(format!("`{path}.{name}` is required"));
                }
            }
        }
        let properties = schema.get("properties").and_then(serde_json::Value::as_object);
        for (name, field) in fields {
            match properties.and_then(|p| p.get(name)) {
                Some(property) => check_value(property, field, &format!("{path}.{name}"), depth + 1)?,
                None if schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false)) => {
                    return Err(format!("`{path}.{name}` is not a known property"));
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(elements)) = (schema.get("items"), value.as_array()) {
        for (index, element) in elements.iter().enumerate() {
            check_value(items, element, &format!("{path}[{index}]"), depth + 1)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arguments_are_checked_against_the_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "to": {"type": "string"},
                "priority": {"type": "string", "enum": ["low", "high"]},
                "count": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["to"],
            "additionalProperties": false,
        });
        assert_eq!(check_arguments(&schema, &json!({"to": "Ada", "count": 2, "tags": ["a"]})), Ok(()));
        let problem = |args: serde_json::Value| check_arguments(&schema, &args).err().unwrap_or_default();
        assert_eq!(problem(json!({})), "`arguments.to` is required");
        assert_eq!(problem(json!({"to": 3})), "`arguments.to` must be of type string");
        assert_eq!(problem(json!({"to": "a", "count": 1.5})), "`arguments.count` must be of type integer");
        assert!(problem(json!({"to": "a", "priority": "urgent"})).contains("must be one of"));
        assert_eq!(problem(json!({"to": "a", "tags": [1]})), "`arguments.tags[0]` must be of type string");
        assert_eq!(problem(json!({"to": "a", "cc": "b"})), "`arguments.cc` is not a known property");
        assert_eq!(problem(json!("text")), "`arguments` must be of type object");
    }

    #[test]
    fn declarations_are_checked() {
        struct Never;
        #[async_trait::async_trait]
        impl ToolProvider for Never {
            async fn call_tool(&self, _call: HostToolCall) -> Result<HostToolResult, ToolCallError> {
                Err(ToolCallError::Failed { reason: "never".into() })
            }
        }
        let spec = |name: &str, parameters: &str, timeout_ms: Option<u64>| HostToolSpec {
            name: name.into(),
            description: "d".into(),
            parameters_json: parameters.into(),
            timeout_ms,
        };
        let object = r#"{"type":"object"}"#;
        let check = |provider: &str, specs: Vec<HostToolSpec>| HostTools::new(provider, specs, Arc::new(Never));
        assert!(check("device", vec![spec("battery", object, None)]).is_ok());
        assert!(check("bad name", vec![spec("battery", object, None)]).is_err());
        assert!(check("device", vec![]).is_err());
        assert!(check("device", vec![spec("bat tery", object, None)]).is_err());
        assert!(check("device", vec![spec("a", object, None), spec("a", object, None)]).is_err());
        assert!(check("device", vec![spec("a", "{", None)]).is_err());
        assert!(check("device", vec![spec("a", r#"{"type":"string"}"#, None)]).is_err());
        assert!(check("device", vec![spec("a", object, Some(0))]).is_err());
        assert!(check("device", vec![spec("a", object, Some(MAX_TOOL_TIMEOUT_MS + 1))]).is_err());
    }

    #[test]
    fn results_are_bounded_and_parsed() {
        let big = "é".repeat(MAX_RESULT_BYTES);
        let out = output_of(HostToolResult { text: big, data_json: None, is_error: false });
        assert!(out.text.len() <= MAX_RESULT_BYTES + " [truncated]".len() && out.text.ends_with("[truncated]"));
        let out = output_of(HostToolResult { text: "t".into(), data_json: Some("{x".into()), is_error: false });
        assert!(out.is_error && out.text.contains("not JSON"));
        let out = output_of(HostToolResult { text: "t".into(), data_json: Some(r#"{"a":1}"#.into()), is_error: false });
        assert_eq!(out.data.to_json(), json!({"a": 1}));
    }
}
