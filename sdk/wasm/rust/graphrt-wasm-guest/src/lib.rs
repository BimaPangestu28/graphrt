//! Guest SDK for graphrt WebAssembly plugins (ABI version 1).
//!
//! Implement [`Plugin`] and call [`export_plugin!`]:
//!
//! ```ignore
//! use graphrt_wasm_guest::{export_plugin, json, Manifest, NodeSpec, Output, Plugin, PluginError, Request};
//!
//! struct Upper;
//!
//! impl Plugin for Upper {
//!     fn manifest() -> Manifest {
//!         Manifest::new("upper", "0.1.0").node(NodeSpec::new("upper", "Uppercase").output("text", "string"))
//!     }
//!     fn run(req: Request) -> Result<Output, PluginError> {
//!         let text = req.config_str("text").unwrap_or_default();
//!         Ok(Output::new().with("text", json!(text.to_uppercase())))
//!     }
//! }
//!
//! export_plugin!(Upper);
//! ```
//!
//! Build with `cargo build --release --target wasm32-unknown-unknown` and a
//! `[lib] crate-type = ["cdylib"]` section.
//!
//! A plugin may also offer tools to `agent` nodes: list them with
//! [`Manifest::tool`] and implement [`Plugin::call_tool`]. Plugins granted
//! network access (`permissions.network` in `graphrt-plugin.json`) can make
//! outbound requests with [`http`].
//!
//! Each node run uses a fresh instance, so the SDK never frees buffers it hands
//! to the host: memory is reclaimed when the instance is dropped.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
pub use serde_json::{self, json, Map, Value};

/// ABI version implemented by this SDK.
pub const ABI_VERSION: i32 = 1;

/// Plugin manifest returned by `graphrt_describe`.
#[derive(Clone, Debug, Serialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub nodes: Vec<NodeSpec>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
}

/// One tool for `agent` nodes.
#[derive(Clone, Debug, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

impl Manifest {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            description: String::new(),
            nodes: Vec::new(),
            tools: Vec::new(),
        }
    }

    /// Offer a tool (served by [`Plugin::call_tool`]).
    pub fn tool(mut self, name: &str, description: &str, parameters: Value) -> Self {
        self.tools.push(ToolSpec { name: name.into(), description: description.into(), parameters });
        self
    }

    pub fn describe(mut self, description: &str) -> Self {
        self.description = description.into();
        self
    }

    pub fn node(mut self, node: NodeSpec) -> Self {
        self.nodes.push(node);
        self
    }
}

/// One node kind (serialized as the host's catalog descriptor).
#[derive(Clone, Debug, Serialize)]
pub struct NodeSpec {
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub category: String,
    pub config_schema: Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<Value>,
    pub handles: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl NodeSpec {
    pub fn new(kind: &str, title: &str) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            description: String::new(),
            category: "other".into(),
            config_schema: json!({}),
            outputs: Vec::new(),
            handles: json!({"kind": "single"}),
            icon: None,
        }
    }

    pub fn describe(mut self, description: &str) -> Self {
        self.description = description.into();
        self
    }

    /// One of `io`, `logic`, `transform`, `model`, `knowledge`, `integration`, `human`, `other`.
    pub fn category(mut self, category: &str) -> Self {
        self.category = category.into();
        self
    }

    /// JSON Schema of the node config.
    pub fn schema(mut self, schema: Value) -> Self {
        self.config_schema = schema;
        self
    }

    /// Declare an output (`ty` is a value type such as `string`, `integer`, `array[string]`).
    pub fn output(mut self, name: &str, ty: &str) -> Self {
        self.outputs.push(json!({"name": name, "type": ty}));
        self
    }

    /// Declare a streamed output (sent incrementally with [`chunk`]).
    pub fn streaming_output(mut self, name: &str, ty: &str) -> Self {
        self.outputs.push(json!({"name": name, "type": ty, "streaming": true}));
        self
    }

    /// Make this a branching node with a fixed set of handles.
    pub fn branches(mut self, handles: &[&str]) -> Self {
        self.handles = json!({"kind": "fixed", "handles": handles});
        self
    }

    pub fn icon(mut self, icon: &str) -> Self {
        self.icon = Some(icon.into());
        self
    }
}

/// A node run request.
#[derive(Clone, Debug, Deserialize)]
pub struct Request {
    pub kind: String,
    pub node_id: String,
    /// Node config with placeholders resolved by the host.
    pub config: Value,
    pub attempt: u32,
    /// `<run_id>:<exec_id>`: the same for every attempt and re-execution of this
    /// node execution; key external side effects on it. Empty with older hosts.
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default)]
    pub resume: Option<Resume>,
    /// Host variables the manifest declares in `permissions.env` (those set on
    /// the host), as strings; read them with [`Request::env`].
    #[serde(default, rename = "env")]
    pub env_vars: Map<String, Value>,
}

/// A tool call from an `agent` node.
#[derive(Clone, Deserialize)]
pub struct ToolCall {
    pub name: String,
    /// Arguments written by the model: untrusted input.
    #[serde(default)]
    pub arguments: Value,
    /// Per-call values the model never sees (the agent's `tool_context`); only
    /// sent to plugins the operator trusts with secrets.
    #[serde(default)]
    pub context: ToolContext,
    /// Host variables the manifest declares in `permissions.env`; read them with [`ToolCall::env`].
    #[serde(default, rename = "env")]
    pub env_vars: Map<String, Value>,
}

impl ToolCall {
    /// A host variable granted by `permissions.env`.
    pub fn env(&self, name: &str) -> Option<&str> {
        self.env_vars.get(name).and_then(Value::as_str)
    }

    /// A tool context secret, e.g. `user_jwt`.
    pub fn secret(&self, name: &str) -> Option<&str> {
        self.context.secrets.get(name).and_then(Value::as_str)
    }
}

impl std::fmt::Debug for ToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCall").field("name", &self.name).finish_non_exhaustive()
    }
}

/// `context` of a [`ToolCall`].
#[derive(Clone, Default, Deserialize)]
pub struct ToolContext {
    /// Secret name -> plaintext, e.g. `user_jwt`.
    #[serde(default)]
    pub secrets: Map<String, Value>,
}

/// Result of a tool call.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ToolResult {
    /// Text handed to the model.
    pub text: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl ToolResult {
    /// A result whose text is `data` as JSON.
    pub fn json(data: Value) -> Self {
        Self { text: data.to_string(), data, is_error: false }
    }
}

/// Data handed back after a pause.
#[derive(Clone, Debug, Deserialize)]
pub struct Resume {
    pub state: Value,
    #[serde(default)]
    pub input: Value,
}

impl Request {
    /// A host variable granted by `permissions.env`.
    pub fn env(&self, name: &str) -> Option<&str> {
        self.env_vars.get(name).and_then(Value::as_str)
    }

    /// A config field.
    pub fn config(&self, key: &str) -> Option<&Value> {
        self.config.get(key)
    }

    /// A config field as a string (numbers and booleans are stringified).
    pub fn config_str(&self, key: &str) -> Option<String> {
        match self.config.get(key)? {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        }
    }
}

/// Successful result.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Output {
    pub outputs: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
    #[serde(skip)]
    pause: Option<Value>,
}

impl Output {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.outputs.insert(key.into(), value);
        self
    }

    pub fn branch(mut self, handle: &str) -> Self {
        self.branch = Some(handle.into());
        self
    }

    pub fn meta(mut self, key: &str, value: Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Pause the run (e.g. wait for a human). `state` comes back in [`Request::resume`].
    pub fn pause(kind: &str, data: Value, state: Value) -> Self {
        Self { pause: Some(json!({"kind": kind, "data": data, "state": state})), ..Self::default() }
    }
}

/// Error result.
#[derive(Clone, Debug, Serialize)]
pub struct PluginError {
    pub kind: String,
    pub message: String,
    pub retryable: bool,
}

impl PluginError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        Self { kind: kind.into(), message: message.into(), retryable: false }
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
}

/// A plugin implementation.
pub trait Plugin {
    fn manifest() -> Manifest;
    fn run(request: Request) -> Result<Output, PluginError>;

    /// Serve a tool listed in the manifest. The default refuses every call.
    fn call_tool(call: ToolCall) -> Result<ToolResult, PluginError> {
        Err(PluginError::new("unknown_tool", format!("no tool `{}`", call.name)))
    }
}

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "graphrt")]
    extern "C" {
        pub fn log(level: i32, ptr: i32, len: i32);
        pub fn chunk(field_ptr: i32, field_len: i32, ptr: i32, len: i32);
        pub fn progress(name_ptr: i32, name_len: i32, ptr: i32, len: i32);
        pub fn http(ptr: i32, len: i32) -> i64;
    }
}

/// An outbound HTTP request (see [`http`]).
#[derive(Clone, Debug, Default, Serialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Deadline in ms; the host caps it (30 s by default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// A response from [`http`].
#[derive(Clone, Debug, Default, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    /// Lower-case header names.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
}

/// Why [`http`] failed: `invalid_request`, `egress_denied`, `timeout`, `too_large` or `http_error`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct HttpError {
    pub kind: String,
    pub message: String,
}

#[derive(Deserialize)]
struct HttpReply {
    #[serde(default)]
    error: Option<HttpError>,
    #[serde(flatten)]
    response: Option<HttpResponse>,
}

/// Send an HTTP request through the host (`graphrt::http`).
///
/// Needs `permissions.network` in the plugin manifest; the host applies its
/// egress policy (public addresses only by default, no redirects) and limits.
pub fn http(request: &HttpRequest) -> Result<HttpResponse, HttpError> {
    let payload = serde_json::to_vec(request)
        .map_err(|e| HttpError { kind: "invalid_request".into(), message: e.to_string() })?;
    let reply = send_http(&payload);
    let reply: HttpReply = serde_json::from_slice(&reply)
        .map_err(|e| HttpError { kind: "http_error".into(), message: format!("host reply: {e}") })?;
    match (reply.error, reply.response) {
        (Some(error), _) => Err(error),
        (None, Some(response)) => Ok(response),
        (None, None) => Err(HttpError { kind: "http_error".into(), message: "empty host reply".into() }),
    }
}

#[cfg(target_arch = "wasm32")]
fn send_http(payload: &[u8]) -> Vec<u8> {
    // SAFETY: the host returns a buffer it wrote through `graphrt_alloc`, which is never freed.
    unsafe {
        let packed = imports::http(payload.as_ptr() as usize as i32, payload.len() as i32) as u64;
        let (ptr, len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        std::slice::from_raw_parts(ptr as *const u8, len).to_vec()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn send_http(_payload: &[u8]) -> Vec<u8> {
    br#"{"error":{"kind":"egress_denied","message":"no host outside wasm32"}}"#.to_vec()
}

fn raw(s: &str) -> (i32, i32) {
    (s.as_ptr() as usize as i32, s.len() as i32)
}

/// Log levels for [`log`].
#[derive(Clone, Copy, Debug)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

/// Write a log line (surfaced in the node's metadata).
pub fn log(level: Level, message: &str) {
    let (p, l) = raw(message);
    #[cfg(target_arch = "wasm32")]
    unsafe {
        imports::log(level as i32, p, l)
    };
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (level, p, l);
}

/// Stream incremental text for output `field`.
pub fn chunk(field: &str, text: &str) {
    let ((fp, fl), (p, l)) = (raw(field), raw(text));
    #[cfg(target_arch = "wasm32")]
    unsafe {
        imports::chunk(fp, fl, p, l)
    };
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (fp, fl, p, l);
}

/// Report structured progress.
pub fn progress(name: &str, data: &Value) {
    let payload = data.to_string();
    let ((np, nl), (p, l)) = (raw(name), raw(&payload));
    #[cfg(target_arch = "wasm32")]
    unsafe {
        imports::progress(np, nl, p, l)
    };
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (np, nl, p, l);
}

/// Hand a buffer to the host; it stays valid until the instance is dropped.
fn leak(bytes: Vec<u8>) -> i64 {
    let bytes = bytes.into_boxed_slice();
    let len = bytes.len() as u64;
    let ptr = Box::leak(bytes).as_ptr() as usize as u64;
    ((ptr << 32) | len) as i64
}

#[doc(hidden)]
pub fn __alloc(len: i32) -> i32 {
    let buf = vec![0u8; len.max(0) as usize].into_boxed_slice();
    Box::leak(buf).as_mut_ptr() as usize as i32
}

#[doc(hidden)]
pub fn __describe<P: Plugin>() -> i64 {
    leak(serde_json::to_vec(&P::manifest()).unwrap_or_default())
}

#[doc(hidden)]
pub fn __run<P: Plugin>(ptr: i32, len: i32) -> i64 {
    // SAFETY: the host wrote `len` bytes at `ptr`, a buffer obtained from `__alloc`.
    let input = unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len.max(0) as usize) };
    let response = match serde_json::from_slice::<Request>(input) {
        Ok(request) => match P::run(request) {
            Ok(out) => match out.pause {
                Some(pause) => json!({ "pause": pause }),
                None => serde_json::to_value(&out).unwrap_or_else(|e| error_json("internal", &e.to_string())),
            },
            Err(err) => json!({ "error": err }),
        },
        Err(e) => error_json("invalid_request", &e.to_string()),
    };
    leak(serde_json::to_vec(&response).unwrap_or_default())
}

#[doc(hidden)]
pub fn __tool_call<P: Plugin>(ptr: i32, len: i32) -> i64 {
    // SAFETY: the host wrote `len` bytes at `ptr`, a buffer obtained from `__alloc`.
    let input = unsafe { std::slice::from_raw_parts(ptr as usize as *const u8, len.max(0) as usize) };
    let response = match serde_json::from_slice::<ToolCall>(input) {
        Ok(call) => match P::call_tool(call) {
            Ok(result) => serde_json::to_value(&result).unwrap_or_else(|e| error_json("internal", &e.to_string())),
            Err(err) => json!({ "error": err }),
        },
        Err(e) => error_json("invalid_request", &e.to_string()),
    };
    leak(serde_json::to_vec(&response).unwrap_or_default())
}

fn error_json(kind: &str, message: &str) -> Value {
    json!({"error": {"kind": kind, "message": message, "retryable": false}})
}

/// Export the ABI functions for a [`Plugin`] type.
///
/// `export_plugin!(MyPlugin, tools)` also exports `graphrt_tool_call` for a
/// plugin whose manifest lists tools (kept out of node-only modules: it adds
/// about 16 KiB).
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty) => {
        #[no_mangle]
        pub extern "C" fn graphrt_abi_version() -> i32 {
            $crate::ABI_VERSION
        }
        #[no_mangle]
        pub extern "C" fn graphrt_alloc(len: i32) -> i32 {
            $crate::__alloc(len)
        }
        #[no_mangle]
        pub extern "C" fn graphrt_describe() -> i64 {
            $crate::__describe::<$plugin>()
        }
        #[no_mangle]
        pub extern "C" fn graphrt_run(ptr: i32, len: i32) -> i64 {
            $crate::__run::<$plugin>(ptr, len)
        }
    };
    ($plugin:ty, tools) => {
        $crate::export_plugin!($plugin);
        #[no_mangle]
        pub extern "C" fn graphrt_tool_call(ptr: i32, len: i32) -> i64 {
            $crate::__tool_call::<$plugin>(ptr, len)
        }
    };
}
