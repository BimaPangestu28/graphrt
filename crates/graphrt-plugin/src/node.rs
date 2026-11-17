//! Graph nodes and tools backed by a plugin transport.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::catalog::{Handles, NodeDescriptor};
use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{
    ConfigError, Node, NodeContext, NodeError, NodeOutcome, NodeOutput, NodeResult, NodeRole, PauseRequest,
};
use graphrt_core::pool::Selector;
use graphrt_core::tool::{ToolError, ToolOutput, ToolRuntime, ToolSpec};
use graphrt_core::value::Value;
use tokio::sync::Semaphore;

use crate::conn::NotifySink;
use crate::error::CallError;
use crate::manifest::PluginManifest;
use crate::protocol::{RunResultMsg, ToolResultMsg};
use crate::render::{collect_selectors, render_config};
use crate::transport::{Transport, with_timeout};

/// State shared by every node and tool of one plugin.
pub(crate) struct PluginShared {
    pub name: String,
    pub transport: Arc<dyn Transport>,
    pub timeout: Duration,
    pub expose_secrets: bool,
    pub semaphore: Arc<Semaphore>,
    pub tools: Vec<ToolSpec>,
}

impl fmt::Debug for PluginShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginShared")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

impl PluginShared {
    pub(crate) fn new(manifest: &PluginManifest, transport: Arc<dyn Transport>) -> Self {
        Self {
            name: manifest.name.clone(),
            transport,
            timeout: Duration::from_millis(manifest.limits.timeout_ms),
            expose_secrets: manifest.permissions.secrets,
            semaphore: Arc::new(Semaphore::new(manifest.limits.max_concurrency)),
            tools: manifest.tools.clone(),
        }
    }
}

/// A node whose work is done by a plugin.
pub(crate) struct PluginNode {
    shared: Arc<PluginShared>,
    kind: String,
    config: serde_json::Map<String, serde_json::Value>,
    selectors: Vec<Selector>,
    role: NodeRole,
    /// Declared handles for `Handles::Fixed` nodes; a returned branch must be one of them.
    fixed_handles: Option<Vec<String>>,
}

impl fmt::Debug for PluginNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginNode")
            .field("plugin", &self.shared.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Build a plugin node from its document, checking required config keys.
pub(crate) fn build_node(
    shared: &Arc<PluginShared>,
    descriptor: &NodeDescriptor,
    doc: &NodeDoc,
) -> Result<Arc<dyn Node>, ConfigError> {
    if let Some(required) = descriptor.config_schema.get("required").and_then(|r| r.as_array()) {
        for key in required.iter().filter_map(|k| k.as_str()) {
            if !doc.config.contains_key(key) {
                return Err(ConfigError::new(doc, format!("missing required config field `{key}`")));
            }
        }
    }
    let role = match descriptor.handles {
        Handles::Single => NodeRole::Executable,
        _ => NodeRole::Branch,
    };
    let fixed_handles = match &descriptor.handles {
        Handles::Fixed { handles } => Some(handles.clone()),
        _ => None,
    };
    Ok(Arc::new(PluginNode {
        shared: shared.clone(),
        kind: descriptor.kind.clone(),
        config: doc.config.clone(),
        selectors: collect_selectors(&doc.config),
        role,
        fixed_handles,
    }))
}

impl Node for PluginNode {
    fn role(&self) -> NodeRole {
        self.role
    }

    fn inputs(&self) -> Vec<Selector> {
        self.selectors.clone()
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let plugin = self.shared.name.as_str();
            let _permit = self
                .shared
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| NodeError::new("plugin_closed", format!("plugin `{plugin}` is shutting down")))?;

            let config = render_config(&self.config, &ctx.pool, self.shared.expose_secrets);
            let id = self.shared.transport.next_id();
            // Resume input follows the same secret policy as the config.
            let expose = self.shared.expose_secrets;
            let resume = ctx.resume.as_ref().map(|r| {
                let input = match r.input.as_ref() {
                    Some(v) if expose => v.to_json_exposed(),
                    Some(v) => v.to_json(),
                    None => serde_json::Value::Null,
                };
                serde_json::json!({"state": r.state, "input": input})
            });
            let params = serde_json::json!({
                "request_id": id,
                "kind": self.kind,
                "node_id": ctx.node_id.as_ref(),
                "attempt": ctx.attempt,
                "config": config,
                "resume": resume,
            });

            let events = ctx.clone();
            let sink: NotifySink = Arc::new(move |n| match n.method.as_str() {
                "node.chunk" => {
                    let field = n.params.get("field").and_then(|f| f.as_str()).unwrap_or("text");
                    if let Some(chunk) = n.params.get("chunk").and_then(|c| c.as_str()) {
                        events.chunk(field, chunk);
                    }
                }
                "node.progress" => {
                    let name = n.params.get("name").and_then(|f| f.as_str()).unwrap_or("progress");
                    let data = n.params.get("data").cloned().unwrap_or(serde_json::Value::Null);
                    events.progress(name, Value::from_json(data));
                }
                "log" => {
                    let level = n.params.get("level").and_then(|f| f.as_str()).unwrap_or("info");
                    let message = n.params.get("message").and_then(|f| f.as_str()).unwrap_or_default();
                    events.progress(
                        "plugin_log",
                        Value::object([("level", Value::from(level)), ("message", Value::from(message))]),
                    );
                }
                _ => {}
            });

            let timeout = match ctx.remaining() {
                Some(left) => left.min(self.shared.timeout),
                None => self.shared.timeout,
            };
            // If the engine's own deadline fires first, this future is dropped before
            // `track` runs; the guard still counts that as a timeout so a wedged
            // process is detected and restarted.
            let mut guard = TimeoutGuard {
                transport: Arc::clone(&self.shared.transport),
                clock: Arc::clone(&ctx.clock),
                deadline_ms: ctx.deadline_ms,
                settled: false,
            };
            let call = self.shared.transport.call(id, "node.run", params, Some(sink), true, &ctx.clock);
            let result = with_timeout(&ctx.clock, timeout, call).await;
            guard.settled = true;
            track(self.shared.transport.as_ref(), &result);
            let result = result.map_err(|e| e.into_node_error(plugin))?;
            outcome_from(plugin, result, self.role, self.fixed_handles.as_deref())
        })
    }
}

/// Counts a request that was cancelled at its deadline (future dropped by the
/// engine) as a timeout. Requests cancelled for other reasons (abort, pause) are
/// not counted.
struct TimeoutGuard {
    transport: Arc<dyn Transport>,
    clock: Arc<dyn graphrt_core::node::Clock>,
    deadline_ms: Option<u64>,
    settled: bool,
}

impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        if let Some(deadline) = self.deadline_ms {
            if self.clock.now_ms() >= deadline {
                self.transport.note_timeout();
            }
        }
    }
}

/// Feed request outcomes to the transport's liveness tracking.
fn track<T>(transport: &dyn Transport, result: &Result<T, CallError>) {
    match result {
        Err(CallError::Timeout(_)) => transport.note_timeout(),
        Ok(_) | Err(CallError::Remote { .. }) => transport.note_answer(),
        Err(_) => {}
    }
}

fn outcome_from(plugin: &str, result: serde_json::Value, role: NodeRole, fixed: Option<&[String]>) -> NodeResult {
    let msg: RunResultMsg = serde_json::from_value(result).map_err(|e| {
        NodeError::fatal("plugin_protocol", format!("plugin `{plugin}` returned an invalid result: {e}"))
    })?;
    if let Some(pause) = msg.pause {
        return Ok(NodeOutcome::Paused(PauseRequest {
            kind: pause.kind,
            data: Value::from_json(pause.data),
            state: pause.state,
        }));
    }
    let mut out = NodeOutput::new();
    for (k, v) in msg.outputs {
        out.outputs.insert(k, Value::from_json(v));
    }
    for (k, v) in msg.metadata {
        out.metadata.insert(k, Value::from_json(v));
    }
    match (role, msg.branch) {
        (NodeRole::Branch, None) => {
            return Err(NodeError::fatal(
                "plugin_protocol",
                format!("plugin `{plugin}`: branching node returned no `branch`"),
            ));
        }
        (NodeRole::Branch, Some(handle)) => {
            if let Some(allowed) = fixed {
                if !allowed.contains(&handle) {
                    return Err(NodeError::fatal(
                        "plugin_protocol",
                        format!("plugin `{plugin}`: branch `{handle}` is not one of the declared handles {allowed:?}"),
                    ));
                }
            }
            out.branch = Some(handle);
        }
        // Non-branching nodes take every edge; a stray `branch` is ignored.
        (_, _) => {}
    }
    out.usage = msg.usage;
    out.writes = msg.writes.into_iter().map(|w| (w.selector, Value::from_json(w.value))).collect();
    Ok(NodeOutcome::Completed(out))
}

/// Tools exposed by a plugin, as a [`ToolRuntime`].
#[derive(Debug)]
pub(crate) struct PluginTools {
    pub shared: Arc<PluginShared>,
}

impl PluginTools {
    fn error(&self, tool: &str, e: CallError) -> ToolError {
        ToolError { tool: tool.to_owned(), message: format!("plugin `{}`: {e}", self.shared.name) }
    }
}

impl ToolRuntime for PluginTools {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        Box::pin(async move {
            if !self.shared.tools.is_empty() {
                return Ok(self.shared.tools.clone());
            }
            let clock = graphrt_core::node::default_clock();
            let id = self.shared.transport.next_id();
            let call = self.shared.transport.call(id, "tool.list", serde_json::json!({}), None, false, &clock);
            match with_timeout(&clock, self.shared.timeout, call).await {
                Ok(result) => {
                    let tools = result.get("tools").cloned().ok_or_else(|| {
                        self.error("tool.list", CallError::Protocol("result has no `tools` array".into()))
                    })?;
                    serde_json::from_value(tools)
                        .map_err(|e| self.error("tool.list", CallError::Protocol(e.to_string())))
                }
                Err(e) if e.is_method_not_found() => Ok(Vec::new()),
                Err(e) => Err(self.error("tool.list", e)),
            }
        })
    }

    fn call<'a>(&'a self, name: &'a str, arguments: serde_json::Value) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let _permit = self
                .shared
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| self.error(name, CallError::Crashed("closed".into())))?;
            let clock = graphrt_core::node::default_clock();
            let id = self.shared.transport.next_id();
            let params = serde_json::json!({"request_id": id, "name": name, "arguments": arguments});
            let call = self.shared.transport.call(id, "tool.call", params, None, true, &clock);
            let result = with_timeout(&clock, self.shared.timeout, call).await;
            track(self.shared.transport.as_ref(), &result);
            let result = result.map_err(|e| self.error(name, e))?;
            let msg: ToolResultMsg =
                serde_json::from_value(result).map_err(|e| self.error(name, CallError::Protocol(e.to_string())))?;
            Ok(ToolOutput { text: msg.text, data: Value::from_json(msg.data), is_error: msg.is_error })
        })
    }
}
