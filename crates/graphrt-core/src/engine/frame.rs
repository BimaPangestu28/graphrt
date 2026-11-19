//! Frame execution: runs one scope (the root graph or one container body run).
//!
//! A frame owns its variable pool and per-node / per-edge state; the graph is
//! shared and immutable. Node futures run concurrently in a
//! [`FuturesUnordered`] (a node running alone is polled directly, without the
//! task allocation that costs); the frame loop is the only place where state changes,
//! so no locking is needed. Container nodes receive a [`ScopeRunner`] that runs
//! their body as a nested frame with the same machinery.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use futures::future::{BoxFuture, Either};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use indexmap::IndexMap;

use super::Limits;
use super::control::{Control, Stop};
use super::ids::{new_id, new_id_arc};
use crate::dsl::{ErrorStrategy, RetryPolicy};
use crate::event::{Event, FrameId, PauseReason, Usage};
use crate::graph::{EdgeIx, FAIL_BRANCH, Graph, NodeIx};
use crate::node::{
    Clock, EventSink, Node, NodeContext, NodeError, NodeOutcome, NodeOutput, NodeResult, NodeRole, PauseRequest,
    Resume, Services, Subgraph, SubgraphRequest, SubgraphResult, merge_writes,
};
use crate::pool::{ENV, SYS, Selector, VariablePool};
use crate::snapshot::{EdgeStatus, FrameSnapshot, NodeSnapshot, NodeStatus};
use crate::value::Value;
use crate::variables::Variables;

/// Node kind whose completion marks a loop body run as "broke".
const LOOP_END_KIND: &str = "loop-end";

/// Upper bound for a single retry delay, whatever the policy or server asks for.
const MAX_RETRY_DELAY_MS: u64 = 5 * 60 * 1000;

/// State shared by every frame of one run.
pub(crate) struct RunShared {
    pub(crate) graph: Arc<Graph>,
    pub(crate) services: Arc<Services>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) limits: Limits,
    pub(crate) control: Arc<Control>,
    pub(crate) sink: EventSink,
    pub(crate) steps: AtomicU64,
    pub(crate) handled_errors: AtomicU32,
    pub(crate) usage: Mutex<Usage>,
    /// Environment variables re-applied to every restored frame (snapshots exclude them).
    pub(crate) env: Vec<(String, Value)>,
    /// Re-supplied plaintext for redacted secrets outside `env` (see `ResumeFrom::secrets`).
    pub(crate) secrets: Vec<(Selector, Value)>,
    /// Host input for paused nodes, keyed by `"<frame>/<node_id>"` or plain node id.
    pub(crate) resume_inputs: IndexMap<String, Value>,
}

impl std::fmt::Debug for RunShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunShared").field("steps", &self.steps).finish_non_exhaustive()
    }
}

impl RunShared {
    pub(crate) fn emit(&self, event: Event) {
        (self.sink)(event);
    }

    /// Count one node attempt. Returns `false` (and stops the run) when the step budget is exhausted.
    fn take_step(&self) -> bool {
        let used = self.steps.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        if used > self.limits.max_steps {
            self.control.stop(Stop::Fail(format!("step limit of {} exceeded", self.limits.max_steps)));
            return false;
        }
        true
    }

    fn add_usage(&self, usage: &Usage) {
        let mut total = self.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        total.add(usage);
    }

    pub(crate) fn total_usage(&self) -> Usage {
        *self.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Host input for a node resumed in `frame`: the frame-qualified key
    /// (`"iter[2]/approve"`) wins over the plain node id (`"approve"`).
    fn resume_input(&self, frame: &str, node_id: &str) -> Option<Value> {
        if !frame.is_empty() {
            if let Some(value) = self.resume_inputs.get(&format!("{frame}/{node_id}")) {
                return Some(value.clone());
            }
        }
        self.resume_inputs.get(node_id).cloned()
    }
}

/// How a frame ended.
#[derive(Debug)]
pub(crate) enum FrameOutcome {
    Completed {
        pool: VariablePool,
        outputs: IndexMap<String, Value>,
        writes: Vec<(Selector, Value)>,
        usage: Usage,
        broke: bool,
    },
    Failed {
        error: NodeError,
        usage: Usage,
    },
    Paused {
        snapshot: Box<FrameSnapshot>,
        reasons: Vec<PauseReason>,
    },
    /// The run was aborted or hit a run-wide limit (see [`Control::stop_reason`]).
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Paused,
}

#[derive(Debug)]
struct NodeRt {
    state: State,
    /// Minted when the node starts, is skipped or is snapshotted, whichever
    /// comes first, so a node costs one clock read for its id and its start time.
    exec_id: Option<Arc<str>>,
    attempt: u32,
    /// Saved state of a node that paused itself (kept while paused, handed back on resume).
    resume_state: Option<serde_json::Value>,
}

/// Result of one node task (all attempts).
struct TaskDone {
    ix: NodeIx,
    attempt: u32,
    result: NodeResult,
    elapsed_ms: u64,
    /// Clock reading taken when the task finished, if it read the clock.
    finished_ms: Option<u64>,
}

/// One executing frame.
pub(crate) struct Frame {
    run: Arc<RunShared>,
    id: FrameId,
    depth: u32,
    is_root: bool,
    scope_nodes: Vec<NodeIx>,
    /// Ids of the scope's nodes, built on the first variable write that needs them.
    scope_ids: std::cell::OnceCell<HashSet<Arc<str>>>,
    pool: VariablePool,
    nodes: ByIndex<NodeRt>,
    edges: ByIndex<EdgeStatus>,
    outputs: IndexMap<String, Value>,
    writes: Vec<(Selector, Value)>,
    usage: Usage,
    broke: bool,
    reasons: Vec<PauseReason>,
    ready: VecDeque<NodeIx>,
    /// Incoming-edge tallies of nodes not yet decided, so readiness checks cost
    /// O(1) per resolved edge instead of rescanning every incoming edge (which
    /// made wide fan-ins quadratic). Built lazily from `edges` on first use.
    incoming: ByIndex<Incoming>,
    /// Clock reading shared by the work done between two waits of the frame
    /// loop (see [`Frame::now_ms`]); cleared before every wait.
    burst_now_ms: Option<u64>,
}

/// Resolution state of a node's incoming edges.
#[derive(Clone, Copy, Debug)]
struct Incoming {
    unresolved: usize,
    taken: bool,
}

/// Hasher for the frame's maps keyed by graph indices (`NodeIx`, `EdgeIx`): one
/// multiplication instead of SipHash. The keys are internal indices, never user
/// input, so hash-flooding resistance buys nothing here.
#[derive(Clone, Copy, Default)]
struct IndexHasher(u64);

impl std::hash::Hasher for IndexHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(*byte)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }

    fn write_usize(&mut self, index: usize) {
        // Fibonacci hashing: spreads consecutive indices over the high bits the
        // hash table uses for its control bytes.
        self.0 = (index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

/// A map keyed by graph indices.
type ByIndex<V> = HashMap<usize, V, std::hash::BuildHasherDefault<IndexHasher>>;

/// An empty [`ByIndex`] sized for `capacity` entries.
fn by_index<V>(capacity: usize) -> ByIndex<V> {
    HashMap::with_capacity_and_hasher(capacity, std::hash::BuildHasherDefault::default())
}

impl Frame {
    /// A fresh frame for the scope owned by `owner` (`None` = root).
    pub(crate) fn fresh(
        run: Arc<RunShared>,
        id: FrameId,
        owner: Option<NodeIx>,
        depth: u32,
        pool: VariablePool,
    ) -> Result<Self, NodeError> {
        let mut frame = Self::empty(run, id, owner, depth, pool)?;
        let entry = frame.scope_entry(owner)?;
        frame.mark_ready(entry);
        Ok(frame)
    }

    /// A frame restored from a snapshot. Environment variables are re-applied.
    pub(crate) fn restore(
        run: Arc<RunShared>,
        id: FrameId,
        owner: Option<NodeIx>,
        depth: u32,
        snapshot: FrameSnapshot,
    ) -> Result<Self, NodeError> {
        let mut pool = VariablePool::from_snapshot(snapshot.pool);
        for (name, value) in &run.env {
            pool.insert(ENV, name, value.clone());
        }
        for (selector, value) in &run.secrets {
            if pool.get(selector).is_some_and(|v| v.is_redacted_secret()) {
                // The selector exists, so the write cannot fail on a missing index.
                if pool.set_path(selector, value.clone()).is_err() {
                    continue;
                }
            }
        }
        let mut frame = Self::empty(run, id, owner, depth, pool)?;
        frame.outputs = snapshot.outputs.into_iter().map(|(k, v)| (k, v.from_snapshot_value())).collect();
        frame.writes = snapshot.writes.into_iter().map(|(s, v)| (s, v.from_snapshot_value())).collect();
        frame.broke = snapshot.broke;
        frame.usage = snapshot.usage;
        let graph = frame.run.graph.clone();
        for (node_id, ns) in snapshot.nodes {
            let ix = graph
                .index_of(&node_id)
                .filter(|ix| frame.scope_nodes.contains(ix))
                .ok_or_else(|| NodeError::fatal("snapshot", format!("snapshot references unknown node `{node_id}`")))?;
            let state = match ns.status {
                NodeStatus::Pending | NodeStatus::Paused => State::Ready,
                NodeStatus::Succeeded => State::Succeeded,
                NodeStatus::Failed => State::Failed,
                NodeStatus::Skipped => State::Skipped,
            };
            // A paused node always resumes, even when its saved state is `null`
            // (which a JSON round trip turns into `None`).
            let resume_state = if ns.status == NodeStatus::Paused {
                Some(ns.resume_state.unwrap_or(serde_json::Value::Null))
            } else {
                None
            };
            frame
                .nodes
                .insert(ix, NodeRt { state, exec_id: Some(Arc::from(ns.exec_id)), attempt: ns.attempt, resume_state });
        }
        let edge_index: HashMap<&str, EdgeIx> =
            graph.edges().iter().enumerate().map(|(ex, e)| (e.id.as_ref(), ex)).collect();
        for (edge_id, status) in snapshot.edges {
            let ex = edge_index
                .get(edge_id.as_str())
                .copied()
                .ok_or_else(|| NodeError::fatal("snapshot", format!("snapshot references unknown edge `{edge_id}`")))?;
            frame.edges.insert(ex, status);
        }
        // Queue pending nodes in topological order.
        let order = frame.scope_nodes.clone();
        for ix in order {
            if frame.nodes.get(&ix).is_some_and(|n| n.state == State::Ready) {
                frame.ready.push_back(ix);
            }
        }
        Ok(frame)
    }

    fn empty(
        run: Arc<RunShared>,
        id: FrameId,
        owner: Option<NodeIx>,
        depth: u32,
        pool: VariablePool,
    ) -> Result<Self, NodeError> {
        let scope = match owner {
            None => run.graph.root_scope(),
            Some(o) => run.graph.scope_of(o),
        }
        .ok_or_else(|| NodeError::fatal("graph", "scope not found"))?;
        let scope_nodes = scope.nodes.clone();
        let scope_len = scope_nodes.len();
        Ok(Self {
            is_root: owner.is_none(),
            run,
            id,
            depth,
            scope_nodes,
            scope_ids: std::cell::OnceCell::new(),
            pool,
            nodes: by_index(scope_len),
            edges: by_index(scope_len),
            outputs: IndexMap::new(),
            writes: Vec::new(),
            usage: Usage::default(),
            broke: false,
            reasons: Vec::new(),
            ready: VecDeque::new(),
            incoming: by_index(scope_len),
            burst_now_ms: None,
        })
    }

    fn scope_ids(&self) -> &HashSet<Arc<str>> {
        self.scope_ids.get_or_init(|| {
            self.scope_nodes.iter().filter_map(|&ix| self.run.graph.node(ix)).map(|n| n.id.clone()).collect()
        })
    }

    fn scope_entry(&self, owner: Option<NodeIx>) -> Result<NodeIx, NodeError> {
        let scope = match owner {
            None => self.run.graph.root_scope(),
            Some(o) => self.run.graph.scope_of(o),
        };
        scope.map(|s| s.entry).ok_or_else(|| NodeError::fatal("graph", "scope not found"))
    }

    /// Set a variable directly in this frame's pool.
    pub(crate) fn set_variable(&mut self, namespace: &str, name: &str, value: Value) {
        self.pool.insert(namespace, name, value);
    }

    fn mark_ready(&mut self, ix: NodeIx) {
        let rt = self.nodes.entry(ix).or_insert_with(|| NodeRt {
            state: State::Ready,
            exec_id: None,
            attempt: 0,
            resume_state: None,
        });
        rt.state = State::Ready;
        self.ready.push_back(ix);
    }

    /// The current time for bookkeeping done without waiting: node starts (id
    /// and start time) and skips.
    ///
    /// Everything between two waits of the frame loop takes microseconds, so it
    /// shares one clock reading, seeded by the finishing time a completed task
    /// already read. A chain then reads the clock once per node instead of
    /// twice, and a batch of nodes started together reads it once.
    fn now_ms(&mut self) -> u64 {
        *self.burst_now_ms.get_or_insert_with(|| self.run.now_ms())
    }

    fn node_id(&self, ix: NodeIx) -> Arc<str> {
        self.run.graph.node(ix).map_or_else(|| Arc::from(""), |n| Arc::clone(&n.id))
    }

    /// Run the frame to completion, failure, pause or stop.
    pub(crate) async fn run(mut self) -> FrameOutcome {
        let control = self.run.control.clone();
        let mut running = FuturesUnordered::new();
        // A node that runs alone (the common case in chains) is polled from
        // this slot instead of `running`, which would allocate a task for it.
        let mut solo = std::pin::pin!(None);
        loop {
            if control.is_stopped() {
                return FrameOutcome::Stopped;
            }
            let seen = control.version();
            if self.is_root {
                self.apply_external_updates();
            }
            // Schedule ready nodes.
            if !control.pause_requested() {
                let max_concurrency = self.run.limits.max_concurrency.max(1);
                while running.len() + usize::from(solo.is_some()) < max_concurrency {
                    let Some(ix) = self.ready.pop_front() else { break };
                    let task = self.start(ix);
                    if solo.is_none() && running.is_empty() && self.ready.is_empty() {
                        solo.set(Some(task));
                    } else {
                        running.push(task);
                    }
                }
            }
            if running.is_empty() && solo.is_none() {
                return self.finish();
            }
            self.burst_now_ms = None;
            let next = futures::future::poll_fn(|cx| poll_tasks(cx, solo.as_mut(), &mut running));
            let changed = control.changed(seen);
            futures::pin_mut!(changed);
            match futures::future::select(next, changed).await {
                Either::Left((Some(done), _)) => {
                    if let Some(outcome) = self.on_done(done) {
                        return outcome;
                    }
                    if let Some(outcome) = self.commit_finished(&mut running) {
                        return outcome;
                    }
                }
                Either::Left((None, _)) | Either::Right(((), _)) => {}
            }
        }
    }

    /// Apply every task in `running` that has already finished, before the
    /// loop starts new nodes.
    ///
    /// Running nodes hold snapshots of the pool, so the first commit after a
    /// node starts copies the namespace table and the touched shard. Starting a
    /// node between every two commits (as happens in a wide fan-out) made every
    /// commit pay that copy; committing the finished batch first makes it share
    /// one copy of the table and of each shard it touches.
    ///
    /// The tasks are polled with a no-op waker. That is sound because the frame
    /// loop always polls `running` again with its real waker before waiting.
    fn commit_finished<F: Future<Output = TaskDone>>(
        &mut self,
        running: &mut FuturesUnordered<F>,
    ) -> Option<FrameOutcome> {
        if running.is_empty() {
            return None;
        }
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        while let std::task::Poll::Ready(Some(done)) = running.poll_next_unpin(&mut cx) {
            if let Some(outcome) = self.on_done(done) {
                return Some(outcome);
            }
        }
        None
    }

    fn apply_external_updates(&mut self) {
        for (selector, value) in self.run.control.take_updates() {
            // An update addressing a missing array index cannot be applied; it is
            // dropped without a `VariableUpdated` event.
            if self.pool.set_path(&selector, value.clone()).is_err() {
                continue;
            }
            self.run.emit(Event::VariableUpdated { frame: self.id.clone(), node_id: Arc::from(""), selector, value });
        }
    }

    /// Nothing is running: decide between completed and paused.
    fn finish(self) -> FrameOutcome {
        let has_paused = self.nodes.values().any(|n| n.state == State::Paused);
        if has_paused || !self.ready.is_empty() {
            let reasons = self.reasons.clone();
            return FrameOutcome::Paused { snapshot: Box::new(self.snapshot()), reasons };
        }
        FrameOutcome::Completed {
            pool: self.pool,
            outputs: self.outputs,
            writes: self.writes,
            usage: self.usage,
            broke: self.broke,
        }
    }

    /// Serializable state of this frame.
    pub(crate) fn snapshot(&self) -> FrameSnapshot {
        let mut nodes = IndexMap::new();
        for &ix in &self.scope_nodes {
            let Some(rt) = self.nodes.get(&ix) else { continue };
            let status = match rt.state {
                State::Ready | State::Running => {
                    if rt.resume_state.is_some() {
                        NodeStatus::Paused
                    } else {
                        NodeStatus::Pending
                    }
                }
                State::Paused => NodeStatus::Paused,
                State::Succeeded => NodeStatus::Succeeded,
                State::Failed => NodeStatus::Failed,
                State::Skipped => NodeStatus::Skipped,
            };
            nodes.insert(
                String::from(&*self.node_id(ix)),
                NodeSnapshot {
                    status,
                    exec_id: rt.exec_id.as_deref().map_or_else(|| new_id(self.run.now_ms()), String::from),
                    attempt: rt.attempt,
                    resume_state: rt.resume_state.clone(),
                },
            );
        }
        let mut edges = IndexMap::new();
        let mut known: Vec<(&EdgeIx, &EdgeStatus)> = self.edges.iter().collect();
        known.sort_by_key(|(ex, _)| **ex);
        for (ex, status) in known {
            if let Some(e) = self.run.graph.edge(*ex) {
                edges.insert(String::from(&*e.id), *status);
            }
        }
        FrameSnapshot {
            pool: self.pool.to_snapshot(),
            nodes,
            edges,
            outputs: self.outputs.iter().map(|(k, v)| (k.clone(), v.to_snapshot_value())).collect(),
            writes: self.writes.iter().map(|(s, v)| (s.clone(), v.to_snapshot_value())).collect(),
            broke: self.broke,
            usage: self.usage,
        }
    }

    /// Start a node: emit `NodeStarted` and build its task future.
    ///
    /// The future is returned unboxed: [`FuturesUnordered`] already stores each
    /// future in its own task allocation, so boxing it first would cost a
    /// second allocation per node execution.
    fn start(&mut self, ix: NodeIx) -> impl Future<Output = TaskDone> + Send + 'static {
        let run = self.run.clone();
        let Some(gn) = run.graph.node(ix) else {
            let error = NodeError::fatal("graph", "unknown node index");
            return Either::Left(futures::future::ready(TaskDone {
                ix,
                attempt: 1,
                result: Err(error),
                elapsed_ms: 0,
                finished_ms: None,
            }));
        };
        let started = self.now_ms();
        let rt = self.nodes.entry(ix).or_insert_with(|| NodeRt {
            state: State::Ready,
            exec_id: None,
            attempt: 0,
            resume_state: None,
        });
        rt.state = State::Running;
        let exec_id = Arc::clone(rt.exec_id.get_or_insert_with(|| new_id_arc(started)));
        // Boxed: resuming is rare, and the task future carries this for its whole life.
        let resume: Option<Box<Resume>> = rt
            .resume_state
            .take()
            .map(|state| Box::new(Resume { state, input: run.resume_input(&self.id, gn.id.as_ref()) }));

        run.emit(Event::NodeStarted {
            frame: self.id.clone(),
            node_id: Arc::clone(&gn.id),
            exec_id: Arc::clone(&exec_id),
            kind: gn.kind.clone(),
            title: gn.title.clone(),
            attempt: 1,
        });

        let subgraph: Option<Arc<dyn Subgraph>> = if gn.role == NodeRole::Container {
            Some(Arc::new(ScopeRunner {
                run: run.clone(),
                owner: ix,
                parent_frame: self.id.clone(),
                depth: self.depth,
            }))
        } else {
            None
        };
        let node: Arc<dyn Node> = gn.node.clone();
        let node_id: Arc<str> = gn.id.clone();
        // Only the retry policy is needed while running; cloning the whole policy
        // would also copy (and allocate for) `default-value` error outputs.
        let retry = gn.policy.retry.clone();
        let timeout_ms = gn.policy.timeout_ms.or(run.limits.default_node_timeout_ms);
        let pool = self.pool.clone();
        let frame = self.id.clone();

        Either::Right(async move {
            let mut attempt: u32 = 1;
            loop {
                if !run.take_step() {
                    let error = NodeError::fatal("limit_exceeded", "step limit exceeded");
                    return TaskDone { ix, attempt, result: Err(error), elapsed_ms: 0, finished_ms: None };
                }
                let deadline_ms = match timeout_ms {
                    Some(ms) if run.clock.supports_timers() => Some(run.now_ms().saturating_add(ms)),
                    _ => None,
                };
                let ctx = NodeContext::new(
                    node_id.clone(),
                    exec_id.clone(),
                    frame.clone(),
                    attempt,
                    pool.clone(),
                    run.services.clone(),
                    run.clock.clone(),
                    run.sink.clone(),
                )
                .with_resume(resume.as_deref().cloned())
                .with_subgraph(subgraph.clone())
                .with_deadline(deadline_ms);
                // The result and the error stay inside this block, so the task
                // does not reserve room for them while it waits for a retry.
                let delay_ms = {
                    let result = run_attempt(node.as_ref(), ctx, timeout_ms, run.clock.as_ref()).await;
                    let error = match result {
                        Err(error) if error.retryable && attempt <= retry.max_retries && !run.control.is_stopped() => {
                            error
                        }
                        result => {
                            let finished = run.now_ms();
                            let elapsed_ms = finished.saturating_sub(started);
                            return TaskDone { ix, attempt, result, elapsed_ms, finished_ms: Some(finished) };
                        }
                    };
                    let delay_ms = retry_delay_ms(&retry, attempt, &error);
                    run.emit(Event::NodeRetry {
                        frame: frame.clone(),
                        node_id: Arc::clone(&node_id),
                        exec_id: Arc::clone(&exec_id),
                        attempt: attempt.saturating_add(1),
                        error: error.to_string(),
                        delay_ms,
                    });
                    delay_ms
                };
                attempt = attempt.saturating_add(1);
                if delay_ms > 0 {
                    run.clock.sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        })
    }

    /// Apply a finished node. Returns `Some` when the frame must end now.
    fn on_done(&mut self, done: TaskDone) -> Option<FrameOutcome> {
        let TaskDone { ix, attempt, result, elapsed_ms, finished_ms } = done;
        if finished_ms.is_some() {
            self.burst_now_ms = finished_ms;
        }
        if let Some(rt) = self.nodes.get_mut(&ix) {
            rt.attempt = attempt;
        }
        if self.run.control.is_stopped() {
            return Some(FrameOutcome::Stopped);
        }
        let Some(gn) = self.run.graph.node(ix) else {
            return Some(FrameOutcome::Failed {
                error: NodeError::fatal("graph", "unknown node index"),
                usage: self.usage,
            });
        };
        let role = gn.role;
        match result {
            Ok(NodeOutcome::Completed(output)) => {
                if role == NodeRole::Branch && output.branch.is_none() {
                    let error = NodeError::fatal("invalid_branch", "branch node completed without selecting a branch");
                    return self.on_error(ix, error, elapsed_ms);
                }
                self.on_success(ix, output, elapsed_ms)
            }
            Ok(NodeOutcome::Paused(request)) => {
                self.on_paused(ix, request);
                None
            }
            Err(error) => self.on_error(ix, error, elapsed_ms),
        }
    }

    fn exec_id(&self, ix: NodeIx) -> Arc<str> {
        self.nodes.get(&ix).and_then(|n| n.exec_id.clone()).unwrap_or_else(|| Arc::from(""))
    }

    fn set_state(&mut self, ix: NodeIx, state: State) {
        if let Some(rt) = self.nodes.get_mut(&ix) {
            rt.state = state;
        }
    }

    fn on_success(&mut self, ix: NodeIx, output: NodeOutput, elapsed_ms: u64) -> Option<FrameOutcome> {
        let graph = self.run.graph.clone();
        let gn = graph.node(ix)?;
        let node_id = Arc::clone(&gn.id);
        if let Err(error) = self.commit(ix, &output.outputs, &output.writes) {
            return self.on_error(ix, error, elapsed_ms);
        }
        if let Some(usage) = &output.usage {
            self.usage.add(usage);
            if gn.role != NodeRole::Container {
                self.run.add_usage(usage);
            }
        }
        if gn.role == NodeRole::Response {
            merge_response(&mut self.outputs, &output.outputs);
        }
        if &*gn.kind == LOOP_END_KIND {
            self.broke = true;
        }
        self.set_state(ix, State::Succeeded);
        self.run.emit(Event::NodeSucceeded {
            frame: self.id.clone(),
            node_id,
            exec_id: self.exec_id(ix),
            outputs: output.outputs,
            branch: output.branch.clone(),
            usage: output.usage,
            metadata: output.metadata,
            elapsed_ms,
        });
        match (gn.role, output.branch) {
            (NodeRole::Branch, Some(handle)) => self.traverse(ix, |h| h == handle),
            _ => self.traverse(ix, |h| h != FAIL_BRANCH),
        }
        None
    }

    fn on_paused(&mut self, ix: NodeIx, request: PauseRequest) {
        let node_id = self.node_id(ix);
        let role = self.run.graph.node(ix).map(|n| n.role);
        let reason = PauseReason {
            frame: self.id.clone(),
            node_id: Some(String::from(&*node_id)),
            kind: request.kind,
            data: request.data,
        };
        if let Some(rt) = self.nodes.get_mut(&ix) {
            rt.state = State::Paused;
            rt.resume_state = Some(request.state);
        }
        // Container pauses are caused by inner nodes, which already reported their reasons.
        if role != Some(NodeRole::Container) {
            self.run.control.add_reason(reason.clone());
        }
        self.reasons.push(reason.clone());
        self.run.emit(Event::NodePaused { frame: self.id.clone(), node_id, exec_id: self.exec_id(ix), reason });
    }

    fn on_error(&mut self, ix: NodeIx, error: NodeError, elapsed_ms: u64) -> Option<FrameOutcome> {
        let graph = self.run.graph.clone();
        let gn = graph.node(ix)?;
        let node_id = Arc::clone(&gn.id);
        let mut outputs = Variables::new();
        let handled = !matches!(gn.policy.on_error, ErrorStrategy::Fail);
        if let ErrorStrategy::DefaultValue { values } = &gn.policy.on_error {
            outputs.extend(values.iter().map(|(k, v)| (k, v.clone())));
        }
        if handled {
            outputs.insert("error_message", Value::from(error.message.as_str()));
            outputs.insert("error_type", Value::from(error.kind.as_str()));
        }
        self.set_state(ix, State::Failed);
        self.run.emit(Event::NodeFailed {
            frame: self.id.clone(),
            node_id,
            exec_id: self.exec_id(ix),
            error: error.message.clone(),
            error_type: error.kind.clone(),
            handled,
            outputs: outputs.clone(),
            elapsed_ms,
        });
        match &gn.policy.on_error {
            ErrorStrategy::Fail => Some(FrameOutcome::Failed { error, usage: self.usage }),
            ErrorStrategy::FailBranch => {
                self.run.handled_errors.fetch_add(1, Ordering::AcqRel);
                if let Err(error) = self.commit(ix, &outputs, &[]) {
                    return Some(FrameOutcome::Failed { error, usage: self.usage });
                }
                self.traverse(ix, |h| h == FAIL_BRANCH);
                None
            }
            ErrorStrategy::DefaultValue { values } => {
                self.run.handled_errors.fetch_add(1, Ordering::AcqRel);
                if let Err(error) = self.commit(ix, &outputs, &[]) {
                    return Some(FrameOutcome::Failed { error, usage: self.usage });
                }
                if gn.role == NodeRole::Branch {
                    // Branch nodes follow the handle named by the `branch` default, if any;
                    // otherwise every outgoing path is skipped.
                    let handle = values.get("branch").and_then(Value::as_str).map(str::to_owned);
                    self.traverse(ix, |h| handle.as_deref() == Some(h));
                } else {
                    self.traverse(ix, |h| h != FAIL_BRANCH);
                }
                None
            }
        }
    }

    /// Store outputs under the node namespace and apply its variable writes.
    ///
    /// All-or-nothing: writes to `sys.*` / `env.*` (host-owned) and writes to
    /// out-of-range array indexes are rejected before anything is committed.
    fn commit(&mut self, ix: NodeIx, outputs: &Variables, writes: &[(Selector, Value)]) -> Result<(), NodeError> {
        let node_id = self.node_id(ix);
        for (selector, _) in writes {
            let namespace = selector.namespace();
            if namespace == SYS || namespace == ENV {
                return Err(NodeError::fatal(
                    "forbidden_write",
                    format!("nodes may not write `{selector}`: `{namespace}` variables are owned by the host"),
                ));
            }
        }
        if writes.is_empty() {
            // Nothing can fail: skip the undo log (one selector and one value
            // lookup per output), which most nodes would never use.
            self.pool.set_outputs_shared(&node_id, outputs);
            return Ok(());
        }
        // Apply in place (cloning the pool here would force a copy-on-write copy on
        // every commit), remembering prior values so a failing write is undone.
        let mut undo: Vec<(Selector, Option<Value>)> = Vec::with_capacity(outputs.len() + writes.len());
        for key in outputs.keys() {
            let root = Selector::of(&node_id, key);
            let previous = self.pool.get(&root);
            undo.push((root, previous));
        }
        self.pool.set_outputs_shared(&node_id, outputs);
        for (selector, value) in writes {
            let root = selector.root();
            let previous = self.pool.get(&root);
            undo.push((root, previous));
            if let Err(e) = self.pool.set_path(selector, value.clone()) {
                for (root, previous) in undo.into_iter().rev() {
                    match previous {
                        Some(value) => self.pool.set(&root, value),
                        None => {
                            self.pool.remove(&root);
                        }
                    }
                }
                return Err(NodeError::fatal("invalid_write", e.to_string()));
            }
        }
        let external: Vec<(Selector, Value)> =
            writes.iter().filter(|(selector, _)| !self.scope_ids().contains(selector.namespace())).cloned().collect();
        merge_writes(&mut self.writes, external);
        for (selector, value) in writes {
            self.run.emit(Event::VariableUpdated {
                frame: self.id.clone(),
                node_id: node_id.clone(),
                selector: selector.clone(),
                value: value.clone(),
            });
        }
        Ok(())
    }

    /// Resolve every outgoing edge of `ix` (taken when `take(handle)`), then
    /// propagate readiness and skips downstream.
    fn traverse(&mut self, ix: NodeIx, take: impl Fn(&str) -> bool) {
        let graph = self.run.graph.clone();
        for &ex in graph.outgoing(ix) {
            let Some(edge) = graph.edge(ex) else { continue };
            let status = if take(&edge.handle) { EdgeStatus::Taken } else { EdgeStatus::Skipped };
            self.resolve_edge(ex, edge.target, status);
        }
        // Last edge first, as the stack-based resolution always did.
        self.resolve_targets(graph.outgoing(ix).iter().rev().filter_map(|&ex| graph.edge(ex)).map(|e| e.target));
    }

    /// Record an edge's status, keeping the target's incoming tally current.
    fn resolve_edge(&mut self, ex: EdgeIx, target: NodeIx, status: EdgeStatus) {
        let previous = self.edges.insert(ex, status);
        if previous.is_some_and(|p| p != EdgeStatus::Unknown) {
            return; // already counted
        }
        if let Some(tally) = self.incoming.get_mut(&target) {
            tally.unresolved = tally.unresolved.saturating_sub(1);
            tally.taken |= status == EdgeStatus::Taken;
        }
    }

    /// Incoming tally of `target`, computed from the edge states on first use.
    fn incoming_tally(&mut self, target: NodeIx) -> Incoming {
        if let Some(tally) = self.incoming.get(&target) {
            return *tally;
        }
        let mut tally = Incoming { unresolved: 0, taken: false };
        for &ex in self.run.graph.incoming(target) {
            match self.edges.get(&ex).copied().unwrap_or(EdgeStatus::Unknown) {
                EdgeStatus::Unknown => tally.unresolved += 1,
                EdgeStatus::Taken => tally.taken = true,
                EdgeStatus::Skipped => {}
            }
        }
        self.incoming.insert(target, tally);
        tally
    }

    /// Worklist propagation: a node becomes ready once every incoming edge is
    /// resolved and at least one is taken; it is skipped when all are skipped.
    fn resolve_targets(&mut self, targets: impl Iterator<Item = NodeIx>) {
        let graph = self.run.graph.clone();
        // Targets reached by skip propagation are resolved before the remaining
        // initial ones (a stack); the stack only allocates when a node is skipped.
        let mut work: Vec<NodeIx> = Vec::new();
        let mut targets = targets;
        while let Some(target) = work.pop().or_else(|| targets.next()) {
            if self.nodes.contains_key(&target) {
                continue; // already decided
            }
            let tally = self.incoming_tally(target);
            if tally.unresolved > 0 {
                continue;
            }
            self.incoming.remove(&target);
            if tally.taken {
                self.mark_ready(target);
                continue;
            }
            // Every incoming edge skipped: skip the node and its outgoing edges.
            let now = self.now_ms();
            self.nodes.insert(
                target,
                NodeRt { state: State::Skipped, exec_id: Some(new_id_arc(now)), attempt: 0, resume_state: None },
            );
            self.run.emit(Event::NodeSkipped { frame: self.id.clone(), node_id: self.node_id(target) });
            for &ex in graph.outgoing(target) {
                if let Some(edge) = graph.edge(ex) {
                    self.resolve_edge(ex, edge.target, EdgeStatus::Skipped);
                    work.push(edge.target);
                }
            }
        }
    }
}

/// Poll the solo task, then the task set, for the next finished node. `None`
/// means nothing is running.
fn poll_tasks<F: Future<Output = TaskDone>>(
    cx: &mut std::task::Context<'_>,
    mut solo: std::pin::Pin<&mut Option<F>>,
    running: &mut FuturesUnordered<F>,
) -> std::task::Poll<Option<TaskDone>> {
    use std::task::Poll;
    if let Some(task) = solo.as_mut().as_pin_mut() {
        if let Poll::Ready(done) = task.poll(cx) {
            solo.set(None);
            return Poll::Ready(Some(done));
        }
    }
    match running.poll_next_unpin(cx) {
        Poll::Ready(None) if solo.is_some() => Poll::Pending,
        other => other,
    }
}

/// Merge a response node's outputs into the frame outputs: `answer` is
/// concatenated, other keys overwrite.
fn merge_response(frame_outputs: &mut IndexMap<String, Value>, outputs: &Variables) {
    for (key, value) in outputs {
        if key == "answer" {
            if let Some(Value::String(existing)) = frame_outputs.get(key) {
                let joined = format!("{existing}{}", value.to_text());
                frame_outputs.insert(key.to_owned(), Value::from(joined));
                continue;
            }
        }
        frame_outputs.insert(key.to_owned(), value.clone());
    }
}

/// Delay before retrying after failed attempt number `attempt`: the larger of
/// the policy delay and the error's `retry_after_ms`, capped.
fn retry_delay_ms(retry: &RetryPolicy, attempt: u32, error: &NodeError) -> u64 {
    retry.delay_ms(attempt).max(error.retry_after_ms.unwrap_or(0)).min(MAX_RETRY_DELAY_MS)
}

/// Run one attempt with an optional timeout, converting panics into node errors.
///
/// The node's future and the timer are created before the returned future is
/// first polled, so it holds only those two (not the [`NodeContext`]): this
/// future lives inside every node task, and a smaller task is cheaper to build
/// and move.
fn run_attempt<'a>(
    node: &'a dyn Node,
    ctx: NodeContext,
    timeout_ms: Option<u64>,
    clock: &dyn Clock,
) -> impl Future<Output = NodeResult> + Send + 'a {
    let guarded = AssertUnwindSafe(node.run(ctx)).catch_unwind().map(|outcome| outcome.unwrap_or_else(panic_error));
    let timer = match timeout_ms {
        Some(ms) if clock.supports_timers() => Some((ms, clock.sleep(Duration::from_millis(ms)))),
        _ => None,
    };
    async move {
        let Some((ms, sleep)) = timer else {
            return guarded.await;
        };
        match futures::future::select(guarded, sleep).await {
            Either::Left((result, _)) => result,
            Either::Right(((), _)) => Err(NodeError::timeout(Duration::from_millis(ms))),
        }
    }
}

/// The node error for a panic payload caught while polling a node.
fn panic_error(panic: Box<dyn std::any::Any + Send>) -> NodeResult {
    let message = panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "node panicked".to_owned());
    Err(NodeError::fatal("panic", message))
}

/// [`Subgraph`] implementation handed to container nodes.
#[derive(Debug)]
pub(crate) struct ScopeRunner {
    run: Arc<RunShared>,
    owner: NodeIx,
    parent_frame: FrameId,
    depth: u32,
}

impl Subgraph for ScopeRunner {
    fn run(&self, request: SubgraphRequest) -> BoxFuture<'_, SubgraphResult> {
        Box::pin(async move {
            let depth = self.depth.saturating_add(1);
            if depth > self.run.limits.max_depth {
                return SubgraphResult::Failed {
                    error: NodeError::fatal(
                        "limit_exceeded",
                        format!("container nesting depth limit of {} exceeded", self.run.limits.max_depth),
                    ),
                    usage: Usage::default(),
                };
            }
            let owner_id = self.run.graph.node(self.owner).map(|n| String::from(&*n.id)).unwrap_or_default();
            let segment = format!("{owner_id}[{}]", request.index);
            let id: FrameId = if self.parent_frame.is_empty() {
                Arc::from(segment)
            } else {
                Arc::from(format!("{}/{segment}", self.parent_frame))
            };
            let frame = match request.resume {
                Some(snapshot) => Frame::restore(self.run.clone(), id, Some(self.owner), depth, snapshot),
                None => Frame::fresh(self.run.clone(), id, Some(self.owner), depth, request.pool),
            };
            let frame = match frame {
                Ok(f) => f,
                Err(error) => return SubgraphResult::Failed { error, usage: Usage::default() },
            };
            match frame.run().await {
                FrameOutcome::Completed { pool, outputs, writes, usage, broke } => {
                    SubgraphResult::Completed { pool, outputs, writes, usage, broke }
                }
                FrameOutcome::Failed { error, usage } => SubgraphResult::Failed { error, usage },
                FrameOutcome::Paused { snapshot, reasons } => SubgraphResult::Paused { snapshot: *snapshot, reasons },
                FrameOutcome::Stopped => {
                    let reason = match self.run.control.stop_reason() {
                        Some(Stop::Abort(r) | Stop::Fail(r)) => r,
                        None => "stopped".to_owned(),
                    };
                    SubgraphResult::Failed { error: NodeError::fatal("aborted", reason), usage: Usage::default() }
                }
            }
        })
    }
}
