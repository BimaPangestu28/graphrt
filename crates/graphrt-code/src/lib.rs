//! # graphrt-code
//!
//! The `code` node: runs user-supplied code over workflow variables.
//!
//! - **JavaScript** runs in an embedded QuickJS engine (feature `js`, on by
//!   default). The engine has no file system, network, timers or module
//!   loader; memory, stack and execution time are capped.
//! - **Other languages** (e.g. `python3`) run in a host-provided
//!   [`RemoteSandbox`] service. The optional `remote` feature provides
//!   [`HttpSandbox`], a client for an HTTP sandbox service.
//!
//! User code defines `main(args)`, which receives an object with the configured
//! input variables and returns an object (or a promise of one):
//!
//! ```js
//! function main({ a, b }) {
//!   return { sum: a + b };
//! }
//! ```
//!
//! Inputs are passed with secrets **revealed**: code nodes frequently need API
//! keys from `env.*`. Treat the sandbox as a trusted sink and do not echo
//! secrets into outputs.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::Deserialize;

use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult, Registry, parse_config};
use graphrt_core::{Selector, Value, ValueType};

#[cfg(feature = "js")]
mod js;
#[cfg(feature = "remote")]
mod remote;

#[cfg(feature = "remote")]
pub use remote::HttpSandbox;

/// Node kind handled by this crate.
pub const KIND: &str = "code";

/// Resource limits for one code execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CodeLimits {
    /// Wall-clock execution budget in milliseconds.
    pub time_ms: u64,
    /// Heap limit in bytes.
    pub memory_bytes: usize,
    /// Native stack limit in bytes.
    pub stack_bytes: usize,
    /// Maximum size of the JSON-encoded result.
    pub max_output_bytes: usize,
    /// Maximum nesting depth of the result.
    pub max_depth: usize,
}

impl Default for CodeLimits {
    fn default() -> Self {
        Self {
            time_ms: 10_000,
            memory_bytes: 64 * 1024 * 1024,
            stack_bytes: 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_depth: 32,
        }
    }
}

/// A sandbox that executes code out of process (for languages without an embedded engine).
///
/// Register it as `Arc<dyn RemoteSandbox>` in [`graphrt_core::Services`].
pub trait RemoteSandbox: Send + Sync + fmt::Debug {
    /// Execute `code` in `language` with `inputs` (a JSON object). Must return the JSON
    /// object produced by the code's `main` function.
    fn execute<'a>(
        &'a self,
        language: &'a str,
        code: &'a str,
        inputs: serde_json::Value,
        limits: CodeLimits,
    ) -> BoxFuture<'a, Result<serde_json::Value, NodeError>>;
}

/// Where code runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxChoice {
    /// Embedded engine when available for the language, otherwise the remote sandbox.
    #[default]
    Auto,
    /// Always use the registered [`RemoteSandbox`].
    Remote,
}

#[derive(Clone, Debug, Deserialize)]
struct InputVar {
    variable: String,
    value_selector: Selector,
}

#[derive(Clone, Debug, Deserialize)]
struct OutputDecl {
    #[serde(rename = "type")]
    ty: ValueType,
}

#[derive(Clone, Debug, Deserialize)]
struct CodeConfig {
    #[serde(default = "default_language")]
    code_language: String,
    code: String,
    #[serde(default)]
    variables: Vec<InputVar>,
    #[serde(default)]
    outputs: IndexMap<String, OutputDecl>,
    #[serde(default)]
    limits: Option<CodeLimits>,
    #[serde(default)]
    sandbox: SandboxChoice,
}

fn default_language() -> String {
    "javascript".to_owned()
}

/// Normalized language name.
fn normalize_language(lang: &str) -> String {
    match lang.trim().to_ascii_lowercase().as_str() {
        "js" | "javascript" | "nodejs" | "node" => "javascript".to_owned(),
        "py" | "python" | "python3" => "python3".to_owned(),
        other => other.to_owned(),
    }
}

/// The `code` node.
#[derive(Debug)]
pub struct CodeNode {
    language: String,
    code: Arc<str>,
    variables: Vec<InputVar>,
    outputs: IndexMap<String, OutputDecl>,
    limits: Option<CodeLimits>,
    sandbox: SandboxChoice,
}

impl CodeNode {
    /// Build from a node document.
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let cfg: CodeConfig = parse_config(doc)?;
        if cfg.code.trim().is_empty() {
            return Err(ConfigError::new(doc, "`code` must not be empty"));
        }
        let language = normalize_language(&cfg.code_language);
        if language == "jinja2" {
            return Err(ConfigError::new(doc, "jinja2 is not a code language; use a template-transform node"));
        }
        Ok(Self {
            language,
            code: Arc::from(cfg.code),
            variables: cfg.variables,
            outputs: cfg.outputs,
            limits: cfg.limits,
            sandbox: cfg.sandbox,
        })
    }

    /// Configured limits, with the time limit capped by the attempt deadline so a
    /// node timeout also interrupts the interpreter (which blocks synchronously).
    fn effective_limits(&self, ctx: &NodeContext) -> CodeLimits {
        let mut limits = self.limits.or_else(|| ctx.service::<CodeLimits>().copied()).unwrap_or_default();
        if let Some(remaining) = ctx.remaining() {
            let remaining_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX).max(1);
            limits.time_ms = limits.time_ms.min(remaining_ms);
        }
        limits
    }

    fn build_inputs(&self, ctx: &NodeContext) -> serde_json::Value {
        let mut args = serde_json::Map::new();
        for var in &self.variables {
            let value = ctx.get(&var.value_selector).unwrap_or(Value::Null);
            args.insert(var.variable.clone(), value.to_json_exposed());
        }
        serde_json::Value::Object(args)
    }

    async fn execute(&self, ctx: &NodeContext) -> Result<serde_json::Value, NodeError> {
        let inputs = self.build_inputs(ctx);
        let limits = self.effective_limits(ctx);
        let use_remote = self.sandbox == SandboxChoice::Remote || !embedded_supports(&self.language);
        if use_remote {
            let sandbox = ctx.service::<Arc<dyn RemoteSandbox>>().ok_or_else(|| {
                NodeError::fatal(
                    "missing_service",
                    format!(
                        "{} code requires a remote sandbox; register an `Arc<dyn RemoteSandbox>` service",
                        self.language
                    ),
                )
            })?;
            return sandbox.execute(&self.language, &self.code, inputs, limits).await;
        }
        run_embedded(self.code.clone(), inputs, limits).await
    }

    fn convert_outputs(&self, result: serde_json::Value, limits: &CodeLimits) -> Result<NodeOutput, NodeError> {
        let serde_json::Value::Object(map) = result else {
            return Err(NodeError::fatal("invalid_output", "main() must return an object"));
        };
        let depth = json_depth(&serde_json::Value::Object(map.clone()));
        if depth > limits.max_depth {
            return Err(NodeError::fatal(
                "invalid_output",
                format!("result nesting depth {depth} exceeds the limit of {}", limits.max_depth),
            ));
        }
        let mut output = NodeOutput::new();
        if self.outputs.is_empty() {
            for (key, value) in map {
                output.outputs.insert(key, Value::from_json(value));
            }
            return Ok(output);
        }
        for (name, decl) in &self.outputs {
            let raw = map
                .get(name)
                .cloned()
                .ok_or_else(|| NodeError::fatal("invalid_output", format!("missing output `{name}`")))?;
            let value = decl
                .ty
                .coerce(Value::from_json(raw))
                .map_err(|e| NodeError::fatal("invalid_output", format!("output `{name}`: {e}")))?;
            output.outputs.insert(name.clone(), value);
        }
        let ignored: Vec<&String> = map.keys().filter(|k| !self.outputs.contains_key(*k)).collect();
        if !ignored.is_empty() {
            let names = ignored.iter().map(|k| Value::from(k.as_str()));
            output.metadata.insert("ignored_outputs".to_owned(), Value::array(names));
        }
        Ok(output)
    }
}

impl Node for CodeNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let limits = self.effective_limits(&ctx);
            let result = self.execute(&ctx).await?;
            let output = self.convert_outputs(result, &limits)?;
            Ok(output.meta("language", self.language.as_str()).into())
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        self.variables.iter().map(|v| v.value_selector.clone()).collect()
    }
}

fn embedded_supports(language: &str) -> bool {
    cfg!(feature = "js") && language == "javascript"
}

#[cfg(feature = "js")]
async fn run_embedded(
    code: Arc<str>,
    inputs: serde_json::Value,
    limits: CodeLimits,
) -> Result<serde_json::Value, NodeError> {
    let input_text = serde_json::to_string(&inputs)
        .map_err(|e| NodeError::fatal("invalid_input", format!("cannot encode inputs: {e}")))?;
    let job = move || js::run(&code, &input_text, &limits);

    // QuickJS is synchronous. Inside a tokio runtime, run it on the blocking pool so the
    // executor thread stays free; elsewhere run inline (bounded by the time limit).
    #[cfg(feature = "tokio")]
    {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            return handle
                .spawn_blocking(job)
                .await
                .map_err(|e| NodeError::new("sandbox", format!("code execution task failed: {e}")))?;
        }
    }
    job()
}

#[cfg(not(feature = "js"))]
async fn run_embedded(
    _code: Arc<str>,
    _inputs: serde_json::Value,
    _limits: CodeLimits,
) -> Result<serde_json::Value, NodeError> {
    Err(NodeError::fatal("unsupported", "embedded JavaScript is disabled (feature `js`)"))
}

fn json_depth(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(items) => 1 + items.iter().map(json_depth).max().unwrap_or(0),
        serde_json::Value::Object(map) => 1 + map.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Register the `code` node kind.
pub fn register(registry: &mut Registry) {
    registry.register(KIND, |doc| Ok(Arc::new(CodeNode::from_doc(doc)?) as Arc<dyn Node>));
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphrt_core::node::NodeOutcome;
    use graphrt_core::{Services, VariablePool};

    fn node(config: serde_json::Value) -> Result<CodeNode, ConfigError> {
        CodeNode::from_doc(&NodeDoc::new("c", KIND, config))
    }

    fn pool() -> VariablePool {
        let mut p = VariablePool::new();
        p.insert("start", "a", Value::Int(2));
        p.insert("start", "b", Value::Int(3));
        p.insert("start", "list", Value::from_json(serde_json::json!([1, 2, 3])));
        p.insert("env", "KEY", Value::Secret(Arc::from("sk-live")));
        p
    }

    async fn run(config: serde_json::Value) -> Result<NodeOutput, NodeError> {
        let n = node(config).map_err(|e| NodeError::fatal("config", e.to_string()))?;
        match n.run(NodeContext::for_test("c", pool())).await? {
            NodeOutcome::Completed(o) => Ok(o),
            NodeOutcome::Paused(_) => Err(NodeError::fatal("test", "unexpected pause")),
        }
    }

    fn vars() -> serde_json::Value {
        serde_json::json!([
            {"variable": "a", "value_selector": ["start", "a"]},
            {"variable": "b", "value_selector": ["start", "b"]},
            {"variable": "list", "value_selector": ["start", "list"]},
            {"variable": "key", "value_selector": ["env", "KEY"]},
        ])
    }

    #[tokio::test]
    async fn arithmetic() {
        let out = run(serde_json::json!({
            "code": "function main({a, b}) { return { sum: a + b, ratio: b / a }; }",
            "variables": vars(),
            "outputs": {"sum": {"type": "number"}, "ratio": {"type": "float"}},
        }))
        .await
        .unwrap();
        assert_eq!(out.outputs.get("sum"), Some(&Value::Int(5)));
        assert_eq!(out.outputs.get("ratio"), Some(&Value::Float(1.5)));
    }

    #[tokio::test]
    async fn object_and_array_returns_untyped() {
        let out = run(serde_json::json!({
            "code": "function main(args) { return { doubled: args.list.map(x => x * 2), obj: { k: 'v' } }; }",
            "variables": vars(),
        }))
        .await
        .unwrap();
        assert_eq!(out.outputs.get("doubled"), Some(&Value::from_json(serde_json::json!([2, 4, 6]))));
        assert_eq!(out.outputs.get("obj"), Some(&Value::from_json(serde_json::json!({"k": "v"}))));
    }

    #[tokio::test]
    async fn async_main_is_supported() {
        let out = run(serde_json::json!({
            "code": "async function main({a}) { const v = await Promise.resolve(a * 10); return { v }; }",
            "variables": vars(),
        }))
        .await
        .unwrap();
        assert_eq!(out.outputs.get("v"), Some(&Value::Int(20)));
    }

    #[tokio::test]
    async fn secrets_are_available_to_code() {
        let out = run(serde_json::json!({
            "code": "function main({key}) { return { len: key.length }; }",
            "variables": vars(),
        }))
        .await
        .unwrap();
        assert_eq!(out.outputs.get("len"), Some(&Value::Int(7)));
    }

    #[tokio::test]
    async fn type_validation_failure() {
        let err = run(serde_json::json!({
            "code": "function main() { return { n: 'not a number' }; }",
            "outputs": {"n": {"type": "number"}},
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind, "invalid_output");
        assert!(!err.retryable);
    }

    #[tokio::test]
    async fn missing_declared_output() {
        let err = run(serde_json::json!({
            "code": "function main() { return { other: 1 }; }",
            "outputs": {"n": {"type": "number"}},
        }))
        .await
        .unwrap_err();
        assert!(err.message.contains("missing output `n`"), "{err}");
    }

    #[tokio::test]
    async fn non_object_result() {
        let err = run(serde_json::json!({ "code": "function main() { return 42; }" })).await.unwrap_err();
        assert!(err.message.contains("must return an object"), "{err}");
    }

    #[tokio::test]
    async fn thrown_error_message_is_reported() {
        let err =
            run(serde_json::json!({ "code": "function main() { throw new Error('boom here'); }" })).await.unwrap_err();
        assert_eq!(err.kind, "code_error");
        assert!(err.message.contains("boom here"), "{err}");
    }

    #[tokio::test]
    async fn syntax_error_is_reported() {
        let err = run(serde_json::json!({ "code": "function main( { return {}; }" })).await.unwrap_err();
        assert_eq!(err.kind, "code_error");
    }

    #[tokio::test]
    async fn missing_main() {
        let err = run(serde_json::json!({ "code": "function notMain() { return {}; }" })).await.unwrap_err();
        assert!(err.message.contains("main"), "{err}");
    }

    #[tokio::test]
    async fn infinite_loop_is_interrupted() {
        let started = std::time::Instant::now();
        let err = run(serde_json::json!({
            "code": "function main() { while (true) {} }",
            "limits": {"time_ms": 200},
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind, "timeout");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn top_level_infinite_loop_is_interrupted() {
        let err = run(serde_json::json!({
            "code": "while (true) {} function main() { return {}; }",
            "limits": {"time_ms": 200},
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind, "timeout");
    }

    #[tokio::test]
    async fn memory_bomb_is_limited() {
        let err = run(serde_json::json!({
            "code": "function main() { const a = []; while (true) { a.push('x'.repeat(1 << 20)); } }",
            "limits": {"memory_bytes": 16777216, "time_ms": 5000},
        }))
        .await
        .unwrap_err();
        assert!(err.kind == "out_of_memory" || err.kind == "code_error", "{err}");
    }

    #[tokio::test]
    async fn output_size_is_limited() {
        let err = run(serde_json::json!({
            "code": "function main() { return { s: 'x'.repeat(100000) }; }",
            "limits": {"max_output_bytes": 1000},
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind, "invalid_output");
    }

    #[tokio::test]
    async fn no_host_capabilities() {
        let out = run(serde_json::json!({
            "code": "function main() { return { t: typeof setTimeout, f: typeof fetch, r: typeof require, s: typeof std, o: typeof os }; }",
        }))
        .await
        .unwrap();
        for key in ["t", "f", "r", "s", "o"] {
            assert_eq!(out.outputs.get(key), Some(&Value::from("undefined")), "{key}");
        }
    }

    #[test]
    fn runs_without_tokio_runtime_context() {
        let n = node(serde_json::json!({ "code": "function main() { return { ok: true }; }" })).unwrap();
        let out = futures::executor::block_on(n.run(NodeContext::for_test("c", pool()))).unwrap();
        assert!(matches!(out, NodeOutcome::Completed(o) if o.outputs.get("ok") == Some(&Value::Bool(true))));
    }

    #[tokio::test]
    async fn python_without_remote_sandbox() {
        let err = run(serde_json::json!({
            "code_language": "python3",
            "code": "def main():\n    return {}",
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind, "missing_service");
        assert!(err.message.contains("remote sandbox"));
    }

    #[derive(Debug)]
    struct EchoSandbox;

    impl RemoteSandbox for EchoSandbox {
        fn execute<'a>(
            &'a self,
            language: &'a str,
            _code: &'a str,
            inputs: serde_json::Value,
            _limits: CodeLimits,
        ) -> BoxFuture<'a, Result<serde_json::Value, NodeError>> {
            Box::pin(async move { Ok(serde_json::json!({ "language": language, "inputs": inputs })) })
        }
    }

    #[tokio::test]
    async fn python_with_remote_sandbox() {
        let n = node(serde_json::json!({
            "code_language": "python3",
            "code": "def main(a):\n    return {}",
            "variables": [{"variable": "a", "value_selector": ["start", "a"]}],
        }))
        .unwrap();
        let services = Services::new().with::<Arc<dyn RemoteSandbox>>(Arc::new(EchoSandbox));
        let ctx = NodeContext::for_test("c", pool()).with_services(Arc::new(services));
        let out = n.run(ctx).await.unwrap();
        let NodeOutcome::Completed(out) = out else { panic!("paused") };
        assert_eq!(out.outputs.get("language"), Some(&Value::from("python3")));
        assert_eq!(out.outputs.get("inputs"), Some(&Value::from_json(serde_json::json!({"a": 2}))));
    }

    #[test]
    fn config_validation() {
        assert!(node(serde_json::json!({ "code": "  " })).is_err());
        assert!(node(serde_json::json!({ "code_language": "jinja2", "code": "x" })).is_err());
        assert!(node(serde_json::json!({ "code_language": "js", "code": "x" })).is_ok());
    }

    #[test]
    fn registers_kind() {
        let mut r = Registry::new();
        register(&mut r);
        assert!(r.contains(KIND));
    }
}
