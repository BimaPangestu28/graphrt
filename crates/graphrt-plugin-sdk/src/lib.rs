//! # graphrt-plugin-sdk
//!
//! Write graphrt plugins in Rust. A plugin is a small executable that serves
//! node kinds (and optionally tools) over the graphrt plugin protocol
//! (newline-delimited JSON-RPC 2.0 on stdin/stdout).
//!
//! ```no_run
//! use graphrt_plugin_sdk::{Plugin, RunResult};
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> std::io::Result<()> {
//!     Plugin::new("acme")
//!         .node("acme.len", |ctx, req| async move {
//!             let text = req.config["text"].as_str().unwrap_or_default().to_owned();
//!             ctx.chunk("log", "counting...");
//!             Ok(RunResult::new().output("length", text.chars().count()))
//!         })
//!         .serve_stdio()
//!         .await
//! }
//! ```
//!
//! Pair the executable with a `graphrt-plugin.json` manifest (see the graphrt
//! plugin documentation). Never print to stdout yourself: it carries the protocol.
//! Use [`RunCtx::log`] or stderr instead.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc};

/// Protocol version implemented by this SDK.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest accepted incoming message.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
type NodeHandler = Arc<dyn Fn(RunCtx, RunRequest) -> BoxFuture<Result<Outcome, PluginError>> + Send + Sync>;
type ToolHandler = Arc<dyn Fn(ToolCall) -> BoxFuture<Result<ToolResult, PluginError>> + Send + Sync>;

/// A `node.run` request.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRequest {
    pub request_id: String,
    /// Node kind being run.
    pub kind: String,
    /// Id of the node in the workflow.
    pub node_id: String,
    /// 1-based attempt number (increments on retries).
    pub attempt: u32,
    /// Node configuration with variables already resolved by the host.
    pub config: Value,
    /// Present when the node resumes after pausing.
    pub resume: Option<Resume>,
}

/// Resume data for a node that paused.
#[derive(Clone, Debug, PartialEq)]
pub struct Resume {
    /// The state returned with [`Outcome::Pause`].
    pub state: Value,
    /// Host-supplied input (e.g. a submitted form), `null` if none.
    pub input: Value,
}

/// Successful node result.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RunResult {
    pub outputs: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub writes: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

impl RunResult {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an output. Values that fail to serialize become `null`.
    pub fn output(mut self, name: impl Into<String>, value: impl Serialize) -> Self {
        self.outputs.insert(name.into(), serde_json::to_value(value).unwrap_or(Value::Null));
        self
    }

    /// Select an outgoing branch handle (for nodes declaring handles).
    pub fn branch(mut self, handle: impl Into<String>) -> Self {
        self.branch = Some(handle.into());
        self
    }

    /// Write a variable outside the node's outputs, e.g. `["conversation", "topic"]`.
    pub fn write(mut self, selector: &[&str], value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).unwrap_or(Value::Null);
        self.writes.push(json!({"selector": selector, "value": value}));
        self
    }

    /// Report token usage.
    pub fn usage(mut self, prompt_tokens: u64, completion_tokens: u64) -> Self {
        self.usage = Some(json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens.saturating_add(completion_tokens),
        }));
        self
    }

    /// Attach observability metadata.
    pub fn meta(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        self.metadata.insert(key.into(), serde_json::to_value(value).unwrap_or(Value::Null));
        self
    }
}

/// What a node handler returns.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Done(RunResult),
    /// Pause the run; the node is called again with [`RunRequest::resume`].
    Pause {
        kind: String,
        data: Value,
        state: Value,
    },
}

impl From<RunResult> for Outcome {
    fn from(r: RunResult) -> Self {
        Outcome::Done(r)
    }
}

impl Outcome {
    /// Pause with a kind (e.g. `approval`), data shown to the host, and private state.
    pub fn pause(kind: impl Into<String>, data: impl Serialize, state: impl Serialize) -> Self {
        Outcome::Pause {
            kind: kind.into(),
            data: serde_json::to_value(data).unwrap_or(Value::Null),
            state: serde_json::to_value(state).unwrap_or(Value::Null),
        }
    }
}

/// An error reported to the host.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginError {
    /// Error category (becomes the node error type).
    pub kind: String,
    pub message: String,
    /// Whether the host may retry.
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
}

impl PluginError {
    /// A non-retryable error.
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self { kind: kind.into(), message: message.into(), retryable: false, retry_after_ms: None }
    }

    /// A retryable error.
    pub fn retryable(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self { retryable: true, ..Self::new(kind, message) }
    }

    pub fn retry_after(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }

    /// Invalid configuration or input.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for PluginError {}

/// A `tool.call` request.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    /// Per-call values from the host, outside the model-written arguments.
    pub context: ToolCallContext,
}

/// The `context` of a `tool.call`: per-run values the host sends next to the
/// arguments (e.g. the end user's JWT from an `agent` node's `tool_context`).
/// Only sent to plugins that request `permissions.secrets` and are trusted;
/// empty otherwise. `Debug` shows only the names.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ToolCallContext {
    /// Secret name → plaintext value.
    pub secrets: BTreeMap<String, String>,
}

impl ToolCallContext {
    /// The plaintext of secret `name`.
    pub fn secret(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }

    /// Read `params.context` (missing or malformed entries are ignored).
    fn from_params(params: &Value) -> Self {
        let secrets = params
            .pointer("/context/secrets")
            .and_then(Value::as_object)
            .map(|map| map.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
            .unwrap_or_default();
        Self { secrets }
    }
}

impl fmt::Debug for ToolCallContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCallContext").field("secrets", &self.secrets.keys().collect::<Vec<_>>()).finish()
    }
}

/// A tool result.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ToolResult {
    pub text: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub data: Value,
    pub is_error: bool,
    /// Images shown to the model (at most 4; png, jpeg, webp or gif; 5 MiB
    /// each). Dropped with a note in `text` when the host does not support them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ToolImage>,
}

/// An image in a tool result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ToolImage {
    /// Inline bytes, base64 encoded (standard alphabet).
    Base64 { media_type: String, data_base64: String },
    /// A remote `https://` image.
    Url { url: String },
}

/// Feature the host lists in `initialize` when `tool.call` results may carry images.
pub const TOOL_IMAGES_FEATURE: &str = "tool_images";

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, ..Self::default() }
    }

    /// Attach an inline image (`media_type` such as `image/png`, data in base64).
    pub fn with_image_base64(mut self, media_type: impl Into<String>, data_base64: impl Into<String>) -> Self {
        self.images.push(ToolImage::Base64 { media_type: media_type.into(), data_base64: data_base64.into() });
        self
    }

    /// Attach a remote `https://` image.
    pub fn with_image_url(mut self, url: impl Into<String>) -> Self {
        self.images.push(ToolImage::Url { url: url.into() });
        self
    }

    /// For hosts that did not announce [`TOOL_IMAGES_FEATURE`]: drop the
    /// images and say so in the text, so older hosts still accept the result.
    fn without_unsupported_images(mut self, host_supports_images: bool) -> Self {
        if host_supports_images || self.images.is_empty() {
            return self;
        }
        let count = std::mem::take(&mut self.images).len();
        let note = format!("[{count} image(s) omitted: the graphrt host does not support image tool results]");
        self.text = if self.text.is_empty() { note } else { format!("{}\n{note}", self.text) };
        self
    }

    pub fn with_data(mut self, data: impl Serialize) -> Self {
        self.data = serde_json::to_value(data).unwrap_or(Value::Null);
        self
    }
}

/// Per-request context: streaming, progress, logs and cancellation.
#[derive(Clone)]
pub struct RunCtx {
    request_id: String,
    out: mpsc::UnboundedSender<String>,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
}

impl fmt::Debug for RunCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunCtx").field("request_id", &self.request_id).finish_non_exhaustive()
    }
}

impl RunCtx {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Stream text for output `field`.
    pub fn chunk(&self, field: &str, chunk: &str) {
        self.notify("node.chunk", json!({"request_id": self.request_id, "field": field, "chunk": chunk}));
    }

    /// Report structured progress.
    pub fn progress(&self, name: &str, data: impl Serialize) {
        let data = serde_json::to_value(data).unwrap_or(Value::Null);
        self.notify("node.progress", json!({"request_id": self.request_id, "name": name, "data": data}));
    }

    /// Log a line on the host (`debug`, `info`, `warn` or `error`).
    pub fn log(&self, level: &str, message: &str) {
        self.notify("log", json!({"level": level, "message": message}));
    }

    /// Whether the host cancelled this request.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves when the host cancels this request.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.cancel_notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    fn notify(&self, method: &str, params: Value) {
        let _host_gone = self.out.send(json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
    }
}

/// A plugin: a set of node kinds and tools.
#[derive(Clone, Default)]
pub struct Plugin {
    name: String,
    nodes: HashMap<String, NodeHandler>,
    descriptors: Vec<Value>,
    tools: HashMap<String, ToolHandler>,
    tool_specs: Vec<Value>,
}

impl fmt::Debug for Plugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut kinds: Vec<&String> = self.nodes.keys().collect();
        kinds.sort();
        f.debug_struct("Plugin").field("name", &self.name).field("nodes", &kinds).finish_non_exhaustive()
    }
}

impl Plugin {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }

    /// Register a node kind handler.
    pub fn node<F, Fut, O>(mut self, kind: impl Into<String>, handler: F) -> Self
    where
        F: Fn(RunCtx, RunRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, PluginError>> + Send + 'static,
        O: Into<Outcome>,
    {
        let handler: NodeHandler = Arc::new(move |ctx, req| {
            let fut = handler(ctx, req);
            Box::pin(async move { fut.await.map(Into::into) })
        });
        self.nodes.insert(kind.into(), handler);
        self
    }

    /// Publish a node descriptor from `initialize` (optional; the manifest is
    /// usually enough). `descriptor` follows the graphrt node catalog format.
    pub fn describe(mut self, descriptor: Value) -> Self {
        self.descriptors.push(descriptor);
        self
    }

    /// Register a tool. `spec` is `{name, description, parameters}`.
    pub fn tool<F, Fut>(mut self, spec: Value, handler: F) -> Self
    where
        F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolResult, PluginError>> + Send + 'static,
    {
        let name = spec.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let handler: ToolHandler = Arc::new(move |call| Box::pin(handler(call)));
        self.tools.insert(name, handler);
        self.tool_specs.push(spec);
        self
    }

    /// Serve the protocol on stdin/stdout until stdin closes or `shutdown` arrives.
    pub async fn serve_stdio(self) -> std::io::Result<()> {
        self.serve(tokio::io::stdin(), tokio::io::stdout()).await
    }

    /// Serve the protocol on any reader/writer pair (useful for tests and
    /// in-process plugins).
    pub async fn serve<R, W>(self, reader: R, writer: W) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let plugin = Arc::new(self);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let writer_task = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(mut line) = out_rx.recv().await {
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
        });

        let running: Arc<Mutex<HashMap<String, Running>>> = Arc::new(Mutex::new(HashMap::new()));
        let mut lines = BufReader::new(reader);
        let mut buf = Vec::new();
        let mut shutdown = false;
        // The host announces its line limit and optional features in `initialize`.
        let mut limit = MAX_MESSAGE_BYTES;
        let mut host_supports_images = false;
        loop {
            buf.clear();
            match read_line_capped(&mut lines, &mut buf, limit).await? {
                Line::Eof => break,
                Line::Oversized => {
                    eprintln!("graphrt plugin {}: dropped a message above {limit} bytes", plugin.name);
                    let id = recover_id(&buf);
                    let msg = format!("message exceeds the {limit}-byte limit");
                    send(&out_tx, error_response(&id, -32600, &msg, Some(json!({"kind": "message_too_large"}))));
                    continue;
                }
                Line::Complete => {}
            }
            let Ok(text) = std::str::from_utf8(&buf) else {
                eprintln!("graphrt plugin {}: message is not valid UTF-8", plugin.name);
                let data = Some(json!({"kind": "plugin_protocol"}));
                send(&out_tx, error_response(&recover_id(&buf), -32700, "message is not valid UTF-8", data));
                continue;
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(text) else {
                eprintln!("graphrt plugin {}: unreadable message", plugin.name);
                let data = Some(json!({"kind": "plugin_protocol"}));
                send(&out_tx, error_response(&recover_id(&buf), -32700, "parse error", data));
                continue;
            };
            if let Some(announced) = msg
                .get("params")
                .and_then(|p| p.get("max_message_bytes"))
                .and_then(Value::as_u64)
                .filter(|_| msg.get("method").and_then(Value::as_str) == Some("initialize"))
            {
                limit = usize::try_from(announced).unwrap_or(usize::MAX).max(1024);
            }
            let id = msg.get("id").cloned();
            let method = msg.get("method").and_then(Value::as_str).unwrap_or_default().to_owned();
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            match (method.as_str(), id) {
                ("shutdown", _) => {
                    shutdown = true;
                    break;
                }
                ("node.cancel", None) => {
                    let request_id = params.get("request_id").and_then(Value::as_str).unwrap_or_default();
                    if let Some(r) = running.lock().ok().and_then(|mut m| m.remove(request_id)) {
                        r.cancelled.store(true, Ordering::SeqCst);
                        r.notify.notify_waiters();
                        r.task.abort();
                    }
                }
                (_, None) => {}
                ("initialize", Some(id)) => {
                    host_supports_images = params
                        .get("features")
                        .and_then(Value::as_array)
                        .is_some_and(|f| f.iter().any(|v| v.as_str() == Some(TOOL_IMAGES_FEATURE)));
                    let mut result = Map::new();
                    result.insert("protocol_version".into(), json!(PROTOCOL_VERSION));
                    result.insert("name".into(), json!(plugin.name));
                    if !plugin.descriptors.is_empty() {
                        result.insert("nodes".into(), Value::Array(plugin.descriptors.clone()));
                    }
                    if !plugin.tool_specs.is_empty() {
                        result.insert("tools".into(), Value::Array(plugin.tool_specs.clone()));
                    }
                    let result = Value::Object(result);
                    send(&out_tx, ok_response(&id, result));
                }
                ("node.run", Some(id)) => plugin.start_run(id, params, &out_tx, &running),
                ("tool.list", Some(id)) => {
                    send(&out_tx, ok_response(&id, json!({"tools": plugin.tool_specs})));
                }
                ("tool.call", Some(id)) => {
                    plugin.start_tool(id, params, &out_tx, ToolHost { supports_images: host_supports_images, limit });
                }
                (other, Some(id)) => {
                    send(&out_tx, error_response(&id, -32601, &format!("method `{other}` not found"), None));
                }
            }
        }
        // `shutdown` stops in-flight work; end of input lets it finish and reply.
        let in_flight: Vec<Running> =
            running.lock().map(|mut m| m.drain().map(|(_, r)| r).collect()).unwrap_or_default();
        for r in in_flight {
            if shutdown {
                r.task.abort();
            } else {
                let _finished_or_aborted = r.task.await;
            }
        }
        // Tool tasks hold their own sender, so the writer drains until they finish.
        drop(out_tx);
        let _writer_done = writer_task.await;
        Ok(())
    }

    fn start_run(
        &self,
        id: Value,
        params: Value,
        out: &mpsc::UnboundedSender<String>,
        running: &Arc<Mutex<HashMap<String, Running>>>,
    ) {
        let request_id =
            params.get("request_id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| id_text(&id));
        let kind = params.get("kind").and_then(Value::as_str).unwrap_or_default().to_owned();
        let Some(handler) = self.nodes.get(&kind).cloned() else {
            let msg = format!("plugin `{}` has no node kind `{kind}`", self.name);
            send(out, error_response(&id, -32602, &msg, Some(json!({"kind": "unknown_kind"}))));
            return;
        };
        let resume = params.get("resume").filter(|r| !r.is_null()).map(|r| Resume {
            state: r.get("state").cloned().unwrap_or(Value::Null),
            input: r.get("input").cloned().unwrap_or(Value::Null),
        });
        let req = RunRequest {
            request_id: request_id.clone(),
            kind,
            node_id: params.get("node_id").and_then(Value::as_str).unwrap_or_default().to_owned(),
            attempt: params.get("attempt").and_then(Value::as_u64).and_then(|a| u32::try_from(a).ok()).unwrap_or(1),
            config: params.get("config").cloned().unwrap_or(Value::Object(Map::new())),
            resume,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let ctx = RunCtx {
            request_id: request_id.clone(),
            out: out.clone(),
            cancelled: cancelled.clone(),
            cancel_notify: notify.clone(),
        };
        let out = out.clone();
        let registry = running.clone();
        let rid = request_id.clone();
        // Hold the registry lock while spawning so a fast task cannot finish (and try
        // to deregister) before it is registered.
        let Ok(mut map) = running.lock() else { return };
        let task = tokio::spawn(async move {
            // The handler runs in its own task so a panic becomes an error reply
            // instead of a silent timeout on the host. Aborting this task (cancel)
            // aborts the handler too.
            let inner = AbortOnDrop(tokio::spawn(handler(ctx, req)));
            let line = match join_handler(inner).await {
                Some(Ok(Outcome::Done(r))) => encode_result(&id, &r),
                Some(Ok(Outcome::Pause { kind, data, state })) => {
                    ok_response(&id, json!({"pause": {"kind": kind, "data": data, "state": state}}))
                }
                Some(Err(e)) => plugin_error_response(&id, &e),
                None => {
                    // Cancelled: the host abandoned the request.
                    if let Ok(mut m) = registry.lock() {
                        m.remove(&rid);
                    }
                    return;
                }
            };
            send(&out, line);
            if let Ok(mut m) = registry.lock() {
                m.remove(&rid);
            }
        });
        map.insert(request_id, Running { task, cancelled, notify });
    }

    fn start_tool(&self, id: Value, params: Value, out: &mpsc::UnboundedSender<String>, host: ToolHost) {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let Some(handler) = self.tools.get(&name).cloned() else {
            let msg = format!("plugin `{}` has no tool `{name}`", self.name);
            send(out, error_response(&id, -32602, &msg, Some(json!({"kind": "unknown_tool"}))));
            return;
        };
        let call = ToolCall {
            name,
            arguments: params.get("arguments").cloned().unwrap_or(Value::Null),
            context: ToolCallContext::from_params(&params),
        };
        let out = out.clone();
        tokio::spawn(async move {
            let inner = AbortOnDrop(tokio::spawn(handler(call)));
            let line = match join_handler(inner).await {
                Some(Ok(r)) => encode_result(&id, &r.without_unsupported_images(host.supports_images)),
                Some(Err(e)) => plugin_error_response(&id, &e),
                None => return,
            };
            // A line above the host's limit would break the connection: report it instead.
            let line = if line.len() > host.limit {
                let msg = format!("tool result is {} bytes, above the {}-byte message limit", line.len(), host.limit);
                error_response(&id, -32000, &msg, Some(json!({"kind": "message_too_large"})))
            } else {
                line
            };
            send(&out, line);
        });
    }
}

/// What the host announced in `initialize` that shapes tool results.
#[derive(Clone, Copy, Debug)]
struct ToolHost {
    supports_images: bool,
    limit: usize,
}

/// Aborts the wrapped task when dropped (so cancelling the outer task stops the handler).
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Wait for a handler task. A panic becomes a `plugin_panic` error; `None` means
/// the task was cancelled.
async fn join_handler<T>(mut task: AbortOnDrop<Result<T, PluginError>>) -> Option<Result<T, PluginError>> {
    match (&mut task.0).await {
        Ok(result) => Some(result),
        Err(e) if e.is_panic() => {
            let payload = e.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            eprintln!("graphrt plugin handler panicked: {message}");
            Some(Err(PluginError::new("plugin_panic", format!("handler panicked: {message}"))))
        }
        Err(_) => None,
    }
}

/// Serialize a handler result, replying with an error when it cannot be encoded
/// (never a `null` result the host would mistake for success).
fn encode_result<T: Serialize>(id: &Value, result: &T) -> String {
    match serde_json::to_value(result) {
        Ok(value) => ok_response(id, value),
        Err(e) => error_response(
            id,
            -32000,
            &format!("result is not valid JSON: {e}"),
            Some(json!({"kind": "invalid_output"})),
        ),
    }
}

/// Best-effort request id from the start of a line that could not be read.
fn recover_id(line: &[u8]) -> Value {
    let head = String::from_utf8_lossy(line.get(..line.len().min(4096)).unwrap_or_default()).into_owned();
    let Some(pos) = head.find("\"id\"") else { return Value::Null };
    let rest = head.get(pos + 4..).unwrap_or_default().trim_start();
    let Some(rest) = rest.strip_prefix(':') else { return Value::Null };
    let rest = rest.trim_start();
    // A string id or an integer id.
    if let Some(body) = rest.strip_prefix('"') {
        let mut out = String::new();
        let mut escaped = false;
        for c in body.chars() {
            match (escaped, c) {
                (true, c) => {
                    out.push(c);
                    escaped = false;
                }
                (false, '\\') => escaped = true,
                (false, '"') => return Value::String(out),
                (false, c) => out.push(c),
            }
        }
        return Value::Null;
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse::<i64>().map(Value::from).unwrap_or(Value::Null)
}

struct Running {
    task: tokio::task::JoinHandle<()>,
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

fn id_text(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn send(out: &mpsc::UnboundedSender<String>, line: String) {
    let _host_gone = out.send(line);
}

fn ok_response(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn error_response(id: &Value, code: i64, message: &str, data: Option<Value>) -> String {
    let mut error = Map::new();
    error.insert("code".into(), json!(code));
    error.insert("message".into(), json!(message));
    if let Some(data) = data {
        error.insert("data".into(), data);
    }
    json!({"jsonrpc": "2.0", "id": id, "error": Value::Object(error)}).to_string()
}

fn plugin_error_response(id: &Value, e: &PluginError) -> String {
    let mut data = Map::new();
    data.insert("kind".into(), json!(e.kind));
    data.insert("retryable".into(), json!(e.retryable));
    if let Some(ms) = e.retry_after_ms {
        data.insert("retry_after_ms".into(), json!(ms));
    }
    error_response(id, -32000, &e.message, Some(Value::Object(data)))
}

/// Result of reading one line.
enum Line {
    Complete,
    /// Longer than the limit: the rest of the line was discarded, `buf` holds its start.
    Oversized,
    Eof,
}

/// Read one `\n`-terminated line. Lines above `max` bytes are skipped up to their
/// newline (keeping the stream in sync) and reported as [`Line::Oversized`].
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Line> {
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(match (buf.is_empty(), oversized) {
                (_, true) => Line::Oversized,
                (true, false) => Line::Eof,
                (false, false) => Line::Complete,
            });
        }
        let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
            Some(pos) => (available.get(..=pos).unwrap_or(available), true),
            None => (available, false),
        };
        if !oversized && buf.len().saturating_add(chunk.len()) > max {
            oversized = true;
            // Keep the head (for recovering the request id), drop the rest.
            let room = 4096usize.saturating_sub(buf.len());
            buf.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
        } else if !oversized {
            buf.extend_from_slice(chunk);
        }
        let used = chunk.len();
        reader.consume(used);
        if done {
            return Ok(if oversized { Line::Oversized } else { Line::Complete });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(plugin: Plugin, input: &str) -> Vec<Value> {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (server_r, server_w) = tokio::io::split(server);
        let serve = tokio::spawn(plugin.serve(server_r, server_w));
        let (client_r, mut client_w) = tokio::io::split(client);
        client_w.write_all(input.as_bytes()).await.unwrap();
        client_w.shutdown().await.unwrap();
        let mut out = String::new();
        let mut reader = BufReader::new(client_r);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap() > 0 {
            out.push_str(&line);
            line.clear();
        }
        serve.await.unwrap().unwrap();
        out.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[tokio::test]
    async fn handles_initialize_run_and_errors() {
        let plugin = Plugin::new("t")
            .node("t.echo", |ctx, req| async move {
                ctx.chunk("text", "hi");
                Ok(RunResult::new().output("echo", req.config["x"].clone()))
            })
            .node("t.fail", |_ctx, _req| async move {
                Err::<RunResult, _>(PluginError::retryable("busy", "try later").retry_after(5))
            })
            .node("t.pause", |_ctx, req| async move {
                match req.resume {
                    None => Ok(Outcome::pause("approval", json!({"q": "ok?"}), json!({"step": 1}))),
                    Some(r) => Ok(RunResult::new().output("state", r.state).output("input", r.input).into()),
                }
            })
            .tool(json!({"name": "add", "description": "", "parameters": {}}), |call| async move {
                let a = call.arguments["a"].as_i64().unwrap_or(0);
                Ok(ToolResult::text(format!("{}", a + 1)))
            })
            .tool(json!({"name": "who", "description": "", "parameters": {}}), |call| async move {
                assert!(!format!("{:?}", call.context).contains("tok-1"));
                Ok(ToolResult::text(call.context.secret("user_jwt").unwrap_or("anonymous")))
            });
        let input = [
            r#"{"jsonrpc":"2.0","id":"i","method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":"r-1","method":"node.run","params":{"request_id":"r-1","kind":"t.echo","config":{"x":5}}}"#,
            r#"{"jsonrpc":"2.0","id":"r-2","method":"node.run","params":{"request_id":"r-2","kind":"t.fail","config":{}}}"#,
            r#"{"jsonrpc":"2.0","id":"r-3","method":"node.run","params":{"request_id":"r-3","kind":"t.nope","config":{}}}"#,
            r#"{"jsonrpc":"2.0","id":"r-4","method":"node.run","params":{"request_id":"r-4","kind":"t.pause","config":{}}}"#,
            r#"{"jsonrpc":"2.0","id":"r-5","method":"node.run","params":{"request_id":"r-5","kind":"t.pause","config":{},"resume":{"state":{"step":1},"input":"yes"}}}"#,
            r#"{"jsonrpc":"2.0","id":"t-1","method":"tool.call","params":{"name":"add","arguments":{"a":41}}}"#,
            r#"{"jsonrpc":"2.0","id":"t-2","method":"tool.call","params":{"name":"who","arguments":{},"context":{"secrets":{"user_jwt":"tok-1"}}}}"#,
            r#"{"jsonrpc":"2.0","id":"t-3","method":"tool.call","params":{"name":"who","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":"x","method":"bogus"}"#,
            "",
        ]
        .join("\n");
        // Give spawned handlers time to answer before stdin closes the loop.
        let msgs = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let plugin = plugin;
            roundtrip_wait(plugin, &input).await
        })
        .await
        .unwrap();
        let by_id = |id: &str| msgs.iter().find(|m| m["id"] == id).cloned().unwrap_or_default();
        assert_eq!(by_id("i")["result"]["protocol_version"], 1);
        assert_eq!(by_id("i")["result"]["tools"][0]["name"], "add");
        assert_eq!(by_id("r-1")["result"]["outputs"]["echo"], 5);
        assert!(msgs.iter().any(|m| m["method"] == "node.chunk" && m["params"]["request_id"] == "r-1"));
        assert_eq!(by_id("r-2")["error"]["data"]["retryable"], true);
        assert_eq!(by_id("r-2")["error"]["data"]["retry_after_ms"], 5);
        assert_eq!(by_id("r-3")["error"]["code"], -32602);
        assert_eq!(by_id("r-4")["result"]["pause"]["state"]["step"], 1);
        assert_eq!(by_id("r-5")["result"]["outputs"]["input"], "yes");
        assert_eq!(by_id("t-1")["result"]["text"], "42");
        assert_eq!(by_id("t-2")["result"]["text"], "tok-1");
        assert_eq!(by_id("t-3")["result"]["text"], "anonymous");
        assert_eq!(by_id("x")["error"]["code"], -32601);
    }

    fn screenshot_plugin() -> Plugin {
        Plugin::new("screen").tool(json!({"name": "shot", "description": "", "parameters": {}}), |call| async move {
            let size = call.arguments["size"].as_u64().unwrap_or(8) as usize;
            Ok(ToolResult::text("800x600").with_image_base64("image/png", "A".repeat(size)))
        })
    }

    async fn call_shot(init_params: &str, size: usize) -> Value {
        let input = [
            format!(r#"{{"jsonrpc":"2.0","id":"i","method":"initialize","params":{init_params}}}"#),
            format!(r#"{{"jsonrpc":"2.0","id":"t","method":"tool.call","params":{{"name":"shot","arguments":{{"size":{size}}}}}}}"#),
            String::new(),
        ]
        .join("\n");
        let msgs = tokio::time::timeout(std::time::Duration::from_secs(5), roundtrip_wait(screenshot_plugin(), &input))
            .await
            .unwrap();
        msgs.into_iter().find(|m| m["id"] == "t").unwrap_or_default()
    }

    #[tokio::test]
    async fn tool_images_are_sent_to_hosts_that_support_them() {
        let reply = call_shot(r#"{"features":["tool_images"]}"#, 8).await;
        assert_eq!(
            reply["result"],
            json!({"text": "800x600", "is_error": false, "images": [{"media_type": "image/png", "data_base64": "AAAAAAAA"}]})
        );
    }

    #[tokio::test]
    async fn tool_images_are_dropped_for_older_hosts() {
        let reply = call_shot("{}", 8).await;
        assert_eq!(
            reply["result"],
            json!({"text": "800x600\n[1 image(s) omitted: the graphrt host does not support image tool results]", "is_error": false})
        );
    }

    #[tokio::test]
    async fn oversized_tool_results_are_reported_not_sent() {
        let reply = call_shot(r#"{"features":["tool_images"],"max_message_bytes":2048}"#, 4096).await;
        assert_eq!(reply["error"]["data"]["kind"], "message_too_large");
        assert!(reply.get("result").is_none());
    }

    /// Like [`roundtrip`], but keeps stdin open until every request is answered.
    async fn roundtrip_wait(plugin: Plugin, input: &str) -> Vec<Value> {
        let expected = input.lines().filter(|l| l.contains("\"id\"")).count();
        let (client, server) = tokio::io::duplex(1 << 16);
        let (server_r, server_w) = tokio::io::split(server);
        let serve = tokio::spawn(plugin.serve(server_r, server_w));
        let (client_r, mut client_w) = tokio::io::split(client);
        client_w.write_all(input.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(client_r);
        let mut msgs = Vec::new();
        let mut responses = 0;
        let mut line = String::new();
        while responses < expected {
            line.clear();
            if reader.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            if v.get("id").is_some() {
                responses += 1;
            }
            msgs.push(v);
        }
        client_w.shutdown().await.unwrap();
        serve.await.unwrap().unwrap();
        msgs
    }

    #[tokio::test]
    async fn shutdown_and_eof_end_the_loop() {
        let msgs = roundtrip(Plugin::new("t"), "{\"jsonrpc\":\"2.0\",\"method\":\"shutdown\"}\n").await;
        assert!(msgs.is_empty());
        let msgs = roundtrip(Plugin::new("t"), "not json\n").await;
        assert_eq!(msgs[0]["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn cancel_aborts_the_handler() {
        let plugin = Plugin::new("t").node("t.slow", |ctx, _req| async move {
            ctx.cancelled().await;
            Ok(RunResult::new())
        });
        let (client, server) = tokio::io::duplex(1 << 16);
        let (server_r, server_w) = tokio::io::split(server);
        let serve = tokio::spawn(plugin.serve(server_r, server_w));
        let (client_r, mut client_w) = tokio::io::split(client);
        let run = r#"{"jsonrpc":"2.0","id":"r-1","method":"node.run","params":{"request_id":"r-1","kind":"t.slow","config":{}}}"#;
        let cancel = r#"{"jsonrpc":"2.0","method":"node.cancel","params":{"request_id":"r-1"}}"#;
        client_w.write_all(format!("{run}\n{cancel}\n").as_bytes()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        client_w.shutdown().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), serve).await.unwrap().unwrap().unwrap();
        let mut rest = String::new();
        let mut reader = BufReader::new(client_r);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap() > 0 {
            rest.push_str(&line);
            line.clear();
        }
        // A cancelled request gets no response.
        assert!(!rest.contains("\"r-1\""), "{rest}");
    }

    #[tokio::test]
    async fn panicking_handler_replies_with_an_error() {
        let plugin = Plugin::new("t").node("t.panic", |_ctx, _req| async move {
            if true {
                panic!("kaboom");
            }
            Ok(RunResult::new())
        });
        let run = r#"{"jsonrpc":"2.0","id":"r-1","method":"node.run","params":{"request_id":"r-1","kind":"t.panic","config":{}}}"#;
        let msgs = roundtrip(plugin, &format!("{run}\n")).await;
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert_eq!(msgs[0]["id"], "r-1");
        assert_eq!(msgs[0]["error"]["data"]["kind"], "plugin_panic");
        assert!(msgs[0]["error"]["message"].as_str().unwrap().contains("kaboom"));
    }

    #[tokio::test]
    async fn oversized_message_is_answered_and_stream_stays_in_sync() {
        let plugin = Plugin::new("t").node("t.ok", |_ctx, _req| async move { Ok(RunResult::new().output("ok", true)) });
        // The host announces a small limit, then sends a line above it, then a normal request.
        let init = r#"{"jsonrpc":"2.0","id":"i","method":"initialize","params":{"max_message_bytes":2048}}"#;
        let big = format!(
            r#"{{"jsonrpc":"2.0","id":"r-big","method":"node.run","params":{{"kind":"t.ok","config":{{"x":"{}"}}}}}}"#,
            "y".repeat(5000)
        );
        let ok = r#"{"jsonrpc":"2.0","id":"r-2","method":"node.run","params":{"request_id":"r-2","kind":"t.ok","config":{}}}"#;
        let msgs = roundtrip(plugin, &format!("{init}\n{big}\n{ok}\n")).await;
        let big_reply = msgs.iter().find(|m| m["id"] == "r-big").expect("reply to oversized request");
        assert_eq!(big_reply["error"]["data"]["kind"], "message_too_large");
        let ok_reply = msgs.iter().find(|m| m["id"] == "r-2").expect("later request still served");
        assert_eq!(ok_reply["result"]["outputs"]["ok"], true);
    }
}
