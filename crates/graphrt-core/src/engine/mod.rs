//! The execution engine — PUBLIC API CONTRACT (implementation pending).
//!
//! The signatures in this file are the stable contract other crates code
//! against. The engine implementer may restructure internals freely (add
//! submodules) but must keep these public items and their semantics.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;
use indexmap::IndexMap;

use crate::event::Event;
use crate::graph::Graph;
use crate::node::{Clock, Services, default_clock};
use crate::pool::Selector;
use crate::snapshot::Snapshot;
use crate::value::Value;

/// Resource limits for a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    /// Maximum node executions (including retries and container bodies). Default 10 000.
    pub max_steps: u64,
    /// Wall-clock budget for the whole run. Enforced only with a timer-capable clock.
    pub max_duration_ms: Option<u64>,
    /// Maximum container nesting depth. Default 8.
    pub max_depth: u32,
    /// Timeout applied to nodes without their own `timeout_ms`.
    pub default_node_timeout_ms: Option<u64>,
    /// Maximum nodes executing concurrently within one frame. Default 64.
    pub max_concurrency: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_steps: 10_000, max_duration_ms: None, max_depth: 8, default_node_timeout_ms: None, max_concurrency: 64 }
    }
}

/// Input for a run.
#[derive(Clone, Debug, Default)]
pub struct RunInput {
    /// Values for the start node's declared variables.
    pub inputs: IndexMap<String, Value>,
    /// System variables (`sys.*`), e.g. `query`, `files`, `user_id`, `conversation_id`.
    pub sys: IndexMap<String, Value>,
    /// Environment variables (`env.*`); override document defaults. Never persisted.
    pub env: IndexMap<String, Value>,
    /// Conversation variables (`conversation.*`); override document defaults.
    pub conversation: IndexMap<String, Value>,
    /// Explicit run id (generated when absent).
    pub run_id: Option<String>,
    /// Resume a paused run.
    pub resume: Option<ResumeFrom>,
}

/// Resume information.
#[derive(Clone, Debug)]
pub struct ResumeFrom {
    pub snapshot: Snapshot,
    /// Host input for paused nodes, keyed by node id (e.g. a submitted human-input form).
    pub inputs: IndexMap<String, Value>,
}

/// Executes runs of one graph.
#[derive(Clone, Debug)]
pub struct Engine {
    graph: Arc<Graph>,
    services: Arc<Services>,
    clock: Arc<dyn Clock>,
    limits: Limits,
}

impl Engine {
    pub fn new(graph: Arc<Graph>) -> Self {
        Self { graph, services: Arc::new(Services::new()), clock: default_clock(), limits: Limits::default() }
    }

    pub fn with_services(mut self, services: Services) -> Self {
        self.services = Arc::new(services);
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn graph(&self) -> &Arc<Graph> {
        &self.graph
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Start a run. The returned stream drives execution when polled (no task is
    /// spawned), so it works on any executor. The final item is always a terminal
    /// event (`RunSucceeded`, `RunFailed`, `RunAborted` or `RunPaused`).
    pub fn run(&self, _input: RunInput) -> RunStream {
        let events = vec![Event::RunFailed { error: "engine not implemented".into(), usage: Default::default(), elapsed_ms: 0 }];
        RunStream { inner: Box::pin(futures::stream::iter(events)), handle: RunHandle::default() }
    }
}

/// Stream of run events. `Send + 'static`.
pub struct RunStream {
    inner: Pin<Box<dyn Stream<Item = Event> + Send>>,
    handle: RunHandle,
}

impl std::fmt::Debug for RunStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStream").finish_non_exhaustive()
    }
}

impl RunStream {
    /// A handle for controlling the run from elsewhere.
    pub fn handle(&self) -> RunHandle {
        self.handle.clone()
    }
}

impl Stream for RunStream {
    type Item = Event;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Controls a running run. Cheap to clone, `Send + Sync`.
#[derive(Clone, Debug, Default)]
pub struct RunHandle {}

impl RunHandle {
    /// Cancel immediately: in-flight nodes are dropped. Ends with `RunAborted`.
    pub fn abort(&self, _reason: impl Into<String>) {}

    /// Stop scheduling new nodes, let in-flight nodes finish, then end with `RunPaused`.
    pub fn pause(&self, _reason: impl Into<String>) {}

    /// Write a variable into the root frame (applied between node completions).
    pub fn update_variable(&self, _selector: Selector, _value: Value) {}
}
