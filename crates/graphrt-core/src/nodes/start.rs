//! `start`: entry point of the root graph.
//!
//! The engine places run inputs under the start node's own namespace before
//! the node runs. The node validates them against the declared variables
//! (required, type, select options, max length), applies defaults and outputs
//! exactly the declared variables. System, environment and conversation
//! variables are never copied into its outputs.

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::dsl::NodeDoc;
use crate::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult, NodeRole};
use crate::pool::Selector;
use crate::value::{Value, ValueType};

/// `start` configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct StartConfig {
    #[serde(default)]
    pub variables: Vec<InputVariable>,
}

/// A declared run input.
#[derive(Clone, Debug, Deserialize)]
pub struct InputVariable {
    pub variable: String,
    /// Display label (informational; accepted for document compatibility).
    #[serde(default)]
    #[allow(dead_code)]
    pub label: Option<String>,
    /// Input kind: `text-input`, `paragraph`, `select`, `number`, `checkbox`,
    /// `file`, `file-list`, `json`, or any value type name (`string`, `integer`...).
    #[serde(rename = "type", default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub max_length: Option<usize>,
}

fn default_kind() -> String {
    "text-input".to_owned()
}

impl InputVariable {
    fn value_type(&self) -> Result<ValueType, String> {
        let ty = match self.kind.as_str() {
            "text-input" | "paragraph" | "select" | "string" | "text" => ValueType::String,
            "number" => ValueType::Number,
            "checkbox" | "boolean" | "bool" => ValueType::Boolean,
            "file" => ValueType::File,
            "file-list" => ValueType::ArrayFile,
            // `json` accepts any JSON document (object, array, scalar); `object` only objects.
            "json" => ValueType::Any,
            "json_object" | "object" => ValueType::Object,
            other => serde_json::from_value::<ValueType>(serde_json::Value::String(other.to_owned()))
                .map_err(|_| format!("variable `{}` has unknown type `{other}`", self.variable))?,
        };
        Ok(ty)
    }
}

#[derive(Debug)]
struct Declared {
    spec: InputVariable,
    ty: ValueType,
}

/// The `start` node.
#[derive(Debug)]
pub struct StartNode {
    variables: Vec<Declared>,
}

impl StartNode {
    pub fn new(doc: &NodeDoc, config: StartConfig) -> Result<Self, ConfigError> {
        let mut variables = Vec::with_capacity(config.variables.len());
        for spec in config.variables {
            let ty = spec.value_type().map_err(|message| ConfigError::new(doc, message))?;
            variables.push(Declared { spec, ty });
        }
        Ok(Self { variables })
    }

    fn validate(&self, ctx: &NodeContext) -> Result<NodeOutput, NodeError> {
        let mut out = NodeOutput::new();
        for declared in &self.variables {
            let spec = &declared.spec;
            let selector = Selector::of(&ctx.node_id, &spec.variable);
            let raw = ctx.get(&selector).unwrap_or(Value::Null);
            let raw = if is_blank(&raw) { spec.default.clone().unwrap_or(Value::Null) } else { raw };
            if is_blank(&raw) {
                if spec.required {
                    return Err(NodeError::invalid_input(format!("input `{}` is required", spec.variable)));
                }
                out.outputs.insert(spec.variable.clone(), Value::Null);
                continue;
            }
            let raw = match (&raw, spec.kind.as_str()) {
                (Value::String(s), "json") => serde_json::from_str::<serde_json::Value>(s)
                    .map(Value::from_json)
                    .map_err(|e| NodeError::invalid_input(format!("input `{}`: invalid JSON: {e}", spec.variable)))?,
                _ => raw,
            };
            let value = coerce_input(declared.ty, raw)
                .map_err(|e| NodeError::invalid_input(format!("input `{}`: {e}", spec.variable)))?;
            if !spec.options.is_empty() && spec.kind == "select" {
                let text = value.to_text_exposed();
                if !spec.options.contains(&text) {
                    return Err(NodeError::invalid_input(format!(
                        "input `{}` must be one of {:?}",
                        spec.variable, spec.options
                    )));
                }
            }
            if let (Some(max), Some(text)) = (spec.max_length, value.expose_secret())
                && text.chars().count() > max
            {
                return Err(NodeError::invalid_input(format!("input `{}` exceeds max length {max}", spec.variable)));
            }
            out.outputs.insert(spec.variable.clone(), value);
        }
        Ok(out)
    }
}

fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// Coerce a raw input to the declared type, accepting the textual forms a form
/// submission produces (numbers, booleans and JSON as strings).
fn coerce_input(ty: ValueType, raw: Value) -> Result<Value, String> {
    let converted = match (ty, &raw) {
        (ValueType::Number | ValueType::Integer | ValueType::Float, Value::String(s)) => {
            let s = s.trim();
            match s.parse::<i64>() {
                Ok(i) => Value::Int(i),
                Err(_) => s.parse::<f64>().map(Value::Float).map_err(|_| format!("`{s}` is not a number"))?,
            }
        }
        (ValueType::Boolean, Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Value::Bool(true),
            "false" | "0" | "no" | "off" => Value::Bool(false),
            other => return Err(format!("`{other}` is not a boolean")),
        },
        (
            ValueType::Object | ValueType::ArrayAny | ValueType::ArrayObject | ValueType::ArrayString,
            Value::String(s),
        ) => {
            let parsed: serde_json::Value = serde_json::from_str(s).map_err(|e| format!("invalid JSON: {e}"))?;
            Value::from_json(parsed)
        }
        // A single file submitted for a file list becomes a one-element list.
        (ValueType::ArrayFile, Value::File(_)) => Value::array([raw.clone()]),
        _ => raw,
    };
    ty.coerce(converted).map_err(|e| e.to_string())
}

impl Node for StartNode {
    fn role(&self) -> NodeRole {
        NodeRole::Root
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move { self.validate(&ctx).map(Into::into) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutcome;
    use crate::pool::VariablePool;
    use serde_json::json;
    use std::sync::Arc;

    fn node(vars: serde_json::Value) -> StartNode {
        let doc = NodeDoc::new("start", "start", json!({ "variables": vars }));
        StartNode::new(&doc, doc.parse_config().unwrap()).unwrap()
    }

    async fn run(node: &StartNode, inputs: &[(&str, Value)]) -> NodeResult {
        let mut pool = VariablePool::new();
        for (k, v) in inputs {
            pool.insert("start", k, v.clone());
        }
        pool.insert("env", "KEY", Value::Secret(Arc::from("s")));
        pool.insert("sys", "query", Value::from("q"));
        node.run(NodeContext::for_test("start", pool)).await
    }

    fn outputs(r: NodeResult) -> indexmap::IndexMap<String, Value> {
        match r.unwrap() {
            NodeOutcome::Completed(o) => o.outputs,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn validates_and_coerces() {
        let n = node(json!([
            {"variable": "name", "type": "text-input", "required": true, "max_length": 5},
            {"variable": "age", "type": "number"},
            {"variable": "ok", "type": "checkbox"},
            {"variable": "color", "type": "select", "options": ["red", "blue"], "default": "red"},
            {"variable": "meta", "type": "json"}
        ]));
        let out = outputs(
            run(
                &n,
                &[("name", "bob".into()), ("age", "41".into()), ("ok", "true".into()), ("meta", "{\"a\":1}".into())],
            )
            .await,
        );
        assert_eq!(out.get("name"), Some(&Value::from("bob")));
        assert_eq!(out.get("age"), Some(&Value::Int(41)));
        assert_eq!(out.get("ok"), Some(&Value::Bool(true)));
        assert_eq!(out.get("color"), Some(&Value::from("red")));
        assert!(matches!(out.get("meta"), Some(Value::Object(_))));
        // No env / sys leakage.
        assert_eq!(out.len(), 5);
    }

    #[tokio::test]
    async fn rejects_invalid_inputs() {
        let n = node(json!([{"variable": "name", "required": true, "max_length": 3}]));
        assert!(run(&n, &[]).await.is_err());
        assert!(run(&n, &[("name", "".into())]).await.is_err());
        assert!(run(&n, &[("name", "toolong".into())]).await.is_err());
        let n = node(json!([{"variable": "c", "type": "select", "options": ["a"]}]));
        assert!(run(&n, &[("c", "b".into())]).await.is_err());
        let n = node(json!([{"variable": "n", "type": "number"}]));
        assert!(run(&n, &[("n", "abc".into())]).await.is_err());
    }

    #[tokio::test]
    async fn optional_missing_is_null() {
        let n = node(json!([{"variable": "x"}]));
        let out = outputs(run(&n, &[]).await);
        assert_eq!(out.get("x"), Some(&Value::Null));
    }

    #[test]
    fn unknown_type_is_config_error() {
        let doc = NodeDoc::new("start", "start", json!({"variables": [{"variable": "x", "type": "weird"}]}));
        assert!(StartNode::new(&doc, doc.parse_config().unwrap()).is_err());
    }
}
