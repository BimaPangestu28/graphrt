//! `loop`: runs the container body repeatedly.
//!
//! Loop variables live at `<loop>.<label>` and are visible (and assignable) in
//! the body; `<loop>.index` is the 0-based round. The loop stops when:
//! - a `loop-end` node completes in the body,
//! - the break conditions hold (checked before the first round, and after each
//!   round against the round's final variables), or
//! - `loop_count` rounds have run.
//!
//! An error while evaluating break conditions fails the node; it is never
//! treated as "keep looping". Outputs are the loop variables plus `loop_round`
//! (number of completed rounds).

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::condition::{Condition, LogicalOperator, evaluate_group};
use crate::dsl::NodeDoc;
use crate::event::{PauseReason, Usage};
use crate::node::{
    ConfigError, Node, NodeContext, NodeError, NodeOutcome, NodeOutput, NodeResult, NodeRole, SubgraphRequest,
    SubgraphResult, merge_writes,
};
use crate::nodes::iteration::{apply_write, pause_request, restore_writes, snapshot_writes};
use crate::pool::{Selector, VariablePool};
use crate::snapshot::FrameSnapshot;
use crate::value::{Value, ValueType};

/// Hard upper bound on `loop_count`.
pub const MAX_LOOP_COUNT: u64 = 10_000;

/// `loop` configuration.
#[derive(Clone, Debug, Deserialize)]
pub struct LoopConfig {
    #[serde(default = "one")]
    pub loop_count: u64,
    #[serde(default)]
    pub break_conditions: Vec<Condition>,
    #[serde(default)]
    pub logical_operator: LogicalOperator,
    #[serde(default)]
    pub loop_variables: Vec<LoopVariable>,
}

fn one() -> u64 {
    1
}

/// A loop variable declaration.
#[derive(Clone, Debug, Deserialize)]
pub struct LoopVariable {
    pub label: String,
    #[serde(default = "any")]
    pub var_type: ValueType,
    #[serde(default)]
    pub value_type: LoopValueSource,
    /// A literal (for `constant`) or a selector (for `variable`).
    #[serde(default)]
    pub value: serde_json::Value,
}

fn any() -> ValueType {
    ValueType::Any
}

/// How a loop variable's initial value is given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopValueSource {
    #[default]
    Constant,
    Variable,
}

#[derive(Clone, Debug)]
enum Initial {
    Constant(Value),
    Variable(Selector),
}

#[derive(Clone, Debug)]
struct Var {
    label: String,
    ty: ValueType,
    initial: Initial,
}

/// Resume state of a paused loop.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct LoopState {
    round: u64,
    vars: IndexMap<String, Value>,
    paused: Option<FrameSnapshot>,
    writes: Vec<(Selector, Value)>,
    usage: Usage,
}

/// The `loop` node.
#[derive(Debug)]
pub struct LoopNode {
    loop_count: u64,
    break_conditions: Vec<Condition>,
    logical_operator: LogicalOperator,
    vars: Vec<Var>,
}

impl LoopNode {
    pub fn new(doc: &NodeDoc, config: LoopConfig) -> Result<Self, ConfigError> {
        if config.loop_count == 0 || config.loop_count > MAX_LOOP_COUNT {
            return Err(ConfigError::new(doc, format!("loop_count must be between 1 and {MAX_LOOP_COUNT}")));
        }
        let mut vars = Vec::with_capacity(config.loop_variables.len());
        for v in config.loop_variables {
            if v.label == "index" {
                return Err(ConfigError::new(doc, "loop variable name `index` is reserved"));
            }
            let initial = match v.value_type {
                LoopValueSource::Variable => Initial::Variable(
                    serde_json::from_value(v.value.clone())
                        .map_err(|e| ConfigError::new(doc, format!("loop variable `{}`: {e}", v.label)))?,
                ),
                LoopValueSource::Constant => {
                    let value = constant_value(v.var_type, v.value.clone());
                    let value = v
                        .var_type
                        .coerce(value)
                        .map_err(|e| ConfigError::new(doc, format!("loop variable `{}`: {e}", v.label)))?;
                    Initial::Constant(value)
                }
            };
            vars.push(Var { label: v.label, ty: v.var_type, initial });
        }
        Ok(Self {
            loop_count: config.loop_count,
            break_conditions: config.break_conditions,
            logical_operator: config.logical_operator,
            vars,
        })
    }

    /// Initial values; variable references may use earlier loop variables.
    fn initial_vars(&self, loop_id: &str, pool: &VariablePool) -> Result<IndexMap<String, Value>, NodeError> {
        let mut scratch = pool.clone();
        let mut vars = IndexMap::with_capacity(self.vars.len());
        for v in &self.vars {
            let value = match &v.initial {
                Initial::Constant(c) => c.clone(),
                Initial::Variable(sel) => v
                    .ty
                    .coerce(scratch.get_or_null(sel))
                    .map_err(|e| NodeError::invalid_input(format!("loop variable `{}` from `{sel}`: {e}", v.label)))?,
            };
            scratch.insert(loop_id, &v.label, value.clone());
            vars.insert(v.label.clone(), value);
        }
        Ok(vars)
    }

    fn should_break(&self, pool: &VariablePool) -> Result<bool, NodeError> {
        if self.break_conditions.is_empty() {
            return Ok(false);
        }
        evaluate_group(&self.break_conditions, self.logical_operator, pool)
            .map(|(r, _)| r)
            .map_err(|e| NodeError::fatal("condition", format!("break condition: {e}")))
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        let loop_id = ctx.node_id.to_string();
        let mut state = match &ctx.resume {
            Some(resume) => {
                let mut state = serde_json::from_value::<LoopState>(resume.state.clone())
                    .map_err(|e| NodeError::fatal("resume", format!("invalid loop resume state: {e}")))?;
                state.vars = state.vars.into_iter().map(|(k, v)| (k, v.from_snapshot_value())).collect();
                state.writes = restore_writes(state.writes);
                state
            }
            None => LoopState { vars: self.initial_vars(&loop_id, &ctx.pool)?, ..LoopState::default() },
        };

        // Base pool: the parent's variables plus writes the body made outside the loop.
        let mut base = ctx.pool.clone();
        for (sel, value) in &state.writes {
            apply_write(&mut base, sel, value.clone())?;
        }

        if ctx.resume.is_none() && self.should_break(&round_pool(&base, &loop_id, &state.vars, 0))? {
            return Ok(self.finish(state).into());
        }
        let subgraph = ctx
            .subgraph
            .clone()
            .ok_or_else(|| NodeError::fatal("missing_subgraph", "loop body executor not available"))?;

        while state.round < self.loop_count {
            let round = state.round;
            ctx.progress("loop_next", Value::object([("round", Value::Int(i64::try_from(round).unwrap_or(i64::MAX)))]));
            let request = SubgraphRequest {
                index: usize::try_from(round).unwrap_or(usize::MAX),
                pool: round_pool(&base, &loop_id, &state.vars, round),
                resume: state.paused.take(),
            };
            match subgraph.run(request).await {
                SubgraphResult::Completed { pool, writes, usage, broke, .. } => {
                    state.usage.add(&usage);
                    for v in &self.vars {
                        let current = pool.get(&Selector::of(&loop_id, &v.label));
                        if let Some(value) = current {
                            let value = v.ty.coerce(value).map_err(|e| {
                                NodeError::fatal("type_mismatch", format!("loop variable `{}`: {e}", v.label))
                            })?;
                            state.vars.insert(v.label.clone(), value);
                        }
                    }
                    let mut external = Vec::new();
                    for (sel, value) in writes {
                        if sel.namespace() == loop_id {
                            continue;
                        }
                        apply_write(&mut base, &sel, value.clone())?;
                        external.push((sel, value));
                    }
                    merge_writes(&mut state.writes, external);
                    state.round += 1;
                    if broke {
                        break;
                    }
                    let mut check = pool;
                    for (label, value) in &state.vars {
                        check.insert(&loop_id, label, value.clone());
                    }
                    if self.should_break(&check)? {
                        break;
                    }
                }
                SubgraphResult::Failed { error, usage } => {
                    state.usage.add(&usage);
                    let message = format!("loop round {round} failed: {}", error.message);
                    return Err(error.with_message(message));
                }
                SubgraphResult::Paused { snapshot, reasons } => {
                    state.paused = Some(snapshot);
                    let reasons = if reasons.is_empty() {
                        vec![PauseReason {
                            frame: ctx.frame.clone(),
                            node_id: Some(loop_id.clone()),
                            kind: "container".into(),
                            data: Value::Null,
                        }]
                    } else {
                        reasons
                    };
                    let persisted = LoopState {
                        vars: state.vars.iter().map(|(k, v)| (k.clone(), v.to_snapshot_value())).collect(),
                        writes: snapshot_writes(&state.writes),
                        ..state
                    };
                    return Ok(NodeOutcome::Paused(pause_request(&reasons, &persisted)?));
                }
            }
        }
        Ok(self.finish(state).into())
    }

    fn finish(&self, state: LoopState) -> NodeOutput {
        let mut out = NodeOutput::new();
        for (label, value) in state.vars {
            out.outputs.insert(label, value);
        }
        out.outputs.insert("loop_round".into(), Value::Int(i64::try_from(state.round).unwrap_or(i64::MAX)));
        out.writes = state.writes;
        if !state.usage.is_zero() {
            out.usage = Some(state.usage);
        }
        out
    }
}

fn round_pool(base: &VariablePool, loop_id: &str, vars: &IndexMap<String, Value>, round: u64) -> VariablePool {
    let mut pool = base.clone();
    for (label, value) in vars {
        pool.insert(loop_id, label, value.clone());
    }
    pool.insert(loop_id, "index", Value::Int(i64::try_from(round).unwrap_or(i64::MAX)));
    pool
}

/// Constants for non-string types may be written as JSON text.
fn constant_value(ty: ValueType, raw: serde_json::Value) -> Value {
    match (ty, raw) {
        (ValueType::String | ValueType::Secret | ValueType::Any, v) => Value::from_json(v),
        (_, serde_json::Value::String(s)) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(parsed) => Value::from_json(parsed),
            Err(_) => Value::from(s),
        },
        (_, v) => Value::from_json(v),
    }
}

impl Node for LoopNode {
    fn role(&self) -> NodeRole {
        NodeRole::Container
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{Resume, Subgraph};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Body: `lp.count += 1`; breaks via loop-end when count reaches `end_at`;
    /// pauses once in round `pause_round`.
    #[derive(Debug)]
    struct Counter {
        calls: AtomicUsize,
        end_at: i64,
        pause_round: Option<usize>,
    }

    impl Subgraph for Counter {
        fn run(&self, req: SubgraphRequest) -> BoxFuture<'_, SubgraphResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if Some(req.index) == self.pause_round && req.resume.is_none() {
                    return SubgraphResult::Paused { snapshot: FrameSnapshot::default(), reasons: vec![] };
                }
                let count = req.pool.get_or_null(&Selector::of("lp", "count")).as_i64().unwrap_or(0) + 1;
                let mut pool = req.pool.clone();
                pool.insert("lp", "count", Value::Int(count));
                SubgraphResult::Completed {
                    pool,
                    outputs: Default::default(),
                    writes: vec![
                        (Selector::of("lp", "count"), Value::Int(count)),
                        (Selector::of("conversation", "seen"), Value::Int(count)),
                    ],
                    usage: Usage::default(),
                    broke: count >= self.end_at,
                }
            })
        }
    }

    fn node(cfg: serde_json::Value) -> Result<LoopNode, ConfigError> {
        let doc = NodeDoc::new("lp", "loop", cfg);
        LoopNode::new(&doc, doc.parse_config().unwrap())
    }

    fn ctx(body: Arc<Counter>) -> NodeContext {
        let mut pool = VariablePool::new();
        pool.insert("n", "start", Value::Int(0));
        NodeContext::for_test("lp", pool).with_subgraph(Some(body))
    }

    fn counter(end_at: i64, pause_round: Option<usize>) -> Arc<Counter> {
        Arc::new(Counter { calls: AtomicUsize::new(0), end_at, pause_round })
    }

    fn base_cfg(count: u64, conds: serde_json::Value) -> serde_json::Value {
        json!({"loop_count": count, "break_conditions": conds, "logical_operator": "and",
               "loop_variables": [{"label": "count", "var_type": "number", "value_type": "variable", "value": ["n", "start"]}]})
    }

    fn outputs(r: NodeResult) -> IndexMap<String, Value> {
        match r.unwrap() {
            NodeOutcome::Completed(o) => o.outputs,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn runs_until_loop_count() {
        let body = counter(100, None);
        let out = outputs(node(base_cfg(3, json!([]))).unwrap().run(ctx(body)).await);
        assert_eq!(out.get("count"), Some(&Value::Int(3)));
        assert_eq!(out.get("loop_round"), Some(&Value::Int(3)));
    }

    #[tokio::test]
    async fn stops_on_break_condition_and_loop_end() {
        let conds = json!([{"variable_selector": ["lp", "count"], "comparison_operator": "≥", "value": "2"}]);
        let out = outputs(node(base_cfg(10, conds)).unwrap().run(ctx(counter(100, None))).await);
        assert_eq!(out.get("loop_round"), Some(&Value::Int(2)));

        let out = outputs(node(base_cfg(10, json!([]))).unwrap().run(ctx(counter(4, None))).await);
        assert_eq!(out.get("count"), Some(&Value::Int(4)));
    }

    #[tokio::test]
    async fn break_before_first_round() {
        let conds = json!([{"variable_selector": ["lp", "count"], "comparison_operator": "=", "value": "0"}]);
        let body = counter(100, None);
        let out = outputs(node(base_cfg(10, conds)).unwrap().run(ctx(body.clone())).await);
        assert_eq!(out.get("loop_round"), Some(&Value::Int(0)));
        assert_eq!(body.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn break_condition_error_fails() {
        let conds = json!([{"variable_selector": ["lp", "count"], "comparison_operator": "start with", "value": "x"}]);
        assert!(node(base_cfg(10, conds)).unwrap().run(ctx(counter(100, None))).await.is_err());
    }

    #[tokio::test]
    async fn pause_and_resume_mid_loop() {
        let body = counter(100, Some(1));
        let n = node(base_cfg(3, json!([]))).unwrap();
        let r = n.run(ctx(body.clone())).await.unwrap();
        let NodeOutcome::Paused(p) = r else { panic!("{r:?}") };
        let r = n.run(ctx(body).with_resume(Some(Resume { state: p.state, input: None }))).await;
        let out = outputs(r);
        assert_eq!(out.get("count"), Some(&Value::Int(3)));
        assert_eq!(out.get("loop_round"), Some(&Value::Int(3)));
    }

    #[tokio::test]
    async fn forwards_outside_writes() {
        let r = node(base_cfg(2, json!([]))).unwrap().run(ctx(counter(100, None))).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!() };
        // Two rounds wrote `conversation.seen`; only the latest write is kept.
        assert_eq!(o.writes.len(), 1);
        assert!(o.writes.iter().all(|(s, _)| s.namespace() == "conversation"));
    }

    #[test]
    fn config_validation() {
        assert!(node(json!({"loop_count": 0})).is_err());
        assert!(node(json!({"loop_count": MAX_LOOP_COUNT + 1})).is_err());
        assert!(node(json!({"loop_variables": [{"label": "x", "var_type": "number", "value": "abc"}]})).is_err());
        let n = node(json!({"loop_variables": [{"label": "x", "var_type": "array[number]", "value": "[1,2]"}]}));
        assert!(n.is_ok());
    }
}
