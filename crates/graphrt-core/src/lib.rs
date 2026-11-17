//! # graphrt-core
//!
//! A small, executor-agnostic runtime for agentic AI workflow graphs.
//!
//! - [`dsl`]: portable workflow documents (JSON / YAML).
//! - [`graph`]: compiled, validated, immutable graphs.
//! - [`node`]: the node contract, registry and host services.
//! - [`engine`]: the scheduler that runs a graph and streams [`event::Event`]s.
//! - [`nodes`]: built-in control-flow and data nodes.
//!
//! The core has no dependency on any async runtime, HTTP client or model
//! provider; those live in separate crates and plug in through
//! [`node::Services`] and [`node::Registry`].

pub mod condition;
pub mod dsl;
pub mod engine;
pub mod event;
pub mod graph;
pub mod node;
pub mod nodes;
pub mod pool;
pub mod response;
pub mod snapshot;
pub mod template;
pub mod tool;
pub mod tool_node;
pub mod value;

pub use dsl::Workflow;
pub use engine::{Engine, RunHandle, RunInput, RunStream};
pub use event::{Event, Usage};
pub use graph::Graph;
pub use node::{Node, NodeContext, NodeError, NodeOutcome, NodeOutput, NodeRole, Registry, Services};
pub use pool::{Selector, VariablePool};
pub use snapshot::Snapshot;
pub use value::{Value, ValueType};

/// Re-exported so hosts use the same `IndexMap` version as the public API.
pub use indexmap;
/// Ordered map used throughout the public API.
pub use indexmap::IndexMap;
/// Re-exported so hosts use the same `serde_json` version as the public API.
pub use serde_json;

/// Top-level error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Document(#[from] dsl::DocumentError),
    #[error(transparent)]
    Graph(#[from] graph::GraphError),
    #[error("invalid run input: {0}")]
    Input(String),
    #[error("snapshot does not match this graph (expected fingerprint {expected}, got {actual})")]
    SnapshotMismatch { expected: String, actual: String },
    #[error("unsupported snapshot format {0}")]
    SnapshotFormat(u32),
}
