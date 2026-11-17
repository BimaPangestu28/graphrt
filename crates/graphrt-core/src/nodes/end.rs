//! `end`: collects the workflow's final outputs.

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::node::{Node, NodeContext, NodeError, NodeOutput, NodeResult, NodeRole};
use crate::pool::Selector;
use crate::value::{Value, ValueType};

/// `end` configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct EndConfig {
    #[serde(default)]
    pub outputs: Vec<OutputVariable>,
}

/// One named output read from a selector.
#[derive(Clone, Debug, Deserialize)]
pub struct OutputVariable {
    pub variable: String,
    pub value_selector: Selector,
    /// Optional declared type; when set the value is checked (missing values stay null).
    #[serde(default)]
    pub value_type: Option<ValueType>,
}

/// The `end` node.
#[derive(Debug)]
pub struct EndNode {
    config: EndConfig,
}

impl EndNode {
    pub fn new(config: EndConfig) -> Self {
        Self { config }
    }
}

impl Node for EndNode {
    fn role(&self) -> NodeRole {
        NodeRole::Response
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let mut out = NodeOutput::new();
            for o in &self.config.outputs {
                let value = ctx.get(&o.value_selector).unwrap_or(Value::Null);
                let value = match o.value_type {
                    Some(ty) => ty
                        .coerce(value)
                        .map_err(|e| NodeError::fatal("type_mismatch", format!("output `{}`: {e}", o.variable)))?,
                    None => value,
                };
                out.outputs.insert(o.variable.clone(), value);
            }
            Ok(out.into())
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        self.config.outputs.iter().map(|o| o.value_selector.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutcome;
    use crate::pool::VariablePool;
    use serde_json::json;

    #[tokio::test]
    async fn collects_outputs_and_checks_types() {
        let cfg: EndConfig = serde_json::from_value(json!({"outputs": [
            {"variable": "a", "value_selector": ["n", "x"]},
            {"variable": "missing", "value_selector": ["n", "nope"], "value_type": "string"}
        ]}))
        .unwrap();
        let mut pool = VariablePool::new();
        pool.insert("n", "x", Value::Int(1));
        let r = EndNode::new(cfg).run(NodeContext::for_test("end", pool.clone())).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!() };
        assert_eq!(o.outputs.get("a"), Some(&Value::Int(1)));
        assert_eq!(o.outputs.get("missing"), Some(&Value::Null));

        let cfg: EndConfig = serde_json::from_value(json!({"outputs": [
            {"variable": "a", "value_selector": ["n", "x"], "value_type": "string"}
        ]}))
        .unwrap();
        assert!(EndNode::new(cfg).run(NodeContext::for_test("end", pool)).await.is_err());
    }
}
