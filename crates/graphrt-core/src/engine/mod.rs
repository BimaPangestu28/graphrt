//! The execution engine.
//!
//! # Model
//!
//! - The [`Graph`] is immutable and shared; each run keeps its own per-frame
//!   node and edge state.
//! - A node becomes ready once **every** incoming edge is resolved and **at
//!   least one** is taken. A node whose incoming edges are all skipped is
//!   skipped too (`NodeSkipped`), and the skip propagates downstream.
//! - Non-branch nodes take every outgoing edge except `fail-branch` edges.
//!   Branch nodes take the edges whose handle equals [`NodeOutput::branch`];
//!   a branch node that selects no handle fails (subject to its error strategy).
//!   A selected handle with no edges simply skips every outgoing path.
//! - Response nodes (answer / end) merge their outputs into the frame outputs:
//!   `answer` is concatenated, other keys overwrite. The root frame outputs are
//!   the run outputs.
//! - Nodes of one frame run concurrently (up to [`Limits::max_concurrency`]) as
//!   futures polled by the run itself. Nothing is spawned, so the engine runs on
//!   any executor, including single-threaded and wasm ones.
//!
//! # Run bootstrap
//!
//! The root pool receives `env.*` (document defaults coerced to their declared
//! types, overridden by [`RunInput::env`]), `conversation.*` (same rule; missing
//! values get their type's zero value), `sys.*` from [`RunInput::sys`] plus
//! `sys.run_id`. [`RunInput::inputs`] are written into the **namespace of the
//! root entry node** (normally the `start` node) before it runs; the start
//! node validates them and re-emits them as its outputs. Inputs are never
//! copied into `sys`, so an input cannot shadow a system variable.
//!
//! # Failures
//!
//! - **Retries** apply to every node kind when `retry.max_retries > 0` and the
//!   error is [`NodeError::retryable`]. The delay is the larger of the policy
//!   delay and [`NodeError::retry_after_ms`] (capped at five minutes), awaited
//!   inside the node's own future, so other nodes keep running. The execution id is stable across
//!   attempts, and a retry that eventually succeeds is not a handled error.
//! - **Timeouts** (`timeout_ms`, else [`Limits::default_node_timeout_ms`]) apply
//!   per attempt when the clock supports timers. The attempt deadline is also
//!   exposed as [`NodeContext::deadline_ms`](crate::node::NodeContext) so nodes
//!   that block synchronously can bound themselves.
//! - After retries, the error strategy applies:
//!   - `fail`: the frame fails. In the root frame the run ends with `RunFailed`
//!     and in-flight nodes are dropped. In a container body, the container node
//!     receives [`SubgraphResult::Failed`](crate::node::SubgraphResult).
//!   - `fail-branch`: outputs `error_message` / `error_type`; only `fail-branch`
//!     edges are taken.
//!   - `default-value`: outputs are the defaults plus `error_message` /
//!     `error_type`; non-branch nodes then continue as on success. Branch nodes
//!     follow the handle named by a `branch` default value, or skip every
//!     outgoing path if there is none.
//!
//!   Handled failures are counted in `RunSucceeded::handled_errors`.
//! - A panicking node fails with error type `panic` (when unwinding is enabled).
//!
//! # Limits
//!
//! `max_steps` counts every node attempt in every frame and persists across
//! pause / resume. `max_duration_ms` covers the whole run (including time before
//! a pause) and is enforced by a timer race. `max_depth` bounds container nesting.
//!
//! # Containers
//!
//! Container nodes receive [`NodeContext::subgraph`](crate::node::NodeContext),
//! which runs the container's body as a child frame (`"<parent>/<container>[i]"`)
//! with the same scheduler, event sink, step budget and control signals. Writes
//! to namespaces outside the body (container variables, conversation variables)
//! are returned in `SubgraphResult::Completed::writes`; `broke` reports whether a
//! `loop-end` node completed. Usage of container nodes themselves is not added
//! to the run total (their children are already counted).
//!
//! # Pause / resume / abort
//!
//! - [`RunHandle::pause`] stops scheduling everywhere, lets in-flight nodes
//!   finish and then ends with `RunPaused` carrying a [`Snapshot`].
//! - A node returning [`NodeOutcome::Paused`](crate::node::NodeOutcome) pauses
//!   only itself: independent branches keep running until nothing else can make
//!   progress, then the run pauses.
//! - On resume ([`RunInput::resume`]) the snapshot's graph fingerprint and
//!   format are verified, env / sys / conversation values from the new
//!   [`RunInput`] are applied, pending nodes re-run and paused nodes are resumed
//!   with their saved state plus their entry in [`ResumeFrom::inputs`].
//!   Inputs are looked up by the frame-qualified key `"<frame>/<node_id>"`
//!   first (e.g. `"review[2]/approve"` for a node paused inside the third
//!   item of iteration `review`; the frame is [`PauseReason::frame`]), then by
//!   the plain node id, which applies to every frame.
//! - [`RunHandle::abort`] drops every in-flight node future immediately.

mod control;
mod frame;
mod ids;
mod root;

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{FutureExt, Stream, StreamExt};
use indexmap::IndexMap;

use self::control::{Control, Stop};
use crate::event::{Event, PauseReason};
use crate::graph::Graph;
use crate::node::{Clock, EventSink, Services, default_clock};
use crate::pool::Selector;
use crate::snapshot::Snapshot;
use crate::value::Value;

#[cfg(doc)]
use crate::node::{NodeError, NodeOutput};

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
        Self {
            max_steps: 10_000,
            max_duration_ms: None,
            max_depth: 8,
            default_node_timeout_ms: None,
            max_concurrency: 64,
        }
    }
}

/// Input for a run.
#[derive(Clone, Debug, Default)]
pub struct RunInput {
    /// Values for the start node's declared variables (written into the entry node's namespace).
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

impl RunInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a start input.
    pub fn input(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.inputs.insert(name.into(), value.into());
        self
    }

    /// Add a system variable.
    pub fn sys(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.sys.insert(name.into(), value.into());
        self
    }

    /// Add an environment variable.
    pub fn env(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Resume from a snapshot.
    pub fn resume(mut self, snapshot: Snapshot, inputs: IndexMap<String, Value>) -> Self {
        self.resume = Some(ResumeFrom::new(snapshot, inputs));
        self
    }
}

/// Resume information.
#[derive(Clone, Debug)]
pub struct ResumeFrom {
    pub snapshot: Snapshot,
    /// Host input for paused nodes (e.g. a submitted human-input form), keyed by
    /// `"<frame>/<node_id>"` for one specific frame or by plain node id for any
    /// frame. Root-frame nodes use the plain node id.
    pub inputs: IndexMap<String, Value>,
    /// Plaintext for secrets outside `env.*` (e.g. secret-typed start inputs or
    /// conversation variables), keyed by dotted selector (`"start.api_key"`).
    /// Snapshots never store secret plaintext: such values are restored as
    /// redacted secrets ([`Value::redacted_secret`]) and nodes that need them fail
    /// until they are re-supplied here. Only selectors that currently hold a
    /// redacted secret are overwritten.
    pub secrets: IndexMap<String, Value>,
}

impl ResumeFrom {
    /// Resume `snapshot` with host input for paused nodes.
    pub fn new(snapshot: Snapshot, inputs: IndexMap<String, Value>) -> Self {
        Self { snapshot, inputs, secrets: IndexMap::new() }
    }

    /// Re-supply a secret that is not an environment variable.
    pub fn secret(mut self, selector: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets.insert(selector.into(), Value::Secret(Arc::from(value.into())));
        self
    }
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

    /// Share an existing services registry.
    pub fn with_shared_services(mut self, services: Arc<Services>) -> Self {
        self.services = services;
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
    pub fn run(&self, input: RunInput) -> RunStream {
        let (tx, rx) = mpsc::unbounded::<Event>();
        let control = Arc::new(Control::default());
        let sink: EventSink = Arc::new(move |event| {
            // The receiver only disappears when the consumer dropped the stream,
            // in which case the run is being torn down anyway.
            let _ = tx.unbounded_send(event);
        });
        let driver = root::run_root(self.clone(), input, control.clone(), sink).boxed();
        RunStream { driver: Some(driver), rx, finished: false, handle: RunHandle { control } }
    }

    /// Run to completion and return the terminal event, discarding the others.
    pub async fn run_to_end(&self, input: RunInput) -> Event {
        let mut stream = self.run(input);
        let mut last = None;
        while let Some(event) = stream.next().await {
            last = Some(event);
        }
        last.unwrap_or(Event::RunFailed {
            error: "run produced no events".to_owned(),
            error_type: "internal".to_owned(),
            usage: Default::default(),
            elapsed_ms: 0,
        })
    }
}

/// Stream of run events. `Send + 'static`. Dropping it cancels the run.
pub struct RunStream {
    driver: Option<BoxFuture<'static, ()>>,
    rx: mpsc::UnboundedReceiver<Event>,
    finished: bool,
    handle: RunHandle,
}

impl std::fmt::Debug for RunStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStream").field("finished", &self.finished).finish_non_exhaustive()
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
        if self.finished {
            return Poll::Ready(None);
        }
        if let Some(driver) = self.driver.as_mut() {
            if driver.as_mut().poll(cx).is_ready() {
                self.driver = None;
            }
        }
        match self.rx.poll_next_unpin(cx) {
            Poll::Ready(Some(event)) => {
                if event.is_terminal() {
                    self.finished = true;
                }
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => {
                self.finished = true;
                Poll::Ready(None)
            }
            Poll::Pending => {
                if self.driver.is_none() {
                    // Every event is sent synchronously, so a finished driver with an
                    // empty queue means the run is over.
                    self.finished = true;
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// Controls a running run. Cheap to clone, `Send + Sync`.
#[derive(Clone, Debug)]
pub struct RunHandle {
    control: Arc<Control>,
}

impl RunHandle {
    /// Cancel immediately: in-flight nodes are dropped. Ends with `RunAborted`.
    pub fn abort(&self, reason: impl Into<String>) {
        self.control.stop(Stop::Abort(reason.into()));
    }

    /// Stop scheduling new nodes, let in-flight nodes finish, then end with `RunPaused`
    /// (or `RunSucceeded` if nothing was left to run).
    pub fn pause(&self, reason: impl Into<String>) {
        self.control.request_pause(Some(PauseReason {
            frame: Arc::from(""),
            node_id: None,
            kind: "external".to_owned(),
            data: Value::from(reason.into()),
        }));
    }

    /// Write a variable into the root frame (applied between node completions).
    pub fn update_variable(&self, selector: Selector, value: Value) {
        self.control.push_update(selector, value);
    }
}
