//! Workflow documents: the portable, serializable description of a workflow.
//!
//! A document is plain data (JSON or YAML). It is turned into an executable
//! [`crate::graph::Graph`] by [`crate::graph::Graph::build`] using a
//! [`crate::node::Registry`].
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
//! For interoperability the loader also accepts the common "node with a `data`
//! object" layout (`{id, parentId, data: {type, title, ...}}`), camelCase edge
//! handles (`sourceHandle`) and documents wrapped in a top-level `workflow` key.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::value::{Value, ValueType};

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
}

impl NodeDoc {
    /// Minimal constructor, mainly for programmatic graph building and tests.
    pub fn new(id: impl Into<String>, kind: impl Into<String>, config: serde_json::Value) -> Self {
        let config = match config {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        Self { id: id.into(), kind: kind.into(), title: None, parent: None, policy: NodePolicy::default(), config }
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
        serde_json::from_value(serde_json::Value::Object(self.config.clone()))
            .map_err(|e| DocumentError::Node { id: self.id.clone(), message: e.to_string() })
    }

    /// Human-readable label.
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }
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
        let raw: serde_json::Value = serde_json::from_str(text)?;
        Self::from_value(raw)
    }

    /// Parse a YAML document.
    #[cfg(feature = "yaml")]
    pub fn from_yaml(text: &str) -> Result<Self, DocumentError> {
        // YAML 1.2 booleans only: `yes`, `no`, `y`, `n`, `on`, `off` stay strings.
        let options = serde_saphyr::options! { strict_booleans: true };
        let raw: serde_json::Value =
            serde_saphyr::from_str_with_options(text, options).map_err(|e| DocumentError::Yaml(e.to_string()))?;
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

    /// Build from an already-decoded JSON value.
    pub fn from_value(raw: serde_json::Value) -> Result<Self, DocumentError> {
        let mut root = match raw {
            serde_json::Value::Object(m) => m,
            _ => return Err(node_err("<document>", "document must be an object")),
        };
        // Documents may nest everything under `workflow`.
        if let Some(serde_json::Value::Object(inner)) = root.remove("workflow") {
            for (k, v) in inner {
                root.entry(k).or_insert(v);
            }
        }
        let app = root.get("app").and_then(|a| a.as_object());
        let name = str_field(&root, "name").or_else(|| app.and_then(|a| str_field(a, "name")));
        let description = str_field(&root, "description").or_else(|| app.and_then(|a| str_field(a, "description")));
        let version = str_field(&root, "version").unwrap_or_else(|| "1".to_owned());

        let env = decls(root.remove("env").or_else(|| root.remove("environment_variables")))?;
        let conversation = decls(root.remove("conversation").or_else(|| root.remove("conversation_variables")))?;

        let graph = match root.remove("graph") {
            Some(serde_json::Value::Object(g)) => g,
            _ => return Err(node_err("<document>", "missing `graph` object")),
        };
        let nodes = match graph.get("nodes") {
            Some(serde_json::Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        let edges = match graph.get("edges") {
            Some(e) => serde_json::from_value::<Vec<EdgeDoc>>(e.clone())?,
            None => Vec::new(),
        };
        let mut docs = Vec::with_capacity(nodes.len());
        for raw in nodes {
            if let Some(doc) = NodeDoc::from_raw(raw)? {
                docs.push(doc);
            }
        }
        Ok(Self { version, name, description, env, conversation, graph: GraphDoc { nodes: docs, edges } })
    }
}

fn str_field(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

fn node_err(id: &str, message: &str) -> DocumentError {
    DocumentError::Node { id: id.to_owned(), message: message.to_owned() }
}

fn decls(raw: Option<serde_json::Value>) -> Result<Vec<VariableDecl>, DocumentError> {
    match raw {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(v) => Ok(serde_json::from_value(v)?),
    }
}

/// Keys handled by [`NodeDoc`] itself and therefore removed from `config`.
const POLICY_KEYS: &[&str] =
    &["on_error", "error_strategy", "default_value", "retry", "retry_config", "timeout_ms", "timeout"];

impl NodeDoc {
    /// Normalize a raw node object. Returns `Ok(None)` for annotation-only nodes (notes).
    pub fn from_raw(raw: serde_json::Value) -> Result<Option<Self>, DocumentError> {
        let serde_json::Value::Object(mut obj) = raw else {
            return Err(node_err("<node>", "node must be an object"));
        };
        let id = match obj.remove("id") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => s,
            _ => return Err(node_err("<node>", "node `id` must be a non-empty string")),
        };
        let mut parent = obj.remove("parentId").or_else(|| obj.remove("parent")).and_then(as_string);
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
            _ => return Err(node_err(&id, "node `type` must be a non-empty string")),
        };
        if kind == "custom-note" || kind == "note" {
            return Ok(None);
        }
        let title = obj.remove("title").and_then(as_string);
        let policy = extract_policy(&id, &mut obj)?;
        for k in [
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
        ] {
            obj.remove(k);
        }
        Ok(Some(Self { id, kind, title, parent, policy, config: obj }))
    }
}

fn as_string(v: serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

fn extract_policy(id: &str, obj: &mut serde_json::Map<String, serde_json::Value>) -> Result<NodePolicy, DocumentError> {
    let mut policy = NodePolicy::default();
    let bad =
        |what: &str, e: serde_json::Error| DocumentError::Node { id: id.to_owned(), message: format!("{what}: {e}") };

    if let Some(v) = obj.remove("on_error") {
        policy.on_error = serde_json::from_value(v).map_err(|e| bad("on_error", e))?;
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
                return Err(DocumentError::Node {
                    id: id.to_owned(),
                    message: format!(
                        "unknown error_strategy `{other}` (expected `fail`, `fail-branch` or `default-value`)"
                    ),
                });
            }
        };
    }
    obj.remove("default_value");

    if let Some(v) = obj.remove("retry") {
        policy.retry = serde_json::from_value(v).map_err(|e| bad("retry", e))?;
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
            None => {
                return Err(DocumentError::Node {
                    id: id.to_owned(),
                    message: format!("timeout_ms must be a non-negative integer number of milliseconds, got {v}"),
                });
            }
        },
    }
    for k in POLICY_KEYS {
        obj.remove(*k);
    }
    Ok(policy)
}

/// Default values may arrive as JSON-encoded strings for non-string types.
fn default_value(declared: &str, value: serde_json::Value) -> Value {
    match (declared, value) {
        ("string", v) => Value::from_json(v),
        (_, serde_json::Value::String(s)) => match serde_json::from_str::<serde_json::Value>(&s) {
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
            entries.sort_by(|a, b| a.0.cmp(&b.0));
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
