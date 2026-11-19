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
///
/// Unknown keys are rejected so a typo in a plugin manifest (`streming`) fails at
/// load time instead of being ignored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
///
/// Unknown keys are rejected: a misspelled `config_schema` or `handles` in a
/// plugin manifest would otherwise silently change how the node behaves.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// Whether this is a bare (placeholder) descriptor without real metadata.
    pub fn is_bare(&self) -> bool {
        self.title == self.kind && self.description.is_empty() && self.source.is_empty()
    }
}

/// Reusable JSON Schema fragments for node config schemas.
pub mod schema {
    use serde_json::{Value, json};

    /// Draft used by every config schema.
    pub const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

    /// Top-level config schema with `properties` and `required`. The schema is
    /// closed (`additionalProperties: false`): workflow validation reports any
    /// other key as an unknown field. Builder metadata belongs in `x-` keys,
    /// which are stripped before configs are checked. Use [`open_object`] for
    /// configs that legitimately take arbitrary keys.
    pub fn object(properties: Value, required: &[&str]) -> Value {
        json!({
            "$schema": DRAFT,
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    }

    /// Like [`object`], but other keys are allowed and not reported.
    pub fn open_object(properties: Value, required: &[&str]) -> Value {
        json!({ "$schema": DRAFT, "type": "object", "properties": properties, "required": required })
    }

    /// A deprecated alias of `replacement`, accepted with a warning.
    pub fn deprecated_alias(schema: Value, replacement: &str) -> Value {
        let mut schema = schema;
        if let Value::Object(map) = &mut schema {
            map.insert("deprecated".into(), Value::Bool(true));
            map.insert("description".into(), Value::String(format!("Deprecated alias of `{replacement}`")));
        }
        schema
    }

    /// Nested object schema (no `$schema` key).
    pub fn nested(properties: Value, required: &[&str]) -> Value {
        json!({ "type": "object", "properties": properties, "required": required })
    }

    /// A variable selector: `["namespace", "name", ...]` or `"namespace.name"`.
    pub fn selector() -> Value {
        json!({
            "description": "Variable selector: [namespace, name, path...] or \"namespace.name\"",
            "oneOf": [
                { "type": "array", "items": { "type": "string", "minLength": 1 }, "minItems": 2 },
                { "type": "string", "pattern": "^[^.\\s]+(\\.[^.\\s]+)+$" }
            ]
        })
    }

    /// A text template that may contain `{{#namespace.name#}}` placeholders.
    pub fn template(description: &str) -> Value {
        json!({ "type": "string", "description": description })
    }

    /// Declared value type names (including form-field aliases).
    pub fn value_type() -> Value {
        json!({
            "type": "string",
            "enum": [
                "any", "none", "string", "secret", "number", "integer", "float", "boolean", "object", "file",
                "array[any]", "array[string]", "array[number]", "array[boolean]", "array[object]", "array[file]",
                "array", "file-list", "json", "text-input", "paragraph", "select", "checkbox"
            ]
        })
    }

    /// `and` / `or`.
    pub fn logical_operator() -> Value {
        json!({ "type": "string", "enum": ["and", "or"], "default": "and" })
    }

    /// Comparison operator names (canonical spellings and ASCII aliases).
    pub fn comparison_operator() -> Value {
        json!({
            "type": "string",
            "enum": [
                "contains", "not contains", "start with", "starts with", "start_with", "starts_with",
                "end with", "ends with", "end_with", "ends_with", "is", "is not", "is_not",
                "empty", "not empty", "not_empty", "in", "not in", "not_in", "all of", "all_of",
                "=", "==", "eq", "≠", "!=", "ne", ">", "gt", "<", "lt", "≥", ">=", "ge", "≤", "<=", "le",
                "null", "is null", "not null", "is not null", "not_null", "exists", "not exists", "not_exists"
            ]
        })
    }

    /// Condition on an attribute (`key`) of list items or files.
    pub fn key_condition() -> Value {
        nested(
            json!({
                "key": { "type": "string" },
                "comparison_operator": comparison_operator(),
                "value": {}
            }),
            &["comparison_operator"],
        )
    }

    /// One condition: `{variable_selector, comparison_operator, value, sub_variable_condition}`.
    pub fn condition() -> Value {
        nested(
            json!({
                "variable_selector": selector(),
                "comparison_operator": comparison_operator(),
                "value": { "description": "Right-hand side; strings may contain placeholders" },
                "sub_variable_condition": nested(
                    json!({
                        "logical_operator": logical_operator(),
                        "conditions": { "type": "array", "items": key_condition() }
                    }),
                    &[]
                )
            }),
            &["variable_selector", "comparison_operator"],
        )
    }

    /// Array of [`condition`]s.
    pub fn conditions() -> Value {
        json!({ "type": "array", "items": condition() })
    }

    /// `[{variable, value_selector}]` bindings.
    pub fn variable_bindings() -> Value {
        json!({
            "type": "array",
            "items": closed(nested(
                json!({ "variable": { "type": "string", "minLength": 1 }, "value_selector": selector() }),
                &["variable", "value_selector"]
            ))
        })
    }

    /// `schema` with `additionalProperties: false`.
    pub fn closed(schema: Value) -> Value {
        let mut schema = schema;
        if let Value::Object(map) = &mut schema {
            map.insert("additionalProperties".into(), Value::Bool(false));
        }
        schema
    }

    /// Model reference used by model-backed nodes.
    pub fn model() -> Value {
        nested(
            json!({
                "provider": { "type": "string", "minLength": 1 },
                "name": { "type": "string", "minLength": 1 },
                "mode": { "type": "string", "enum": ["chat", "completion"] },
                "completion_params": { "type": "object" }
            }),
            &["provider", "name"],
        )
    }
}
