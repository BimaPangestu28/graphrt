//! `variable-aggregator`: outputs the first non-null value among several
//! selectors (typically one per branch), optionally in named groups.

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::node::{Node, NodeContext, NodeOutput, NodeResult};
use crate::pool::{Selector, VariablePool};
use crate::value::Value;

/// `variable-aggregator` configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AggregatorConfig {
    #[serde(default)]
    pub variables: Vec<Selector>,
    #[serde(default)]
    pub advanced_settings: Option<AdvancedSettings>,
}

/// Grouped aggregation settings.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AdvancedSettings {
    #[serde(default)]
    pub group_enabled: bool,
    #[serde(default)]
    pub groups: Vec<Group>,
}

/// A named group of candidate selectors.
#[derive(Clone, Debug, Deserialize)]
pub struct Group {
    pub group_name: String,
    #[serde(default)]
    pub variables: Vec<Selector>,
}

/// The `variable-aggregator` node.
#[derive(Debug)]
pub struct AggregatorNode {
    config: AggregatorConfig,
}

impl AggregatorNode {
    pub fn new(config: AggregatorConfig) -> Self {
        Self { config }
    }
}

fn first_non_null(selectors: &[Selector], pool: &VariablePool) -> Value {
    selectors.iter().filter_map(|s| pool.get(s)).find(|v| !v.is_null()).unwrap_or(Value::Null)
}

impl Node for AggregatorNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let mut out = NodeOutput::new();
            match &self.config.advanced_settings {
                Some(adv) if adv.group_enabled => {
                    for g in &adv.groups {
                        let value = first_non_null(&g.variables, &ctx.pool);
                        out.outputs.insert(g.group_name.clone(), Value::object([("output", value)]));
                    }
                }
                _ => {
                    out.outputs.insert("output".into(), first_non_null(&self.config.variables, &ctx.pool));
                }
            }
            Ok(out.into())
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        let mut all = self.config.variables.clone();
        if let Some(adv) = &self.config.advanced_settings {
            all.extend(adv.groups.iter().flat_map(|g| g.variables.iter().cloned()));
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutcome;
    use serde_json::json;

    async fn run(cfg: serde_json::Value, pool: VariablePool) -> NodeOutput {
        let cfg: AggregatorConfig = serde_json::from_value(cfg).unwrap();
        match AggregatorNode::new(cfg).run(NodeContext::for_test("agg", pool)).await.unwrap() {
            NodeOutcome::Completed(o) => o,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn first_non_null_wins() {
        let mut pool = VariablePool::new();
        pool.insert("b", "text", Value::from("from b"));
        let o = run(json!({"variables": [["a", "text"], ["b", "text"]]}), pool.clone()).await;
        assert_eq!(o.outputs.get("output"), Some(&Value::from("from b")));

        let o = run(
            json!({"advanced_settings": {"group_enabled": true, "groups": [
                {"group_name": "g1", "variables": [["b", "text"]]},
                {"group_name": "g2", "variables": [["c", "x"]]}
            ]}}),
            pool,
        )
        .await;
        assert_eq!(o.outputs.get("g1").and_then(|v| v.get_path(&["output"])), Some(Value::from("from b")));
        assert_eq!(o.outputs.get("g2").and_then(|v| v.get_path(&["output"])), Some(Value::Null));
    }
}
