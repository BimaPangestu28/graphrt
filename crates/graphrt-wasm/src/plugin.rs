//! Loading plugins and exposing their node kinds.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::catalog::{Handles, NodeDescriptor};
use graphrt_core::dsl::NodeDoc;
use graphrt_core::event::Usage;
use graphrt_core::node::{
    ConfigError, Node, NodeContext, NodeError, NodeFactory, NodeOutcome, NodeOutput, NodeResult, NodeRole,
    PauseRequest, Registry,
};
use graphrt_core::pool::Selector;
use graphrt_core::value::Value;

use crate::abi::{Manifest, ResumeData, RunRequest, RunResponse};
use crate::error::WasmError;
use crate::host::{Call, CallOutput, Compiled, LogLine};
use crate::limits::WasmLimits;
use crate::resolve::{collect_selectors, resolve_config};

struct Inner {
    compiled: Compiled,
    manifest: Manifest,
}

/// A loaded WebAssembly plugin. Cheap to clone; safe to share across runs and threads.
#[derive(Clone)]
pub struct WasmPlugin {
    inner: Arc<Inner>,
}

impl fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<&str> = self.inner.manifest.nodes.iter().map(|n| n.kind.as_str()).collect();
        f.debug_struct("WasmPlugin").field("name", &self.inner.manifest.name).field("kinds", &kinds).finish()
    }
}

impl WasmPlugin {
    /// Validate, compile and describe a module from memory.
    pub fn load(bytes: &[u8], limits: WasmLimits) -> Result<Self, WasmError> {
        let compiled = Compiled::new(bytes, limits)?;
        let out = compiled.call(Call::Describe, None)?;
        let mut manifest: Manifest =
            serde_json::from_slice(&out.bytes).map_err(|e| WasmError::Manifest(e.to_string()))?;
        validate_manifest(&mut manifest)?;
        Ok(Self { inner: Arc::new(Inner { compiled, manifest }) })
    }

    /// Read and load a `.wasm` file.
    pub fn load_file(path: impl AsRef<Path>, limits: WasmLimits) -> Result<Self, WasmError> {
        let path = path.as_ref();
        let io = |e: std::io::Error| WasmError::Io { path: path.display().to_string(), message: e.to_string() };
        let size = std::fs::metadata(path).map_err(io)?.len();
        if usize::try_from(size).map_or(true, |s| s > limits.module_bytes) {
            return Err(WasmError::ModuleTooLarge {
                size: usize::try_from(size).unwrap_or(usize::MAX),
                limit: limits.module_bytes,
            });
        }
        let bytes = std::fs::read(path).map_err(io)?;
        Self::load(&bytes, limits)
    }

    /// The plugin manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.inner.manifest
    }

    /// Catalog descriptors of every node kind in the plugin.
    pub fn descriptors(&self) -> Vec<NodeDescriptor> {
        self.inner.manifest.nodes.clone()
    }

    /// Limits applied to calls.
    pub fn limits(&self) -> &WasmLimits {
        self.inner.compiled.limits()
    }

    /// Register every node kind (with its descriptor) in `registry`.
    /// Existing kinds with the same name are replaced.
    pub fn register(&self, registry: &mut Registry) {
        for descriptor in &self.inner.manifest.nodes {
            let factory = self.make_factory(descriptor);
            registry.register_described(descriptor.clone(), move |doc| factory(doc));
        }
    }

    /// Factory for one node kind.
    pub fn factory(&self, kind: &str) -> Result<NodeFactory, WasmError> {
        let descriptor = self
            .inner
            .manifest
            .nodes
            .iter()
            .find(|d| d.kind == kind)
            .ok_or_else(|| WasmError::UnknownKind(kind.to_owned()))?;
        Ok(self.make_factory(descriptor))
    }

    fn make_factory(&self, descriptor: &NodeDescriptor) -> NodeFactory {
        let inner = self.inner.clone();
        let kind = descriptor.kind.clone();
        let role = if matches!(descriptor.handles, Handles::Single) { NodeRole::Executable } else { NodeRole::Branch };
        let fixed_handles = match &descriptor.handles {
            Handles::Fixed { handles } => Some(handles.clone()),
            _ => None,
        };
        Arc::new(move |doc: &NodeDoc| -> Result<Arc<dyn Node>, ConfigError> {
            let config = serde_json::Value::Object(doc.config.clone());
            let mut selectors = Vec::new();
            collect_selectors(&config, &mut selectors);
            Ok(Arc::new(WasmNode {
                inner: inner.clone(),
                kind: kind.clone(),
                node_id: doc.id.clone(),
                config,
                role,
                fixed_handles: fixed_handles.clone(),
                selectors,
            }))
        })
    }
}

fn validate_manifest(manifest: &mut Manifest) -> Result<(), WasmError> {
    if manifest.name.trim().is_empty() {
        return Err(WasmError::Manifest("`name` must not be empty".into()));
    }
    if manifest.nodes.is_empty() {
        return Err(WasmError::Manifest("plugin declares no node kinds".into()));
    }
    let mut seen = HashSet::new();
    for node in &mut manifest.nodes {
        if node.kind.trim().is_empty() || node.kind.contains(char::is_whitespace) {
            return Err(WasmError::Manifest(format!("invalid node kind `{}`", node.kind)));
        }
        if !seen.insert(node.kind.clone()) {
            return Err(WasmError::DuplicateKind(node.kind.clone()));
        }
        if node.container {
            return Err(WasmError::Manifest(format!(
                "`{}`: container nodes are not supported by wasm plugins",
                node.kind
            )));
        }
        if node.source.is_empty() {
            node.source = format!("wasm:{}", manifest.name);
        }
    }
    Ok(())
}

/// A node implemented by a guest module.
struct WasmNode {
    inner: Arc<Inner>,
    kind: String,
    node_id: String,
    config: serde_json::Value,
    role: NodeRole,
    fixed_handles: Option<Vec<String>>,
    selectors: Vec<Selector>,
}

impl fmt::Debug for WasmNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmNode").field("kind", &self.kind).field("node_id", &self.node_id).finish()
    }
}

impl Node for WasmNode {
    fn role(&self) -> NodeRole {
        self.role
    }

    fn inputs(&self) -> Vec<Selector> {
        self.selectors.clone()
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let limits = self.inner.compiled.limits();
            let config = resolve_config(&self.config, &ctx.pool, limits.expose_secrets);
            let resume = ctx.resume.as_ref().map(|r| ResumeData {
                state: r.state.clone(),
                input: r.input.as_ref().map(Value::to_json).unwrap_or(serde_json::Value::Null),
            });
            let request = RunRequest {
                kind: self.kind.clone(),
                node_id: self.node_id.clone(),
                config,
                attempt: ctx.attempt,
                resume,
            };
            let input = serde_json::to_vec(&request)
                .map_err(|e| NodeError::fatal("wasm_internal", format!("encoding request: {e}")))?;
            let output = execute(self.inner.clone(), input, ctx).await?;
            self.decode(output)
        })
    }
}

impl WasmNode {
    fn decode(&self, output: CallOutput) -> NodeResult {
        let response: RunResponse = serde_json::from_slice(&output.bytes)
            .map_err(|e| NodeError::from(WasmError::InvalidOutput(format!("response is not valid JSON: {e}"))))?;
        if let Some(err) = response.error {
            let kind = if err.kind.trim().is_empty() { "plugin_error".to_owned() } else { err.kind };
            let mut e = NodeError::new(kind, err.message);
            e.retryable = err.retryable;
            return Err(e);
        }
        if let Some(pause) = response.pause {
            return Ok(NodeOutcome::Paused(PauseRequest {
                kind: pause.kind,
                data: Value::from_json(pause.data),
                state: pause.state,
            }));
        }
        let Some(outputs) = response.outputs else {
            return Err(WasmError::InvalidOutput("response has none of `outputs`, `pause` or `error`".into()).into());
        };
        let mut out = NodeOutput::new();
        for (key, value) in outputs {
            out.outputs.insert(key, Value::from_json(value));
        }
        match (self.role, response.branch) {
            (NodeRole::Branch, None) => {
                return Err(
                    WasmError::InvalidOutput(format!("branching node `{}` returned no `branch`", self.kind)).into()
                );
            }
            (NodeRole::Branch, Some(handle)) => {
                if let Some(allowed) = &self.fixed_handles {
                    if !allowed.contains(&handle) {
                        return Err(WasmError::InvalidOutput(format!(
                            "branch `{handle}` is not one of the declared handles {allowed:?}"
                        ))
                        .into());
                    }
                }
                out.branch = Some(handle);
            }
            // Non-branching nodes always take every edge; a stray `branch` is ignored.
            (_, _) => {}
        }
        if let Some(u) = response.usage {
            out.usage = Some(Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens.max(u.prompt_tokens.saturating_add(u.completion_tokens)),
            });
        }
        for (key, value) in response.metadata.unwrap_or_default() {
            out.metadata.insert(key, Value::from_json(value));
        }
        if !output.logs.is_empty() {
            out.metadata.insert("logs".into(), logs_value(&output.logs));
        }
        if output.dropped_logs > 0 {
            out.metadata
                .insert("dropped_logs".into(), Value::Int(i64::try_from(output.dropped_logs).unwrap_or(i64::MAX)));
        }
        Ok(NodeOutcome::Completed(out))
    }
}

fn logs_value(logs: &[LogLine]) -> Value {
    Value::array(logs.iter().map(|l| {
        let level = ["trace", "debug", "info", "warn", "error"].get(usize::from(l.level)).copied().unwrap_or("info");
        Value::object([("level", Value::from(level)), ("message", Value::from(l.message.as_str()))])
    }))
}

/// Run the guest call. Inside a tokio runtime (feature `tokio`) the interpreter runs on
/// the blocking pool so it never stalls other nodes; otherwise it runs inline. Either
/// way the call is bounded by the fuel limit.
async fn execute(inner: Arc<Inner>, input: Vec<u8>, ctx: NodeContext) -> Result<CallOutput, NodeError> {
    #[cfg(feature = "tokio")]
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            let joined = tokio::task::spawn_blocking(move || inner.compiled.call(Call::Run(&input), Some(ctx))).await;
            return match joined {
                Ok(result) => result.map_err(NodeError::from),
                Err(e) => Err(NodeError::fatal("wasm_internal", format!("guest task failed: {e}"))),
            };
        }
    }
    inner.compiled.call(Call::Run(&input), Some(ctx)).map_err(NodeError::from)
}
