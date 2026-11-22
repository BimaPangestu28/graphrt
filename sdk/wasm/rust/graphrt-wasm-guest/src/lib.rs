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
//! Each node run uses a fresh instance, so the SDK never frees buffers it hands
//! to the host: memory is reclaimed when the instance is dropped.

pub use serde_json::{self, json, Map, Value};
use serde::{Deserialize, Serialize};

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
}

impl Manifest {
    pub fn new(name: &str, version: &str) -> Self {
        Self { name: name.into(), version: version.into(), description: String::new(), nodes: Vec::new() }
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
}

/// Data handed back after a pause.
#[derive(Clone, Debug, Deserialize)]
pub struct Resume {
    pub state: Value,
    #[serde(default)]
    pub input: Value,
}

impl Request {
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
}

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "graphrt")]
    extern "C" {
        pub fn log(level: i32, ptr: i32, len: i32);
        pub fn chunk(field_ptr: i32, field_len: i32, ptr: i32, len: i32);
        pub fn progress(name_ptr: i32, name_len: i32, ptr: i32, len: i32);
    }
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

fn error_json(kind: &str, message: &str) -> Value {
    json!({"error": {"kind": kind, "message": message, "retryable": false}})
}

/// Export the ABI functions for a [`Plugin`] type.
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
}
