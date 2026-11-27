//! The generic `tool` node and [`NativeTools`], a [`ToolRuntime`] built from
//! Rust closures.
//!
//! The `tool` node calls one tool of a provider registered in the
//! [`ToolRegistry`] service:
//!
//! ```yaml
//! - id: weather
//!   type: tool
//!   provider_id: builtin          # or e.g. "mcp:github"
//!   tool_name: get_weather
//!   tool_parameters:
//!     city:  { type: variable, value: [start, city] }
//!     units: { type: constant, value: metric }
//!     note:  { type: mixed,    value: "asked by {{#sys.user_id#}}" }
//! ```
//!
//! A simpler `arguments` map is also accepted: string values are templates
//! (a lone `{{#a.b#}}` placeholder keeps its typed value), other values are
//! constants.
//!
//! Outputs: `text` (string), `json` (structured result, or `null`; when the text
//! looks like JSON but does not parse, metadata `json_error` says why), `files`
//! (array, currently always empty), `is_error` (bool) and `error` (the tool's error
//! text, or `null`). A tool-reported error fails the node unless
//! `ignore_tool_errors` is set, in which case `is_error` / `error` let a downstream
//! branch react. `variable` arguments that do not resolve are sent as `null` and
//! listed in metadata `missing_arguments`.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::Deserialize;

use crate::dsl::NodeDoc;
use crate::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult, Registry, parse_config};
use crate::pool::{Selector, VariablePool};
use crate::template::Template;
use crate::tool::{ToolCallContext, ToolError, ToolOutput, ToolRegistry, ToolRuntime, ToolSpec};
use crate::value::Value;

/// Node kind handled by [`ToolNode`].
pub const KIND: &str = "tool";

/// Register the `tool` node kind.
pub fn register(registry: &mut Registry) {
    registry.register_lazily_described(KIND, descriptor, |doc| {
        ToolNode::from_doc(doc).map(|n| Arc::new(n) as Arc<dyn Node>)
    });
}

/// Catalog descriptor of the `tool` node.
pub fn descriptor() -> crate::catalog::NodeDescriptor {
    use crate::catalog::schema::{nested, object, selector};
    use crate::catalog::{NodeCategory, NodeDescriptor, OutputDescriptor};
    use crate::value::ValueType;

    let parameter = serde_json::json!({
        "oneOf": [
            nested(serde_json::json!({ "type": { "const": "mixed" }, "value": { "type": "string" } }), &["type", "value"]),
            nested(serde_json::json!({ "type": { "const": "variable" }, "value": selector() }), &["type", "value"]),
            nested(serde_json::json!({ "type": { "const": "constant" }, "value": {} }), &["type"])
        ]
    });
    NodeDescriptor::new(KIND, "Tool", NodeCategory::Integration)
        .describe(
            "Calls one tool from a registered tool provider (native tools, MCP servers, plugins). \
             Arguments: `tool_parameters` (mixed template / variable / constant) override `arguments` \
             (strings are templates) which override `tool_configurations` (constants).",
        )
        .icon("wrench")
        .source("builtin")
        .schema(object(
            serde_json::json!({
                "provider_id": { "type": "string", "minLength": 1 },
                "provider": { "type": "string", "minLength": 1 },
                "provider_name": { "type": "string", "minLength": 1 },
                "tool_name": { "type": "string", "minLength": 1 },
                "tool_parameters": { "type": "object", "additionalProperties": parameter },
                "tool_configurations": { "type": "object" },
                "arguments": { "type": "object" },
                "ignore_tool_errors": { "type": "boolean", "default": false }
            }),
            &["tool_name"],
        ))
        .output(OutputDescriptor::new("text", ValueType::String).describe("Text result"))
        .output(OutputDescriptor::new("json", ValueType::Any).describe("Structured result, or the text parsed as JSON"))
        .output(OutputDescriptor::new("files", ValueType::ArrayFile))
        .output(OutputDescriptor::new("is_error", ValueType::Boolean))
        .output(OutputDescriptor::new("error", ValueType::String).describe("Error text when the tool reported one"))
}

/// How one tool argument is produced.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ArgSource {
    /// Text template, rendered against the pool.
    Mixed { value: Template },
    /// Value read from a selector (typed).
    Variable { value: Selector },
    /// Literal value.
    Constant {
        #[serde(default)]
        value: Value,
    },
}

#[derive(Debug, Deserialize)]
struct ToolNodeConfig {
    #[serde(alias = "provider", alias = "provider_name")]
    provider_id: String,
    tool_name: String,
    #[serde(default)]
    tool_parameters: IndexMap<String, ArgSource>,
    /// Constant configuration parameters; lowest precedence.
    #[serde(default)]
    tool_configurations: IndexMap<String, Value>,
    /// Shorthand: strings are templates, everything else a constant.
    #[serde(default)]
    arguments: IndexMap<String, Value>,
    /// When true, a tool-reported error becomes a normal output instead of a node failure.
    #[serde(default)]
    ignore_tool_errors: bool,
}

/// Calls one tool from the [`ToolRegistry`] service.
#[derive(Debug, Clone)]
pub struct ToolNode {
    provider: String,
    tool: String,
    /// Arguments in precedence order (later entries override earlier ones).
    args: Vec<(String, ArgSource)>,
    ignore_tool_errors: bool,
}

impl ToolNode {
    /// Build from a node document.
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let cfg: ToolNodeConfig = parse_config(doc)?;
        if cfg.provider_id.trim().is_empty() {
            return Err(ConfigError::new(doc, "`provider_id` must not be empty"));
        }
        if cfg.tool_name.trim().is_empty() {
            return Err(ConfigError::new(doc, "`tool_name` must not be empty"));
        }
        let mut args: Vec<(String, ArgSource)> = Vec::new();
        for (k, v) in cfg.tool_configurations {
            args.push((k, ArgSource::Constant { value: v }));
        }
        for (k, v) in cfg.arguments {
            let source = match v {
                Value::String(s) => ArgSource::Mixed { value: Template::parse(&s) },
                other => ArgSource::Constant { value: other },
            };
            args.push((k, source));
        }
        for (k, v) in cfg.tool_parameters {
            args.push((k, v));
        }
        Ok(Self { provider: cfg.provider_id, tool: cfg.tool_name, args, ignore_tool_errors: cfg.ignore_tool_errors })
    }

    /// Resolve arguments. Returns `(plaintext arguments for the call, masked copy for observability)`.
    fn resolve_args(&self, pool: &VariablePool) -> (serde_json::Value, Value) {
        let mut exposed = serde_json::Map::new();
        let mut masked: IndexMap<String, Value> = IndexMap::new();
        for (name, source) in &self.args {
            match source {
                // Text templates: render twice so the observable copy keeps secrets masked.
                ArgSource::Mixed { value } if !matches!(value.parts(), [crate::template::Part::Var(_)]) => {
                    exposed.insert(name.clone(), serde_json::Value::String(value.render_exposed(pool)));
                    masked.insert(name.clone(), Value::from(value.render(pool)));
                }
                _ => {
                    let value = match source {
                        // A lone placeholder keeps the typed value (numbers stay numbers).
                        ArgSource::Mixed { value } => value.resolve(pool),
                        ArgSource::Variable { value } => pool.get_or_null(value),
                        ArgSource::Constant { value } => value.clone(),
                    };
                    exposed.insert(name.clone(), value.to_json_exposed());
                    masked.insert(name.clone(), value);
                }
            }
        }
        (serde_json::Value::Object(exposed), Value::from(masked))
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        let registry = ctx.require_service::<ToolRegistry>("tool registry")?;
        let runtime = registry
            .get(&self.provider)
            .ok_or_else(|| NodeError::fatal("tool", format!("unknown tool provider `{}`", self.provider)))?
            .clone();
        let (arguments, masked_args) = self.resolve_args(&ctx.pool);
        let missing = self.missing_variables(&ctx.pool);
        // Arguments already carry rendered secrets here: the node config is written by the
        // workflow author, not a model, so there is no separate context channel.
        let output = runtime
            .call(&self.tool, arguments, ToolCallContext::empty())
            .await
            .map_err(|e| NodeError::new("tool", e.to_string()))?;
        if output.is_error && !self.ignore_tool_errors {
            return Err(NodeError::fatal("tool_error", output.text));
        }
        // Files a native tool returned in its structured result (plugins and MCP servers
        // cannot create file values).
        #[cfg(feature = "tool-files")]
        let files: Vec<Value> = crate::tool::collect_files(&output.data).into_iter().map(Value::File).collect();
        #[cfg(not(feature = "tool-files"))]
        let files: Vec<Value> = Vec::new();
        let (json, json_error) =
            if output.data.is_null() { parse_json_text(&output.text) } else { (output.data, None) };
        // With `ignore_tool_errors`, the failure stays visible to the graph through the
        // `is_error` / `error` outputs so a downstream branch can react to it.
        let error = if output.is_error { Value::from(output.text.as_str()) } else { Value::Null };
        let mut result = NodeOutput::new()
            .with("text", output.text)
            .with("json", json)
            .with("files", Value::array(files))
            .with("is_error", output.is_error)
            .with("error", error)
            .meta("provider", self.provider.as_str())
            .meta("tool", self.tool.as_str())
            .meta("arguments", masked_args);
        if let Some(e) = json_error {
            result = result.meta("json_error", e);
        }
        if !missing.is_empty() {
            result = result.meta("missing_arguments", Value::array(missing.into_iter().map(Value::from)));
        }
        Ok(result.into())
    }

    /// Selectors of `variable` arguments that do not resolve (sent to the tool as `null`).
    fn missing_variables(&self, pool: &VariablePool) -> Vec<String> {
        self.args
            .iter()
            .filter_map(|(_, source)| match source {
                ArgSource::Variable { value } if pool.get(value).is_none() => Some(value.to_string()),
                _ => None,
            })
            .collect()
    }
}

/// Structured view of a text result when it looks like a JSON object or array.
/// Returns the parse error when it looks like JSON but is not valid.
fn parse_json_text(text: &str) -> (Value, Option<String>) {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return match serde_json::from_slice::<serde_json::Value>(text.as_bytes()) {
            Ok(json) => (Value::from_json(json), None),
            Err(e) => (Value::Null, Some(e.to_string())),
        };
    }
    (Value::Null, None)
}

impl Node for ToolNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }

    fn inputs(&self) -> Vec<Selector> {
        let mut out = Vec::new();
        for (_, source) in &self.args {
            match source {
                ArgSource::Mixed { value } => out.extend(value.selectors().cloned()),
                ArgSource::Variable { value } => out.push(value.clone()),
                ArgSource::Constant { .. } => {}
            }
        }
        out
    }
}

type ToolFn = Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, Result<ToolOutput, ToolError>> + Send + Sync>;

/// A [`ToolRuntime`] whose tools are Rust closures.
///
/// ```
/// use graphrt_core::tool::{ToolOutput, ToolSpec};
/// use graphrt_core::tool_node::NativeTools;
///
/// let tools = NativeTools::new().with(
///     ToolSpec {
///         name: "echo".into(),
///         description: "Echo the `text` argument".into(),
///         parameters: serde_json::json!({
///             "type": "object",
///             "properties": { "text": { "type": "string" } },
///             "required": ["text"]
///         }),
///     },
///     |args| async move { Ok(ToolOutput::text(args["text"].as_str().unwrap_or_default())) },
/// );
/// ```
#[derive(Clone, Default)]
pub struct NativeTools {
    tools: IndexMap<String, (ToolSpec, ToolFn)>,
}

impl fmt::Debug for NativeTools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeTools").field("tools", &self.tools.keys().collect::<Vec<_>>()).finish()
    }
}

impl NativeTools {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) a tool.
    pub fn add<F, Fut>(&mut self, spec: ToolSpec, handler: F) -> &mut Self
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let handler: ToolFn = Arc::new(move |args| Box::pin(handler(args)));
        self.tools.insert(spec.name.clone(), (spec, handler));
        self
    }

    /// Builder-style [`NativeTools::add`].
    pub fn with<F, Fut>(mut self, spec: ToolSpec, handler: F) -> Self
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        self.add(spec, handler);
        self
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl ToolRuntime for NativeTools {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        let specs = self.tools.values().map(|(spec, _)| spec.clone()).collect();
        Box::pin(futures::future::ready(Ok(specs)))
    }

    /// Native handlers take only arguments; the context is not forwarded
    /// (`accepts_secrets` stays `false`).
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
        _context: &'a ToolCallContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        let Some((spec, handler)) = self.tools.get(name) else {
            return Box::pin(futures::future::ready(Err(ToolError {
                tool: name.to_owned(),
                message: "unknown tool".to_owned(),
            })));
        };
        if let Err(message) = check_required(&spec.parameters, &arguments) {
            return Box::pin(futures::future::ready(Err(ToolError { tool: name.to_owned(), message })));
        }
        handler(arguments)
    }
}

/// Lightweight argument check: the arguments must be an object containing every
/// property listed in the schema's `required` array. Full JSON Schema validation
/// is left to the tool itself.
fn check_required(schema: &serde_json::Value, arguments: &serde_json::Value) -> Result<(), String> {
    let Some(args) = arguments.as_object() else {
        return Err("arguments must be a JSON object".to_owned());
    };
    let required = schema.get("required").and_then(serde_json::Value::as_array);
    let missing: Vec<&str> = required
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|key| args.get(*key).is_none_or(serde_json::Value::is_null))
        .collect();
    if missing.is_empty() { Ok(()) } else { Err(format!("missing required arguments: {}", missing.join(", "))) }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::node::{NodeOutcome, Services};

    #[derive(Debug, Default)]
    struct Recorder {
        calls: Mutex<Vec<(String, serde_json::Value)>>,
        reply: Option<ToolOutput>,
    }

    impl ToolRuntime for Recorder {
        fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
            Box::pin(futures::future::ready(Ok(Vec::new())))
        }
        fn call<'a>(
            &'a self,
            name: &'a str,
            arguments: serde_json::Value,
            _context: &'a ToolCallContext,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            self.calls.lock().unwrap().push((name.to_owned(), arguments));
            let reply = self.reply.clone().unwrap_or_else(|| ToolOutput::text("{\"ok\":true}"));
            Box::pin(futures::future::ready(Ok(reply)))
        }
    }

    fn doc(config: serde_json::Value) -> NodeDoc {
        NodeDoc::new("t", KIND, config)
    }

    fn pool() -> VariablePool {
        let mut p = VariablePool::new();
        p.insert("start", "city", Value::from("Jakarta"));
        p.insert("start", "days", Value::Int(3));
        p.insert("env", "TOKEN", Value::Secret(Arc::from("s3cret")));
        p
    }

    fn ctx_with(runtime: Arc<dyn ToolRuntime>) -> NodeContext {
        let mut reg = ToolRegistry::new();
        reg.add("mock", runtime);
        NodeContext::for_test("t", pool()).with_services(Arc::new(Services::new().with(reg)))
    }

    fn completed(r: NodeResult) -> NodeOutput {
        match r {
            Ok(NodeOutcome::Completed(o)) => o,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolves_all_argument_kinds() {
        let rec = Arc::new(Recorder::default());
        let node = ToolNode::from_doc(&doc(serde_json::json!({
            "provider_id": "mock",
            "tool_name": "weather",
            "tool_configurations": {"units": "imperial", "lang": "en"},
            "arguments": {"note": "for {{#start.city#}}", "raw": 5},
            "tool_parameters": {
                "city": {"type": "variable", "value": ["start", "city"]},
                "days": {"type": "mixed", "value": "{{#start.days#}}"},
                "units": {"type": "constant", "value": "metric"},
                "auth": {"type": "mixed", "value": "Bearer {{#env.TOKEN#}}"}
            }
        })))
        .unwrap();
        let out = completed(node.run(ctx_with(rec.clone())).await);
        let calls = rec.calls.lock().unwrap();
        let (name, args) = &calls[0];
        assert_eq!(name, "weather");
        assert_eq!(args["city"], "Jakarta");
        assert_eq!(args["days"], 3);
        assert_eq!(args["units"], "metric", "tool_parameters override configurations");
        assert_eq!(args["lang"], "en");
        assert_eq!(args["note"], "for Jakarta");
        assert_eq!(args["raw"], 5);
        assert_eq!(args["auth"], "Bearer s3cret", "tool receives the plaintext secret");
        // Observability metadata never contains the secret.
        let meta = serde_json::to_string(&out.metadata).unwrap();
        assert!(!meta.contains("s3cret"));
        assert_eq!(out.outputs["text"], Value::from("{\"ok\":true}"));
        assert_eq!(out.outputs["json"].get_path(&["ok"]), Some(Value::Bool(true)));
        assert_eq!(out.outputs["files"], Value::array([]));
        assert_eq!(node.inputs().len(), 4); // note, city, days, auth
    }

    #[tokio::test]
    async fn tool_error_fails_unless_ignored() {
        let rec = Arc::new(Recorder { reply: Some(ToolOutput::error("boom")), ..Recorder::default() });
        let cfg = serde_json::json!({"provider": "mock", "tool_name": "x"});
        let node = ToolNode::from_doc(&doc(cfg)).unwrap();
        let err = node.run(ctx_with(rec.clone())).await.unwrap_err();
        assert_eq!(err.kind, "tool_error");
        assert!(!err.retryable);

        let cfg = serde_json::json!({"provider": "mock", "tool_name": "x", "ignore_tool_errors": true});
        let node = ToolNode::from_doc(&doc(cfg)).unwrap();
        let out = completed(node.run(ctx_with(rec)).await);
        assert_eq!(out.outputs["text"], Value::from("boom"));
        assert_eq!(out.outputs["is_error"], Value::Bool(true));
        assert_eq!(out.outputs["error"], Value::from("boom"));
    }

    #[tokio::test]
    async fn successful_call_has_no_error_output() {
        let rec = Arc::new(Recorder { reply: Some(ToolOutput::text("{\"a\": 1")), ..Recorder::default() });
        let node = ToolNode::from_doc(&doc(serde_json::json!({
            "provider": "mock", "tool_name": "x",
            "tool_parameters": {"city": {"type": "variable", "value": ["start", "nope"]}}
        })))
        .unwrap();
        let out = completed(node.run(ctx_with(rec)).await);
        assert_eq!(out.outputs["is_error"], Value::Bool(false));
        assert_eq!(out.outputs["error"], Value::Null);
        // Looks like JSON but is truncated: `json` is null and the reason is recorded.
        assert_eq!(out.outputs["json"], Value::Null);
        assert!(out.metadata.contains_key("json_error"));
        assert_eq!(out.metadata["missing_arguments"], Value::array([Value::from("start.nope")]));
    }

    #[tokio::test]
    async fn missing_provider_or_registry_is_fatal() {
        let node = ToolNode::from_doc(&doc(serde_json::json!({"provider_id": "nope", "tool_name": "x"}))).unwrap();
        let err = node.run(ctx_with(Arc::new(Recorder::default()))).await.unwrap_err();
        assert!(!err.retryable);
        let err = node.run(NodeContext::for_test("t", pool())).await.unwrap_err();
        assert_eq!(err.kind, "missing_service");
    }

    #[test]
    fn rejects_invalid_config() {
        assert!(ToolNode::from_doc(&doc(serde_json::json!({"tool_name": "x"}))).is_err());
        assert!(ToolNode::from_doc(&doc(serde_json::json!({"provider_id": " ", "tool_name": "x"}))).is_err());
        let bad_param = serde_json::json!({"provider_id": "p", "tool_name": "x",
            "tool_parameters": {"a": {"type": "variable", "value": ["only"]}}});
        assert!(ToolNode::from_doc(&doc(bad_param)).is_err());
    }

    #[test]
    fn registers_kind() {
        let mut r = Registry::new();
        register(&mut r);
        assert!(r.contains(KIND));
    }

    fn echo_spec() -> ToolSpec {
        ToolSpec {
            name: "echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({"type": "object", "required": ["text"]}),
        }
    }

    #[tokio::test]
    async fn native_tools_dispatch_and_validate() {
        let tools = NativeTools::new().with(echo_spec(), |args| async move {
            Ok(ToolOutput::text(args["text"].as_str().unwrap_or_default().to_uppercase()))
        });
        assert_eq!(tools.len(), 1);
        assert_eq!(tools.list().await.unwrap()[0].name, "echo");
        let out = tools.call("echo", serde_json::json!({"text": "hi"}), ToolCallContext::empty()).await.unwrap();
        assert_eq!(out.text, "HI");
        let err = tools.call("echo", serde_json::json!({}), ToolCallContext::empty()).await.unwrap_err();
        assert!(err.message.contains("text"));
        assert!(tools.call("echo", serde_json::json!("str"), ToolCallContext::empty()).await.is_err());
        assert!(tools.call("nope", serde_json::json!({}), ToolCallContext::empty()).await.is_err());
    }

    #[tokio::test]
    async fn tool_node_with_native_tools() {
        let tools = NativeTools::new().with(echo_spec(), |args| async move {
            Ok(ToolOutput {
                text: "done".into(),
                data: Value::from_json(args),
                is_error: false,
                ..ToolOutput::default()
            })
        });
        let node = ToolNode::from_doc(&doc(serde_json::json!({
            "provider_id": "mock", "tool_name": "echo", "arguments": {"text": "{{#start.city#}}"}
        })))
        .unwrap();
        let out = completed(node.run(ctx_with(Arc::new(tools))).await);
        assert_eq!(out.outputs["json"].get_path(&["text"]), Some(Value::from("Jakarta")));
    }
}
