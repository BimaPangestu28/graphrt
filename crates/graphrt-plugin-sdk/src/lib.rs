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

use std::collections::HashMap;
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
}

/// A tool result.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ToolResult {
    pub text: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub data: Value,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), data: Value::Null, is_error: false }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), data: Value::Null, is_error: true }
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
        loop {
            buf.clear();
            let n = read_line_capped(&mut lines, &mut buf).await?;
            if n == 0 {
                break;
            }
            let Ok(text) = std::str::from_utf8(&buf) else { continue };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(text) else {
                send(&out_tx, error_response(&Value::Null, -32700, "parse error", None));
                continue;
            };
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
                ("tool.call", Some(id)) => plugin.start_tool(id, params, &out_tx),
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
            let result = handler(ctx, req).await;
            let line = match result {
                Ok(Outcome::Done(r)) => ok_response(&id, serde_json::to_value(r).unwrap_or(Value::Null)),
                Ok(Outcome::Pause { kind, data, state }) => {
                    ok_response(&id, json!({"pause": {"kind": kind, "data": data, "state": state}}))
                }
                Err(e) => plugin_error_response(&id, &e),
            };
            send(&out, line);
            if let Ok(mut m) = registry.lock() {
                m.remove(&rid);
            }
        });
        map.insert(request_id, Running { task, cancelled, notify });
    }

    fn start_tool(&self, id: Value, params: Value, out: &mpsc::UnboundedSender<String>) {
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let Some(handler) = self.tools.get(&name).cloned() else {
            let msg = format!("plugin `{}` has no tool `{name}`", self.name);
            send(out, error_response(&id, -32602, &msg, Some(json!({"kind": "unknown_tool"}))));
            return;
        };
        let call = ToolCall { name, arguments: params.get("arguments").cloned().unwrap_or(Value::Null) };
        let out = out.clone();
        tokio::spawn(async move {
            let line = match handler(call).await {
                Ok(r) => ok_response(&id, serde_json::to_value(r).unwrap_or(Value::Null)),
                Err(e) => plugin_error_response(&id, &e),
            };
            send(&out, line);
        });
    }
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

async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len());
        }
        let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
            Some(pos) => (available.get(..=pos).unwrap_or(available), true),
            None => (available, false),
        };
        if buf.len().saturating_add(chunk.len()) > MAX_MESSAGE_BYTES {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too large"));
        }
        buf.extend_from_slice(chunk);
        let used = chunk.len();
        reader.consume(used);
        if done {
            return Ok(buf.len());
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
            });
        let input = [
            r#"{"jsonrpc":"2.0","id":"i","method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":"r-1","method":"node.run","params":{"request_id":"r-1","kind":"t.echo","config":{"x":5}}}"#,
            r#"{"jsonrpc":"2.0","id":"r-2","method":"node.run","params":{"request_id":"r-2","kind":"t.fail","config":{}}}"#,
            r#"{"jsonrpc":"2.0","id":"r-3","method":"node.run","params":{"request_id":"r-3","kind":"t.nope","config":{}}}"#,
            r#"{"jsonrpc":"2.0","id":"r-4","method":"node.run","params":{"request_id":"r-4","kind":"t.pause","config":{}}}"#,
            r#"{"jsonrpc":"2.0","id":"r-5","method":"node.run","params":{"request_id":"r-5","kind":"t.pause","config":{},"resume":{"state":{"step":1},"input":"yes"}}}"#,
            r#"{"jsonrpc":"2.0","id":"t-1","method":"tool.call","params":{"name":"add","arguments":{"a":41}}}"#,
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
        assert_eq!(by_id("x")["error"]["code"], -32601);
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
}
