//! Node catalog: machine-readable descriptions of node kinds.
//!
//! Every node kind can publish a [`NodeDescriptor`] (title, category, config
//! JSON Schema, outputs, branch handles). A visual builder uses the catalog to
//! render palettes and config forms, and to validate documents before they
//! reach a runtime. Plugins publish descriptors through their manifests, so a
//! plugin's nodes appear in the catalog exactly like built-in ones.

use serde::{Deserialize, Serialize};

use crate::value::ValueType;

/// Broad grouping used by builders to organize the node palette.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum NodeCategory {
    /// Entry / exit / answer nodes.
    Io,
    /// Branching, loops, iteration.
    Logic,
    /// Data transformation (templates, assigners, list operations).
    Transform,
    /// Model-backed nodes (LLM, classifier, extractor, agent).
    Model,
    /// Knowledge / retrieval nodes.
    Knowledge,
    /// External integrations (HTTP, tools, code).
    Integration,
    /// Human-in-the-loop nodes.
    Human,
    /// Anything else, including most plugin nodes.
    #[default]
    Other,
}

/// How a node selects outgoing edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Handles {
    /// Every outgoing edge uses the default `source` handle.
    #[default]
    Single,
    /// A fixed set of handles.
    Fixed { handles: Vec<String> },
    /// Handles are derived from configuration (e.g. if-else case ids, classifier class ids).
    /// `config_path` points at the config field listing them, for builders.
    Dynamic {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_path: Option<String>,
    },
}

/// One declared output of a node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputDescriptor {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: ValueType,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Whether this output is streamed incrementally (`node_chunk` events).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub streaming: bool,
}

impl OutputDescriptor {
    pub fn new(name: impl Into<String>, ty: ValueType) -> Self {
        Self { name: name.into(), ty, description: String::new(), streaming: false }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn streaming(mut self) -> Self {
        self.streaming = true;
        self
    }
}

/// Description of a node kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeDescriptor {
    /// Node `type` used in workflow documents.
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default)]
    pub category: NodeCategory,
    /// JSON Schema of the node's config object (the node fields other than
    /// id/type/title/parent/policy). `{}` when unspecified.
    #[serde(default = "empty_schema")]
    pub config_schema: serde_json::Value,
    /// Static outputs. Nodes whose outputs depend on config (start, end, code...)
    /// list none and set `dynamic_outputs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputDescriptor>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dynamic_outputs: bool,
    #[serde(default)]
    pub handles: Handles,
    /// Whether the node owns a child scope (iteration, loop).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub container: bool,
    /// Short icon hint (e.g. `brain`, `globe`, `code`), purely cosmetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Origin: `builtin`, the crate name, or `plugin:<name>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

fn empty_schema() -> serde_json::Value {
    serde_json::json!({})
}

impl NodeDescriptor {
    pub fn new(kind: impl Into<String>, title: impl Into<String>, category: NodeCategory) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            description: String::new(),
            category,
            config_schema: empty_schema(),
            outputs: Vec::new(),
            dynamic_outputs: false,
            handles: Handles::Single,
            container: false,
            icon: None,
            source: String::new(),
        }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn schema(mut self, schema: serde_json::Value) -> Self {
        self.config_schema = schema;
        self
    }

    pub fn output(mut self, output: OutputDescriptor) -> Self {
        self.outputs.push(output);
        self
    }

    pub fn dynamic_outputs(mut self) -> Self {
        self.dynamic_outputs = true;
        self
    }

    pub fn handles(mut self, handles: Handles) -> Self {
        self.handles = handles;
        self
    }

    pub fn container(mut self) -> Self {
        self.container = true;
        self
    }

    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Minimal descriptor used for kinds registered without one.
    pub fn bare(kind: &str) -> Self {
        Self::new(kind, kind, NodeCategory::Other)
    }
}
