//! Whole-document validation that collects every finding.
//!
//! [`Graph::build`] stops at the first problem, which suits programs. People
//! editing a document want all of them at once, each pointing at the text
//! that caused it. [`validate_text`] parses a JSON or YAML document, checks
//! it against the node catalog and returns every [`Diagnostic`] with its
//! document path (`graph.nodes[1].template`), source position, and a
//! "did you mean" suggestion where one is close enough. When nothing is
//! wrong it also returns the compiled graph.
//!
//! Checks, in addition to everything [`Graph::build`] enforces:
//!
//! - unknown fields, for the document, graph, nodes, edges, policies and node
//!   configs whose catalog schema is closed (`additionalProperties: false`;
//!   every built-in kind). Plugin configs with open schemas take any key;
//! - required config fields from the catalog schema;
//! - unknown node types, node ids in edges, parents and selectors, and edge
//!   handles the source node never selects;
//! - deprecated spellings (warnings).
//!
//! Documents in the import layout (a top-level `workflow` or `app` key, or
//! nodes wrapped in `data`) are checked leniently: unknown fields are not
//! reported for them.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use serde::Serialize;

use crate::catalog::{Handles, NodeDescriptor};
use crate::dsl::{ErrorStrategy, NodeDoc};
use crate::graph::{FAIL_BRANCH, Graph, GraphError, SOURCE_HANDLE};
use crate::node::{ConfigError, Node, NodeRole, Registry};
pub use crate::source_map::{DocPath, PathSegment, SourceMap, SourceSpan};

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The document is rejected.
    Error,
    /// The document runs, but probably not as intended, or uses a deprecated spelling.
    Warning,
}

/// One validation finding.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Diagnostic {
    pub severity: Severity,
    /// Stable machine-readable category, e.g. `unknown-field`, `unknown-node`.
    pub code: &'static str,
    pub message: String,
    /// Where in the document, e.g. `graph.nodes[1].template`.
    pub path: DocPath,
    /// Source position (`line`, `column`, `length`), when known.
    #[serde(flatten)]
    pub span: Option<SourceSpan>,
    /// Id of the node the finding is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Likely intended spelling ("did you mean `template`?").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// Extra guidance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Point at the key of `path` rather than its value.
    #[serde(skip)]
    pub at_key: bool,
}

impl Diagnostic {
    pub fn error(code: &'static str, path: DocPath, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, path, message.into())
    }

    pub fn warning(code: &'static str, path: DocPath, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, path, message.into())
    }

    fn new(severity: Severity, code: &'static str, path: DocPath, message: String) -> Self {
        Self { severity, code, message, path, span: None, node: None, suggestion: None, help: None, at_key: false }
    }

    /// Point at the key of the path (for unknown or misspelled fields).
    #[must_use]
    pub fn at_key(mut self) -> Self {
        self.at_key = true;
        self
    }

    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Option<String>) -> Self {
        self.suggestion = suggestion;
        self
    }

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    #[must_use]
    pub fn with_node(mut self, id: &str) -> Self {
        self.node = Some(id.to_owned());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)?;
        if let Some(s) = &self.suggestion {
            write!(f, " (did you mean `{s}`?)")?;
        }
        Ok(())
    }
}

/// Result of [`validate_text`].
#[derive(Debug)]
pub struct Validation {
    /// Every finding, in document order.
    pub diagnostics: Vec<Diagnostic>,
    /// The compiled graph, when there are no errors.
    pub graph: Option<Graph>,
}

impl Validation {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.is_error())
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| !d.is_error())
    }
}

/// The closest candidate to `word` by edit distance, if it is close enough to
/// be a likely typo (at most one edit per three characters, minimum one).
/// A case-insensitive exact match always wins.
pub fn suggest<'a, I>(word: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let limit = (word.chars().count().max(3) / 3).max(1);
    let lower = word.to_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        if candidate == word {
            continue;
        }
        let distance =
            if candidate.to_lowercase() == lower { 0 } else { edit_distance(&lower, &candidate.to_lowercase()) };
        if distance <= limit && best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, c)| c.to_owned())
}

/// Optimal string alignment distance (Levenshtein plus adjacent transpositions).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let width = b.len() + 1;
    let mut table = vec![0usize; (a.len() + 1) * width];
    let at = |i: usize, j: usize| i * width + j;
    for i in 0..=a.len() {
        if let Some(cell) = table.get_mut(at(i, 0)) {
            *cell = i;
        }
    }
    for j in 0..=b.len() {
        if let Some(cell) = table.get_mut(at(0, j)) {
            *cell = j;
        }
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let get = |i: usize, j: usize| table.get(at(i, j)).copied().unwrap_or(usize::MAX - 1);
            let same = a.get(i - 1) == b.get(j - 1);
            let mut value = (get(i - 1, j) + 1).min(get(i, j - 1) + 1).min(get(i - 1, j - 1) + usize::from(!same));
            if i > 1 && j > 1 && a.get(i - 1) == b.get(j - 2) && a.get(i - 2) == b.get(j - 1) {
                value = value.min(get(i - 2, j - 2) + 1);
            }
            if let Some(cell) = table.get_mut(at(i, j)) {
                *cell = value;
            }
        }
    }
    table.last().copied().unwrap_or(0)
}

/// Rendered " (did you mean `x`?)" suffix, or nothing.
pub(crate) fn did_you_mean(suggestion: &Option<String>) -> String {
    suggestion.as_ref().map(|s| format!("; did you mean `{s}`?")).unwrap_or_default()
}

/// Parse, check and compile a JSON or YAML document, collecting every finding.
pub fn validate_text(text: &str, registry: &Registry) -> Validation {
    let raw = match parse_raw(text) {
        Ok(raw) => raw,
        Err(diagnostic) => return Validation { diagnostics: vec![*diagnostic], graph: None },
    };
    let mut validation = validate_value(raw, registry);
    // Positions are only needed to report findings; a clean document skips the
    // second, span-tracking parse.
    if validation.diagnostics.iter().any(|d| d.span.is_none()) {
        let map = SourceMap::from_text(text);
        for d in &mut validation.diagnostics {
            if d.span.is_none() {
                d.span = map.locate(&d.path, d.at_key);
            }
        }
    }
    // Document order; findings without a position keep their relative order at the end.
    validation.diagnostics.sort_by_key(|d| d.span.map_or((u32::MAX, u32::MAX), |s| (s.line, s.column)));
    validation
}

/// Check and compile an already-decoded document (no source positions).
pub fn validate_value(raw: serde_json::Value, registry: &Registry) -> Validation {
    let decoded = crate::dsl::decode(raw);
    let mut diagnostics = decoded.diagnostics;
    let paths = Paths { nodes: decoded.node_paths, edges: decoded.edge_paths };
    let built = check_graph(&decoded.workflow, &paths, registry, &mut diagnostics);
    if diagnostics.iter().any(Diagnostic::is_error) {
        return Validation { diagnostics, graph: None };
    }
    let ids: Vec<String> = decoded.workflow.graph.nodes.iter().map(|n| n.id.clone()).collect();
    let checked: Option<Vec<Arc<dyn Node>>> = built.into_iter().collect();
    let graph = match checked {
        Some(nodes) => Graph::build_checked(decoded.workflow, registry, nodes),
        None => Graph::build(decoded.workflow, registry),
    };
    match graph {
        Ok(graph) => {
            for warning in graph.warnings() {
                let path = backticked(warning)
                    .and_then(|id| ids.iter().position(|n| n == id))
                    .and_then(|ix| paths.nodes.get(ix).cloned())
                    .unwrap_or_default();
                diagnostics.push(Diagnostic::warning("graph", path, warning.clone()));
            }
            Validation { diagnostics, graph: Some(graph) }
        }
        Err(error) => {
            let path = backticked(&error.to_string())
                .and_then(|id| ids.iter().position(|n| n == id))
                .and_then(|ix| paths.nodes.get(ix).cloned())
                .unwrap_or_else(|| DocPath::root().key("graph"));
            diagnostics.push(Diagnostic::error("graph", path, error.to_string()));
            Validation { diagnostics, graph: None }
        }
    }
}

/// The first `` `quoted` `` word of a message.
fn backticked(message: &str) -> Option<&str> {
    let start = message.find('`')? + 1;
    let len = message.get(start..)?.find('`')?;
    message.get(start..start + len)
}

/// Parse JSON (text starting with `{`) or YAML into a JSON value.
fn parse_raw(text: &str) -> Result<serde_json::Value, Box<Diagnostic>> {
    let syntax = |message: String, line: usize, column: usize| {
        let mut d = Diagnostic::error("syntax", DocPath::root(), message);
        if let (Ok(line), Ok(column)) = (u32::try_from(line), u32::try_from(column))
            && line > 0
        {
            d.span = Some(SourceSpan { line, column: column.max(1), len: 1 });
        }
        Box::new(d)
    };
    if text.trim_start().starts_with('{') {
        return serde_json::from_str(text)
            .map_err(|e| syntax(format!("invalid JSON: {}", strip_position(&e.to_string())), e.line(), e.column()));
    }
    #[cfg(feature = "yaml")]
    {
        let options = serde_saphyr::options! { strict_booleans: true };
        serde_saphyr::from_str_with_options(text, options).map_err(|e| {
            let (line, column) = e
                .location()
                .map(|l| (usize::try_from(l.line()).unwrap_or(0), usize::try_from(l.column()).unwrap_or(0)))
                .unwrap_or((0, 0));
            let message = e.to_string();
            let first = message.lines().next().unwrap_or("invalid YAML").trim().to_owned();
            syntax(format!("invalid YAML: {}", strip_position(&first)), line, column)
        })
    }
    #[cfg(not(feature = "yaml"))]
    {
        serde_json::from_str(text).map_err(|e| {
            syntax(
                format!("invalid JSON (YAML support is not compiled in): {}", strip_position(&e.to_string())),
                e.line(),
                e.column(),
            )
        })
    }
}

/// Drop a trailing " at line X column Y" that the position already carries.
fn strip_position(message: &str) -> String {
    match message.rfind(" at line ") {
        Some(ix) => message.get(..ix).unwrap_or(message).to_owned(),
        None => message.to_owned(),
    }
}

struct Paths {
    nodes: Vec<DocPath>,
    edges: Vec<DocPath>,
}

impl Paths {
    fn node(&self, ix: usize) -> DocPath {
        self.nodes.get(ix).cloned().unwrap_or_default()
    }
    fn edge(&self, ix: usize) -> DocPath {
        self.edges.get(ix).cloned().unwrap_or_default()
    }
}

/// Graph-level checks over every decoded node and edge.
/// Check the whole document, collecting every finding in `out`. Returns the node
/// built for each document node (`None` where it could not be built), so a clean
/// document can be compiled without building its nodes again.
fn check_graph(
    workflow: &crate::Workflow,
    paths: &Paths,
    registry: &Registry,
    out: &mut Vec<Diagnostic>,
) -> Vec<Option<Arc<dyn Node>>> {
    let nodes = &workflow.graph.nodes;
    let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(nodes.len());
    for (ix, node) in nodes.iter().enumerate() {
        let path = paths.node(ix).key("id");
        if let Err(GraphError::InvalidNodeId { reason, .. }) = crate::graph::validate_node_id(&node.id) {
            out.push(
                Diagnostic::error("invalid-node-id", path.clone(), format!("node id `{}` {reason}", node.id))
                    .with_node(&node.id),
            );
        }
        if let Some(first) = index.insert(&node.id, ix) {
            index.insert(&node.id, first);
            out.push(
                Diagnostic::error("duplicate-node", path, format!("duplicate node id `{}`", node.id))
                    .with_node(&node.id)
                    .with_help(format!("first defined at {}", paths.node(first))),
            );
        }
    }

    let mut built: Vec<Option<Arc<dyn Node>>> = Vec::with_capacity(nodes.len());
    for (ix, doc) in nodes.iter().enumerate() {
        built.push(check_node(doc, &paths.node(ix), registry, out));
    }

    check_parents(nodes, &index, &built, paths, out);
    check_edges(workflow, &index, &ids, &built, paths, registry, out);
    check_references(nodes, &index, &ids, &built, paths, out);
    built
}

/// Type, fields and configuration of one node. Returns the built node.
fn check_node(doc: &NodeDoc, path: &DocPath, registry: &Registry, out: &mut Vec<Diagnostic>) -> Option<Arc<dyn Node>> {
    if !registry.contains(&doc.kind) {
        let mut kinds: Vec<&str> = registry.kinds().collect();
        kinds.sort_unstable();
        out.push(
            Diagnostic::error("unknown-node-type", path.key("type"), format!("unknown node type `{}`", doc.kind))
                .with_node(&doc.id)
                .with_suggestion(suggest(&doc.kind, kinds))
                .with_help("node types come from built-in features and plugins"),
        );
        return None;
    }
    if doc.strict
        && let Some(descriptor) = registry.descriptor_ref(&doc.kind)
    {
        out.extend(config_diagnostics(doc, descriptor, path));
    }
    match registry.build(doc) {
        Ok(node) => Some(node),
        Err(error) => {
            out.push(config_error(&error, path));
            None
        }
    }
}

fn config_error(error: &ConfigError, base: &DocPath) -> Diagnostic {
    let path = error.field.as_deref().map(|field| join_dotted(base, field)).unwrap_or_else(|| base.clone());
    Diagnostic::error(
        "invalid-config",
        path,
        format!("invalid `{}` node `{}`: {}", error.kind, error.node_id, error.message),
    )
    .with_node(&error.node_id)
}

/// `base` extended by a serde-style path such as `variables[0].value_selector`.
fn join_dotted(base: &DocPath, field: &str) -> DocPath {
    let mut path = base.clone();
    for part in field.split('.') {
        let (key, indices) = match part.find('[') {
            Some(ix) => (part.get(..ix).unwrap_or(part), part.get(ix..).unwrap_or("")),
            None => (part, ""),
        };
        if !key.is_empty() && key != "?" {
            path = path.key(key);
        }
        for index in indices.split(['[', ']']).filter(|s| !s.is_empty()) {
            if let Ok(ix) = index.parse() {
                path = path.index(ix);
            }
        }
    }
    path
}

/// Unknown, missing and deprecated config fields of a strict node, judged by its catalog schema.
pub(crate) fn config_diagnostics(doc: &NodeDoc, descriptor: &NodeDescriptor, base: &DocPath) -> Vec<Diagnostic> {
    let mut issues = Vec::new();
    let config = serde_json::Value::Object(doc.config.clone());
    let scope = Scope { kind: &doc.kind, depth: base.segments().len() };
    walk_schema(&descriptor.config_schema, &config, base, &scope, &mut issues);
    for issue in &mut issues {
        issue.node = Some(doc.id.clone());
    }
    issues
}

/// Report unknown keys (closed object schemas only), missing required keys and
/// deprecated keys. Composition keywords (`oneOf`, `anyOf`, ...) are not followed.
fn walk_schema(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    path: &DocPath,
    scope: &Scope<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(schema) = schema.as_object() else { return };
    match value {
        serde_json::Value::Object(map) => {
            let properties = schema.get("properties").and_then(serde_json::Value::as_object);
            let closed = schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false))
                && !schema.contains_key("patternProperties");
            let extra = schema.get("additionalProperties").filter(|s| s.is_object());
            let known: Vec<&str> = properties
                .map(|p| {
                    p.iter()
                        .filter(|(_, s)| s.get("deprecated") != Some(&serde_json::Value::Bool(true)))
                        .map(|(k, _)| k.as_str())
                        .collect()
                })
                .unwrap_or_default();
            let mut suggested = HashSet::new();
            for (key, child) in map {
                let child_path = path.key(key);
                match properties.and_then(|p| p.get(key)) {
                    Some(sub) => {
                        if sub.get("deprecated") == Some(&serde_json::Value::Bool(true)) {
                            let note = sub.get("description").and_then(serde_json::Value::as_str).unwrap_or("");
                            let replacement = backticked(note).map(str::to_owned);
                            out.push(
                                Diagnostic::warning("deprecated", child_path.clone(), format!("`{key}` is deprecated"))
                                    .at_key()
                                    .with_suggestion(replacement),
                            );
                        }
                        walk_schema(sub, child, &child_path, scope, out);
                    }
                    None => match extra {
                        Some(sub) => walk_schema(sub, child, &child_path, scope, out),
                        None if closed => {
                            let suggestion = suggest(key, known.iter().copied());
                            if let Some(s) = &suggestion {
                                suggested.insert(s.clone());
                            }
                            let message = format!("unknown field `{key}` {}", scope.describe(path));
                            let mut d = Diagnostic::error("unknown-field", child_path, message)
                                .at_key()
                                .with_suggestion(suggestion);
                            if d.suggestion.is_none() && !known.is_empty() {
                                d = d.with_help(format!("expected one of: {}", known.join(", ")));
                            }
                            out.push(d);
                        }
                        None => {}
                    },
                }
            }
            for required in schema.get("required").and_then(serde_json::Value::as_array).into_iter().flatten() {
                let Some(required) = required.as_str() else { continue };
                if !map.contains_key(required) && !suggested.contains(required) {
                    out.push(Diagnostic::error(
                        "missing-field",
                        path.clone(),
                        format!("missing required field `{required}` {}", scope.describe(path)),
                    ));
                }
            }
        }
        serde_json::Value::Array(items) => {
            if let Some(sub) = schema.get("items").filter(|s| s.is_object()) {
                for (ix, item) in items.iter().enumerate() {
                    walk_schema(sub, item, &path.index(ix), scope, out);
                }
            }
        }
        _ => {}
    }
}

/// Where a config finding is, for messages.
struct Scope<'a> {
    kind: &'a str,
    /// Path length of the node config root.
    depth: usize,
}

impl Scope<'_> {
    fn describe(&self, path: &DocPath) -> String {
        let inner = path.segments().get(self.depth..).unwrap_or_default();
        if inner.is_empty() {
            return format!("for node type `{}`", self.kind);
        }
        format!("in `{}` of a `{}` node", join_segments(&DocPath::root(), inner), self.kind)
    }
}

fn check_parents(
    nodes: &[NodeDoc],
    index: &HashMap<&str, usize>,
    built: &[Option<Arc<dyn Node>>],
    paths: &Paths,
    out: &mut Vec<Diagnostic>,
) {
    let containers: Vec<&str> = nodes
        .iter()
        .zip(built)
        .filter(|(_, n)| n.as_ref().is_some_and(|n| n.role() == NodeRole::Container))
        .map(|(d, _)| d.id.as_str())
        .collect();
    for (ix, doc) in nodes.iter().enumerate() {
        let Some(parent) = &doc.parent else { continue };
        let path = paths.node(ix).key("parent");
        match index.get(parent.as_str()) {
            None => out.push(
                Diagnostic::error(
                    "unknown-node",
                    path,
                    format!("parent `{parent}` of node `{}` is not a node id", doc.id),
                )
                .with_node(&doc.id)
                .with_suggestion(suggest(parent, containers.iter().copied())),
            ),
            Some(&p) => {
                let role = built.get(p).and_then(|n| n.as_ref()).map(|n| n.role());
                if role.is_some_and(|r| r != NodeRole::Container) {
                    out.push(
                        Diagnostic::error(
                            "invalid-parent",
                            path,
                            format!("parent `{parent}` of node `{}` is not a container (iteration or loop)", doc.id),
                        )
                        .with_node(&doc.id),
                    );
                }
            }
        }
    }
}

fn check_edges(
    workflow: &crate::Workflow,
    index: &HashMap<&str, usize>,
    ids: &[&str],
    built: &[Option<Arc<dyn Node>>],
    paths: &Paths,
    registry: &Registry,
    out: &mut Vec<Diagnostic>,
) {
    let nodes = &workflow.graph.nodes;
    let mut edge_ids = HashSet::new();
    for (k, edge) in workflow.graph.edges.iter().enumerate() {
        let path = paths.edge(k);
        if let Some(id) = &edge.id
            && !edge_ids.insert(id.as_str())
        {
            out.push(Diagnostic::error("duplicate-edge", path.key("id"), format!("duplicate edge id `{id}`")));
        }
        let mut lookup = |end: &str, id: &str| match index.get(id) {
            Some(&ix) => Some(ix),
            None => {
                out.push(
                    Diagnostic::error("unknown-node", path.key(end), format!("edge {end} `{id}` is not a node id"))
                        .with_suggestion(suggest(id, ids.iter().copied())),
                );
                None
            }
        };
        let (Some(source), Some(target)) = (lookup("source", &edge.source), lookup("target", &edge.target)) else {
            continue;
        };
        let (Some(source_doc), Some(target_doc)) = (nodes.get(source), nodes.get(target)) else { continue };
        if source_doc.parent != target_doc.parent {
            let scope = |p: &Option<String>| p.as_deref().map_or("the root graph".to_owned(), |p| format!("`{p}`"));
            out.push(Diagnostic::error(
                "cross-scope-edge",
                path.clone(),
                format!(
                    "edge `{}` -> `{}` crosses scopes: `{}` is in {} but `{}` is in {}",
                    edge.source,
                    edge.target,
                    edge.source,
                    scope(&source_doc.parent),
                    edge.target,
                    scope(&target_doc.parent)
                ),
            ));
        }
        let Some(node) = built.get(source).and_then(|n| n.as_ref()) else { continue };
        let Some(allowed) = allowed_handles(node.as_ref(), registry.descriptor_ref(&source_doc.kind)) else {
            continue;
        };
        let handle = edge.handle.as_deref().filter(|h| !h.is_empty()).unwrap_or(SOURCE_HANDLE);
        if let Some(message) = dead_fail_branch(source_doc, handle) {
            out.push(Diagnostic::warning("dead-edge", path.key("handle"), message).with_node(&source_doc.id));
        }
        if !allowed.iter().any(|h| h == handle) {
            let at = if edge.handle.is_some() { path.key("handle") } else { path.clone() };
            out.push(
                Diagnostic::error("unknown-handle", at, handle_message(source_doc, handle, &allowed))
                    .with_node(&source_doc.id)
                    .with_suggestion(suggest(handle, allowed.iter().map(String::as_str))),
            );
        }
    }
}

pub(crate) fn handle_message(source: &NodeDoc, handle: &str, allowed: &[String]) -> String {
    format!(
        "node `{}` ({}) has no handle `{handle}`; its handles are: {}",
        source.id,
        source.kind,
        allowed.iter().map(|h| format!("`{h}`")).collect::<Vec<_>>().join(", ")
    )
}

/// The handles a node can select, or `None` when they cannot be known statically.
pub(crate) fn allowed_handles(node: &dyn Node, descriptor: Option<&NodeDescriptor>) -> Option<Vec<String>> {
    let mut handles = match node.branch_handles() {
        Some(handles) => handles,
        None => match descriptor.map(|d| &d.handles) {
            Some(Handles::Fixed { handles }) => handles.clone(),
            Some(Handles::Dynamic { .. }) => return None,
            _ if node.role() == NodeRole::Branch => return None,
            _ => vec![SOURCE_HANDLE.to_owned()],
        },
    };
    // Always allowed: without `on_error: {strategy: fail-branch}` such an edge is
    // never taken, which is reported as a warning (see `dead_fail_branch`).
    handles.push(FAIL_BRANCH.to_owned());
    Some(handles)
}

/// Warning text for a `fail-branch` edge whose source does not use that strategy.
pub(crate) fn dead_fail_branch(source: &NodeDoc, handle: &str) -> Option<String> {
    (handle == FAIL_BRANCH && source.policy.on_error != ErrorStrategy::FailBranch).then(|| {
        format!(
            "edge from `{}` uses handle `fail-branch`, but the node does not set \
             `on_error: {{strategy: fail-branch}}`; the edge is never taken",
            source.id
        )
    })
}

fn check_references(
    nodes: &[NodeDoc],
    index: &HashMap<&str, usize>,
    ids: &[&str],
    built: &[Option<Arc<dyn Node>>],
    paths: &Paths,
    out: &mut Vec<Diagnostic>,
) {
    let mut candidates: Vec<&str> = ids.to_vec();
    candidates.extend_from_slice(crate::graph::RESERVED_NAMESPACES);
    for (ix, (doc, node)) in nodes.iter().zip(built).enumerate() {
        let Some(node) = node else { continue };
        let mut seen = HashSet::new();
        for selector in node.inputs() {
            let namespace = selector.namespace();
            if crate::graph::RESERVED_NAMESPACES.contains(&namespace) || index.contains_key(namespace) {
                continue;
            }
            if !seen.insert(selector.to_string()) {
                continue;
            }
            let path = locate_reference(&doc.config, namespace)
                .map(|rel| join_segments(&paths.node(ix), &rel))
                .unwrap_or_else(|| paths.node(ix));
            out.push(
                Diagnostic::error(
                    "unknown-reference",
                    path,
                    format!("node `{}` references `{selector}`, but there is no node `{namespace}`", doc.id),
                )
                .with_node(&doc.id)
                .with_suggestion(suggest(namespace, candidates.iter().copied()))
                .with_help("selectors start with a node id or `sys`, `env`, `conversation`"),
            );
        }
    }
}

fn join_segments(base: &DocPath, rel: &[PathSegment]) -> DocPath {
    rel.iter().fold(base.clone(), |path, segment| match segment {
        PathSegment::Key(k) => path.key(k),
        PathSegment::Index(i) => path.index(*i),
    })
}

/// Find where in a config a selector with `namespace` appears: an array whose
/// first item is the namespace, a dotted string `namespace.x`, or a template
/// placeholder `{{#namespace.x#}}`.
fn locate_reference(config: &serde_json::Map<String, serde_json::Value>, namespace: &str) -> Option<Vec<PathSegment>> {
    fn search(value: &serde_json::Value, namespace: &str, path: &mut Vec<PathSegment>) -> bool {
        match value {
            serde_json::Value::String(s) => {
                s.contains(&format!("{{{{#{namespace}.")) || s.starts_with(&format!("{namespace}."))
            }
            serde_json::Value::Array(items) => {
                let is_selector = items.first().and_then(serde_json::Value::as_str) == Some(namespace)
                    && items.iter().all(serde_json::Value::is_string)
                    && items.len() >= 2;
                if is_selector {
                    // Point at the namespace part, the one that is wrong.
                    path.push(PathSegment::Index(0));
                    return true;
                }
                for (ix, item) in items.iter().enumerate() {
                    path.push(PathSegment::Index(ix));
                    if search(item, namespace, path) {
                        return true;
                    }
                    path.pop();
                }
                false
            }
            serde_json::Value::Object(map) => {
                for (key, item) in map {
                    path.push(PathSegment::Key(key.clone()));
                    if search(item, namespace, path) {
                        return true;
                    }
                    path.pop();
                }
                false
            }
            _ => false,
        }
    }
    let mut path = Vec::new();
    search(&serde_json::Value::Object(config.clone()), namespace, &mut path).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions() {
        assert_eq!(suggest("templat", ["template", "variables"]), Some("template".into()));
        assert_eq!(suggest("varaibles", ["template", "variables"]), Some("variables".into()));
        assert_eq!(suggest("Template", ["template"]), Some("template".into()));
        assert_eq!(suggest("zzz", ["template", "variables"]), None);
        assert_eq!(suggest("a", ["b"]), Some("b".into()));
        assert_eq!(suggest("x", ["x"]), None, "an exact match is not a suggestion");
        assert_eq!(edit_distance("abc", "acb"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[test]
    fn dotted_paths_join() {
        let base = DocPath::root().key("graph").key("nodes").index(2);
        assert_eq!(
            join_dotted(&base, "variables[0].value_selector").to_string(),
            "graph.nodes[2].variables[0].value_selector"
        );
        assert_eq!(join_dotted(&base, "a[1][2]").to_string(), "graph.nodes[2].a[1][2]");
    }

    #[test]
    fn finds_references() {
        let config = serde_json::json!({"variables": [{"variable": "x", "value_selector": ["strat", "x"]}], "t": "{{#llm.text#}}"});
        let config = config.as_object().cloned().unwrap_or_default();
        let path = locate_reference(&config, "strat").unwrap_or_default();
        assert_eq!(join_segments(&DocPath::root(), &path).to_string(), "variables[0].value_selector[0]");
        let path = locate_reference(&config, "llm").unwrap_or_default();
        assert_eq!(join_segments(&DocPath::root(), &path).to_string(), "t");
    }
}
