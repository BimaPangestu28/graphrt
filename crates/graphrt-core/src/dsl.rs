//! Workflow documents: the portable, serializable description of a workflow.
//!
//! A document is plain data (JSON or YAML). It is turned into an executable
//! [`crate::graph::Graph`] by [`crate::graph::Graph::build`] using a
//! [`crate::node::Registry`]. The format is frozen as version `1`; see
//! `docs/workflow-format.md` for the stability promise and
//! `schema/workflow-v1.schema.json` for the JSON Schema.
//!
//! ```yaml
//! version: "1"
//! name: hello
//! env:
//!   - { name: API_KEY, type: secret }
//! graph:
//!   nodes:
//!     - id: start
//!       type: start
//!       variables: [{ variable: name, type: string, required: true }]
//!     - id: greet
//!       type: template-transform
//!       template: "Hello {{ name }}"
//!       variables: [{ variable: name, value_selector: [start, name] }]
//!     - id: end
//!       type: end
//!       outputs: [{ variable: greeting, value_selector: [greet, output] }]
//!   edges:
//!     - { source: start, target: greet }
//!     - { source: greet, target: end }
//! ```
//!
//! v1 documents are strict: unknown fields are errors (keys starting with
//! `x-` are free for tools and ignored). For interoperability the loader also
//! accepts an *import layout*, checked leniently: nodes in the "node with a
//! `data` object" shape (`{id, parentId, data: {type, title, ...}}`), camelCase
//! edge handles (`sourceHandle`) and documents wrapped in a top-level
//! `workflow` key.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::source_map::DocPath;
use crate::validate::{Diagnostic, suggest};
use crate::value::{Value, ValueType};

/// The document format version this build reads and writes.
pub const FORMAT_VERSION: &str = "1";

/// Error raised while decoding a workflow document.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DocumentError {
    #[error("invalid JSON workflow document: {0}")]
    Json(#[from] serde_json::Error),
    /// Only produced when the `yaml` feature is enabled (the variant always exists
    /// so enabling the feature never breaks downstream matches).
    #[error("invalid YAML workflow document: {0}")]
    Yaml(String),
    #[error("invalid node `{id}`: {message}")]
    Node { id: String, message: String },
    /// A problem outside any node, at a document path such as `graph.edges[0].source`.
    #[error("{path}: {message}")]
    Field { path: String, message: String },
}

/// A complete workflow document.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Workflow {
    /// Document format version.
    pub version: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Environment variables (`env.*`). Values may be omitted and supplied by the host at run time.
    pub env: Vec<VariableDecl>,
    /// Conversation variables (`conversation.*`), persisted by the host between runs.
    pub conversation: Vec<VariableDecl>,
    pub graph: GraphDoc,
}

/// Declaration of an env or conversation variable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VariableDecl {
    pub name: String,
    #[serde(rename = "type", alias = "value_type", default = "any_type")]
    pub ty: ValueType,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
}

fn any_type() -> ValueType {
    ValueType::Any
}

/// Nodes and edges.
#[derive(Clone, Debug, Default, Serialize)]
pub struct GraphDoc {
    pub nodes: Vec<NodeDoc>,
    pub edges: Vec<EdgeDoc>,
}

/// One node. Node-specific settings live in `config`.
///
/// Serializes to the flat layout the loader accepts (`id`, `type`, `title`,
/// `parent`, `on_error`, `retry`, `timeout_ms`, then the config keys), so a
/// document survives a serialize / parse round trip unchanged.
#[derive(Clone, Debug)]
pub struct NodeDoc {
    pub id: String,
    /// Node kind, e.g. `llm`, `if-else`, `http-request`.
    pub kind: String,
    pub title: Option<String>,
    /// Owning container (iteration / loop) for nodes inside a container body.
    pub parent: Option<String>,
    pub policy: NodePolicy,
    /// Remaining node-specific configuration.
    pub config: serde_json::Map<String, serde_json::Value>,
    /// Whether unknown config fields are errors (judged by the node kind's
    /// catalog schema). True for v1 documents and programmatic nodes; false for
    /// nodes read from the import layout, which carry foreign builder fields.
    pub strict: bool,
}

impl NodeDoc {
    /// Minimal constructor, mainly for programmatic graph building and tests.
    pub fn new(id: impl Into<String>, kind: impl Into<String>, config: serde_json::Value) -> Self {
        let config = match config {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        Self {
            id: id.into(),
            kind: kind.into(),
            title: None,
            parent: None,
            policy: NodePolicy::default(),
            config,
            strict: true,
        }
    }

    /// Set the owning container.
    pub fn with_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// Set the failure policy.
    pub fn with_policy(mut self, policy: NodePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Deserialize `config` into a node-specific config struct.
    pub fn parse_config<T: serde::de::DeserializeOwned>(&self) -> Result<T, DocumentError> {
        parse_config_at(&self.config)
            .map_err(|(path, e)| DocumentError::Node { id: self.id.clone(), message: with_field(&path, &e) })
    }

    /// Human-readable label.
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }
}

/// Deserialize a config map, reporting the path of the offending field
/// (`variables[0].value_selector`, or `.` for the config itself). The path is
/// recovered from the error only when there is one ([`crate::value_path`]),
/// so each config type is compiled against one plain deserializer.
pub(crate) fn parse_config_at<T: serde::de::DeserializeOwned>(
    config: &serde_json::Map<String, serde_json::Value>,
) -> Result<T, (String, serde_json::Error)> {
    let value = serde_json::Value::Object(config.clone());
    T::deserialize(&value).map_err(|e| (crate::value_path::error_path_text(&value, &e), e))
}

fn with_field(path: &str, error: &serde_json::Error) -> String {
    if path == "." { error.to_string() } else { format!("{path}: {error}") }
}

/// Failure handling, retries and timeouts for one node.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodePolicy {
    #[serde(default)]
    pub on_error: ErrorStrategy,
    #[serde(default)]
    pub retry: RetryPolicy,
    /// Per-attempt timeout in milliseconds. `None` uses the engine default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// What happens when a node fails after exhausting retries.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ErrorStrategy {
    /// Fail the frame (and the run, unless a container handles it).
    #[default]
    Fail,
    /// Route through outgoing edges whose handle is [`crate::graph::FAIL_BRANCH`];
    /// outputs are `error_message` and `error_type`.
    FailBranch,
    /// Continue normally with these outputs, plus `error_message` and `error_type`.
    DefaultValue {
        #[serde(default)]
        values: IndexMap<String, Value>,
    },
}

/// Retry policy. Applies to every node kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default)]
    pub max_retries: u32,
    /// Delay before the first retry, in milliseconds.
    #[serde(default)]
    pub interval_ms: u64,
    /// Multiplier applied to the delay after each retry (1.0 = constant).
    #[serde(default = "one")]
    pub backoff: f64,
}

fn one() -> f64 {
    1.0
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_retries: 0, interval_ms: 0, backoff: 1.0 }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (1-based).
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let factor = self.backoff.max(1.0).powi(i32::try_from(attempt.saturating_sub(1)).unwrap_or(i32::MAX));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let ms = (self.interval_ms as f64 * factor).min(3_600_000.0) as u64;
        ms
    }
}

/// A directed edge. `handle` selects the branch of the source node (default `source`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeDoc {
    #[serde(default)]
    pub id: Option<String>,
    pub source: String,
    pub target: String,
    #[serde(default, alias = "sourceHandle", alias = "source_handle", skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

impl EdgeDoc {
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self { id: None, source: source.into(), target: target.into(), handle: None }
    }

    pub fn with_handle(mut self, handle: impl Into<String>) -> Self {
        self.handle = Some(handle.into());
        self
    }
}

impl Workflow {
    /// Parse a JSON document.
    pub fn from_json(text: &str) -> Result<Self, DocumentError> {
        let raw: serde_json::Value = serde_json::from_slice(text.as_bytes())?;
        Self::from_value(raw)
    }

    /// Parse a YAML document.
    #[cfg(feature = "yaml")]
    pub fn from_yaml(text: &str) -> Result<Self, DocumentError> {
        // YAML 1.2 booleans only: `yes`, `no`, `y`, `n`, `on`, `off` stay strings.
        let raw = crate::yaml::to_value(text, true).map_err(|e| DocumentError::Yaml(e.to_string()))?;
        Self::from_value(raw)
    }

    /// Parse JSON or YAML, detecting the format from the first non-blank character.
    pub fn parse(text: &str) -> Result<Self, DocumentError> {
        let first = text.trim_start().chars().next();
        if matches!(first, Some('{')) {
            return Self::from_json(text);
        }
        #[cfg(feature = "yaml")]
        {
            Self::from_yaml(text)
        }
        #[cfg(not(feature = "yaml"))]
        {
            Self::from_json(text)
        }
    }

    /// Build from an already-decoded JSON value. Fails with the first error;
    /// [`crate::validate::validate_text`] reports all of them.
    pub fn from_value(raw: serde_json::Value) -> Result<Self, DocumentError> {
        let decoded = decode(raw);
        match decoded.diagnostics.into_iter().find(Diagnostic::is_error) {
            None => Ok(decoded.workflow),
            Some(d) => {
                let message = match &d.suggestion {
                    Some(s) => format!("{}; did you mean `{s}`?", d.message),
                    None => d.message,
                };
                Err(match d.node {
                    Some(id) => DocumentError::Node { id, message },
                    None => DocumentError::Field { path: d.path.to_string(), message },
                })
            }
        }
    }
}

/// A decoded document plus where each node and edge came from and every
/// finding made while decoding.
pub(crate) struct Decoded {
    pub workflow: Workflow,
    /// Source path of each node in `workflow.graph.nodes`.
    pub node_paths: Vec<DocPath>,
    /// Source path of each edge in `workflow.graph.edges`.
    pub edge_paths: Vec<DocPath>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Which keys an object may have.
struct KeyRules<'a> {
    what: &'a str,
    known: &'a [&'a str],
    /// `(old, new)`: accepted with a warning.
    deprecated: &'a [(&'a str, &'a str)],
    /// Reserved for future versions of the format.
    reserved: &'a [&'a str],
}

const TOP_LEVEL: KeyRules<'static> = KeyRules {
    what: "the document",
    known: &["version", "name", "description", "env", "conversation", "graph"],
    deprecated: &[("environment_variables", "env"), ("conversation_variables", "conversation")],
    reserved: &["imports", "inputs", "outputs", "triggers", "metadata", "requires", "subgraphs"],
};

const GRAPH_KEYS: KeyRules<'static> =
    KeyRules { what: "`graph`", known: &["nodes", "edges"], deprecated: &[], reserved: &[] };

/// Node-level keys other than `id`/`type`/`title` and the node config.
const NODE_KEYS: KeyRules<'static> = KeyRules {
    what: "a node",
    known: &[],
    deprecated: &[
        ("parentId", "parent"),
        ("container_id", "parent"),
        ("iteration_id", "parent"),
        ("loop_id", "parent"),
        ("error_strategy", "on_error"),
        ("default_value", "on_error"),
        ("retry_config", "retry"),
    ],
    reserved: &["disabled", "cache", "metadata"],
};

/// Fields every node has, whatever its type.
pub const COMMON_NODE_FIELDS: &[&str] = &["id", "type", "title", "parent", "retry", "timeout_ms", "on_error"];

/// Node fields reserved for future versions; node configs may not use them.
pub const RESERVED_NODE_FIELDS: &[&str] = NODE_KEYS.reserved;

const EDGE_KEYS: KeyRules<'static> = KeyRules {
    what: "an edge",
    known: &["id", "source", "target", "handle"],
    deprecated: &[("sourceHandle", "handle"), ("source_handle", "handle")],
    reserved: &[],
};

const DECL_KEYS: KeyRules<'static> = KeyRules {
    what: "a variable declaration",
    known: &["name", "type", "value", "description"],
    deprecated: &[("value_type", "type")],
    reserved: &[],
};

const RETRY_KEYS: KeyRules<'static> =
    KeyRules { what: "`retry`", known: &["max_retries", "interval_ms", "backoff"], deprecated: &[], reserved: &[] };

const ON_ERROR_KEYS: KeyRules<'static> =
    KeyRules { what: "`on_error`", known: &["strategy", "values"], deprecated: &[], reserved: &[] };

const STRATEGIES: &[&str] = &["fail", "fail-branch", "default-value"];

/// Keys written by visual builders. Ignored (new tools should use `x-` keys).
const BUILDER_KEYS: &[&str] = &[
    "desc",
    "selected",
    "position",
    "positionAbsolute",
    "width",
    "height",
    "sourcePosition",
    "targetPosition",
    "zIndex",
    "isInIteration",
    "isInLoop",
];

/// Whether `key` is an extension key (`x-...`), free for tools and ignored.
pub fn is_extension_key(key: &str) -> bool {
    key.starts_with("x-")
}

/// Check the keys of a strict object. With `unknown_is_error` false only
/// deprecated and reserved keys are reported (for objects that mix fixed
/// fields with open config).
fn check_keys(
    obj: &serde_json::Map<String, serde_json::Value>,
    path: &DocPath,
    rules: &KeyRules<'_>,
    unknown_is_error: bool,
    out: &mut Vec<Diagnostic>,
) {
    for key in obj.keys() {
        let key_path = path.key(key);
        if is_extension_key(key) || rules.known.contains(&key.as_str()) {
            continue;
        }
        if let Some((_, new)) = rules.deprecated.iter().find(|(old, _)| old == key) {
            out.push(
                Diagnostic::warning("deprecated", key_path, format!("`{key}` in {} is deprecated", rules.what))
                    .at_key()
                    .with_suggestion(Some((*new).to_owned())),
            );
        } else if rules.reserved.contains(&key.as_str()) {
            out.push(
                Diagnostic::error(
                    "reserved-field",
                    key_path,
                    format!("`{key}` in {} is reserved for a future version of the format", rules.what),
                )
                .at_key()
                .with_help("keys starting with `x-` are free for tools and ignored"),
            );
        } else if unknown_is_error {
            out.push(
                Diagnostic::error("unknown-field", key_path, format!("unknown field `{key}` in {}", rules.what))
                    .at_key()
                    .with_suggestion(suggest(key, rules.known.iter().copied()))
                    .with_help(format!("expected one of: {}", rules.known.join(", "))),
            );
        }
    }
}

/// Decode a raw document, collecting every finding instead of stopping at the first.
pub(crate) fn decode(raw: serde_json::Value) -> Decoded {
    let mut out = Vec::new();
    let mut decoded = Decoded {
        workflow: Workflow { version: FORMAT_VERSION.to_owned(), ..Workflow::default() },
        node_paths: Vec::new(),
        edge_paths: Vec::new(),
        diagnostics: Vec::new(),
    };
    let serde_json::Value::Object(mut root) = raw else {
        out.push(Diagnostic::error("invalid-value", DocPath::root(), "a workflow document must be an object"));
        decoded.diagnostics = out;
        return decoded;
    };
    // Import layout: everything nested under `workflow`, app metadata under `app`.
    let import = matches!(root.get("workflow"), Some(serde_json::Value::Object(_))) || root.contains_key("app");
    let strict = !import;
    let mut inner_keys = std::collections::HashSet::new();
    if let Some(serde_json::Value::Object(inner)) = root.remove("workflow") {
        for (k, v) in inner {
            if !root.contains_key(&k) {
                inner_keys.insert(k.clone());
                root.insert(k, v);
            }
        }
    }
    let at = |key: &str| {
        if inner_keys.contains(key) { DocPath::root().key("workflow").key(key) } else { DocPath::root().key(key) }
    };
    if strict {
        check_keys(&root, &DocPath::root(), &TOP_LEVEL, true, &mut out);
    }

    let wf = &mut decoded.workflow;
    if strict {
        wf.version = check_version(root.get("version"), &at("version"), &mut out);
    } else if let Some(v) = str_field(&root, "version") {
        wf.version = v;
    }
    let app = root.get("app").and_then(|a| a.as_object());
    wf.name =
        text_field(&root, "name", &at("name"), strict, &mut out).or_else(|| app.and_then(|a| str_field(a, "name")));
    wf.description = text_field(&root, "description", &at("description"), strict, &mut out)
        .or_else(|| app.and_then(|a| str_field(a, "description")));

    for (field, legacy) in [("env", "environment_variables"), ("conversation", "conversation_variables")] {
        let (key, raw) = match root.remove(field) {
            Some(v) => (field, Some(v)),
            None => (legacy, root.remove(legacy)),
        };
        let decls = decode_decls(raw, &at(key), strict, &mut out);
        if field == "env" {
            wf.env = decls;
        } else {
            wf.conversation = decls;
        }
    }

    let graph_path = at("graph");
    let graph = match root.remove("graph") {
        Some(serde_json::Value::Object(g)) => g,
        Some(_) => {
            out.push(Diagnostic::error(
                "invalid-value",
                graph_path,
                "`graph` must be an object with `nodes` and `edges`",
            ));
            decoded.diagnostics = out;
            return decoded;
        }
        None => {
            out.push(
                Diagnostic::error("missing-field", DocPath::root(), "missing `graph`")
                    .with_help("a workflow needs `graph: {nodes: [...], edges: [...]}`"),
            );
            decoded.diagnostics = out;
            return decoded;
        }
    };
    if strict {
        check_keys(&graph, &graph_path, &GRAPH_KEYS, true, &mut out);
    }
    for (i, raw) in list_field(&graph, "nodes", &graph_path, &mut out).into_iter().enumerate() {
        let path = graph_path.key("nodes").index(i);
        if let Some(doc) = NodeDoc::decode(raw, &path, strict, &mut out) {
            wf.graph.nodes.push(doc);
            decoded.node_paths.push(path);
        }
    }
    for (i, raw) in list_field(&graph, "edges", &graph_path, &mut out).into_iter().enumerate() {
        let path = graph_path.key("edges").index(i);
        if let Some(edge) = decode_edge(raw, &path, strict, &mut out) {
            wf.graph.edges.push(edge);
            decoded.edge_paths.push(path);
        }
    }
    decoded.diagnostics = out;
    decoded
}

/// `version` of a v1 document: absent (defaults to `"1"`), `"1"` or `1`.
fn check_version(raw: Option<&serde_json::Value>, path: &DocPath, out: &mut Vec<Diagnostic>) -> String {
    match raw {
        None => FORMAT_VERSION.to_owned(),
        Some(serde_json::Value::String(s)) if s == FORMAT_VERSION => s.clone(),
        Some(serde_json::Value::Number(n)) if n.as_u64() == Some(1) => FORMAT_VERSION.to_owned(),
        Some(other) => {
            let shown = other.as_str().map_or_else(|| other.to_string(), str::to_owned);
            out.push(
                Diagnostic::error(
                    "unsupported-version",
                    path.clone(),
                    format!("unsupported document version `{shown}`; this build reads version \"{FORMAT_VERSION}\""),
                )
                .with_help("newer documents need a newer graphrt"),
            );
            shown
        }
    }
}

fn str_field(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

/// An optional string field; a non-string is an error in strict documents.
fn text_field(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &DocPath,
    strict: bool,
    out: &mut Vec<Diagnostic>,
) -> Option<String> {
    match map.get(key) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => {
            if strict {
                out.push(Diagnostic::error("invalid-value", path.clone(), format!("`{key}` must be a string")));
            }
            None
        }
    }
}

/// The items of an optional array field.
fn list_field(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    parent: &DocPath,
    out: &mut Vec<Diagnostic>,
) -> Vec<serde_json::Value> {
    match map.get(key) {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => items.clone(),
        Some(_) => {
            out.push(Diagnostic::error("invalid-value", parent.key(key), format!("`{key}` must be a list")));
            Vec::new()
        }
    }
}

fn decode_decls(
    raw: Option<serde_json::Value>,
    path: &DocPath,
    strict: bool,
    out: &mut Vec<Diagnostic>,
) -> Vec<VariableDecl> {
    let items = match raw {
        None | Some(serde_json::Value::Null) => return Vec::new(),
        Some(serde_json::Value::Array(items)) => items,
        Some(_) => {
            out.push(Diagnostic::error("invalid-value", path.clone(), "variable declarations must be a list"));
            return Vec::new();
        }
    };
    let mut decls: Vec<VariableDecl> = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let item_path = path.index(i);
        if strict && let serde_json::Value::Object(obj) = &item {
            check_keys(obj, &item_path, &DECL_KEYS, true, out);
        }
        match serde_json::from_value::<VariableDecl>(item) {
            Ok(decl) => {
                if decls.iter().any(|d| d.name == decl.name) {
                    out.push(Diagnostic::error(
                        "duplicate-variable",
                        item_path.key("name"),
                        format!("variable `{}` is declared twice", decl.name),
                    ));
                }
                decls.push(decl);
            }
            Err(e) => {
                out.push(Diagnostic::error("invalid-value", item_path, format!("invalid variable declaration: {e}")))
            }
        }
    }
    decls
}

fn decode_edge(raw: serde_json::Value, path: &DocPath, strict: bool, out: &mut Vec<Diagnostic>) -> Option<EdgeDoc> {
    let serde_json::Value::Object(mut obj) = raw else {
        out.push(Diagnostic::error("invalid-value", path.clone(), "an edge must be an object `{source, target}`"));
        return None;
    };
    if strict {
        check_keys(&obj, path, &EDGE_KEYS, true, out);
    }
    obj.retain(|k, _| !is_extension_key(k));
    let mut ok = true;
    for end in ["source", "target"] {
        match obj.get(end) {
            Some(serde_json::Value::String(s)) if !s.is_empty() => {}
            Some(_) => {
                out.push(Diagnostic::error("invalid-value", path.key(end), format!("edge `{end}` must be a node id")));
                ok = false;
            }
            None => {
                out.push(Diagnostic::error("missing-field", path.clone(), format!("edge is missing `{end}`")));
                ok = false;
            }
        }
    }
    if !ok {
        return None;
    }
    match serde_json::from_value::<EdgeDoc>(serde_json::Value::Object(obj)) {
        Ok(edge) => Some(edge),
        Err(e) => {
            out.push(Diagnostic::error("invalid-value", path.clone(), format!("invalid edge: {e}")));
            None
        }
    }
}

/// Keys handled by [`NodeDoc`] itself and therefore removed from `config`.
const POLICY_KEYS: &[&str] = &["on_error", "error_strategy", "default_value", "retry", "retry_config", "timeout_ms"];

impl NodeDoc {
    /// Normalize a raw node object. Returns `Ok(None)` for annotation-only nodes (notes).
    pub fn from_raw(raw: serde_json::Value) -> Result<Option<Self>, DocumentError> {
        let mut out = Vec::new();
        let doc = Self::decode(raw, &DocPath::root(), true, &mut out);
        match out.into_iter().find(Diagnostic::is_error) {
            Some(d) => Err(DocumentError::Node {
                id: d.node.unwrap_or_else(|| "<node>".to_owned()),
                message: match d.suggestion {
                    Some(s) => format!("{}; did you mean `{s}`?", d.message),
                    None => d.message,
                },
            }),
            None => Ok(doc),
        }
    }

    /// Decode one node at `path`, collecting findings. `None` for notes and for
    /// nodes too broken to keep (no usable `id` or `type`).
    fn decode(raw: serde_json::Value, path: &DocPath, strict_doc: bool, out: &mut Vec<Diagnostic>) -> Option<Self> {
        let serde_json::Value::Object(mut obj) = raw else {
            out.push(Diagnostic::error("invalid-value", path.clone(), "a node must be an object `{id, type, ...}`"));
            return None;
        };
        let id = match obj.remove("id") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => s,
            Some(_) => {
                out.push(Diagnostic::error("invalid-value", path.key("id"), "node `id` must be a non-empty string"));
                return None;
            }
            None => {
                out.push(Diagnostic::error("missing-field", path.clone(), "node is missing `id`"));
                return None;
            }
        };
        let wrapped = matches!(obj.get("data"), Some(serde_json::Value::Object(_)));
        let strict = strict_doc && !wrapped;
        if strict {
            let before = out.len();
            check_keys(&obj, path, &NODE_KEYS, false, out);
            for d in out.iter_mut().skip(before) {
                d.node = Some(id.clone());
            }
        }
        let legacy_parent = obj.remove("parentId").and_then(as_string);
        let mut parent = obj.remove("parent").and_then(as_string).or(legacy_parent);
        // Flatten the `data` wrapper layout.
        if let Some(serde_json::Value::Object(data)) = obj.remove("data") {
            for (k, v) in data {
                obj.insert(k, v);
            }
        }
        for key in ["container_id", "iteration_id", "loop_id"] {
            let v = obj.remove(key).and_then(as_string);
            if parent.is_none() {
                parent = v;
            }
        }
        let kind = match obj.remove("type") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => s,
            Some(_) => {
                out.push(
                    Diagnostic::error("invalid-value", path.key("type"), "node `type` must be a non-empty string")
                        .with_node(&id),
                );
                return None;
            }
            None => {
                out.push(Diagnostic::error("missing-field", path.clone(), "node is missing `type`").with_node(&id));
                return None;
            }
        };
        if kind == "custom-note" || kind == "note" {
            return None;
        }
        let title = match obj.remove("title") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s),
            None | Some(serde_json::Value::Null | serde_json::Value::String(_)) => None,
            Some(_) => {
                if strict {
                    out.push(
                        Diagnostic::error("invalid-value", path.key("title"), "node `title` must be a string")
                            .with_node(&id),
                    );
                }
                None
            }
        };
        let policy = extract_policy(&id, &mut obj, path, strict, out);
        for k in BUILDER_KEYS {
            obj.remove(*k);
        }
        obj.retain(|k, _| !is_extension_key(k));
        Some(Self { id, kind, title, parent, policy, config: obj, strict })
    }
}

fn as_string(v: serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

fn extract_policy(
    id: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
    path: &DocPath,
    strict: bool,
    out: &mut Vec<Diagnostic>,
) -> NodePolicy {
    let mut policy = NodePolicy::default();
    let mut push = |d: Diagnostic| out.push(d.with_node(id));

    if let Some(v) = obj.remove("on_error") {
        let at = path.key("on_error");
        if let serde_json::Value::Object(map) = &v
            && strict
        {
            let mut found = Vec::new();
            check_keys(map, &at, &ON_ERROR_KEYS, true, &mut found);
            found.into_iter().for_each(&mut push);
        }
        let strategy = v.get("strategy").and_then(serde_json::Value::as_str).map(str::to_owned);
        match (&strategy, serde_json::from_value::<ErrorStrategy>(v)) {
            (_, Ok(s)) => policy.on_error = s,
            (Some(name), Err(_)) if !STRATEGIES.contains(&name.as_str()) => push(
                Diagnostic::error("invalid-value", at.key("strategy"), format!("unknown `on_error` strategy `{name}`"))
                    .with_suggestion(suggest(name, STRATEGIES.iter().copied()))
                    .with_help("expected `fail`, `fail-branch` or `default-value`"),
            ),
            (_, Err(e)) => push(
                Diagnostic::error("invalid-value", at, format!("invalid `on_error`: {e}"))
                    .with_help("expected `{strategy: fail | fail-branch | default-value, values: {...}}`"),
            ),
        }
    } else if let Some(serde_json::Value::String(strategy)) = obj.remove("error_strategy") {
        policy.on_error = match strategy.as_str() {
            "fail-branch" => ErrorStrategy::FailBranch,
            "" | "none" | "fail" => ErrorStrategy::Fail,
            "default-value" => {
                // Accept `[{key, type, value}]` lists or plain maps.
                let values = match obj.remove("default_value") {
                    Some(serde_json::Value::Array(items)) => items
                        .into_iter()
                        .filter_map(|item| {
                            let key = item.get("key")?.as_str()?.to_owned();
                            let declared = item.get("type").and_then(|t| t.as_str()).unwrap_or("any").to_owned();
                            let value = item.get("value").cloned().unwrap_or(serde_json::Value::Null);
                            Some((key, default_value(&declared, value)))
                        })
                        .collect(),
                    Some(serde_json::Value::Object(map)) => {
                        map.into_iter().map(|(k, v)| (k, Value::from_json(v))).collect()
                    }
                    _ => IndexMap::new(),
                };
                ErrorStrategy::DefaultValue { values }
            }
            other => {
                push(
                    Diagnostic::error(
                        "invalid-value",
                        path.key("error_strategy"),
                        format!("unknown error_strategy `{other}` (expected `fail`, `fail-branch` or `default-value`)"),
                    )
                    .with_suggestion(suggest(other, STRATEGIES.iter().copied())),
                );
                ErrorStrategy::Fail
            }
        };
    }
    obj.remove("default_value");

    if let Some(v) = obj.remove("retry") {
        let at = path.key("retry");
        if let serde_json::Value::Object(map) = &v
            && strict
        {
            let mut found = Vec::new();
            check_keys(map, &at, &RETRY_KEYS, true, &mut found);
            found.into_iter().for_each(&mut push);
        }
        match serde_json::from_value(v) {
            Ok(retry) => policy.retry = retry,
            Err(e) => push(Diagnostic::error("invalid-value", at, format!("invalid `retry`: {e}"))),
        }
    } else if let Some(serde_json::Value::Object(cfg)) = obj.remove("retry_config") {
        let enabled = cfg.get("retry_enabled").and_then(serde_json::Value::as_bool).unwrap_or(false);
        if enabled {
            policy.retry.max_retries = cfg
                .get("max_retries")
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);
            policy.retry.interval_ms = cfg.get("retry_interval").and_then(serde_json::Value::as_u64).unwrap_or(0);
        }
    }
    match obj.remove("timeout_ms") {
        None | Some(serde_json::Value::Null) => {}
        Some(v) => match v.as_u64() {
            Some(ms) => policy.timeout_ms = Some(ms),
            None => push(Diagnostic::error(
                "invalid-value",
                path.key("timeout_ms"),
                format!("timeout_ms must be a non-negative integer number of milliseconds, got {v}"),
            )),
        },
    }
    for k in POLICY_KEYS {
        obj.remove(*k);
    }
    policy
}

/// Default values may arrive as JSON-encoded strings for non-string types.
fn default_value(declared: &str, value: serde_json::Value) -> Value {
    match (declared, value) {
        ("string", v) => Value::from_json(v),
        (_, serde_json::Value::String(s)) => match serde_json::from_slice::<serde_json::Value>(s.as_bytes()) {
            Ok(parsed) => Value::from_json(parsed),
            Err(_) => Value::from(s),
        },
        (_, v) => Value::from_json(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_layout() {
        let doc = Workflow::from_json(
            r#"{"graph":{"nodes":[{"id":"s","type":"start","variables":[]},
                {"id":"h","type":"http-request","retry":{"max_retries":2,"interval_ms":10},"timeout_ms":500}],
                "edges":[{"source":"s","target":"h"}]}}"#,
        );
        let doc = doc.unwrap();
        assert_eq!(doc.graph.nodes.len(), 2);
        let h = &doc.graph.nodes[1];
        assert_eq!(h.policy.retry.max_retries, 2);
        assert_eq!(h.policy.timeout_ms, Some(500));
        assert!(!h.config.contains_key("retry"));
    }

    #[test]
    fn parses_data_wrapper_layout() {
        let doc = Workflow::from_json(
            r#"{"workflow":{"environment_variables":[{"name":"K","value_type":"secret","value":"x"}],
              "graph":{"nodes":[
                {"id":"a","type":"custom","data":{"type":"llm","title":"LLM","error_strategy":"default-value",
                  "default_value":[{"key":"n","type":"number","value":"42"}],
                  "retry_config":{"retry_enabled":true,"max_retries":3,"retry_interval":100}}},
                {"id":"b","type":"custom","parentId":"it","data":{"type":"code"}},
                {"id":"note","type":"custom-note","data":{"type":"custom-note"}}],
              "edges":[{"source":"a","target":"b","sourceHandle":"source"}]}}}"#,
        );
        let doc = doc.unwrap();
        assert_eq!(doc.env.len(), 1);
        assert_eq!(doc.env[0].ty, ValueType::Secret);
        assert_eq!(doc.graph.nodes.len(), 2);
        let a = &doc.graph.nodes[0];
        assert_eq!(a.kind, "llm");
        assert_eq!(a.title.as_deref(), Some("LLM"));
        assert_eq!(a.policy.retry.max_retries, 3);
        match &a.policy.on_error {
            ErrorStrategy::DefaultValue { values } => assert_eq!(values.get("n"), Some(&Value::Int(42))),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(doc.graph.nodes[1].parent.as_deref(), Some("it"));
        assert_eq!(doc.graph.edges[0].handle.as_deref(), Some("source"));
    }

    #[test]
    fn backoff_delay() {
        let p = RetryPolicy { max_retries: 3, interval_ms: 100, backoff: 2.0 };
        assert_eq!(p.delay_ms(1), 100);
        assert_eq!(p.delay_ms(3), 400);
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn parses_yaml() {
        let doc = Workflow::parse("graph:\n  nodes:\n    - {id: s, type: start}\n  edges: []\n");
        assert!(matches!(doc, Ok(d) if d.graph.nodes.len() == 1));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn yaml_uses_strict_booleans() {
        let doc = Workflow::parse("graph:\n  nodes:\n    - {id: s, type: start, label: n, flag: true}\n").unwrap();
        let cfg = &doc.graph.nodes[0].config;
        assert_eq!(cfg.get("label"), Some(&serde_json::json!("n")));
        assert_eq!(cfg.get("flag"), Some(&serde_json::json!(true)));
    }
}

impl Serialize for NodeDoc {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as _, SerializeMap};
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("type", &self.kind)?;
        if let Some(title) = &self.title {
            map.serialize_entry("title", title)?;
        }
        if let Some(parent) = &self.parent {
            map.serialize_entry("parent", parent)?;
        }
        if self.policy.on_error != ErrorStrategy::Fail {
            map.serialize_entry("on_error", &self.policy.on_error)?;
        }
        if self.policy.retry != RetryPolicy::default() {
            map.serialize_entry("retry", &self.policy.retry)?;
        }
        if let Some(ms) = self.policy.timeout_ms {
            map.serialize_entry("timeout_ms", &ms)?;
        }
        for (key, value) in &self.config {
            if matches!(key.as_str(), "id" | "type" | "title" | "parent") || POLICY_KEYS.contains(&key.as_str()) {
                return Err(S::Error::custom(format!("node `{}`: config key `{key}` is reserved", self.id)));
            }
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl Workflow {
    /// Serialize to JSON in the same layout [`Workflow::from_json`] accepts.
    pub fn to_json(&self) -> Result<String, DocumentError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Canonical JSON of the graph section (object keys sorted recursively).
    /// Used for fingerprints, so key order in the source document does not matter.
    pub fn canonical_graph_json(&self) -> Result<String, DocumentError> {
        let value = serde_json::to_value(&self.graph)?;
        Ok(serde_json::to_string(&canonicalize(value))?)
    }
}

/// Recursively sort object keys.
fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(entries.into_iter().map(|(k, v)| (k, canonicalize(v))).collect())
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(items.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;

    const DOC: &str = r#"{"name":"demo","env":[{"name":"K","type":"secret"}],"graph":{"nodes":[
        {"id":"s","type":"start","variables":[]},
        {"id":"h","type":"custom","data":{"type":"http-request","title":"Call","url":"https://x",
          "error_strategy":"default-value","default_value":{"body":"none"},
          "retry_config":{"retry_enabled":true,"max_retries":2,"retry_interval":10},"timeout_ms":500}},
        {"id":"b","type":"code","parentId":"it"}],
        "edges":[{"source":"s","target":"h","sourceHandle":"source"}]}}"#;

    #[test]
    fn workflow_round_trips() {
        let a = Workflow::from_json(DOC).unwrap();
        let text = a.to_json().unwrap();
        let b = Workflow::from_json(&text).unwrap();
        assert_eq!(text, b.to_json().unwrap());
        let h = &b.graph.nodes[1];
        assert_eq!(h.kind, "http-request");
        assert_eq!(h.title.as_deref(), Some("Call"));
        assert_eq!(h.policy, a.graph.nodes[1].policy);
        assert_eq!(h.policy.timeout_ms, Some(500));
        assert_eq!(h.config.get("url"), Some(&serde_json::json!("https://x")));
        assert!(!h.config.contains_key("config"));
        assert_eq!(b.graph.nodes[2].parent.as_deref(), Some("it"));
        assert_eq!(a.canonical_graph_json().unwrap(), b.canonical_graph_json().unwrap());
    }

    #[test]
    fn canonical_json_ignores_key_order() {
        let a = Workflow::from_json(r#"{"graph":{"nodes":[{"id":"s","type":"start","x":1,"y":2}],"edges":[]}}"#);
        let b = Workflow::from_json(r#"{"graph":{"nodes":[{"y":2,"x":1,"type":"start","id":"s"}],"edges":[]}}"#);
        assert_eq!(a.unwrap().canonical_graph_json().unwrap(), b.unwrap().canonical_graph_json().unwrap());
    }

    #[test]
    fn typos_are_errors() {
        let bad_strategy = r#"{"graph":{"nodes":[{"id":"s","type":"start","error_strategy":"fail-brnch"}]}}"#;
        assert!(matches!(Workflow::from_json(bad_strategy), Err(DocumentError::Node { .. })));
        let bad_timeout = r#"{"graph":{"nodes":[{"id":"s","type":"start","timeout_ms":"5s"}]}}"#;
        assert!(matches!(Workflow::from_json(bad_timeout), Err(DocumentError::Node { .. })));
        let none = r#"{"graph":{"nodes":[{"id":"s","type":"start","error_strategy":"none"}]}}"#;
        assert!(Workflow::from_json(none).is_ok());
    }
}
