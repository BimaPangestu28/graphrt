//! Persistable run state for pause / resume.
//!
//! A snapshot never contains the graph, environment variables or secrets. To
//! resume, the host rebuilds the same graph (checked via
//! [`Snapshot::graph_fingerprint`]) and supplies env variables again.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::event::Usage;
use crate::pool::PoolSnapshot;
use crate::value::Value;

/// Current snapshot format version.
pub const SNAPSHOT_FORMAT: u32 = 1;

/// Complete persisted state of a paused run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format: u32,
    pub run_id: String,
    /// Fingerprint of the graph the snapshot was taken from.
    pub graph_fingerprint: String,
    pub elapsed_ms: u64,
    pub steps: u64,
    pub usage: Usage,
    pub handled_errors: u32,
    pub root: FrameSnapshot,
}

/// Persisted state of one execution frame (the root graph, or one container body run).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameSnapshot {
    pub pool: PoolSnapshot,
    /// Per-node state, keyed by node id. Nodes absent from the map have not been reached.
    pub nodes: IndexMap<String, NodeSnapshot>,
    /// Edge states, keyed by edge id.
    pub edges: IndexMap<String, EdgeStatus>,
    /// Accumulated response outputs of this frame (answer text, end outputs).
    pub outputs: IndexMap<String, Value>,
}

/// Lifecycle state of a node inside a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Ready to run (or was running when the frame paused and must be re-run).
    Pending,
    /// Paused by the node itself; it will be resumed with its saved state.
    Paused,
    Succeeded,
    Failed,
    Skipped,
}

/// Persisted state of one node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeSnapshot {
    pub status: NodeStatus,
    pub exec_id: String,
    pub attempt: u32,
    /// Node-owned resume state (for nodes that paused themselves, e.g. human input
    /// or a container holding the snapshots of its child frames).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_state: Option<serde_json::Value>,
}

/// Traversal state of an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStatus {
    Unknown,
    Taken,
    Skipped,
}

impl Snapshot {
    /// Encode as JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Decode from JSON.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}
