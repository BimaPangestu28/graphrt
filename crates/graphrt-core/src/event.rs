//! Events emitted by a run.
//!
//! Events are plain serializable data (tagged by `type`), so they can be
//! streamed over SSE / websockets as-is. Secrets inside values are masked by
//! [`crate::value::Value`]'s serializer.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::pool::Selector;
use crate::snapshot::Snapshot;
use crate::value::Value;
use crate::variables::Variables;

/// Identifies an execution frame. The root frame is the empty string; child
/// frames are `"<container_node>[<index>]"` segments joined by `/`, e.g.
/// `"iter[2]/loop[0]"`.
pub type FrameId = std::sync::Arc<str>;

/// Token usage reported by model-calling nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self.completion_tokens.saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }

    pub fn is_zero(&self) -> bool {
        self.total_tokens == 0 && self.prompt_tokens == 0 && self.completion_tokens == 0
    }
}

/// Why a run or node paused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PauseReason {
    /// Frame of the node that requested the pause (empty for external pauses).
    pub frame: FrameId,
    /// Node that requested the pause, if any.
    pub node_id: Option<String>,
    /// Machine-readable kind, e.g. `human_input`, `external`, `approval`.
    pub kind: String,
    /// Free-form details (form definition, message, ...).
    #[serde(default)]
    pub data: Value,
}

/// A run event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    RunStarted {
        run_id: String,
        resumed: bool,
    },
    NodeStarted {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
        exec_id: std::sync::Arc<str>,
        kind: std::sync::Arc<str>,
        title: std::sync::Arc<str>,
        attempt: u32,
    },
    /// Incremental output, e.g. LLM tokens for output `field`.
    NodeChunk {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
        exec_id: std::sync::Arc<str>,
        field: String,
        chunk: String,
    },
    /// Structured progress emitted by a node (agent steps, tool calls, iteration progress...).
    NodeProgress {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
        exec_id: std::sync::Arc<str>,
        name: String,
        data: Value,
    },
    NodeRetry {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
        exec_id: std::sync::Arc<str>,
        attempt: u32,
        error: String,
        delay_ms: u64,
    },
    NodeSucceeded {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
        exec_id: std::sync::Arc<str>,
        /// The node's outputs, shared with its pool namespace (cloning is cheap).
        outputs: Variables,
        /// Selected branch handle for branch nodes.
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        metadata: IndexMap<String, Value>,
        elapsed_ms: u64,
    },
    /// A node failed. `handled` is true when its error strategy kept the run going.
    NodeFailed {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
        exec_id: std::sync::Arc<str>,
        error: String,
        error_type: String,
        handled: bool,
        /// Outputs committed by the error strategy (`error_message`, defaults...).
        #[serde(default, skip_serializing_if = "Variables::is_empty")]
        outputs: Variables,
        elapsed_ms: u64,
    },
    /// The node will not run because every incoming path was skipped.
    NodeSkipped {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
    },
    NodePaused {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
        exec_id: std::sync::Arc<str>,
        reason: PauseReason,
    },
    /// A node wrote a variable outside its own outputs (assigner, conversation variable...).
    VariableUpdated {
        frame: FrameId,
        node_id: std::sync::Arc<str>,
        selector: Selector,
        value: Value,
    },
    RunSucceeded {
        outputs: IndexMap<String, Value>,
        usage: Usage,
        /// Failures handled by an error strategy (non-zero = partial success).
        handled_errors: u32,
        elapsed_ms: u64,
    },
    RunFailed {
        error: String,
        /// Machine-readable category: `invalid_input` (bad run input or resume
        /// data), `snapshot_mismatch`, `limit_exceeded` (steps / duration / depth),
        /// `internal`, or the failing node's own error type (e.g. `timeout`,
        /// `http`, `model`).
        #[serde(default)]
        error_type: String,
        usage: Usage,
        elapsed_ms: u64,
    },
    RunAborted {
        reason: String,
        elapsed_ms: u64,
    },
    /// The run paused. Persist `snapshot` and pass it back to resume.
    RunPaused {
        reasons: Vec<PauseReason>,
        snapshot: Box<Snapshot>,
    },
}

impl Event {
    /// Whether this is the final event of a run.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Event::RunSucceeded { .. } | Event::RunFailed { .. } | Event::RunAborted { .. } | Event::RunPaused { .. }
        )
    }

    /// Frame of node-level events.
    pub fn frame(&self) -> Option<&FrameId> {
        match self {
            Event::NodeStarted { frame, .. }
            | Event::NodeChunk { frame, .. }
            | Event::NodeProgress { frame, .. }
            | Event::NodeRetry { frame, .. }
            | Event::NodeSucceeded { frame, .. }
            | Event::NodeFailed { frame, .. }
            | Event::NodeSkipped { frame, .. }
            | Event::NodePaused { frame, .. }
            | Event::VariableUpdated { frame, .. } => Some(frame),
            _ => None,
        }
    }
}
