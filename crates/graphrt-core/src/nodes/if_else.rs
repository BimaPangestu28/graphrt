//! `if-else`: selects the first case whose conditions hold.
//!
//! The selected branch handle is the case's `case_id`, or `false` when no case
//! matches. Outputs: `result` (whether any case matched) and
//! `selected_case_id`. Every evaluated condition is recorded in the
//! `conditions` metadata.

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::condition::{Condition, LogicalOperator, evaluate_group};
use crate::dsl::NodeDoc;
use crate::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult, NodeRole};
use crate::pool::Selector;
use crate::value::Value;

/// Handle selected when no case matches.
pub const ELSE_HANDLE: &str = "false";
/// Case id used for the single-case legacy configuration.
pub const LEGACY_CASE_ID: &str = "true";

/// `if-else` configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct IfElseConfig {
    #[serde(default)]
    pub cases: Option<Vec<Case>>,
    /// Legacy single-case form.
    #[serde(default)]
    pub conditions: Option<Vec<Condition>>,
    #[serde(default)]
    pub logical_operator: LogicalOperator,
}

/// One case.
#[derive(Clone, Debug, Deserialize)]
pub struct Case {
    pub case_id: String,
    #[serde(default)]
    pub logical_operator: LogicalOperator,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// The `if-else` node.
#[derive(Debug)]
pub struct IfElseNode {
    cases: Vec<Case>,
}

impl IfElseNode {
    pub fn new(doc: &NodeDoc, config: IfElseConfig) -> Result<Self, ConfigError> {
        let cases = match (config.cases, config.conditions) {
            (Some(cases), _) if !cases.is_empty() => cases,
            (_, Some(conditions)) => {
                vec![Case { case_id: LEGACY_CASE_ID.to_owned(), logical_operator: config.logical_operator, conditions }]
            }
            _ => Vec::new(),
        };
        if cases.iter().any(|c| c.case_id == ELSE_HANDLE) {
            return Err(ConfigError::new(doc, format!("case id `{ELSE_HANDLE}` is reserved for the else branch")));
        }
        Ok(Self { cases })
    }
}

impl Node for IfElseNode {
    fn role(&self) -> NodeRole {
        NodeRole::Branch
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let mut traces = Vec::new();
            let mut selected: Option<&str> = None;
            for case in &self.cases {
                let (matched, case_traces) = evaluate_group(&case.conditions, case.logical_operator, &ctx.pool)
                    .map_err(|e| NodeError::fatal("condition", e.to_string()))?;
                traces.push(Value::object([
                    ("case_id", Value::from(case.case_id.as_str())),
                    ("result", Value::Bool(matched)),
                    ("conditions", Value::array(case_traces.iter().map(|t| t.to_value()))),
                ]));
                if matched {
                    selected = Some(&case.case_id);
                    break;
                }
            }
            let handle = selected.unwrap_or(ELSE_HANDLE);
            let out = NodeOutput::new()
                .with("result", selected.is_some())
                .with("selected_case_id", selected.map(Value::from).unwrap_or(Value::Null))
                .branch(handle)
                .meta("cases", Value::array(traces));
            Ok(out.into())
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        self.cases.iter().flat_map(|c| c.conditions.iter().flat_map(Condition::selectors)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeOutcome;
    use crate::pool::VariablePool;
    use serde_json::json;

    fn node(cfg: serde_json::Value) -> IfElseNode {
        let doc = NodeDoc::new("if", "if-else", cfg);
        IfElseNode::new(&doc, doc.parse_config().unwrap()).unwrap()
    }

    async fn branch(n: &IfElseNode, pool: VariablePool) -> Option<String> {
        match n.run(NodeContext::for_test("if", pool)).await.unwrap() {
            NodeOutcome::Completed(o) => o.branch,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn first_matching_case_wins() {
        let n = node(json!({"cases": [
            {"case_id": "big", "logical_operator": "and", "conditions": [
                {"variable_selector": ["n", "x"], "comparison_operator": ">", "value": "10"}]},
            {"case_id": "pos", "conditions": [
                {"variable_selector": ["n", "x"], "comparison_operator": ">", "value": "0"}]}
        ]}));
        let mut pool = VariablePool::new();
        pool.insert("n", "x", Value::Int(5));
        assert_eq!(branch(&n, pool.clone()).await.as_deref(), Some("pos"));
        pool.insert("n", "x", Value::Int(50));
        assert_eq!(branch(&n, pool.clone()).await.as_deref(), Some("big"));
        pool.insert("n", "x", Value::Int(-1));
        assert_eq!(branch(&n, pool).await.as_deref(), Some("false"));
    }

    #[tokio::test]
    async fn legacy_single_case_and_missing_variable() {
        let n = node(json!({"logical_operator": "or", "conditions": [
            {"variable_selector": ["n", "missing"], "comparison_operator": "not exists"}]}));
        assert_eq!(branch(&n, VariablePool::new()).await.as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn type_error_fails_node() {
        let n = node(json!({"cases": [{"case_id": "a", "conditions": [
            {"variable_selector": ["n", "x"], "comparison_operator": "start with", "value": "1"}]}]}));
        let mut pool = VariablePool::new();
        pool.insert("n", "x", Value::Int(1));
        assert!(n.run(NodeContext::for_test("if", pool)).await.is_err());
    }

    #[test]
    fn reserved_case_id() {
        let doc = NodeDoc::new("if", "if-else", json!({"cases": [{"case_id": "false", "conditions": []}]}));
        assert!(IfElseNode::new(&doc, doc.parse_config().unwrap()).is_err());
    }
}
