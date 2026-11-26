//! JSON Schema (draft 2020-12) of a whole v1 workflow document.
//!
//! Built from the node catalog: every node kind becomes one `oneOf` branch
//! keyed on `type`, combining the fields every node has (`id`, `title`,
//! `parent`, `retry`, `timeout_ms`, `on_error`) with the kind's config schema.
//! The checked-in `schema/workflow-v1.schema.json` is this schema for the
//! default build; `graphrt schema` prints it for the running build, including
//! plugin node kinds.
//!
//! The schema describes the strict v1 layout only. The import layout (nodes
//! wrapped in `data`, documents under a `workflow` key) is accepted by the
//! loader but not by the schema.

use serde_json::{Map, Value, json};

use crate::catalog::NodeDescriptor;
use crate::catalog::schema::{DRAFT, selector, value_type};

/// Canonical location of the published schema.
pub const SCHEMA_ID: &str = "https://raw.githubusercontent.com/Graphrt/graphrt/main/schema/workflow-v1.schema.json";

/// The document schema for these node kinds (usually `Registry::descriptors()`).
pub fn workflow_schema(descriptors: &[NodeDescriptor]) -> Value {
    let mut defs = Map::new();
    defs.insert("selector".into(), selector());
    defs.insert("variable".into(), variable_def());
    defs.insert("retry".into(), retry_def());
    defs.insert("on_error".into(), on_error_def());
    defs.insert("edge".into(), edge_def());
    let mut branches = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        let name = format!("node:{}", descriptor.kind);
        branches.push(json!({ "$ref": format!("#/$defs/{name}") }));
        defs.insert(name, node_def(descriptor));
    }
    defs.insert(
        "node".into(),
        json!({
            "description": "A node; `type` selects the node kind and its config fields",
            "type": "object",
            "required": ["id", "type"],
            "properties": { "type": { "enum": descriptors.iter().map(|d| d.kind.as_str()).collect::<Vec<_>>() } },
            "oneOf": branches
        }),
    );
    let decls = json!({ "type": "array", "items": { "$ref": "#/$defs/variable" } });
    json!({
        "$schema": DRAFT,
        "$id": SCHEMA_ID,
        "title": "graphrt workflow document (format version 1)",
        "description": "A graphrt workflow. Keys starting with `x-` are free for tools and ignored by the runtime.",
        "type": "object",
        "required": ["graph"],
        "properties": {
            "version": {
                "description": "Document format version. Omitted means \"1\".",
                "enum": ["1", 1]
            },
            "name": { "type": "string" },
            "description": { "type": "string" },
            "env": {
                "description": "Environment variables, read as {{#env.NAME#}}",
                "type": "array", "items": { "$ref": "#/$defs/variable" }
            },
            "conversation": {
                "description": "Conversation variables, read as {{#conversation.NAME#}}",
                "type": "array", "items": { "$ref": "#/$defs/variable" }
            },
            "environment_variables": deprecated(decls.clone(), "env"),
            "conversation_variables": deprecated(decls, "conversation"),
            "graph": {
                "type": "object",
                "required": ["nodes"],
                "properties": {
                    "nodes": { "type": "array", "items": { "$ref": "#/$defs/node" } },
                    "edges": { "type": "array", "items": { "$ref": "#/$defs/edge" } }
                },
                "patternProperties": extension_keys(),
                "additionalProperties": false
            }
        },
        "patternProperties": extension_keys(),
        "additionalProperties": false,
        "$defs": defs
    })
}

fn extension_keys() -> Value {
    json!({ "^x-": { "description": "Extension data for tools; ignored by the runtime" } })
}

fn deprecated(schema: Value, replacement: &str) -> Value {
    crate::catalog::schema::deprecated_alias(schema, replacement)
}

fn variable_def() -> Value {
    json!({
        "type": "object",
        "required": ["name"],
        "properties": {
            "name": { "type": "string", "minLength": 1 },
            "type": value_type(),
            "value": { "description": "Default value; omit to have the host supply it" },
            "description": { "type": "string" },
            "value_type": deprecated(value_type(), "type")
        },
        "patternProperties": extension_keys(),
        "additionalProperties": false
    })
}

fn retry_def() -> Value {
    json!({
        "description": "Retries of retryable errors; applies to every node kind",
        "type": "object",
        "properties": {
            "max_retries": { "type": "integer", "minimum": 0, "default": 0 },
            "interval_ms": { "type": "integer", "minimum": 0, "default": 0, "description": "Delay before the first retry" },
            "backoff": { "type": "number", "minimum": 1, "default": 1, "description": "Delay multiplier per retry" }
        },
        "additionalProperties": false
    })
}

fn on_error_def() -> Value {
    json!({
        "description": "What happens when the node fails after its retries",
        "type": "object",
        "required": ["strategy"],
        "properties": {
            "strategy": { "enum": ["fail", "fail-branch", "default-value"] },
            "values": { "type": "object", "description": "Outputs used with `default-value`" }
        },
        "additionalProperties": false
    })
}

fn edge_def() -> Value {
    json!({
        "type": "object",
        "required": ["source", "target"],
        "properties": {
            "id": { "type": "string", "minLength": 1 },
            "source": { "type": "string", "minLength": 1, "description": "Source node id" },
            "target": { "type": "string", "minLength": 1, "description": "Target node id" },
            "handle": {
                "type": "string",
                "default": "source",
                "description": "Branch of the source node: `source`, `fail-branch`, or a branch node's handle (case id, class id, action id...)"
            },
            "sourceHandle": deprecated(json!({ "type": "string" }), "handle"),
            "source_handle": deprecated(json!({ "type": "string" }), "handle")
        },
        "patternProperties": extension_keys(),
        "additionalProperties": false
    })
}

/// Fields every node has.
fn common_properties(kind: &str) -> Map<String, Value> {
    let parent_alias = || deprecated(json!({ "type": "string" }), "parent");
    let props = json!({
        "id": {
            "type": "string",
            "description": "Unique node id; also the namespace of the node's outputs",
            "pattern": "^[^.\\s]+$",
            "not": { "enum": ["sys", "env", "conversation"] }
        },
        "type": { "const": kind },
        "title": { "type": "string" },
        "parent": { "type": "string", "description": "Owning iteration / loop node for nodes in a container body" },
        "retry": { "$ref": "#/$defs/retry" },
        "timeout_ms": { "type": "integer", "minimum": 0, "description": "Per-attempt timeout" },
        "on_error": { "$ref": "#/$defs/on_error" },
        "parentId": parent_alias(),
        "container_id": parent_alias(),
        "iteration_id": parent_alias(),
        "loop_id": parent_alias(),
        "error_strategy": deprecated(json!({ "enum": ["none", "fail", "fail-branch", "default-value"] }), "on_error"),
        "default_value": deprecated(json!({}), "on_error"),
        "retry_config": deprecated(json!({ "type": "object" }), "retry")
    });
    match props {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// One `oneOf` branch: common node fields merged with the kind's config schema.
fn node_def(descriptor: &NodeDescriptor) -> Value {
    let mut config = match &descriptor.config_schema {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    config.remove("$schema");
    let mut required = vec![json!("id"), json!("type")];
    let mut def = Map::new();
    def.insert("title".into(), json!(descriptor.title));
    if !descriptor.description.is_empty() {
        def.insert("description".into(), json!(descriptor.description));
    }
    def.insert("type".into(), json!("object"));
    let mut properties = common_properties(&descriptor.kind);
    let mergeable = config.keys().all(|k| {
        matches!(k.as_str(), "type" | "properties" | "required" | "additionalProperties" | "description" | "title")
    });
    if mergeable {
        if let Some(Value::Object(config_props)) = config.remove("properties") {
            for (key, schema) in config_props {
                properties.entry(key).or_insert(schema);
            }
        }
        if let Some(Value::Array(config_required)) = config.remove("required") {
            required.extend(config_required);
        }
        def.insert("properties".into(), Value::Object(properties));
        def.insert("required".into(), Value::Array(required));
        def.insert("patternProperties".into(), extension_keys());
        if let Some(extra) = config.remove("additionalProperties") {
            def.insert("additionalProperties".into(), extra);
        }
    } else {
        // A schema using composition keywords: keep it whole (open node object).
        def.insert("properties".into(), Value::Object(properties));
        def.insert("required".into(), Value::Array(required));
        def.insert("allOf".into(), json!([Value::Object(config)]));
    }
    Value::Object(def)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::Registry;

    #[test]
    fn merges_node_kinds() {
        let schema = workflow_schema(&Registry::with_builtins().descriptors());
        let start = &schema["$defs"]["node:start"];
        assert_eq!(start["properties"]["type"]["const"], "start");
        assert_eq!(start["additionalProperties"], false);
        assert!(start["properties"]["variables"].is_object());
        assert!(start["properties"]["on_error"].is_object());
        assert!(schema["$defs"]["node"]["oneOf"].as_array().is_some_and(|b| b.len() > 10));
        assert!(start.get("$schema").is_none());
    }

    #[test]
    fn open_plugin_schemas_stay_open() {
        let d = NodeDescriptor::new("acme", "Acme", crate::catalog::NodeCategory::Other)
            .schema(json!({"type": "object", "properties": {"x": {"type": "string"}}}));
        let schema = workflow_schema(&[d]);
        let node = &schema["$defs"]["node:acme"];
        assert!(node.get("additionalProperties").is_none());
        assert_eq!(node["properties"]["x"]["type"], "string");
    }
}
