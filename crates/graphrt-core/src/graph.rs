//! Compiled, validated, immutable graph.
//!
//! A [`Graph`] is built once from a [`Workflow`] document and shared (via
//! `Arc`) by every run and every container frame. Runtime state (node and
//! edge status) lives in the engine's per-frame state, never in the graph, so
//! a graph can execute any number of runs concurrently.
//!
//! Each node belongs to exactly one *scope*: the root scope, or the body of a
//! container node (iteration / loop). Edges never cross scopes, and every
//! scope must be acyclic and have exactly one entry node.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::dsl::{NodeDoc, NodePolicy, Workflow};
use crate::node::{ConfigError, Node, NodeRole, Registry};

/// Default edge handle for non-branch nodes.
pub const SOURCE_HANDLE: &str = "source";
/// Handle taken when a node with the `fail-branch` strategy fails.
pub const FAIL_BRANCH: &str = "fail-branch";

/// Index of a node in [`Graph::nodes`].
pub type NodeIx = usize;
/// Index of an edge in [`Graph::edges`].
pub type EdgeIx = usize;

/// A compiled node.
pub struct GraphNode {
    pub id: Arc<str>,
    /// Shared with every `NodeStarted` event, so starting a node copies no strings.
    pub kind: Arc<str>,
    pub title: Arc<str>,
    /// Owning container, `None` for the root scope.
    pub parent: Option<NodeIx>,
    pub policy: NodePolicy,
    pub role: NodeRole,
    pub node: Arc<dyn Node>,
    /// The source document (kept for introspection and tooling).
    pub doc: NodeDoc,
}

impl fmt::Debug for GraphNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphNode").field("id", &self.id).field("kind", &self.kind).field("role", &self.role).finish()
    }
}

/// A compiled edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEdge {
    pub id: Arc<str>,
    pub source: NodeIx,
    pub target: NodeIx,
    pub handle: Arc<str>,
}

/// One scope: the root graph or a container body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// Container node owning this scope, `None` for the root.
    pub owner: Option<NodeIx>,
    /// Entry node.
    pub entry: NodeIx,
    /// Nodes in this scope, in topological order.
    pub nodes: Vec<NodeIx>,
}

/// Graph construction / validation error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GraphError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("duplicate node id `{0}`")]
    DuplicateNode(String),
    #[error("duplicate edge id `{0}`")]
    DuplicateEdge(String),
    #[error("edge `{edge}` references unknown node `{node}`{}", did_you_mean(.suggestion))]
    MissingNode { edge: String, node: String, suggestion: Option<String> },
    #[error("node `{node}` references unknown parent `{parent}`{}", did_you_mean(.suggestion))]
    MissingParent { node: String, parent: String, suggestion: Option<String> },
    #[error("node `{0}` has a parent that is not a container node")]
    ParentNotContainer(String),
    #[error("container nesting cycle at node `{0}`")]
    ParentCycle(String),
    #[error("edge `{edge}` crosses scopes (`{source_node}` -> `{target}`)")]
    CrossScopeEdge { edge: String, source_node: String, target: String },
    #[error("cycle detected in scope `{scope}` involving node `{node}`")]
    Cycle { scope: String, node: String },
    #[error("scope `{scope}` has no entry node")]
    NoEntry { scope: String },
    #[error("scope `{scope}` has multiple entry nodes: {nodes:?}")]
    MultipleEntries { scope: String, nodes: Vec<String> },
    #[error("graph has no nodes")]
    Empty,
    #[error("workflow cannot be serialized: {0}")]
    Unserializable(String),
    #[error("invalid node id `{id}`: {reason}")]
    InvalidNodeId { id: String, reason: String },
    #[error(
        "node `{node}` references `{selector}`, but no node or variable namespace `{namespace}` exists{}",
        did_you_mean(.suggestion)
    )]
    UnknownReference { node: String, selector: String, namespace: String, suggestion: Option<String> },
    /// A config field the node kind's catalog schema does not allow, or a
    /// required one that is missing (strict nodes only).
    #[error("node `{node}`: {message}")]
    InvalidField { node: String, path: String, message: String },
    /// An edge whose handle the source node never selects.
    #[error("edge `{edge}`: {message}")]
    UnknownHandle { edge: String, message: String },
}

use crate::validate::did_you_mean;

/// Namespaces that are not node ids.
pub(crate) const RESERVED_NAMESPACES: &[&str] = &[crate::pool::SYS, crate::pool::ENV, crate::pool::CONVERSATION];

pub(crate) fn validate_node_id(id: &str) -> Result<(), GraphError> {
    let reason = if id.is_empty() {
        Some("must not be empty")
    } else if RESERVED_NAMESPACES.contains(&id) {
        Some("is a reserved variable namespace")
    } else if id.contains('.') {
        Some("must not contain `.` (it separates selector parts)")
    } else if id.chars().any(char::is_whitespace) {
        Some("must not contain whitespace")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(GraphError::InvalidNodeId { id: id.to_owned(), reason: reason.to_owned() }),
        None => Ok(()),
    }
}

/// An executable graph.
pub struct Graph {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    index: HashMap<Arc<str>, NodeIx>,
    outgoing: Vec<Vec<EdgeIx>>,
    incoming: Vec<Vec<EdgeIx>>,
    scopes: HashMap<Option<NodeIx>, Scope>,
    warnings: Vec<String>,
    /// Computed on first use: only snapshots, run records and traces need it, and
    /// hashing the canonical document is a large share of loading big workflows.
    fingerprint: std::sync::OnceLock<String>,
    workflow: Workflow,
}

impl fmt::Debug for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph")
            .field("nodes", &self.nodes.len())
            .field("edges", &self.edges.len())
            .field("fingerprint", &self.fingerprint.get())
            .finish()
    }
}

impl Graph {
    /// Build and validate a graph from a workflow document.
    pub fn build(workflow: Workflow, registry: &Registry) -> Result<Self, GraphError> {
        Self::build_inner(workflow, registry, None)
    }

    /// [`Graph::build`] for a document the validator already checked: `checked`
    /// holds the node built from each document node, in order, after its fields
    /// were checked against the catalog. Reusing them avoids constructing every
    /// node and walking every config schema twice, which made loading large
    /// workflows several times slower.
    pub(crate) fn build_checked(
        workflow: Workflow,
        registry: &Registry,
        checked: Vec<Arc<dyn Node>>,
    ) -> Result<Self, GraphError> {
        Self::build_inner(workflow, registry, Some(checked))
    }

    fn build_inner(
        workflow: Workflow,
        registry: &Registry,
        checked: Option<Vec<Arc<dyn Node>>>,
    ) -> Result<Self, GraphError> {
        let docs = &workflow.graph.nodes;
        // Only a complete set of pre-built nodes can stand in for building them here.
        let mut checked = checked.filter(|nodes| nodes.len() == docs.len()).map(Vec::into_iter);
        if docs.is_empty() {
            return Err(GraphError::Empty);
        }

        // Nodes.
        let mut index: HashMap<Arc<str>, NodeIx> = HashMap::with_capacity(docs.len());
        for (ix, doc) in docs.iter().enumerate() {
            validate_node_id(&doc.id)?;
            if index.insert(Arc::from(doc.id.as_str()), ix).is_some() {
                return Err(GraphError::DuplicateNode(doc.id.clone()));
            }
        }
        let mut nodes = Vec::with_capacity(docs.len());
        for doc in docs {
            let parent = match &doc.parent {
                Some(p) => Some(*index.get(p.as_str()).ok_or_else(|| GraphError::MissingParent {
                    node: doc.id.clone(),
                    parent: p.clone(),
                    suggestion: crate::validate::suggest(p, docs.iter().map(|d| d.id.as_str())),
                })?),
                None => None,
            };
            let node = match checked.as_mut().and_then(Iterator::next) {
                Some(node) => node,
                None => {
                    let node = registry.build(doc)?;
                    if doc.strict {
                        check_fields(doc, registry)?;
                    }
                    node
                }
            };
            nodes.push(GraphNode {
                id: Arc::from(doc.id.as_str()),
                kind: Arc::from(doc.kind.as_str()),
                title: Arc::from(doc.label()),
                parent,
                policy: doc.policy.clone(),
                role: node.role(),
                node,
                doc: doc.clone(),
            });
        }

        // Parents must be containers and nesting must be acyclic.
        for (ix, n) in nodes.iter().enumerate() {
            if let Some(p) = n.parent
                && nodes.get(p).map(|pn| pn.role) != Some(NodeRole::Container)
            {
                return Err(GraphError::ParentNotContainer(n.id.to_string()));
            }
            let mut seen = HashSet::new();
            let mut cur = Some(ix);
            while let Some(c) = cur {
                if !seen.insert(c) {
                    return Err(GraphError::ParentCycle(n.id.to_string()));
                }
                cur = nodes.get(c).and_then(|x| x.parent);
            }
        }

        // Edges.
        let mut edges = Vec::with_capacity(workflow.graph.edges.len());
        let mut dead_edges = Vec::new();
        let mut edge_ids = HashSet::new();
        let mut outgoing = vec![Vec::new(); nodes.len()];
        let mut incoming = vec![Vec::new(); nodes.len()];
        for (i, e) in workflow.graph.edges.iter().enumerate() {
            let id: Arc<str> = Arc::from(e.id.clone().unwrap_or_else(|| format!("{}-{}-{i}", e.source, e.target)));
            if !edge_ids.insert(id.clone()) {
                return Err(GraphError::DuplicateEdge(id.to_string()));
            }
            let lookup = |n: &str| {
                index.get(n).copied().ok_or_else(|| GraphError::MissingNode {
                    edge: id.to_string(),
                    node: n.to_owned(),
                    suggestion: crate::validate::suggest(n, docs.iter().map(|d| d.id.as_str())),
                })
            };
            let source = lookup(&e.source)?;
            let target = lookup(&e.target)?;
            let (sp, tp) = (nodes.get(source).and_then(|n| n.parent), nodes.get(target).and_then(|n| n.parent));
            if sp != tp {
                return Err(GraphError::CrossScopeEdge {
                    edge: id.to_string(),
                    source_node: e.source.clone(),
                    target: e.target.clone(),
                });
            }
            let handle = e.handle.as_deref().filter(|h| !h.is_empty()).unwrap_or(SOURCE_HANDLE);
            if let Some(n) = nodes.get(source) {
                let descriptor = registry.descriptor_ref(&n.doc.kind);
                if let Some(message) = crate::validate::dead_fail_branch(&n.doc, handle) {
                    dead_edges.push(message);
                }
                if let Some(allowed) = crate::validate::allowed_handles(n.node.as_ref(), descriptor)
                    && !allowed.iter().any(|h| h == handle)
                {
                    let mut message = crate::validate::handle_message(&n.doc, handle, &allowed);
                    message
                        .push_str(&did_you_mean(&crate::validate::suggest(handle, allowed.iter().map(String::as_str))));
                    return Err(GraphError::UnknownHandle { edge: id.to_string(), message });
                }
            }
            let ex = edges.len();
            edges.push(GraphEdge { id, source, target, handle: Arc::from(handle) });
            if let Some(o) = outgoing.get_mut(source) {
                o.push(ex);
            }
            if let Some(inc) = incoming.get_mut(target) {
                inc.push(ex);
            }
        }

        // Scopes.
        let mut members: HashMap<Option<NodeIx>, Vec<NodeIx>> = HashMap::new();
        for (ix, n) in nodes.iter().enumerate() {
            members.entry(n.parent).or_default().push(ix);
        }
        // Containers without children still get an (invalid) scope check below.
        for (ix, n) in nodes.iter().enumerate() {
            if n.role == NodeRole::Container {
                members.entry(Some(ix)).or_default();
            }
        }

        let mut warnings = dead_edges;
        let mut scopes = HashMap::new();
        for (owner, scope_nodes) in members {
            let scope_name =
                owner.and_then(|o| nodes.get(o)).map(|n| n.id.to_string()).unwrap_or_else(|| "root".into());
            let entry = find_entry(&nodes, &incoming, owner, &scope_nodes, &scope_name)?;
            let order = topo_order(&scope_nodes, &outgoing, &incoming, &edges, &nodes, &scope_name)?;
            // Reachability from the entry: unreachable nodes never run.
            let reachable = reachable_from(entry, &outgoing, &edges);
            for &ix in &scope_nodes {
                if !reachable.contains(&ix)
                    && let Some(n) = nodes.get(ix)
                {
                    warnings.push(format!("node `{}` is not reachable from the entry of scope `{scope_name}`", n.id));
                }
            }
            scopes.insert(owner, Scope { owner, entry, nodes: order });
        }

        validate_references(&nodes, &index, &mut warnings)?;

        let fingerprint = std::sync::OnceLock::new();
        Ok(Self { nodes, edges, index, outgoing, incoming, scopes, warnings, fingerprint, workflow })
    }

    /// Parse (JSON or YAML) and build in one step.
    pub fn from_text(text: &str, registry: &Registry) -> Result<Self, crate::Error> {
        let wf = Workflow::parse(text)?;
        Ok(Self::build(wf, registry)?)
    }

    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
    }

    pub fn node(&self, ix: NodeIx) -> Option<&GraphNode> {
        self.nodes.get(ix)
    }

    pub fn edge(&self, ix: EdgeIx) -> Option<&GraphEdge> {
        self.edges.get(ix)
    }

    /// Look up a node index by id.
    pub fn index_of(&self, id: &str) -> Option<NodeIx> {
        self.index.get(id).copied()
    }

    pub fn node_by_id(&self, id: &str) -> Option<&GraphNode> {
        self.index_of(id).and_then(|ix| self.nodes.get(ix))
    }

    pub fn outgoing(&self, ix: NodeIx) -> &[EdgeIx] {
        self.outgoing.get(ix).map(Vec::as_slice).unwrap_or_default()
    }

    pub fn incoming(&self, ix: NodeIx) -> &[EdgeIx] {
        self.incoming.get(ix).map(Vec::as_slice).unwrap_or_default()
    }

    /// The root scope.
    pub fn root_scope(&self) -> Option<&Scope> {
        self.scopes.get(&None)
    }

    /// The body scope of a container node.
    pub fn scope_of(&self, container: NodeIx) -> Option<&Scope> {
        self.scopes.get(&Some(container))
    }

    /// Whether `ix` is (transitively) inside container `ancestor`.
    pub fn is_descendant(&self, ix: NodeIx, ancestor: NodeIx) -> bool {
        let mut cur = self.nodes.get(ix).and_then(|n| n.parent);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.nodes.get(c).and_then(|n| n.parent);
        }
        false
    }

    /// Non-fatal findings (e.g. unreachable nodes).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Stable fingerprint of the source document, used to validate snapshots.
    ///
    /// Computed on first call and cached. A workflow document always serializes
    /// (string keys only); should that ever fail, the fingerprint is derived from
    /// the error text instead, which still never matches a real document's.
    pub fn fingerprint(&self) -> &str {
        self.fingerprint.get_or_init(|| {
            fingerprint(&self.workflow).unwrap_or_else(|error| fnv_hex(format!("unserializable: {error}").as_bytes()))
        })
    }

    /// The source document.
    pub fn workflow(&self) -> &Workflow {
        &self.workflow
    }
}

/// Check every selector a node declares in [`Node::inputs`]:
///
/// - its namespace must be a reserved namespace (`sys`, `env`, `conversation`)
///   or a node id — otherwise the build fails, so typos surface at load time
///   instead of silently resolving to null at run time;
/// - a referenced node should be visible: in the same scope or an enclosing
///   one (including the enclosing containers themselves), or inside the
///   referencing container's own body. Other references only produce a warning,
///   because they resolve to null at run time.
fn validate_references(
    nodes: &[GraphNode],
    index: &HashMap<Arc<str>, NodeIx>,
    warnings: &mut Vec<String>,
) -> Result<(), GraphError> {
    for (ix, n) in nodes.iter().enumerate() {
        for selector in n.node.inputs() {
            let namespace = selector.namespace();
            if RESERVED_NAMESPACES.contains(&namespace) {
                continue;
            }
            let Some(&target) = index.get(namespace) else {
                let candidates = nodes.iter().map(|n| &*n.id).chain(RESERVED_NAMESPACES.iter().copied());
                return Err(GraphError::UnknownReference {
                    node: n.id.to_string(),
                    selector: selector.to_string(),
                    namespace: namespace.to_owned(),
                    suggestion: crate::validate::suggest(namespace, candidates),
                });
            };
            if !is_visible(nodes, ix, target) {
                warnings.push(format!(
                    "node `{}` references `{selector}` from a different container body; it will resolve to null",
                    n.id
                ));
            }
        }
    }
    Ok(())
}

/// Reject config fields the kind's catalog schema does not allow (closed
/// schemas only) and missing required ones.
fn check_fields(doc: &NodeDoc, registry: &Registry) -> Result<(), GraphError> {
    let Some(descriptor) = registry.descriptor_ref(&doc.kind) else { return Ok(()) };
    let base = crate::source_map::DocPath::root();
    match crate::validate::config_diagnostics(doc, descriptor, &base).into_iter().find(|d| d.is_error()) {
        None => Ok(()),
        Some(d) => Err(GraphError::InvalidField {
            node: doc.id.clone(),
            path: d.path.to_string(),
            message: format!("{}{}", d.message, did_you_mean(&d.suggestion)),
        }),
    }
}

/// Whether node `from` can read the outputs of node `target`.
fn is_visible(nodes: &[GraphNode], from: NodeIx, target: NodeIx) -> bool {
    let parent_of = |ix: NodeIx| nodes.get(ix).and_then(|n| n.parent);
    let target_scope = parent_of(target);
    // Same or enclosing scope.
    let mut scope = parent_of(from);
    loop {
        if scope == target_scope {
            return true;
        }
        match scope {
            Some(s) => scope = parent_of(s),
            None => break,
        }
    }
    // Inside the referencing node's own body (a container reading its body outputs).
    let mut cur = target_scope;
    while let Some(c) = cur {
        if c == from {
            return true;
        }
        cur = parent_of(c);
    }
    false
}

fn find_entry(
    nodes: &[GraphNode],
    incoming: &[Vec<EdgeIx>],
    owner: Option<NodeIx>,
    scope_nodes: &[NodeIx],
    scope_name: &str,
) -> Result<NodeIx, GraphError> {
    let wanted = if owner.is_some() { NodeRole::ScopeStart } else { NodeRole::Root };
    let explicit: Vec<NodeIx> =
        scope_nodes.iter().copied().filter(|&ix| nodes.get(ix).map(|n| n.role) == Some(wanted)).collect();
    let candidates = if explicit.is_empty() {
        // Fall back to the unique node without incoming edges.
        scope_nodes.iter().copied().filter(|&ix| incoming.get(ix).is_none_or(Vec::is_empty)).collect()
    } else {
        explicit
    };
    match candidates.as_slice() {
        [] => Err(GraphError::NoEntry { scope: scope_name.to_owned() }),
        [one] => Ok(*one),
        many => Err(GraphError::MultipleEntries {
            scope: scope_name.to_owned(),
            nodes: many.iter().filter_map(|&ix| nodes.get(ix)).map(|n| n.id.to_string()).collect(),
        }),
    }
}

fn topo_order(
    scope_nodes: &[NodeIx],
    outgoing: &[Vec<EdgeIx>],
    incoming: &[Vec<EdgeIx>],
    edges: &[GraphEdge],
    nodes: &[GraphNode],
    scope_name: &str,
) -> Result<Vec<NodeIx>, GraphError> {
    let mut indeg: HashMap<NodeIx, usize> =
        scope_nodes.iter().map(|&ix| (ix, incoming.get(ix).map_or(0, Vec::len))).collect();
    let mut queue: VecDeque<NodeIx> = scope_nodes.iter().copied().filter(|ix| indeg.get(ix) == Some(&0)).collect();
    let mut order = Vec::with_capacity(scope_nodes.len());
    while let Some(ix) = queue.pop_front() {
        order.push(ix);
        for &ex in outgoing.get(ix).map(Vec::as_slice).unwrap_or_default() {
            if let Some(e) = edges.get(ex)
                && let Some(d) = indeg.get_mut(&e.target)
            {
                *d = d.saturating_sub(1);
                if *d == 0 {
                    queue.push_back(e.target);
                }
            }
        }
    }
    if order.len() != scope_nodes.len() {
        let stuck = scope_nodes
            .iter()
            .find(|ix| indeg.get(ix).is_some_and(|d| *d > 0))
            .and_then(|&ix| nodes.get(ix))
            .map(|n| n.id.to_string())
            .unwrap_or_default();
        return Err(GraphError::Cycle { scope: scope_name.to_owned(), node: stuck });
    }
    Ok(order)
}

fn reachable_from(entry: NodeIx, outgoing: &[Vec<EdgeIx>], edges: &[GraphEdge]) -> HashSet<NodeIx> {
    let mut seen = HashSet::from([entry]);
    let mut stack = vec![entry];
    while let Some(ix) = stack.pop() {
        for &ex in outgoing.get(ix).map(Vec::as_slice).unwrap_or_default() {
            if let Some(e) = edges.get(ex)
                && seen.insert(e.target)
            {
                stack.push(e.target);
            }
        }
    }
    seen
}

/// FNV-1a over the canonical JSON of the graph section (stable across processes,
/// unlike `DefaultHasher`).
fn fingerprint(workflow: &Workflow) -> Result<String, GraphError> {
    let text = workflow.canonical_graph_json().map_err(|e| GraphError::Unserializable(e.to_string()))?;
    let mut h = Fnv(FNV_OFFSET);
    text.hash(&mut h);
    Ok(format!("{:016x}", h.finish()))
}

/// FNV-1a of raw bytes, as hex.
fn fnv_hex(bytes: &[u8]) -> String {
    let mut h = Fnv(FNV_OFFSET);
    h.write(bytes);
    format!("{:016x}", h.finish())
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// 64-bit FNV-1a.
struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}
