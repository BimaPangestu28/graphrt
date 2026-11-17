//! `list-operator`: filter, extract, sort and limit an array.
//!
//! Pipeline order: filter (all conditions must hold) → extract (1-based
//! `serial`) → order → limit. Works on arrays of strings, numbers, booleans,
//! files and objects (`key` is a file attribute or a dotted object path).
//! Outputs: `result`, `first_record`, `last_record`.

use std::cmp::Ordering;

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::condition::{KeyCondition, LogicalOperator, item_attribute, key_group_matches};
use crate::dsl::NodeDoc;
use crate::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult};
use crate::pool::{Selector, VariablePool};
use crate::template::Template;
use crate::value::Value;

/// `list-operator` configuration.
#[derive(Clone, Debug, Deserialize)]
pub struct ListOperatorConfig {
    pub variable: Selector,
    #[serde(default)]
    pub filter_by: FilterBy,
    #[serde(default)]
    pub extract_by: ExtractBy,
    #[serde(default)]
    pub order_by: OrderBy,
    #[serde(default)]
    pub limit: Limit,
}

/// Filter step.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FilterBy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub conditions: Vec<KeyCondition>,
}

/// Extract-one step. `serial` is 1-based and may be a template.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ExtractBy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub serial: Option<serde_json::Value>,
}

/// Sort direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Asc,
    Desc,
}

/// Sort step.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct OrderBy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub key: String,
    #[serde(default, rename = "value")]
    pub direction: Direction,
}

/// Limit step. Negative size means unlimited.
#[derive(Clone, Debug, Deserialize)]
pub struct Limit {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "unlimited")]
    pub size: i64,
}

impl Default for Limit {
    fn default() -> Self {
        Self { enabled: false, size: -1 }
    }
}

fn unlimited() -> i64 {
    -1
}

/// The `list-operator` node.
#[derive(Debug)]
pub struct ListOperatorNode {
    config: ListOperatorConfig,
}

impl ListOperatorNode {
    pub fn new(doc: &NodeDoc, config: ListOperatorConfig) -> Result<Self, ConfigError> {
        if config.extract_by.enabled && config.extract_by.serial.is_none() {
            return Err(ConfigError::new(doc, "extract_by.serial is required when extraction is enabled"));
        }
        Ok(Self { config })
    }

    fn compute(&self, pool: &VariablePool) -> Result<NodeOutput, NodeError> {
        let mut items: Vec<Value> = match pool.get_or_null(&self.config.variable) {
            Value::Null => Vec::new(),
            Value::Array(a) => a.as_ref().clone(),
            other => {
                return Err(NodeError::invalid_input(format!(
                    "`{}` must be an array, got {}",
                    self.config.variable,
                    other.type_name()
                )));
            }
        };

        if self.config.filter_by.enabled && !self.config.filter_by.conditions.is_empty() {
            let mut kept = Vec::with_capacity(items.len());
            for item in items {
                let ok = key_group_matches(&item, &self.config.filter_by.conditions, LogicalOperator::And, pool)
                    .map_err(|e| NodeError::fatal("filter", e.to_string()))?;
                if ok {
                    kept.push(item);
                }
            }
            items = kept;
        }

        if self.config.extract_by.enabled {
            let serial = self.serial(pool)?;
            let picked = serial.checked_sub(1).and_then(|i| items.get(i).cloned()).ok_or_else(|| {
                NodeError::invalid_input(format!("serial {serial} is out of range (list has {} items)", items.len()))
            })?;
            items = vec![picked];
        }

        if self.config.order_by.enabled {
            let key = self.config.order_by.key.as_str();
            items.sort_by(|a, b| compare_values(&item_attribute(a, key), &item_attribute(b, key)));
            if self.config.order_by.direction == Direction::Desc {
                items.reverse();
            }
        }

        if self.config.limit.enabled && self.config.limit.size >= 0 {
            let size = usize::try_from(self.config.limit.size).unwrap_or(usize::MAX);
            items.truncate(size);
        }

        let first = items.first().cloned().unwrap_or(Value::Null);
        let last = items.last().cloned().unwrap_or(Value::Null);
        Ok(NodeOutput::new().with("result", Value::array(items)).with("first_record", first).with("last_record", last))
    }

    fn serial(&self, pool: &VariablePool) -> Result<usize, NodeError> {
        let raw = self.config.extract_by.serial.clone().unwrap_or(serde_json::Value::Null);
        let resolved = match raw {
            serde_json::Value::String(s) => Template::parse(&s).resolve(pool),
            other => Value::from_json(other),
        };
        let n = match &resolved {
            Value::Int(i) => Some(*i),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            other => other.as_i64(),
        };
        n.and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n >= 1)
            .ok_or_else(|| NodeError::invalid_input(format!("serial must be a positive integer, got {resolved:?}")))
    }
}

/// Total order for sorting: nulls first, then numbers, booleans, strings; other
/// kinds compare by their text.
fn compare_values(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Int(_) | Value::Float(_) => 1,
            Value::Bool(_) => 2,
            Value::String(_) | Value::Secret(_) => 3,
            _ => 4,
        }
    }
    match (a, b) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            a.as_f64().unwrap_or(0.0).total_cmp(&b.as_f64().unwrap_or(0.0))
        }
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)).then_with(|| a.to_text().cmp(&b.to_text())),
    }
}

impl Node for ListOperatorNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move { self.compute(&ctx.pool).map(Into::into) })
    }

    fn inputs(&self) -> Vec<Selector> {
        vec![self.config.variable.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutcome;
    use serde_json::json;

    fn node(cfg: serde_json::Value) -> ListOperatorNode {
        let doc = NodeDoc::new("l", "list-operator", cfg);
        ListOperatorNode::new(&doc, doc.parse_config().unwrap()).unwrap()
    }

    async fn run(n: &ListOperatorNode, list: serde_json::Value) -> Result<NodeOutput, NodeError> {
        let mut pool = VariablePool::new();
        pool.insert("n", "list", Value::from_json(list));
        pool.insert("n", "pick", Value::from("2"));
        match n.run(NodeContext::for_test("l", pool)).await? {
            NodeOutcome::Completed(o) => Ok(o),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn filter_order_limit_numbers() {
        let n = node(json!({
            "variable": ["n", "list"],
            "filter_by": {"enabled": true, "conditions": [{"comparison_operator": ">", "value": "1"}]},
            "order_by": {"enabled": true, "value": "desc"},
            "limit": {"enabled": true, "size": 2}
        }));
        let o = run(&n, json!([3, 1, 5, 2])).await.unwrap();
        assert_eq!(o.outputs.get("result"), Some(&Value::from_json(json!([5, 3]))));
        assert_eq!(o.outputs.get("first_record"), Some(&Value::Int(5)));
        assert_eq!(o.outputs.get("last_record"), Some(&Value::Int(3)));
    }

    #[tokio::test]
    async fn objects_by_key_and_extract_template() {
        let n = node(json!({
            "variable": ["n", "list"],
            "order_by": {"enabled": true, "key": "age", "value": "asc"},
            "extract_by": {"enabled": true, "serial": "{{#n.pick#}}"}
        }));
        // Extraction happens before ordering.
        let o = run(&n, json!([{"name": "a", "age": 30}, {"name": "b", "age": 20}])).await.unwrap();
        assert_eq!(o.outputs.get("first_record").and_then(|v| v.get_path(&["name"])), Some(Value::from("b")));
    }

    #[tokio::test]
    async fn string_in_is_membership_and_empty_input() {
        let n = node(json!({
            "variable": ["n", "list"],
            "filter_by": {"enabled": true, "conditions": [{"comparison_operator": "in", "value": ["ab", "c"]}]}
        }));
        let o = run(&n, json!(["a", "ab", "c"])).await.unwrap();
        assert_eq!(o.outputs.get("result"), Some(&Value::from_json(json!(["ab", "c"]))));
        let o = run(&n, json!(null)).await.unwrap();
        assert_eq!(o.outputs.get("first_record"), Some(&Value::Null));
    }

    #[tokio::test]
    async fn errors() {
        let n = node(json!({"variable": ["n", "list"], "extract_by": {"enabled": true, "serial": "9"}}));
        assert!(run(&n, json!([1])).await.is_err());
        let n = node(json!({"variable": ["n", "list"]}));
        assert!(run(&n, json!("not a list")).await.is_err());
        let n = node(json!({"variable": ["n", "list"],
            "filter_by": {"enabled": true, "conditions": [{"comparison_operator": ">", "value": "x"}]}}));
        assert!(run(&n, json!([1])).await.is_err());
    }

    #[tokio::test]
    async fn files_by_attribute() {
        let n = node(json!({
            "variable": ["n", "list"],
            "filter_by": {"enabled": true, "conditions": [{"key": "extension", "comparison_operator": "is", "value": ".pdf"}]}
        }));
        let o = run(
            &n,
            json!([{"$file": true, "name": "a", "extension": ".pdf"}, {"$file": true, "name": "b", "extension": ".png"}]),
        )
        .await
        .unwrap();
        assert!(matches!(o.outputs.get("result"), Some(Value::Array(a)) if a.len() == 1));
    }
}
