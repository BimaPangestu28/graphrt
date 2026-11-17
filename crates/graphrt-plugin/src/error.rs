//! Error types.

use std::path::PathBuf;

use graphrt_core::NodeError;

/// Error loading, installing or registering plugins.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PluginError {
    #[error("invalid plugin manifest: {0}")]
    Manifest(String),
    #[error("{}: {message}", path.display())]
    Io { path: PathBuf, message: String },
    #[error("plugin `{plugin}`: node kind `{kind}` is already registered")]
    KindConflict { plugin: String, kind: String },
    #[error("plugin `{0}` is loaded twice")]
    DuplicatePlugin(String),
    #[error("plugin `{plugin}`: no loader registered for runtime `{runtime}`")]
    NoLoader { plugin: String, runtime: String },
    #[error("plugin `{plugin}`: runtime `{runtime}` is not enabled in this build")]
    RuntimeDisabled { plugin: String, runtime: String },
    #[error("plugin `{plugin}`: checksum mismatch for `{file}`")]
    Checksum { plugin: String, file: String },
    #[error("plugin `{plugin}`: {message}")]
    Runtime { plugin: String, message: String },
    #[error("install failed: {0}")]
    Install(String),
}

/// Failure of one protocol call.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum CallError {
    /// The plugin answered with a JSON-RPC error.
    #[error("{message}")]
    Remote { code: i64, message: String, data: Option<serde_json::Value> },
    /// The plugin process exited or the connection broke.
    #[error("plugin crashed: {0}")]
    Crashed(String),
    /// The plugin could not be started.
    #[error("plugin failed to start: {0}")]
    Start(String),
    /// The plugin violated the protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The request timed out.
    #[error("plugin request timed out after {0} ms")]
    Timeout(u64),
    /// Transport-level failure (HTTP).
    #[error("transport error: {0}")]
    Transport(String),
}

/// JSON-RPC "method not found".
pub const METHOD_NOT_FOUND: i64 = -32601;

impl CallError {
    /// Whether this is a JSON-RPC "method not found" error.
    pub fn is_method_not_found(&self) -> bool {
        matches!(self, CallError::Remote { code, .. } if *code == METHOD_NOT_FOUND)
    }

    /// Convert to a node error.
    pub fn into_node_error(self, plugin: &str) -> NodeError {
        match self {
            CallError::Remote { code, message, data } => {
                let data = data.unwrap_or(serde_json::Value::Null);
                // Standard JSON-RPC codes (parse error, invalid request, method not
                // found, invalid params, internal error) signal protocol misuse.
                let standard = matches!(code, -32700 | -32600 | -32601 | -32602 | -32603);
                if standard && data.get("kind").is_none() {
                    return NodeError::fatal("plugin_protocol", format!("plugin `{plugin}`: {message} (code {code})"));
                }
                let kind = data.get("kind").and_then(|k| k.as_str()).unwrap_or("plugin_error").to_owned();
                let retryable = data.get("retryable").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let retry_after = data.get("retry_after_ms").and_then(serde_json::Value::as_u64);
                let mut err = if retryable { NodeError::new(kind, message) } else { NodeError::fatal(kind, message) };
                err = err.with_retry_after(retry_after);
                err
            }
            CallError::Crashed(m) => NodeError::new("plugin_crashed", format!("plugin `{plugin}` crashed: {m}")),
            CallError::Start(m) => NodeError::new("plugin_start", format!("plugin `{plugin}` failed to start: {m}")),
            CallError::Protocol(m) => NodeError::fatal("plugin_protocol", format!("plugin `{plugin}`: {m}")),
            CallError::Timeout(ms) => NodeError::new("timeout", format!("plugin `{plugin}` timed out after {ms} ms")),
            CallError::Transport(m) => NodeError::new("plugin_transport", format!("plugin `{plugin}`: {m}")),
        }
    }
}
