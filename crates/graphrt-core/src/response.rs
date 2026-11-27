//! Ordered response streaming.
//!
//! The engine emits raw events as soon as they happen: tokens from parallel
//! LLM nodes interleave, and nodes finish in any order. A chat-style consumer
//! wants something different: the text of each response node (`answer`, and
//! optionally `end`) rendered in template order, with model tokens streamed
//! through as early as it is safe to do so.
//!
//! [`ResponseAssembler`] is a consumer-side transducer that does exactly that:
//!
//! ```ignore
//! let mut assembler = ResponseAssembler::new(engine.graph());
//! while let Some(event) = run.next().await {
//!     for chunk in assembler.push(&event) {
//!         print!("{}", chunk.text);
//!     }
//! }
//! ```
//!
//! ## Rules
//!
//! - Only response nodes of the **root scope** are assembled. Events from child
//!   frames (iteration / loop bodies) are ignored, unless
//!   [`ResponseAssembler::pass_through_unreferenced`] is enabled, in which case
//!   their chunks are forwarded verbatim.
//! - At most one response is *active* at a time. Responses are activated in the
//!   order they become **certain** to execute (ties broken by topological
//!   order). Certainty is a linear-time fixpoint over the graph — never a path
//!   enumeration:
//!   - a node that started or finished is certain, and so is the root entry;
//!   - a node is certain when one incoming edge comes from a certain source and
//!     that edge will be taken if the source succeeds: every edge of a
//!     non-branching node (except `fail-branch` edges), or the edge matching the
//!     handle a finished branch node selected. Node failures are not anticipated,
//!     except that an unfinished node with the `fail-branch` strategy makes none
//!     of its edges certain (either side may be taken).
//! - While a response is active, its template is drained left to right:
//!   literal text is emitted immediately; a variable produced by a graph node
//!   emits that node's buffered stream chunks for the variable's field as they
//!   arrive; once the node finishes, a variable that never streamed is emitted
//!   whole (`Value::to_text`). Variables of skipped or failed nodes render as
//!   nothing. Variables outside graph nodes (`sys.*`, `conversation.*`, ...)
//!   are resolved from values given via [`ResponseAssembler::with_value`] /
//!   [`ResponseAssembler::with_pool`], or render as nothing.
//! - `end` nodes do not stream by default (their outputs are structured data,
//!   delivered in `RunSucceeded`). Enable [`ResponseAssembler::include_end`] to
//!   render them as their output values joined by newlines.
//! - **Retries and handled failures never mix attempts.** A source node whose
//!   policy allows retries (`retry.max_retries > 0`) or handles errors
//!   (`on_error` other than `fail`) is *not* streamed live: its chunks are
//!   buffered and emitted only once the node finishes. A `node_retry` event
//!   discards the buffered chunks of the failed attempt. On success the final
//!   attempt's text is emitted; after a handled `default-value` failure the
//!   default output is emitted instead of any partial text; after a
//!   `fail-branch` failure (or an unhandled one) nothing is emitted for the
//!   variable. The trade-off: such nodes lose token-by-token streaming in
//!   exchange for an answer that always matches the node's final output.
//! - Ordering after handled failures follows the engine: a non-branch node with
//!   `default-value` continues along its regular edges, a `fail-branch` node
//!   along its `fail-branch` edges, and a branch node with `default-value`
//!   along the handle named by its `branch` default value (no edge if absent).
//! - `RunSucceeded` and `RunAborted` flush everything that can still be
//!   rendered, as does [`ResponseAssembler::finish`].
//! - `RunFailed` renders nothing more, and takes back what was rendered ahead
//!   of time: text is emitted as soon as a response node is certain to run,
//!   which does not anticipate failures. Chunks already handed out cannot be
//!   recalled, but [`ResponseAssembler::text_so_far`] then holds only the text
//!   of response nodes that ran, so the answer kept for a failed run never
//!   quotes a node that did not execute.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde::Deserialize;

use crate::dsl::ErrorStrategy;
use crate::event::Event;
use crate::graph::{FAIL_BRANCH, Graph, NodeIx};
use crate::node::NodeRole;
use crate::pool::{Selector, VariablePool};
use crate::snapshot::{EdgeStatus, FrameSnapshot, NodeStatus};
use crate::template::{Part, Template};
use crate::value::Value;

/// A piece of ordered response text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseChunk {
    /// Response node the text belongs to (or, for passed-through chunks, the
    /// node that produced them). Shared with the graph, so a chunk does not
    /// copy it.
    pub node_id: Arc<str>,
    pub text: String,
    /// `true` exactly once per response node, on an empty chunk marking that
    /// its template is fully rendered.
    pub is_final: bool,
}

impl ResponseChunk {
    fn text(node_id: &Arc<str>, text: String) -> Self {
        Self { node_id: Arc::clone(node_id), text, is_final: false }
    }

    fn done(node_id: &Arc<str>) -> Self {
        Self { node_id: Arc::clone(node_id), text: String::new(), is_final: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Unknown,
    Started,
    Succeeded,
    /// Failed but kept going through its error strategy.
    Handled,
    /// Failed without an error strategy: outputs never appear.
    Failed,
    Skipped,
}

impl Status {
    fn finished(self) -> bool {
        matches!(self, Status::Succeeded | Status::Handled | Status::Failed | Status::Skipped)
    }
}

#[derive(Debug)]
struct Response {
    node: NodeIx,
    id: Arc<str>,
    parts: Vec<Part>,
    queued: bool,
    done: bool,
}

#[derive(Debug, Default)]
struct StreamBuf {
    text: String,
}

/// Stream buffers by node id, then field (a node has few referenced fields).
type Streams = HashMap<Arc<str>, Vec<(Arc<str>, StreamBuf)>>;

fn stream_buf<'a>(streams: &'a Streams, node: &str, field: &str) -> Option<&'a StreamBuf> {
    streams.get(node)?.iter().find(|(f, _)| f.as_ref() == field).map(|(_, buf)| buf)
}

#[derive(Debug)]
struct Active {
    response: usize,
    part: usize,
    /// Byte offset already emitted from the current variable's stream buffer.
    offset: usize,
}

#[derive(Deserialize)]
struct EndOutput {
    value_selector: Selector,
}

/// Turns raw run events into ordered response text. See the module docs.
#[derive(Debug)]
pub struct ResponseAssembler {
    graph: Arc<Graph>,
    responses: Vec<Response>,
    status: Vec<Status>,
    branch: Vec<Option<String>>,
    /// Nodes whose streams are buffered until they finish (retry / handled-error policy).
    deferred: Vec<bool>,
    /// Edge states restored from a snapshot (indexed like `Graph::edges`).
    edge_state: Vec<Option<EdgeStatus>>,
    /// Response nodes fully rendered before a pause (restored via `resume_from`).
    rendered: Vec<bool>,
    /// Stream buffers of the `(node id, field)` pairs templates reference, by
    /// node id. Looked up by `&str`, so a chunk costs no allocation.
    streams: Streams,
    /// Namespaces templates reference: only their outputs are kept in `pool`.
    referenced: HashSet<Arc<str>>,
    pool: VariablePool,
    queue: VecDeque<usize>,
    active: Option<Active>,
    emitted: String,
    /// The pieces `emitted` is made of, in order: the response (an index into
    /// `responses`) and the byte length of its text.
    segments: Vec<(usize, usize)>,
    pass_through: bool,
    include_end: bool,
    finished: bool,
    /// Certainty of each node, maintained incrementally between events.
    certain: Vec<bool>,
    /// Whether `certain` reflects the current state (false after construction / resume).
    certain_valid: bool,
    /// Root nodes whose status changed since the last certainty refresh.
    changed: Vec<NodeIx>,
}

impl ResponseAssembler {
    /// Build an assembler for runs of `graph`.
    pub fn new(graph: &Arc<Graph>) -> Self {
        let mut this = Self {
            graph: Arc::clone(graph),
            responses: Vec::new(),
            status: vec![Status::Unknown; graph.nodes().len()],
            branch: vec![None; graph.nodes().len()],
            deferred: graph
                .nodes()
                .iter()
                .map(|n| n.policy.retry.max_retries > 0 || !matches!(n.policy.on_error, ErrorStrategy::Fail))
                .collect(),
            edge_state: vec![None; graph.edges().len()],
            rendered: vec![false; graph.nodes().len()],
            streams: Streams::new(),
            referenced: HashSet::new(),
            pool: VariablePool::new(),
            queue: VecDeque::new(),
            active: None,
            emitted: String::new(),
            segments: Vec::new(),
            pass_through: false,
            include_end: false,
            finished: false,
            certain: vec![false; graph.nodes().len()],
            certain_valid: false,
            changed: Vec::new(),
        };
        this.rebuild_responses();
        this
    }

    /// Forward chunks that no template references (and child-frame chunks) verbatim.
    pub fn pass_through_unreferenced(mut self, enabled: bool) -> Self {
        self.pass_through = enabled;
        self
    }

    /// Also render `end` nodes (their outputs joined by newlines).
    pub fn include_end(mut self, enabled: bool) -> Self {
        self.include_end = enabled;
        self.rebuild_responses();
        self
    }

    /// Provide a value for a non-node variable such as `sys.query`.
    pub fn with_value(mut self, selector: &Selector, value: Value) -> Self {
        self.pool.set(selector, value);
        self
    }

    /// Provide non-node variables in bulk (e.g. the run's `sys` / `conversation` values).
    pub fn with_pool(mut self, pool: VariablePool) -> Self {
        for (ns, name, value) in pool.iter() {
            self.pool.insert(ns, name, value.clone());
        }
        self
    }

    /// Continue assembling a resumed run from the root frame snapshot carried by
    /// `RunPaused` (`snapshot.root`).
    ///
    /// Restores every variable value of the snapshot (node outputs included, so
    /// templates can reference nodes that will not run again), node and edge
    /// states (for correct ordering), and marks response nodes that already
    /// succeeded before the pause as rendered, so they are not emitted twice.
    /// A response that was only partially rendered when the run paused is
    /// rendered again from the start.
    pub fn resume_from(mut self, snapshot: &FrameSnapshot) -> Self {
        let restored = VariablePool::from_snapshot(snapshot.pool.clone());
        for (ns, name, value) in restored.iter() {
            self.pool.insert(ns, name, value.clone());
        }
        for (ix, edge) in self.graph.edges().iter().enumerate() {
            if let (Some(state), Some(slot)) = (snapshot.edges.get(edge.id.as_ref()), self.edge_state.get_mut(ix)) {
                *slot = Some(*state);
            }
        }
        for (node_id, node) in &snapshot.nodes {
            let Some(ix) = self.root_node(node_id) else { continue };
            let status = match node.status {
                NodeStatus::Succeeded => Status::Succeeded,
                NodeStatus::Skipped => Status::Skipped,
                NodeStatus::Failed if self.has_taken_outgoing(ix) => Status::Handled,
                NodeStatus::Failed => Status::Failed,
                NodeStatus::Paused => Status::Started,
                NodeStatus::Pending => Status::Unknown,
            };
            if let Some(slot) = self.status.get_mut(ix) {
                *slot = status;
            }
            if let Some(slot) = self.rendered.get_mut(ix) {
                *slot = status == Status::Succeeded;
            }
        }
        self.certain_valid = false;
        self.rebuild_responses();
        self
    }

    fn has_taken_outgoing(&self, ix: NodeIx) -> bool {
        self.graph.outgoing(ix).iter().any(|&ex| self.edge_state.get(ex).copied().flatten() == Some(EdgeStatus::Taken))
    }

    /// All response text emitted so far, concatenated.
    pub fn text_so_far(&self) -> &str {
        &self.emitted
    }

    /// Feed one event; returns the response chunks it unlocks.
    pub fn push(&mut self, event: &Event) -> Vec<ResponseChunk> {
        let mut out = Vec::new();
        self.push_into(event, &mut out);
        out
    }

    /// [`ResponseAssembler::push`] appending to `out` instead of returning a
    /// new list, so a consumer can reuse one buffer for a whole run.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use graphrt_core::{Graph, Registry, response::ResponseAssembler};
    /// # let graph = Arc::new(Graph::from_text(r#"{"graph": {"nodes": [{"id": "start", "type": "start"}], "edges": []}}"#, &Registry::with_builtins()).unwrap());
    /// # let events: Vec<graphrt_core::Event> = Vec::new();
    /// let mut assembler = ResponseAssembler::new(&graph);
    /// let mut chunks = Vec::new();
    /// for event in &events {
    ///     assembler.push_into(event, &mut chunks);
    ///     for chunk in chunks.drain(..) {
    ///         print!("{}", chunk.text);
    ///     }
    /// }
    /// ```
    pub fn push_into(&mut self, event: &Event, out: &mut Vec<ResponseChunk>) {
        if self.finished {
            return;
        }
        match event {
            Event::NodeStarted { frame, node_id, .. } if frame.is_empty() => {
                self.set_status(node_id, Status::Started);
            }
            Event::NodeChunk { frame, node_id, field, chunk, .. } => {
                let buf = match self.streams.get_mut(node_id.as_ref()) {
                    Some(fields) if frame.is_empty() => fields.iter_mut().find(|(f, _)| f == field),
                    _ => None,
                };
                let buffered = match buf {
                    Some((_, buf)) => {
                        buf.text.push_str(chunk);
                        true
                    }
                    None => {
                        if self.pass_through && !chunk.is_empty() {
                            out.push(ResponseChunk::text(node_id, chunk.clone()));
                        }
                        false
                    }
                };
                if self.certain_valid {
                    // A chunk changes no node status: at most the active response advances.
                    if buffered {
                        self.drain(false, out);
                    }
                    return;
                }
            }
            Event::NodeSucceeded { frame, node_id, outputs, branch, .. } if frame.is_empty() => {
                if self.referenced.contains(node_id.as_ref()) {
                    self.pool.set_outputs(node_id, outputs);
                }
                if let Some(ix) = self.root_node(node_id)
                    && let Some(b) = self.branch.get_mut(ix)
                {
                    b.clone_from(branch);
                }
                self.set_status(node_id, Status::Succeeded);
            }
            Event::NodeFailed { frame, node_id, handled, outputs, .. } if frame.is_empty() => {
                if *handled && self.referenced.contains(node_id.as_ref()) {
                    self.pool.set_outputs(node_id, outputs);
                }
                self.set_status(node_id, if *handled { Status::Handled } else { Status::Failed });
            }
            Event::NodeSkipped { frame, node_id } if frame.is_empty() => {
                self.set_status(node_id, Status::Skipped);
            }
            Event::NodeRetry { frame, node_id, .. } if frame.is_empty() => {
                // The failed attempt's partial output must never reach the answer.
                if let Some(fields) = self.streams.get_mut(node_id.as_ref()) {
                    fields.iter_mut().for_each(|(_, buf)| buf.text.clear());
                }
                return;
            }
            Event::RunSucceeded { .. } | Event::RunAborted { .. } => {
                self.finish_into(out);
                return;
            }
            Event::RunFailed { .. } => {
                // Nothing more is rendered: what is left belongs to nodes that will not run.
                self.keep_text_of_nodes_that_ran();
                self.finished = true;
                return;
            }
            _ => return,
        }
        self.refresh_certainty();
        self.drain(false, out);
    }

    /// Flush: treat every pending variable as resolved and render what is
    /// available. Idempotent.
    pub fn finish(&mut self) -> Vec<ResponseChunk> {
        let mut out = Vec::new();
        self.finish_into(&mut out);
        out
    }

    fn finish_into(&mut self, out: &mut Vec<ResponseChunk>) {
        if self.finished {
            return;
        }
        self.refresh_certainty();
        self.drain(true, out);
        self.finished = true;
    }

    /// Drop from the emitted text what was rendered for response nodes that
    /// never executed (they were certain to run until the run failed).
    fn keep_text_of_nodes_that_ran(&mut self) {
        let mut kept = String::with_capacity(self.emitted.len());
        let mut segments = Vec::with_capacity(self.segments.len());
        let mut start = 0;
        for &(response, len) in &self.segments {
            let end = start + len;
            let ran = self
                .responses
                .get(response)
                .and_then(|r| self.status.get(r.node))
                .is_some_and(|status| matches!(status, Status::Succeeded | Status::Handled));
            if ran && let Some(text) = self.emitted.get(start..end) {
                kept.push_str(text);
                segments.push((response, len));
            }
            start = end;
        }
        self.emitted = kept;
        self.segments = segments;
    }

    fn rebuild_responses(&mut self) {
        let Some(scope) = self.graph.root_scope() else {
            return;
        };
        let mut responses = Vec::new();
        for &ix in &scope.nodes {
            let Some(node) = self.graph.node(ix) else { continue };
            if node.role != NodeRole::Response {
                continue;
            }
            let parts = match &*node.kind {
                "answer" => node
                    .doc
                    .config
                    .get("answer")
                    .and_then(serde_json::Value::as_str)
                    .map(|s| Template::parse(s).parts().to_vec())
                    .unwrap_or_default(),
                "end" if self.include_end => end_parts(&node.doc.config),
                _ => continue,
            };
            let rendered = self.rendered.get(ix).copied().unwrap_or(false);
            responses.push(Response { node: ix, id: node.id.clone(), parts, queued: rendered, done: rendered });
        }
        let mut old = std::mem::take(&mut self.streams);
        self.referenced.clear();
        for sel in responses.iter().flat_map(|r| r.parts.iter()).filter_map(|p| match p {
            Part::Var(sel) => Some(sel),
            Part::Text(_) => None,
        }) {
            let node: Arc<str> = Arc::from(sel.namespace());
            self.referenced.insert(Arc::clone(&node));
            if !sel.path().is_empty() {
                continue;
            }
            let fields = self.streams.entry(node).or_default();
            if fields.iter().any(|(f, _)| f.as_ref() == sel.name()) {
                continue;
            }
            // Keep text already buffered for the pair, if any.
            let buf = old
                .get_mut(sel.namespace())
                .and_then(|v| v.iter_mut().find(|(f, _)| f.as_ref() == sel.name()))
                .map(|(_, buf)| std::mem::take(buf))
                .unwrap_or_default();
            fields.push((Arc::from(sel.name()), buf));
        }
        self.responses = responses;
    }

    fn root_node(&self, id: &str) -> Option<NodeIx> {
        self.graph.index_of(id).filter(|&ix| self.graph.node(ix).is_some_and(|n| n.parent.is_none()))
    }

    fn set_status(&mut self, node_id: &str, status: Status) {
        if let Some(ix) = self.root_node(node_id)
            && let Some(s) = self.status.get_mut(ix)
        {
            // Never downgrade a finished node back to "started" (retries reuse the id).
            if !(s.finished() && status == Status::Started) {
                if *s != status {
                    self.changed.push(ix);
                }
                *s = status;
            }
        }
    }

    fn status_of(&self, ix: NodeIx) -> Status {
        self.status.get(ix).copied().unwrap_or(Status::Unknown)
    }

    /// Whether `ix` is certain to run, given the certainty of its predecessors.
    fn compute_certain(&self, ix: NodeIx, entry: NodeIx, certain: &[bool]) -> bool {
        match self.status_of(ix) {
            Status::Skipped => false,
            Status::Unknown => ix == entry || self.has_certain_edge(ix, certain),
            _ => true,
        }
    }

    /// Bring the certain set up to date and enqueue newly certain responses.
    ///
    /// Certainty only grows as a run progresses, so after one full pass each
    /// status change is propagated forward from the changed node: O(affected
    /// nodes) per event instead of O(graph). A node losing certainty (not
    /// expected in practice) falls back to a full recomputation.
    fn refresh_certainty(&mut self) {
        let Some(scope) = self.graph.root_scope() else {
            return;
        };
        let entry = scope.entry;
        let mut certain = std::mem::take(&mut self.certain);
        certain.resize(self.graph.nodes().len(), false);
        // The change list doubles as the work list; its buffer is handed back below.
        let mut work = std::mem::take(&mut self.changed);
        let mut full = !self.certain_valid;
        if !full {
            while let Some(ix) = work.pop() {
                let now = self.compute_certain(ix, entry, &certain);
                let before = certain.get(ix).copied().unwrap_or(false);
                if before && !now {
                    full = true;
                    break;
                }
                if now != before
                    && let Some(c) = certain.get_mut(ix)
                {
                    *c = now;
                }
                // Even when `ix` itself was already certain, its outgoing edges may
                // have become certain (e.g. a branch picked its handle).
                if now {
                    for &ex in self.graph.outgoing(ix) {
                        if let Some(edge) = self.graph.edge(ex)
                            && !certain.get(edge.target).copied().unwrap_or(false)
                            && self.compute_certain(edge.target, entry, &certain)
                        {
                            if let Some(c) = certain.get_mut(edge.target) {
                                *c = true;
                            }
                            work.push(edge.target);
                        }
                    }
                }
            }
        }
        work.clear();
        self.changed = work;
        if full {
            certain.iter_mut().for_each(|c| *c = false);
            for &ix in &scope.nodes {
                let is_certain = self.compute_certain(ix, entry, &certain);
                if let Some(c) = certain.get_mut(ix) {
                    *c = is_certain;
                }
            }
        }
        for (i, r) in self.responses.iter_mut().enumerate() {
            if !r.queued && certain.get(r.node).copied().unwrap_or(false) {
                r.queued = true;
                self.queue.push_back(i);
            }
        }
        self.certain = certain;
        self.certain_valid = true;
    }

    /// Whether an incoming edge of `ix` will be taken given its (certain) source.
    fn has_certain_edge(&self, ix: NodeIx, certain: &[bool]) -> bool {
        self.graph.incoming(ix).iter().filter_map(|&ex| self.graph.edge(ex).map(|e| (ex, e))).any(|(ex, edge)| {
            if !certain.get(edge.source).copied().unwrap_or(false) {
                return false;
            }
            // States restored from a snapshot are authoritative.
            match self.edge_state.get(ex).copied().flatten() {
                Some(EdgeStatus::Taken) => return true,
                Some(EdgeStatus::Skipped) => return false,
                Some(EdgeStatus::Unknown) | None => {}
            }
            let Some(src) = self.graph.node(edge.source) else { return false };
            let status = self.status_of(edge.source);
            let fail_branch = matches!(src.policy.on_error, ErrorStrategy::FailBranch);
            match status {
                Status::Failed | Status::Skipped => false,
                Status::Handled if fail_branch => edge.handle.as_ref() == FAIL_BRANCH,
                Status::Handled if src.role == NodeRole::Branch => {
                    default_branch(&src.policy.on_error).as_deref() == Some(edge.handle.as_ref())
                }
                Status::Handled => edge.handle.as_ref() != FAIL_BRANCH,
                Status::Succeeded if src.role == NodeRole::Branch => {
                    self.branch.get(edge.source).and_then(Option::as_deref) == Some(edge.handle.as_ref())
                }
                _ if src.role == NodeRole::Branch => false,
                // Unfinished fail-branch node: which side it takes is not known yet.
                _ if fail_branch => false,
                _ => edge.handle.as_ref() != FAIL_BRANCH,
            }
        })
    }

    /// Render as much as possible. With `flush`, pending variables are treated as resolved.
    fn drain(&mut self, flush: bool, out: &mut Vec<ResponseChunk>) {
        loop {
            if self.active.is_none() {
                match self.queue.pop_front() {
                    Some(response) => self.active = Some(Active { response, part: 0, offset: 0 }),
                    None => return,
                }
            }
            let Some(active) = self.active.as_mut() else { return };
            let Some(response) = self.responses.get(active.response) else {
                self.active = None;
                continue;
            };
            // A response node that ended up skipped renders nothing.
            if self.status.get(response.node).copied() == Some(Status::Skipped) {
                self.finish_active(None, out);
                continue;
            }
            // Parts are borrowed, not cloned: this runs for every streamed chunk.
            match response.parts.get(active.part) {
                None => {
                    let id = Arc::clone(&response.id);
                    self.finish_active(Some(&id), out);
                }
                Some(Part::Text(text)) => {
                    let piece = Piece { response: active.response, id: &response.id };
                    emit(&mut self.emitted, &mut self.segments, out, &piece, text.clone());
                    active.part += 1;
                    active.offset = 0;
                }
                Some(Part::Var(sel)) => {
                    let sources = Sources {
                        graph: &self.graph,
                        status: &self.status,
                        deferred: &self.deferred,
                        streams: &self.streams,
                        pool: &self.pool,
                    };
                    let step = resolve_var(&sources, sel, active.offset, flush);
                    if !step.text.is_empty() {
                        let piece = Piece { response: active.response, id: &response.id };
                        emit(&mut self.emitted, &mut self.segments, out, &piece, step.text);
                    }
                    active.offset = step.offset;
                    if step.complete {
                        active.part += 1;
                        active.offset = 0;
                    } else {
                        return;
                    }
                }
            }
        }
    }

    fn finish_active(&mut self, id: Option<&Arc<str>>, out: &mut Vec<ResponseChunk>) {
        if let Some(active) = self.active.take() {
            if let Some(r) = self.responses.get_mut(active.response) {
                r.done = true;
            }
            if let Some(id) = id {
                out.push(ResponseChunk::done(id));
            }
        }
    }
}

struct Step {
    text: String,
    offset: usize,
    complete: bool,
}

/// Read-only view of the state a template variable is resolved from.
struct Sources<'a> {
    graph: &'a Graph,
    status: &'a [Status],
    deferred: &'a [bool],
    streams: &'a Streams,
    pool: &'a VariablePool,
}

fn resolve_var(sources: &Sources<'_>, sel: &Selector, offset: usize, flush: bool) -> Step {
    let Sources { graph, status, deferred, streams, pool } = *sources;
    let node_ix = graph.index_of(sel.namespace()).filter(|&ix| graph.node(ix).is_some_and(|n| n.parent.is_none()));
    let Some(ix) = node_ix else {
        // Not produced by a root graph node: host-provided value (or nothing).
        return Step { text: pool.get(sel).map(|v| v.to_text()).unwrap_or_default(), offset: 0, complete: true };
    };
    let node_status = status.get(ix).copied().unwrap_or(Status::Unknown);

    let is_deferred = deferred.get(ix).copied().unwrap_or(false);
    let buffered = if sel.path().is_empty() {
        stream_buf(streams, sel.namespace(), sel.name()).filter(|b| !b.text.is_empty())
    } else {
        None
    };

    if is_deferred {
        // Emitted only once the node has finished, from its final attempt.
        let text = match node_status {
            Status::Succeeded => match buffered {
                Some(buf) => buf.text.clone(),
                None => pool.get(sel).map(|v| v.to_text()).unwrap_or_default(),
            },
            // Default-value outputs (or nothing on fail-branch, whose outputs are only error fields).
            Status::Handled => pool.get(sel).map(|v| v.to_text()).unwrap_or_default(),
            _ => String::new(),
        };
        return Step { text, offset: 0, complete: node_status.finished() || flush };
    }

    // Live streaming: emit whatever was buffered beyond `offset`.
    let mut text = String::new();
    let mut new_offset = offset;
    let streamed = buffered.is_some();
    if let Some(buf) = buffered
        && let Some(rest) = buf.text.get(offset..)
    {
        text.push_str(rest);
        new_offset = buf.text.len();
    }

    let complete = match node_status {
        Status::Skipped | Status::Failed => true,
        Status::Succeeded | Status::Handled => {
            if !streamed && let Some(v) = pool.get(sel) {
                text.push_str(&v.to_text());
            }
            true
        }
        Status::Unknown | Status::Started => {
            if flush
                && !streamed
                && let Some(v) = pool.get(sel)
            {
                text.push_str(&v.to_text());
            }
            flush
        }
    };
    Step { text, offset: new_offset, complete }
}

/// Handle named by a `branch` entry of a `default-value` strategy.
fn default_branch(strategy: &ErrorStrategy) -> Option<String> {
    match strategy {
        ErrorStrategy::DefaultValue { values } => values.get("branch").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// The response a piece of text is emitted for.
struct Piece<'a> {
    /// Its index in `ResponseAssembler::responses`.
    response: usize,
    id: &'a Arc<str>,
}

fn emit(
    emitted: &mut String,
    segments: &mut Vec<(usize, usize)>,
    out: &mut Vec<ResponseChunk>,
    piece: &Piece<'_>,
    text: String,
) {
    if text.is_empty() {
        return;
    }
    emitted.push_str(&text);
    match segments.last_mut() {
        Some((response, len)) if *response == piece.response => *len += text.len(),
        _ => segments.push((piece.response, text.len())),
    }
    out.push(ResponseChunk::text(piece.id, text));
}

fn end_parts(config: &serde_json::Map<String, serde_json::Value>) -> Vec<Part> {
    let outputs: Vec<EndOutput> = config
        .get("outputs")
        .cloned()
        .and_then(|v| serde_json::from_value::<Vec<serde_json::Value>>(v).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let mut parts = Vec::with_capacity(outputs.len() * 2);
    for (i, o) in outputs.into_iter().enumerate() {
        if i > 0 {
            parts.push(Part::Text("\n".to_owned()));
        }
        parts.push(Part::Var(o.value_selector));
    }
    parts
}
