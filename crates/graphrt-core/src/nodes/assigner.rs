//! `assigner` (also accepted as `variable-assigner`): updates existing
//! variables — typically conversation variables or loop variables.
//!
//! All items are computed first against a private copy of the pool (later
//! items see earlier writes); only if every item succeeds are the writes
//! returned in [`NodeOutput::writes`]. A failing item therefore leaves every
//! variable untouched.
//!
//! Two configuration shapes are accepted:
//! - `items: [{variable_selector, input_type: variable|constant, operation, value}]`
//! - single-item form: `assigned_variable_selector`, `input_variable_selector`,
//!   `write_mode: over-write|append|clear`.

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::dsl::NodeDoc;
use crate::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult};
use crate::pool::{Selector, VariablePool};
use crate::value::{Value, ValueType};

/// `assigner` configuration (either shape).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AssignerConfig {
    #[serde(default)]
    pub items: Option<Vec<AssignItem>>,
    #[serde(default)]
    pub assigned_variable_selector: Option<Selector>,
    #[serde(default)]
    pub input_variable_selector: Option<Selector>,
    #[serde(default)]
    pub write_mode: Option<Operation>,
}

/// One assignment.
#[derive(Clone, Debug, Deserialize)]
pub struct AssignItem {
    pub variable_selector: Selector,
    #[serde(default)]
    pub input_type: InputType,
    pub operation: Operation,
    /// A selector (for `variable` inputs) or a literal (for `constant` inputs).
    #[serde(default)]
    pub value: serde_json::Value,
}

/// Where an item's input comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    Variable,
    #[default]
    Constant,
}

/// Assignment operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Operation {
    #[serde(rename = "over-write", alias = "overwrite")]
    Overwrite,
    #[serde(rename = "clear")]
    Clear,
    #[serde(rename = "set")]
    Set,
    #[serde(rename = "append")]
    Append,
    #[serde(rename = "extend")]
    Extend,
    #[serde(rename = "remove-first")]
    RemoveFirst,
    #[serde(rename = "remove-last")]
    RemoveLast,
    #[serde(rename = "+=")]
    Add,
    #[serde(rename = "-=")]
    Subtract,
    #[serde(rename = "*=")]
    Multiply,
    #[serde(rename = "/=")]
    Divide,
}

impl Operation {
    fn needs_input(self) -> bool {
        !matches!(self, Operation::Clear | Operation::RemoveFirst | Operation::RemoveLast)
    }
}

#[derive(Clone, Debug)]
enum Input {
    None,
    Selector(Selector),
    Constant(Value),
}

#[derive(Clone, Debug)]
struct Item {
    target: Selector,
    operation: Operation,
    input: Input,
}

/// The `assigner` node.
#[derive(Debug)]
pub struct AssignerNode {
    items: Vec<Item>,
}

impl AssignerNode {
    pub fn new(doc: &NodeDoc, config: AssignerConfig) -> Result<Self, ConfigError> {
        let raw_items = match (config.items, config.assigned_variable_selector) {
            (Some(items), _) => items,
            (None, Some(target)) => {
                let operation = config.write_mode.unwrap_or(Operation::Overwrite);
                let value = match config.input_variable_selector {
                    Some(sel) => serde_json::to_value(&sel).unwrap_or(serde_json::Value::Null),
                    None => serde_json::Value::Null,
                };
                vec![AssignItem { variable_selector: target, input_type: InputType::Variable, operation, value }]
            }
            (None, None) => {
                return Err(ConfigError::new(doc, "either `items` or `assigned_variable_selector` is required"));
            }
        };
        let mut items = Vec::with_capacity(raw_items.len());
        for raw in raw_items {
            let input = if !raw.operation.needs_input() {
                Input::None
            } else {
                match raw.input_type {
                    InputType::Variable => {
                        let sel: Selector = serde_json::from_value(raw.value.clone()).map_err(|e| {
                            ConfigError::new(
                                doc,
                                format!("item for `{}`: invalid input selector: {e}", raw.variable_selector),
                            )
                        })?;
                        Input::Selector(sel)
                    }
                    InputType::Constant => Input::Constant(Value::from_json(raw.value.clone())),
                }
            };
            items.push(Item { target: raw.variable_selector, operation: raw.operation, input });
        }
        Ok(Self { items })
    }
}

fn err(target: &Selector, message: impl Into<String>) -> NodeError {
    NodeError::fatal("assign", format!("`{target}`: {}", message.into()))
}

/// Most values an assigned variable may hold, nested ones included.
pub const MAX_ASSIGNED_VALUES: usize = 1_000_000;
/// Deepest nesting an assigned variable may have (serde_json's parse limit).
pub const MAX_ASSIGNED_DEPTH: usize = 128;

/// Reject an input read from another variable when it is too large or too deep.
///
/// The assigner is the only node that feeds a variable back into itself (a
/// loop variable, a conversation variable). Values share their children, so
/// `append xs to xs` or `extend xs with xs` in a loop doubles the logical size
/// every round in almost no memory, and anything that later walks the value
/// (serializing events, the store, SSE) takes exponential time or overflows the
/// stack. Bounding every input bounds each round's growth. The check stops
/// after `MAX_ASSIGNED_VALUES` steps, so a typical input costs a few steps.
fn bounded_input(target: &Selector, input: Value) -> Result<Value, NodeError> {
    if input.fits_shape(MAX_ASSIGNED_VALUES, MAX_ASSIGNED_DEPTH) {
        return Ok(input);
    }
    Err(err(
        target,
        format!("input holds more than {MAX_ASSIGNED_VALUES} values or nests deeper than {MAX_ASSIGNED_DEPTH} levels"),
    ))
}

/// The declared type a new value must satisfy, derived from the current value.
fn target_type(current: &Value) -> ValueType {
    match current.infer_type() {
        ValueType::Integer | ValueType::Float => ValueType::Number,
        ValueType::Null => ValueType::Any,
        // An empty array accepts any array.
        t if t.is_array() && current.as_array().is_some_and(<[Value]>::is_empty) => ValueType::ArrayAny,
        t => t,
    }
}

fn check_type(target: &Selector, current: &Value, new: Value) -> Result<Value, NodeError> {
    let ty = target_type(current);
    let new = match (ty, new) {
        // Object targets accept JSON text.
        (ValueType::Object, Value::String(s)) => Value::from_json(
            serde_json::from_slice(s.as_bytes()).map_err(|e| err(target, format!("invalid JSON object: {e}")))?,
        ),
        (_, v) => v,
    };
    if ty.is_array() && !matches!(new, Value::Array(_)) && !new.is_null() {
        return Err(err(target, format!("expected an array, got {}", new.type_name())));
    }
    ty.coerce(new).map_err(|e| err(target, e.to_string()))
}

fn arithmetic(target: &Selector, op: Operation, current: &Value, input: &Value) -> Result<Value, NodeError> {
    let parse = |v: &Value| -> Option<Value> {
        match v {
            Value::Int(_) | Value::Float(_) => Some(v.clone()),
            Value::String(s) => {
                let s = s.trim();
                s.parse::<i64>().map(Value::Int).ok().or_else(|| s.parse::<f64>().ok().map(Value::Float))
            }
            _ => None,
        }
    };
    let a = parse(current).ok_or_else(|| err(target, format!("cannot do arithmetic on {}", current.type_name())))?;
    let b = parse(input).ok_or_else(|| err(target, format!("operand must be a number, got {}", input.type_name())))?;
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        let r = match op {
            Operation::Add => x.checked_add(*y),
            Operation::Subtract => x.checked_sub(*y),
            Operation::Multiply => x.checked_mul(*y),
            _ => None,
        };
        if let Some(r) = r {
            return Ok(Value::Int(r));
        }
        if op != Operation::Divide {
            return Err(err(target, "integer overflow"));
        }
    }
    let (x, y) = (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(0.0));
    let r = match op {
        Operation::Add => x + y,
        Operation::Subtract => x - y,
        Operation::Multiply => x * y,
        _ => {
            if y == 0.0 {
                return Err(err(target, "division by zero"));
            }
            x / y
        }
    };
    if !r.is_finite() {
        return Err(err(target, "result is not a finite number"));
    }
    Ok(Value::Float(r))
}

fn array_items(target: &Selector, current: &Value) -> Result<Vec<Value>, NodeError> {
    match current {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => Ok(items.as_ref().clone()),
        other => Err(err(target, format!("expected an array, got {}", other.type_name()))),
    }
}

/// Element type of an existing non-empty typed array.
fn element_type(current: &Value) -> ValueType {
    match current.as_array() {
        Some(items) if !items.is_empty() => current.infer_type().element().unwrap_or(ValueType::Any),
        _ => ValueType::Any,
    }
}

fn apply(item: &Item, current: &Value, input: Option<Value>) -> Result<Option<Value>, NodeError> {
    let target = &item.target;
    let input_value = input.unwrap_or(Value::Null);
    let new = match item.operation {
        Operation::Overwrite | Operation::Set => {
            if input_value.is_null() && item.operation == Operation::Overwrite {
                // Nothing to write (input variable absent).
                return Ok(None);
            }
            check_type(target, current, input_value)?
        }
        Operation::Clear => target_type(current).zero(),
        Operation::Add | Operation::Subtract | Operation::Multiply | Operation::Divide => {
            arithmetic(target, item.operation, current, &input_value)?
        }
        Operation::Append => {
            if input_value.is_null() {
                return Ok(None);
            }
            let elem = element_type(current).coerce(input_value).map_err(|e| err(target, e.to_string()))?;
            let mut items = array_items(target, current)?;
            items.push(elem);
            Value::array(items)
        }
        Operation::Extend => {
            let extra = match input_value {
                Value::Null => return Ok(None),
                Value::Array(a) => a,
                other => return Err(err(target, format!("extend needs an array, got {}", other.type_name()))),
            };
            let elem_ty = element_type(current);
            let mut items = array_items(target, current)?;
            for e in extra.iter() {
                items.push(elem_ty.coerce(e.clone()).map_err(|x| err(target, x.to_string()))?);
            }
            Value::array(items)
        }
        Operation::RemoveFirst | Operation::RemoveLast => {
            let mut items = array_items(target, current)?;
            if !items.is_empty() {
                if item.operation == Operation::RemoveFirst {
                    items.remove(0);
                } else {
                    items.pop();
                }
            }
            Value::array(items)
        }
    };
    Ok(Some(new))
}

impl AssignerNode {
    fn compute(&self, pool: &VariablePool) -> Result<NodeOutput, NodeError> {
        let mut scratch = pool.clone();
        let mut touched: Vec<Selector> = Vec::new();
        let mut updated = Vec::new();
        for item in &self.items {
            let current = scratch.get_or_null(&item.target);
            let input = match &item.input {
                Input::None => None,
                Input::Constant(v) => Some(v.clone()),
                Input::Selector(sel) => Some(bounded_input(&item.target, scratch.get_or_null(sel))?),
            };
            let Some(new) = apply(item, &current, input)? else { continue };
            scratch.set_path(&item.target, new.clone()).map_err(|e| NodeError::invalid_input(e.to_string()))?;
            let root = item.target.root();
            if !touched.contains(&root) {
                touched.push(root);
            }
            updated.push(Value::object([("selector", Value::from(item.target.to_string())), ("value", new)]));
        }
        let mut out = NodeOutput::new().meta("updated", Value::array(updated));
        for root in touched {
            let value = scratch.get_or_null(&root);
            out.writes.push((root, value));
        }
        Ok(out)
    }
}

impl Node for AssignerNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move { self.compute(&ctx.pool).map(Into::into) })
    }

    fn inputs(&self) -> Vec<Selector> {
        let mut out = Vec::new();
        for item in &self.items {
            out.push(item.target.clone());
            if let Input::Selector(s) = &item.input {
                out.push(s.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutcome;
    use serde_json::json;

    fn node(cfg: serde_json::Value) -> AssignerNode {
        let doc = NodeDoc::new("a", "assigner", cfg);
        AssignerNode::new(&doc, doc.parse_config().unwrap()).unwrap()
    }

    async fn writes(n: &AssignerNode, pool: VariablePool) -> Result<Vec<(Selector, Value)>, NodeError> {
        match n.run(NodeContext::for_test("a", pool)).await? {
            NodeOutcome::Completed(o) => Ok(o.writes),
            other => panic!("{other:?}"),
        }
    }

    fn pool() -> VariablePool {
        let mut p = VariablePool::new();
        p.insert("conversation", "count", Value::Int(1));
        p.insert("conversation", "list", Value::from_json(json!(["a"])));
        p.insert("conversation", "name", Value::from("x"));
        p.insert("conversation", "obj", Value::from_json(json!({"k": 1})));
        p.insert("llm", "text", Value::from("hello"));
        p
    }

    #[tokio::test]
    async fn read_after_write_and_single_write_per_target() {
        let n = node(json!({"items": [
            {"variable_selector": ["conversation", "count"], "operation": "+=", "value": 2},
            {"variable_selector": ["conversation", "count"], "operation": "*=", "value": 10},
            {"variable_selector": ["conversation", "list"], "input_type": "variable", "operation": "append", "value": ["llm", "text"]},
            {"variable_selector": ["conversation", "name"], "input_type": "variable", "operation": "over-write", "value": ["llm", "text"]}
        ]}));
        let w = writes(&n, pool()).await.unwrap();
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].1, Value::Int(30));
        assert_eq!(w[1].1, Value::from_json(json!(["a", "hello"])));
        assert_eq!(w[2].1, Value::from("hello"));
    }

    #[tokio::test]
    async fn rejects_inputs_beyond_the_shape_limits() {
        let mut p = pool();
        let wide = Value::array(vec![Value::Int(0); MAX_ASSIGNED_VALUES]);
        p.insert("conversation", "wide", wide);
        let mut deep = Value::Int(0);
        for _ in 0..=MAX_ASSIGNED_DEPTH {
            deep = Value::array(vec![deep]);
        }
        p.insert("conversation", "deep", deep);
        for source in ["wide", "deep"] {
            let n = node(json!({"items": [{"variable_selector": ["conversation", "list"], "input_type": "variable",
                "operation": "over-write", "value": ["conversation", source]}]}));
            let e = writes(&n, p.clone()).await.unwrap_err();
            assert!(e.message.contains("more than"), "{source}: {e:?}");
        }
    }

    #[tokio::test]
    async fn atomic_on_error() {
        let n = node(json!({"items": [
            {"variable_selector": ["conversation", "count"], "operation": "+=", "value": 1},
            {"variable_selector": ["conversation", "count"], "operation": "/=", "value": 0}
        ]}));
        assert!(writes(&n, pool()).await.is_err());
    }

    #[tokio::test]
    async fn type_checks() {
        let n = node(json!({"items": [
            {"variable_selector": ["conversation", "count"], "operation": "set", "value": "nope"}]}));
        assert!(writes(&n, pool()).await.is_err());
        let n = node(json!({"items": [
            {"variable_selector": ["conversation", "obj"], "operation": "set", "value": "{\"z\": 2}"}]}));
        let w = writes(&n, pool()).await.unwrap();
        assert_eq!(w[0].1, Value::from_json(json!({"z": 2})));
    }

    #[tokio::test]
    async fn array_ops_and_clear() {
        let n = node(json!({"items": [
            {"variable_selector": ["conversation", "list"], "operation": "extend", "input_type": "constant", "value": ["b", "c"]},
            {"variable_selector": ["conversation", "list"], "operation": "remove-first"},
            {"variable_selector": ["conversation", "list"], "operation": "remove-last"},
            {"variable_selector": ["conversation", "name"], "operation": "clear"}
        ]}));
        let w = writes(&n, pool()).await.unwrap();
        assert_eq!(w[0].1, Value::from_json(json!(["b"])));
        assert_eq!(w[1].1, Value::from(""));
    }

    #[tokio::test]
    async fn int_division_yields_float_and_legacy_shape() {
        let n = node(json!({"items": [
            {"variable_selector": ["conversation", "count"], "operation": "/=", "value": 2}]}));
        assert_eq!(writes(&n, pool()).await.unwrap()[0].1, Value::Float(0.5));

        let n = node(json!({
            "assigned_variable_selector": ["conversation", "list"],
            "input_variable_selector": ["llm", "text"],
            "write_mode": "append"
        }));
        assert_eq!(writes(&n, pool()).await.unwrap()[0].1, Value::from_json(json!(["a", "hello"])));
    }

    #[tokio::test]
    async fn nested_target_path() {
        let n = node(json!({"items": [
            {"variable_selector": ["conversation", "obj", "k"], "operation": "+=", "value": 5}]}));
        let w = writes(&n, pool()).await.unwrap();
        assert_eq!(w[0].0.to_string(), "conversation.obj");
        assert_eq!(w[0].1, Value::from_json(json!({"k": 6})));
    }
}
