//! The node contract.
//!
//! A node is an immutable, pre-validated object built once from its
//! [`NodeDoc`] by a factory in the [`Registry`]. At run time the engine calls
//! [`Node::run`] with an owned [`NodeContext`]; the node reads inputs from the
//! context's pool snapshot and returns a [`NodeOutcome`]. Nodes never mutate
//! shared state directly: outputs and variable writes are returned and
//! committed by the engine.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::future::BoxFuture;
use indexmap::IndexMap;

use crate::dsl::NodeDoc;
use crate::event::{Event, FrameId, PauseReason, Usage};
use crate::pool::{Selector, VariablePool};
use crate::redact::SecretSet;
use crate::snapshot::FrameSnapshot;
use crate::value::Value;
use crate::variables::{VarName, Variables};

/// How the scheduler treats a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeRole {
    /// Entry point of the root graph.
    Root,
    /// Entry point of a container body (iteration-start / loop-start).
    ScopeStart,
    /// Regular node: on success every outgoing edge is taken.
    Executable,
    /// Selects exactly one outgoing handle via [`NodeOutput::branch`].
    Branch,
    /// Produces user-facing output (answer / end). Outputs are merged into the frame outputs.
    Response,
    /// Owns a child scope executed through [`NodeContext::subgraph`].
    Container,
}

/// Error produced by a node.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct NodeError {
    /// Machine-readable category, e.g. `timeout`, `http`, `invalid_input`, `model`.
    pub kind: String,
    pub message: String,
    /// Whether a retry may succeed. Non-retryable errors skip the retry policy.
    pub retryable: bool,
    /// Minimum delay before a retry, as requested by the remote side (e.g. an
    /// HTTP `Retry-After` header or a provider rate limit). The engine waits for
    /// the larger of this and the node's retry policy delay.
    pub retry_after_ms: Option<u64>,
}

impl NodeError {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self { kind: kind.into(), message: message.into(), retryable: true, retry_after_ms: None }
    }

    /// Error that retrying cannot fix (bad configuration, invalid input).
    pub fn fatal(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self { kind: kind.into(), message: message.into(), retryable: false, retry_after_ms: None }
    }

    /// Attach a server-requested retry delay.
    pub fn with_retry_after(mut self, retry_after_ms: Option<u64>) -> Self {
        self.retry_after_ms = retry_after_ms;
        self
    }

    /// Replace the message, keeping kind, retryability and retry delay.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::fatal("invalid_input", message)
    }

    pub fn timeout(after: Duration) -> Self {
        Self::new("timeout", format!("node timed out after {} ms", after.as_millis()))
    }
}

/// Error raised when a node's configuration is invalid (at graph build time).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("node `{node_id}` ({kind}): {}{message}", field_prefix(.field))]
pub struct ConfigError {
    pub node_id: String,
    pub kind: String,
    pub message: String,
    /// Config field the error is about, as a path below the node such as
    /// `variables[0].value_selector`, when known.
    pub field: Option<String>,
}

fn field_prefix(field: &Option<String>) -> String {
    field.as_ref().map(|f| format!("{f}: ")).unwrap_or_default()
}

impl ConfigError {
    pub fn new(doc: &NodeDoc, message: impl Into<String>) -> Self {
        Self { node_id: doc.id.clone(), kind: doc.kind.clone(), message: message.into(), field: None }
    }

    /// Attach the config field the error is about.
    #[must_use]
    pub fn at_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }
}

/// Successful node result.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeOutput {
    /// Outputs stored under the node's namespace (`<node_id>.<key>`). The
    /// engine stores this map as the namespace and emits it in
    /// [`Event::NodeSucceeded`] without copying it.
    pub outputs: Variables,
    /// Selected handle for branch nodes.
    pub branch: Option<String>,
    /// Writes to other namespaces (conversation variables, loop variables, ...), applied in order.
    pub writes: Vec<(Selector, Value)>,
    pub usage: Option<Usage>,
    /// Observability data (resolved inputs, request details...). Not stored in the pool.
    pub metadata: IndexMap<String, Value>,
}

impl NodeOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an output. A string-literal name costs no allocation.
    pub fn with(mut self, key: impl Into<VarName>, value: impl Into<Value>) -> Self {
        self.outputs.insert(key.into(), value.into());
        self
    }

    pub fn branch(mut self, handle: impl Into<String>) -> Self {
        self.branch = Some(handle.into());
        self
    }

    pub fn write(mut self, selector: Selector, value: Value) -> Self {
        self.writes.push((selector, value));
        self
    }

    pub fn usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn meta(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// A request from a node to pause the run (e.g. waiting for a human).
#[derive(Clone, Debug, PartialEq)]
pub struct PauseRequest {
    /// Machine-readable kind, e.g. `human_input`.
    pub kind: String,
    /// Details shown to the host (form schema, prompt...).
    pub data: Value,
    /// State handed back to the node in [`Resume::state`].
    pub state: serde_json::Value,
}

/// What a node run produced.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeOutcome {
    Completed(NodeOutput),
    Paused(PauseRequest),
}

impl From<NodeOutput> for NodeOutcome {
    fn from(o: NodeOutput) -> Self {
        NodeOutcome::Completed(o)
    }
}

/// Data given to a node that previously paused.
#[derive(Clone, Debug, PartialEq)]
pub struct Resume {
    /// The node's own saved state ([`PauseRequest::state`]).
    pub state: serde_json::Value,
    /// Host-supplied input for this node (e.g. a submitted human-input form), if any.
    pub input: Option<Value>,
}

/// Result type of [`Node::run`].
pub type NodeResult = Result<NodeOutcome, NodeError>;

/// An executable node.
pub trait Node: Send + Sync + fmt::Debug {
    /// Scheduling role. Must be constant for a given instance.
    fn role(&self) -> NodeRole {
        NodeRole::Executable
    }

    /// Execute the node. Dropping the returned future cancels the node.
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult>;

    /// Selectors this node reads. [`crate::graph::Graph::build`] rejects selectors
    /// whose namespace is neither a node id nor `sys` / `env` / `conversation`
    /// (catching typos at load time) and warns about references into other
    /// container bodies. Return every selector the node may read, including
    /// placeholders inside templates.
    fn inputs(&self) -> Vec<Selector> {
        Vec::new()
    }

    /// Every handle a [`NodeRole::Branch`] node may select, when known from its
    /// configuration (if-else case ids, classifier class ids...). Graph building
    /// rejects edges with other handles. `None` (the default) falls back to the
    /// kind's catalog descriptor: `Fixed` handles are checked, `Dynamic` ones are
    /// not, and non-branch nodes have the single handle `source`.
    fn branch_handles(&self) -> Option<Vec<String>> {
        None
    }
}

/// Append `new` writes to `acc`, keeping only the latest write per exact selector.
///
/// An earlier write to the same selector is removed and the new one is appended
/// at the end, so replaying `acc` in order still yields the same final state
/// while its length stays bounded by the number of distinct selectors (no
/// quadratic growth across loop rounds or iteration items).
pub fn merge_writes(acc: &mut Vec<(Selector, Value)>, new: impl IntoIterator<Item = (Selector, Value)>) {
    for (selector, value) in new {
        if let Some(pos) = acc.iter().position(|(s, _)| *s == selector) {
            acc.remove(pos);
        }
        acc.push((selector, value));
    }
}

/// Request to execute a container's body once.
#[derive(Debug)]
pub struct SubgraphRequest {
    /// Iteration index / loop round. Used for the child frame id.
    pub index: usize,
    /// Variables visible to the body. Usually a cheap clone of the context pool plus
    /// the container's per-round values.
    pub pool: VariablePool,
    /// Resume a previously paused body run.
    pub resume: Option<FrameSnapshot>,
}

/// Outcome of a container body run.
///
/// Produced once per body run and moved straight into the container node, so
/// the size difference between variants does not matter.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SubgraphResult {
    Completed {
        /// Final pool of the body run.
        pool: VariablePool,
        /// Response outputs accumulated in the body (answer text...).
        outputs: IndexMap<String, Value>,
        /// Writes to namespaces outside the body (container variables, conversation...),
        /// in commit order. The container should forward them via [`NodeOutput::writes`].
        writes: Vec<(Selector, Value)>,
        usage: Usage,
        /// Whether a `loop-end` (break) node completed in this body run.
        broke: bool,
    },
    Failed {
        error: NodeError,
        usage: Usage,
    },
    Paused {
        snapshot: FrameSnapshot,
        reasons: Vec<PauseReason>,
    },
}

/// Executes a container's child scope. Provided to container nodes by the engine.
pub trait Subgraph: Send + Sync + fmt::Debug {
    fn run(&self, request: SubgraphRequest) -> BoxFuture<'_, SubgraphResult>;
}

/// Time source used by the engine and nodes (retries, timeouts, elapsed time).
pub trait Clock: Send + Sync + fmt::Debug {
    /// Milliseconds since an arbitrary fixed origin.
    fn now_ms(&self) -> u64;
    /// Wait for `duration`.
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
    /// Whether [`Clock::sleep`] actually waits. Timeouts are only enforced when true.
    fn supports_timers(&self) -> bool {
        true
    }
}

/// A clock without timers: sleeps complete immediately and timeouts are not enforced.
/// Useful for tests and for executors without a timer facility.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTimerClock;

impl Clock for NoTimerClock {
    fn now_ms(&self) -> u64 {
        system_now_ms()
    }
    fn sleep(&self, _duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(futures::future::ready(()))
    }
    fn supports_timers(&self) -> bool {
        false
    }
}

/// Wall-clock milliseconds since the Unix epoch.
///
/// `wasm32-unknown-unknown` has no system clock (`SystemTime::now` panics there),
/// so that target returns a monotonically increasing counter instead. Hosts on
/// that target should supply their own [`Clock`] for meaningful timestamps.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn system_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn system_now_ms() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Tokio-backed clock.
#[cfg(feature = "tokio")]
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioClock;

#[cfg(feature = "tokio")]
impl Clock for TokioClock {
    fn now_ms(&self) -> u64 {
        NoTimerClock.now_ms()
    }
    /// Outside a Tokio runtime this completes immediately instead of panicking
    /// (and [`Clock::supports_timers`] reports `false`, so timeouts are not
    /// enforced). Inside a runtime, the runtime must have its time driver enabled
    /// (`enable_time()` / `enable_all()`).
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        if tokio::runtime::Handle::try_current().is_ok() {
            Box::pin(tokio::time::sleep(duration))
        } else {
            Box::pin(futures::future::ready(()))
        }
    }

    /// `true` only when called from inside a Tokio runtime.
    fn supports_timers(&self) -> bool {
        tokio::runtime::Handle::try_current().is_ok()
    }
}

/// The default clock for this build: [`TokioClock`] with the `tokio` feature, else [`NoTimerClock`].
pub fn default_clock() -> Arc<dyn Clock> {
    #[cfg(feature = "tokio")]
    {
        Arc::new(TokioClock)
    }
    #[cfg(not(feature = "tokio"))]
    {
        Arc::new(NoTimerClock)
    }
}

/// Type-indexed registry of host services (LLM providers, HTTP clients, tool
/// runtimes, file resolvers...). Nodes look services up at run time, so node
/// factories stay pure and graphs can be built without any integration.
#[derive(Default, Clone)]
pub struct Services {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Services({} entries)", self.map.len())
    }
}

impl Services {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a service. `T` is usually an `Arc<dyn Trait>` or a concrete struct.
    pub fn insert<T: Any + Send + Sync>(&mut self, service: T) -> &mut Self {
        self.map.insert(TypeId::of::<T>(), Arc::new(service));
        self
    }

    /// Builder-style [`Services::insert`].
    pub fn with<T: Any + Send + Sync>(mut self, service: T) -> Self {
        self.insert(service);
        self
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>()).and_then(|s| s.downcast_ref::<T>())
    }
}

/// Marker service: the host records message content (rendered prompts, tool
/// results, retrieval queries), so nodes should also report it as progress.
///
/// Hosts register it only when content capture is on (for example
/// `OTEL_INSTRUMENTATION_GENAI_CAPTURE_CONTENT=SPAN_ONLY`). Without it, nodes
/// skip building that progress entirely, so capture costs nothing when off.
/// Content progress carries the same masked values as node outputs (secret
/// values serialize as `******`).
///
/// ```
/// let services = graphrt_core::Services::new().with(graphrt_core::node::CaptureContent);
/// let ctx = graphrt_core::NodeContext::for_test("n", graphrt_core::VariablePool::new())
///     .with_services(std::sync::Arc::new(services));
/// assert!(ctx.captures_content());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureContent;

/// Callback receiving events from nodes.
pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// Everything a node may use while running. Cheap to clone.
#[derive(Clone)]
pub struct NodeContext {
    pub node_id: Arc<str>,
    pub exec_id: Arc<str>,
    pub frame: FrameId,
    /// 1-based attempt number (increments on retries).
    pub attempt: u32,
    /// Snapshot of the frame's variables at the time the node started.
    pub pool: VariablePool,
    pub services: Arc<Services>,
    pub clock: Arc<dyn Clock>,
    /// Present when the node is resumed after pausing.
    pub resume: Option<Resume>,
    /// Present for container nodes.
    pub subgraph: Option<Arc<dyn Subgraph>>,
    /// Absolute deadline of the current attempt in [`Clock::now_ms`] time, when
    /// the node has an effective timeout. See [`NodeContext::remaining`].
    pub deadline_ms: Option<u64>,
    sink: EventSink,
    /// The run's secret registry, when the engine provides one.
    secret_values: Option<Arc<SecretSet>>,
}

impl fmt::Debug for NodeContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeContext")
            .field("node_id", &self.node_id)
            .field("exec_id", &self.exec_id)
            .field("frame", &self.frame)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

impl NodeContext {
    /// The idempotency key of this node execution's side effects:
    /// `<run_id>:<exec_id>` (just `exec_id` when the run has no `sys.run_id`).
    ///
    /// Execution is at-least-once per node: a node can run again after a retry,
    /// or after its run was preempted and resumed (the snapshot keeps the
    /// `exec_id`). The key is the same in all those cases and different for
    /// every other execution (another run, another loop iteration), so a
    /// downstream service that dedupes on it applies the effect once. Hosts send
    /// it as `Idempotency-Key` (the `http-request` node) or hand it to plugins.
    ///
    /// @example
    /// // "run-1a2b:1a20151eb86-997f9542-0"
    /// let key = ctx.idempotency_key();
    pub fn idempotency_key(&self) -> String {
        match self.pool.get(&Selector::of(crate::pool::SYS, "run_id")) {
            Some(Value::String(run_id)) if !run_id.is_empty() => format!("{run_id}:{}", self.exec_id),
            _ => self.exec_id.to_string(),
        }
    }

    /// Construct a context. Used by the engine and by node unit tests.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        node_id: impl Into<Arc<str>>,
        exec_id: impl Into<Arc<str>>,
        frame: FrameId,
        attempt: u32,
        pool: VariablePool,
        services: Arc<Services>,
        clock: Arc<dyn Clock>,
        sink: EventSink,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            exec_id: exec_id.into(),
            frame,
            attempt,
            pool,
            services,
            clock,
            resume: None,
            subgraph: None,
            deadline_ms: None,
            sink,
            secret_values: None,
        }
    }

    /// A context with an empty pool and a no-op sink, for unit tests.
    pub fn for_test(node_id: &str, pool: VariablePool) -> Self {
        Self::new(
            node_id,
            format!("{node_id}-exec"),
            Arc::from(""),
            1,
            pool,
            Arc::new(Services::new()),
            Arc::new(NoTimerClock),
            Arc::new(|_| {}),
        )
    }

    pub fn with_services(mut self, services: Arc<Services>) -> Self {
        self.services = services;
        self
    }

    pub fn with_sink(mut self, sink: EventSink) -> Self {
        self.sink = sink;
        self
    }

    pub fn with_resume(mut self, resume: Option<Resume>) -> Self {
        self.resume = resume;
        self
    }

    pub fn with_subgraph(mut self, subgraph: Option<Arc<dyn Subgraph>>) -> Self {
        self.subgraph = subgraph;
        self
    }

    pub fn with_deadline(mut self, deadline_ms: Option<u64>) -> Self {
        self.deadline_ms = deadline_ms;
        self
    }

    /// Attach the run's secret registry (see [`NodeContext::observe_secret`]).
    pub fn with_secret_values(mut self, secret_values: Arc<SecretSet>) -> Self {
        self.secret_values = Some(secret_values);
        self
    }

    /// Register plaintext this node handles as a secret, so the engine scrubs it
    /// from every later node error message of the run. A no-op outside the engine.
    pub fn observe_secret(&self, plaintext: &str) {
        if let Some(secret_values) = &self.secret_values {
            secret_values.observe_plaintext(plaintext);
        }
    }

    /// The run's secret registry, for a node that keeps text beyond the run
    /// (a saved note, for example) and must mask the secrets in it first. It
    /// only masks: the values themselves cannot be read from it. `None`
    /// outside the engine.
    pub fn secret_values(&self) -> Option<Arc<SecretSet>> {
        self.secret_values.clone()
    }

    /// Time left before the current attempt times out, if it has a deadline.
    /// Nodes that block synchronously (e.g. an embedded interpreter) should
    /// cap their own work by this.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline_ms.map(|deadline| Duration::from_millis(deadline.saturating_sub(self.clock.now_ms())))
    }

    /// Look up a host service.
    pub fn service<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.services.get::<T>()
    }

    /// Look up a host service or fail with a descriptive error.
    pub fn require_service<T: Any + Send + Sync>(&self, what: &str) -> Result<&T, NodeError> {
        self.services.get::<T>().ok_or_else(|| NodeError::fatal("missing_service", format!("no {what} configured")))
    }

    /// Resolve a selector against the pool snapshot.
    pub fn get(&self, selector: &Selector) -> Option<Value> {
        self.pool.get(selector)
    }

    /// Stream an incremental chunk for output `field`.
    ///
    /// Allocates the field name for every chunk; nodes that stream many chunks
    /// should keep the name in an `Arc<str>` and use [`NodeContext::chunk_shared`].
    pub fn chunk(&self, field: &str, chunk: impl Into<String>) {
        self.chunk_shared(&Arc::from(field), chunk);
    }

    /// Stream an incremental chunk for output `field`, sharing the field name
    /// (no allocation besides the chunk text itself).
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # let ctx = graphrt_core::NodeContext::for_test("n", graphrt_core::VariablePool::new());
    /// let field: Arc<str> = Arc::from("text");
    /// for token in ["Hel", "lo"] {
    ///     ctx.chunk_shared(&field, token);
    /// }
    /// ```
    pub fn chunk_shared(&self, field: &Arc<str>, chunk: impl Into<String>) {
        (self.sink)(Event::NodeChunk {
            frame: self.frame.clone(),
            node_id: Arc::clone(&self.node_id),
            exec_id: Arc::clone(&self.exec_id),
            field: Arc::clone(field),
            chunk: chunk.into(),
        });
    }

    /// Report structured progress.
    pub fn progress(&self, name: &str, data: Value) {
        (self.sink)(Event::NodeProgress {
            frame: self.frame.clone(),
            node_id: Arc::clone(&self.node_id),
            exec_id: Arc::clone(&self.exec_id),
            name: name.to_owned(),
            data,
        });
    }

    /// Whether the host asked for content progress (see [`CaptureContent`]).
    /// Check it before building content-bearing progress data.
    pub fn captures_content(&self) -> bool {
        self.services.get::<CaptureContent>().is_some()
    }

    /// Raw access to the event sink (used by container implementations).
    pub fn sink(&self) -> &EventSink {
        &self.sink
    }
}

/// Builds a node from its document.
pub type NodeFactory = Arc<dyn Fn(&NodeDoc) -> Result<Arc<dyn Node>, ConfigError> + Send + Sync>;

/// Builds a node kind's catalog descriptor.
pub type DescriptorFn = fn() -> crate::catalog::NodeDescriptor;

/// A catalog descriptor, either given up front or built on first request.
///
/// Descriptors carry JSON Schemas and are only read by catalog consumers
/// (`graphrt nodes`, `GET /v1/nodes`, builders), never while compiling or
/// running a graph, so built-in kinds build theirs lazily: registering every
/// built-in eagerly cost about half a millisecond of every process start.
#[derive(Clone)]
enum DescriptorSlot {
    Ready(Box<crate::catalog::NodeDescriptor>),
    Lazy(Arc<LazyDescriptor>),
}

struct LazyDescriptor {
    build: DescriptorFn,
    built: OnceLock<crate::catalog::NodeDescriptor>,
}

/// Maps node kinds (`llm`, `if-else`, ...) to factories.
#[derive(Clone, Default)]
pub struct Registry {
    factories: HashMap<String, NodeFactory>,
    descriptors: HashMap<String, DescriptorSlot>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut kinds: Vec<&String> = self.factories.keys().collect();
        kinds.sort_unstable();
        f.debug_struct("Registry").field("kinds", &kinds).finish()
    }
}

impl Registry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with every built-in core node.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        crate::nodes::register_builtins(&mut r);
        r
    }

    /// Register (or replace) a factory for `kind`.
    pub fn register<F>(&mut self, kind: &str, factory: F) -> &mut Self
    where
        F: Fn(&NodeDoc) -> Result<Arc<dyn Node>, ConfigError> + Send + Sync + 'static,
    {
        self.factories.insert(kind.to_owned(), Arc::new(factory));
        self
    }

    /// Register (or replace) a factory together with its catalog descriptor.
    /// The descriptor's `kind` is the registered node type.
    pub fn register_described<F>(&mut self, descriptor: crate::catalog::NodeDescriptor, factory: F) -> &mut Self
    where
        F: Fn(&NodeDoc) -> Result<Arc<dyn Node>, ConfigError> + Send + Sync + 'static,
    {
        let kind = descriptor.kind.clone();
        self.factories.insert(kind.clone(), Arc::new(factory));
        self.descriptors.insert(kind, DescriptorSlot::Ready(Box::new(descriptor)));
        self
    }

    /// Register a factory whose catalog descriptor is built by `descriptor` the
    /// first time it is requested. Use this for kinds registered on every start
    /// (built-ins), so processes that never list the catalog skip building it.
    /// The built descriptor's `kind` must be `kind`.
    pub fn register_lazily_described<F>(&mut self, kind: &str, descriptor: DescriptorFn, factory: F) -> &mut Self
    where
        F: Fn(&NodeDoc) -> Result<Arc<dyn Node>, ConfigError> + Send + Sync + 'static,
    {
        self.factories.insert(kind.to_owned(), Arc::new(factory));
        let slot = LazyDescriptor { build: descriptor, built: OnceLock::new() };
        self.descriptors.insert(kind.to_owned(), DescriptorSlot::Lazy(Arc::new(slot)));
        self
    }

    /// Attach or replace the descriptor of an already-registered kind.
    pub fn describe(&mut self, descriptor: crate::catalog::NodeDescriptor) -> &mut Self {
        if self.factories.contains_key(&descriptor.kind) {
            self.descriptors.insert(descriptor.kind.clone(), DescriptorSlot::Ready(Box::new(descriptor)));
        }
        self
    }

    /// Descriptor of a kind. Kinds registered without one get a bare descriptor.
    pub fn descriptor(&self, kind: &str) -> Option<crate::catalog::NodeDescriptor> {
        if !self.factories.contains_key(kind) {
            return None;
        }
        let descriptor = match self.descriptors.get(kind) {
            None => crate::catalog::NodeDescriptor::bare(kind),
            Some(DescriptorSlot::Ready(descriptor)) => (**descriptor).clone(),
            Some(DescriptorSlot::Lazy(slot)) => slot.built.get_or_init(slot.build).clone(),
        };
        Some(descriptor)
    }

    /// Borrowed descriptor of a kind registered with one (`None` for unknown
    /// kinds and kinds registered without a descriptor). Builds a lazy
    /// descriptor on first use.
    pub fn descriptor_ref(&self, kind: &str) -> Option<&crate::catalog::NodeDescriptor> {
        match self.descriptors.get(kind)? {
            DescriptorSlot::Ready(descriptor) => Some(descriptor),
            DescriptorSlot::Lazy(slot) => Some(slot.built.get_or_init(slot.build)),
        }
    }

    /// Catalog of every registered kind, sorted by kind.
    pub fn descriptors(&self) -> Vec<crate::catalog::NodeDescriptor> {
        let mut kinds: Vec<&String> = self.factories.keys().collect();
        kinds.sort_unstable();
        kinds.into_iter().filter_map(|k| self.descriptor(k)).collect()
    }

    /// Register an alias resolving to an already-registered kind.
    pub fn alias(&mut self, alias: &str, kind: &str) -> &mut Self {
        if let Some(f) = self.factories.get(kind).cloned() {
            self.factories.insert(alias.to_owned(), f);
        }
        self
    }

    pub fn contains(&self, kind: &str) -> bool {
        self.factories.contains_key(kind)
    }

    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.factories.keys().map(String::as_str)
    }

    /// Build a node from its document.
    pub fn build(&self, doc: &NodeDoc) -> Result<Arc<dyn Node>, ConfigError> {
        let factory = self.factories.get(&doc.kind).ok_or_else(|| {
            let suggestion = crate::validate::suggest(&doc.kind, self.kinds());
            ConfigError::new(
                doc,
                format!("unknown node type `{}`{}", doc.kind, crate::validate::did_you_mean(&suggestion)),
            )
        })?;
        factory(doc)
    }
}

/// Helper for factories: parse the node config into `T`, mapping errors to [`ConfigError`].
pub fn parse_config<T: serde::de::DeserializeOwned>(doc: &NodeDoc) -> Result<T, ConfigError> {
    crate::dsl::parse_config_at(&doc.config).map_err(|(path, e)| {
        let error = ConfigError::new(doc, e.to_string());
        if path == "." { error } else { error.at_field(path) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_keys_name_the_run_and_the_node_execution() {
        let mut pool = VariablePool::new();
        pool.insert(crate::pool::SYS, "run_id", Value::from("run-7"));
        assert_eq!(NodeContext::for_test("h", pool).idempotency_key(), "run-7:h-exec");
        // A retry keeps the execution id, so the key.
        let mut pool = VariablePool::new();
        pool.insert(crate::pool::SYS, "run_id", Value::from("run-7"));
        let mut retry = NodeContext::for_test("h", pool);
        retry.attempt = 3;
        assert_eq!(retry.idempotency_key(), "run-7:h-exec");
        // Without a run id (a node run outside a run), the execution id alone.
        assert_eq!(NodeContext::for_test("h", VariablePool::new()).idempotency_key(), "h-exec");
    }
}
