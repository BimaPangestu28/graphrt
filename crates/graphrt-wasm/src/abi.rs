//! Wire format exchanged with guest modules (ABI version 1).
//!
//! # Guest exports
//!
//! | Export | Signature | Purpose |
//! | --- | --- | --- |
//! | `memory` | memory | Linear memory used for all buffers. |
//! | `graphrt_abi_version` | `() -> i32` | Must return [`ABI_VERSION`]. |
//! | `graphrt_alloc` | `(len: i32) -> i32` | Allocate `len` bytes for a host-written buffer. |
//! | `graphrt_describe` | `() -> i64` | Packed `(ptr << 32) \| len` of a [`Manifest`] JSON. |
//! | `graphrt_run` | `(ptr: i32, len: i32) -> i64` | Run one node: reads a [`RunRequest`] JSON, returns a packed [`RunResponse`] JSON. |
//! | `graphrt_tool_call` | `(ptr: i32, len: i32) -> i64` | Optional, required when the manifest lists `tools`: reads a [`ToolCallRequest`], returns a packed [`ToolCallResponse`]. |
//!
//! # Host imports (module `graphrt`)
//!
//! | Import | Signature | Purpose |
//! | --- | --- | --- |
//! | `log` | `(level: i32, ptr: i32, len: i32)` | UTF-8 log line; level 0=trace .. 4=error. |
//! | `chunk` | `(field_ptr: i32, field_len: i32, ptr: i32, len: i32)` | Stream text for output `field`. |
//! | `progress` | `(name_ptr: i32, name_len: i32, ptr: i32, len: i32)` | Structured progress, JSON payload. |
//! | `http` | `(ptr: i32, len: i32) -> i64` | Outbound HTTP request JSON in, packed response JSON out (written with `graphrt_alloc`). Only for plugins granted network access; see [`crate::net`]. |
//!
//! Guests may import any subset of these; importing anything else fails at load.
//! `tools`, `graphrt_tool_call`, `env` and `http` are additions to version 1:
//! older hosts ignore `tools` and refuse modules importing `http`.

use std::collections::BTreeMap;

use graphrt_core::catalog::NodeDescriptor;
use graphrt_core::indexmap::IndexMap;
use graphrt_core::tool::{ToolImage, ToolSpec};
use serde::{Deserialize, Serialize};

/// ABI version implemented by this host.
pub const ABI_VERSION: i32 = 1;

/// Module providing host imports.
pub const HOST_MODULE: &str = "graphrt";

/// What a guest module offers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Plugin name, e.g. `text-tools`.
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Node kinds implemented by the module.
    #[serde(default)]
    pub nodes: Vec<NodeDescriptor>,
    /// Tools for `agent` / `tool` nodes, called through `graphrt_tool_call`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
}

/// Input for one node run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRequest {
    /// Node kind being run (one module may implement several).
    pub kind: String,
    /// Workflow node id.
    pub node_id: String,
    /// Node config with `{{#..#}}` placeholders resolved and `*selector` keys
    /// accompanied by `<key>_value`.
    pub config: serde_json::Value,
    /// 1-based attempt number.
    pub attempt: u32,
    /// `<run_id>:<exec_id>`: the same for every attempt and re-execution of
    /// this node execution; key external side effects on it.
    #[serde(default)]
    pub idempotency_key: String,
    /// Present when resuming after the node paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeData>,
    /// Host variables granted to the plugin (its manifest's `permissions.env`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// Input for one tool call (`graphrt_tool_call`).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Tool name from the manifest's `tools`.
    pub name: String,
    /// Arguments object written by the model.
    pub arguments: serde_json::Value,
    /// Per-call values the model never sees (agent `tool_context`); only sent to
    /// plugins trusted with plaintext secrets.
    #[serde(default, skip_serializing_if = "ToolCallContextData::is_empty")]
    pub context: ToolCallContextData,
    /// Host variables granted to the plugin (its manifest's `permissions.env`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl std::fmt::Debug for ToolCallRequest {
    /// Context secrets and env values are never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallRequest")
            .field("name", &self.name)
            .field("context", &self.context.secrets.keys().collect::<Vec<_>>())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// `context` of a [`ToolCallRequest`].
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallContextData {
    /// Secret name -> plaintext (e.g. `user_jwt`).
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

impl std::fmt::Debug for ToolCallContextData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallContextData").field("secrets", &self.secrets.keys().collect::<Vec<_>>()).finish()
    }
}

impl ToolCallContextData {
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }
}

/// Guest result of a tool call: the shape of a process plugin's `tool.call` result,
/// or `error` for a call the tool refused.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallResponse {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub images: Vec<ToolImage>,
    #[serde(default)]
    pub error: Option<ErrorData>,
}

/// Resume data handed back to a guest that paused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResumeData {
    pub state: serde_json::Value,
    #[serde(default)]
    pub input: serde_json::Value,
}

/// Guest result. Exactly one of `outputs`, `pause` or `error` is expected;
/// a response with `error` or `pause` set is treated as such even if `outputs` is present.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunResponse {
    #[serde(default)]
    pub outputs: Option<IndexMap<String, serde_json::Value>>,
    /// Selected handle for branching nodes.
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub usage: Option<UsageData>,
    #[serde(default)]
    pub metadata: Option<IndexMap<String, serde_json::Value>>,
    #[serde(default)]
    pub pause: Option<PauseData>,
    #[serde(default)]
    pub error: Option<ErrorData>,
}

/// Token usage reported by a guest (e.g. one that proxies a model via the host in future ABIs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageData {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// Pause request from a guest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PauseData {
    pub kind: String,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub state: serde_json::Value,
}

/// Error reported by a guest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorData {
    #[serde(default = "default_error_kind")]
    pub kind: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

fn default_error_kind() -> String {
    "plugin_error".to_owned()
}

/// Split a packed `(ptr << 32) | len` return value.
pub fn unpack(packed: i64) -> (u32, u32) {
    let bits = u64::from_ne_bytes(packed.to_ne_bytes());
    let ptr = u32::try_from(bits >> 32).unwrap_or(u32::MAX);
    let len = u32::try_from(bits & 0xffff_ffff).unwrap_or(u32::MAX);
    (ptr, len)
}

/// Pack a pointer and length the way guests return them.
pub fn pack(ptr: u32, len: u32) -> i64 {
    let bits = (u64::from(ptr) << 32) | u64::from(len);
    i64::from_ne_bytes(bits.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip() {
        for (p, l) in [(0, 0), (1, 2), (u32::MAX, 7), (65536, u32::MAX)] {
            assert_eq!(unpack(pack(p, l)), (p, l));
        }
    }

    #[test]
    fn response_defaults() {
        let r: RunResponse = serde_json::from_str(r#"{"error":{"message":"boom"}}"#).unwrap();
        let e = r.error.unwrap();
        assert_eq!(e.kind, "plugin_error");
        assert!(!e.retryable);
    }
}
