//! `iteration`: runs the container body once per array item.
//!
//! Each item runs in its own child frame with `<iteration>.item` and
//! `<iteration>.index` set. Items run with bounded concurrency when
//! `is_parallel` is set; the `output` array is always in input order.
//!
//! Error modes:
//! - `terminated` (default): the first failure cancels in-flight items and fails the node.
//! - `continue-on-error`: a failed item contributes `null`.
//! - `remove-abnormal-output`: a failed item contributes nothing.
//!
//! When an item pauses (e.g. waiting for human input), no new items are
//! started; in-flight items finish, then the node pauses with its progress and
//! the paused item snapshots as resume state.

use std::collections::{BTreeMap, VecDeque};

use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};

use crate::dsl::NodeDoc;
use crate::event::{PauseReason, Usage};
use crate::node::{
    ConfigError, Node, NodeContext, NodeError, NodeOutcome, NodeOutput, NodeResult, NodeRole, PauseRequest,
    SubgraphRequest, SubgraphResult, merge_writes,
};
use crate::pool::{Selector, VariablePool};
use crate::snapshot::FrameSnapshot;
use crate::value::Value;

/// Default number of concurrently running items in parallel mode.
pub const DEFAULT_PARALLELISM: usize = 10;
/// Upper bound on concurrently running items.
pub const MAX_PARALLELISM: usize = 100;

/// Failure handling for individual items.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorMode {
    #[default]
    Terminated,
    ContinueOnError,
    RemoveAbnormalOutput,
}

/// `iteration` configuration.
#[derive(Clone, Debug, Deserialize)]
pub struct IterationConfig {
    pub iterator_selector: Selector,
    #[serde(default)]
    pub output_selector: Option<Selector>,
    #[serde(default)]
    pub is_parallel: bool,
    #[serde(default = "default_parallelism")]
    pub parallel_nums: usize,
    #[serde(default)]
    pub error_handle_mode: ErrorMode,
    #[serde(default = "yes")]
    pub flatten_output: bool,
}

fn default_parallelism() -> usize {
    DEFAULT_PARALLELISM
}

fn yes() -> bool {
    true
}

/// Result recorded for one item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ItemOutcome {
    Done { value: Value },
    Removed,
}

/// Resume state of a paused iteration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct IterationState {
    total: usize,
    results: BTreeMap<usize, ItemOutcome>,
    paused: BTreeMap<usize, FrameSnapshot>,
    writes: Vec<(Selector, Value)>,
    usage: Usage,
    /// Failed items (continue-on-error / remove-abnormal-output), by index.
    #[serde(default)]
    errors: BTreeMap<usize, String>,
}

impl IterationState {
    /// Copy with secrets replaced by snapshot markers (see [`Value::to_snapshot_value`]).
    fn to_persisted(&self) -> Self {
        let results = self
            .results
            .iter()
            .map(|(i, outcome)| {
                let outcome = match outcome {
                    ItemOutcome::Done { value } => ItemOutcome::Done { value: value.to_snapshot_value() },
                    ItemOutcome::Removed => ItemOutcome::Removed,
                };
                (*i, outcome)
            })
            .collect();
        Self { results, writes: snapshot_writes(&self.writes), ..self.clone() }
    }

    /// Inverse of [`IterationState::to_persisted`].
    fn restored(self) -> Self {
        let results = self
            .results
            .into_iter()
            .map(|(i, outcome)| {
                let outcome = match outcome {
                    ItemOutcome::Done { value } => ItemOutcome::Done { value: value.from_snapshot_value() },
                    ItemOutcome::Removed => ItemOutcome::Removed,
                };
                (i, outcome)
            })
            .collect();
        Self { results, writes: restore_writes(self.writes), ..self }
    }
}

/// Apply one body write to a container's base pool.
pub(crate) fn apply_write(pool: &mut VariablePool, selector: &Selector, value: Value) -> Result<(), NodeError> {
    pool.set_path(selector, value).map_err(|e| NodeError::fatal("invalid_write", e.to_string()))
}

/// Writes in persistable form (secrets replaced by markers).
pub(crate) fn snapshot_writes(writes: &[(Selector, Value)]) -> Vec<(Selector, Value)> {
    writes.iter().map(|(s, v)| (s.clone(), v.to_snapshot_value())).collect()
}

/// Inverse of [`snapshot_writes`].
pub(crate) fn restore_writes(writes: Vec<(Selector, Value)>) -> Vec<(Selector, Value)> {
    writes.into_iter().map(|(s, v)| (s, v.from_snapshot_value())).collect()
}

/// The `iteration` node.
#[derive(Debug)]
pub struct IterationNode {
    config: IterationConfig,
}

impl IterationNode {
    pub fn new(doc: &NodeDoc, config: IterationConfig) -> Result<Self, ConfigError> {
        if config.is_parallel && config.parallel_nums == 0 {
            return Err(ConfigError::new(doc, "parallel_nums must be at least 1"));
        }
        Ok(Self { config })
    }

    fn concurrency(&self) -> usize {
        if self.config.is_parallel { self.config.parallel_nums.clamp(1, MAX_PARALLELISM) } else { 1 }
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        let node_id = ctx.node_id.to_string();
        let items: Vec<Value> = match ctx.get(&self.config.iterator_selector) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.as_ref().clone(),
            Some(other) => {
                return Err(NodeError::invalid_input(format!(
                    "iterator `{}` must be an array, got {}",
                    self.config.iterator_selector,
                    other.type_name()
                )));
            }
        };

        let mut state = match &ctx.resume {
            Some(resume) => serde_json::from_value::<IterationState>(resume.state.clone())
                .map_err(|e| NodeError::fatal("resume", format!("invalid iteration resume state: {e}")))?
                .restored(),
            None => IterationState { total: items.len(), ..IterationState::default() },
        };
        if state.total != items.len() {
            return Err(NodeError::fatal(
                "resume",
                format!("iterator length changed from {} to {} while paused", state.total, items.len()),
            ));
        }
        if items.is_empty() {
            return Ok(NodeOutput::new().with("output", Value::array([])).into());
        }
        let subgraph = ctx
            .subgraph
            .clone()
            .ok_or_else(|| NodeError::fatal("missing_subgraph", "iteration body executor not available"))?;

        let mut queue: VecDeque<(usize, Option<FrameSnapshot>)> =
            (0..items.len()).filter(|i| !state.results.contains_key(i)).map(|i| (i, state.paused.remove(&i))).collect();
        let mut running = FuturesUnordered::new();
        let mut pause_reasons: Vec<PauseReason> = Vec::new();
        let concurrency = self.concurrency();

        // Base pool: the parent's variables plus every write completed items made
        // outside the body. Each item starts from the base as it is when the item
        // starts, so sequential items see earlier items' writes; with parallel
        // items, concurrent writes to the same variable are last-write-wins.
        let mut base = ctx.pool.clone();
        for (sel, value) in &state.writes {
            apply_write(&mut base, sel, value.clone())?;
        }

        loop {
            while pause_reasons.is_empty() && running.len() < concurrency {
                let Some((index, resume)) = queue.pop_front() else { break };
                let Some(item) = items.get(index).cloned() else { break };
                let mut pool = base.clone();
                pool.insert(&node_id, "item", item);
                pool.insert(&node_id, "index", Value::Int(i64::try_from(index).unwrap_or(i64::MAX)));
                ctx.progress(
                    "iteration_next",
                    Value::object([
                        ("index", Value::Int(i64::try_from(index).unwrap_or(i64::MAX))),
                        ("total", Value::Int(i64::try_from(items.len()).unwrap_or(i64::MAX))),
                    ]),
                );
                let sg = subgraph.clone();
                running.push(async move {
                    let result = sg.run(SubgraphRequest { index, pool, resume }).await;
                    (index, result)
                });
            }
            let Some((index, result)) = running.next().await else { break };
            match result {
                SubgraphResult::Completed { pool, writes, usage, .. } => {
                    let value = match &self.config.output_selector {
                        Some(sel) => pool.get_or_null(sel),
                        None => Value::Null,
                    };
                    state.results.insert(index, ItemOutcome::Done { value });
                    for (sel, value) in &writes {
                        apply_write(&mut base, sel, value.clone())?;
                    }
                    merge_writes(&mut state.writes, writes);
                    state.usage.add(&usage);
                }
                SubgraphResult::Failed { error, usage } => {
                    state.usage.add(&usage);
                    match self.config.error_handle_mode {
                        ErrorMode::Terminated => {
                            // Dropping `running` cancels every in-flight item.
                            let message = format!("iteration item {index} failed: {}", error.message);
                            return Err(error.with_message(message));
                        }
                        ErrorMode::ContinueOnError => {
                            state.results.insert(index, ItemOutcome::Done { value: Value::Null });
                        }
                        ErrorMode::RemoveAbnormalOutput => {
                            state.results.insert(index, ItemOutcome::Removed);
                        }
                    }
                    state.errors.insert(index, error.message.clone());
                }
                SubgraphResult::Paused { snapshot, reasons } => {
                    state.paused.insert(index, snapshot);
                    if reasons.is_empty() {
                        pause_reasons.push(PauseReason {
                            frame: ctx.frame.clone(),
                            node_id: Some(node_id.clone()),
                            kind: "container".into(),
                            data: Value::Null,
                        });
                    } else {
                        pause_reasons.extend(reasons);
                    }
                }
            }
        }

        if !pause_reasons.is_empty() {
            return Ok(NodeOutcome::Paused(pause_request(&pause_reasons, &state.to_persisted())?));
        }

        let errors = Value::array(state.errors.iter().map(|(index, message)| {
            Value::object([
                ("index", Value::Int(i64::try_from(*index).unwrap_or(i64::MAX))),
                ("error", Value::from(message.as_str())),
            ])
        }));
        let ordered: Vec<Value> = state
            .results
            .into_values()
            .filter_map(|o| match o {
                ItemOutcome::Done { value } => Some(value),
                ItemOutcome::Removed => None,
            })
            .collect();
        let output = if self.config.flatten_output { flatten(ordered) } else { Value::array(ordered) };
        let mut out = NodeOutput::new().with("output", output).with("errors", errors);
        out.writes = state.writes;
        if !state.usage.is_zero() {
            out.usage = Some(state.usage);
        }
        Ok(out.into())
    }
}

/// Build the pause request a container returns when a body run paused.
pub(crate) fn pause_request<S: Serialize>(reasons: &[PauseReason], state: &S) -> Result<PauseRequest, NodeError> {
    let kind = reasons.first().map(|r| r.kind.clone()).unwrap_or_else(|| "container".into());
    let data =
        serde_json::to_value(reasons).map(Value::from_json).map_err(|e| NodeError::fatal("pause", e.to_string()))?;
    let state = serde_json::to_value(state).map_err(|e| NodeError::fatal("pause", e.to_string()))?;
    Ok(PauseRequest { kind, data: Value::object([("reasons", data)]), state })
}

/// Concatenate item outputs when every one is an array or null (and at least one is an array).
fn flatten(values: Vec<Value>) -> Value {
    let all_arrays = values.iter().all(|v| matches!(v, Value::Array(_) | Value::Null));
    let any_array = values.iter().any(|v| matches!(v, Value::Array(_)));
    if !(all_arrays && any_array) {
        return Value::array(values);
    }
    let mut flat = Vec::new();
    for v in values {
        if let Value::Array(items) = v {
            flat.extend(items.iter().cloned());
        }
    }
    Value::array(flat)
}

impl Node for IterationNode {
    fn role(&self) -> NodeRole {
        NodeRole::Container
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }

    fn inputs(&self) -> Vec<Selector> {
        let mut out = vec![self.config.iterator_selector.clone()];
        out.extend(self.config.output_selector.iter().cloned());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{Resume, Subgraph};
    use crate::pool::VariablePool;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Body: doubles `it.item` into `body.out`; fails on item 3; pauses on item 4
    /// unless resumed.
    #[derive(Debug, Default)]
    struct FakeBody {
        calls: AtomicUsize,
    }

    impl Subgraph for FakeBody {
        fn run(&self, req: SubgraphRequest) -> BoxFuture<'_, SubgraphResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let item = req.pool.get_or_null(&Selector::of("it", "item"));
                let n = item.as_i64().unwrap_or(0);
                if n == 3 {
                    return SubgraphResult::Failed {
                        error: NodeError::new("boom", "item three"),
                        usage: Usage::default(),
                    };
                }
                if n == 4 && req.resume.is_none() {
                    return SubgraphResult::Paused {
                        snapshot: FrameSnapshot::default(),
                        reasons: vec![PauseReason {
                            frame: Arc::from("it[x]"),
                            node_id: Some("human".into()),
                            kind: "human_input".into(),
                            data: Value::Null,
                        }],
                    };
                }
                let mut pool = req.pool.clone();
                pool.insert("body", "out", Value::Int(n * 2));
                SubgraphResult::Completed {
                    pool,
                    outputs: Default::default(),
                    writes: vec![(Selector::of("conversation", "last"), Value::Int(n))],
                    usage: Usage { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
                    broke: false,
                }
            })
        }
    }

    fn node(cfg: serde_json::Value) -> IterationNode {
        let doc = NodeDoc::new("it", "iteration", cfg);
        IterationNode::new(&doc, doc.parse_config().unwrap()).unwrap()
    }

    fn ctx(list: serde_json::Value, body: Arc<FakeBody>) -> NodeContext {
        let mut pool = VariablePool::new();
        pool.insert("n", "list", Value::from_json(list));
        NodeContext::for_test("it", pool).with_subgraph(Some(body))
    }

    fn cfg(mode: &str, parallel: bool) -> serde_json::Value {
        json!({"iterator_selector": ["n", "list"], "output_selector": ["body", "out"],
               "is_parallel": parallel, "parallel_nums": 2, "error_handle_mode": mode})
    }

    #[tokio::test]
    async fn outputs_in_order_with_usage_and_writes() {
        let body = Arc::new(FakeBody::default());
        let r = node(cfg("terminated", true)).run(ctx(json!([1, 2, 5]), body)).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!("{r:?}") };
        assert_eq!(o.outputs.get("output"), Some(&Value::from_json(json!([2, 4, 10]))));
        assert_eq!(o.usage.map(|u| u.total_tokens), Some(6));
        // Three items wrote `conversation.last`; only the latest write is kept.
        assert_eq!(o.writes.len(), 1);
        assert_eq!(o.outputs.get("errors"), Some(&Value::array([])));
    }

    #[tokio::test]
    async fn error_modes() {
        let body = Arc::new(FakeBody::default());
        assert!(node(cfg("terminated", false)).run(ctx(json!([1, 3, 5]), body.clone())).await.is_err());
        // Sequential + terminated: item 5 never starts.
        assert_eq!(body.calls.load(Ordering::SeqCst), 2);

        let r = node(cfg("continue-on-error", false)).run(ctx(json!([1, 3]), body.clone())).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!() };
        assert_eq!(o.outputs.get("output"), Some(&Value::from_json(json!([2, null]))));

        let r = node(cfg("remove-abnormal-output", false)).run(ctx(json!([1, 3]), body)).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!() };
        assert_eq!(o.outputs.get("output"), Some(&Value::from_json(json!([2]))));
    }

    #[tokio::test]
    async fn empty_and_invalid_iterators() {
        let body = Arc::new(FakeBody::default());
        let r = node(cfg("terminated", false)).run(ctx(json!([]), body.clone())).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!() };
        assert_eq!(o.outputs.get("output"), Some(&Value::array([])));
        assert!(node(cfg("terminated", false)).run(ctx(json!("x"), body)).await.is_err());
    }

    #[tokio::test]
    async fn flattens_array_outputs() {
        assert_eq!(
            flatten(vec![Value::from_json(json!([1])), Value::Null, Value::from_json(json!([2, 3]))]),
            Value::from_json(json!([1, 2, 3]))
        );
        assert_eq!(flatten(vec![Value::Int(1), Value::from_json(json!([2]))]), Value::from_json(json!([1, [2]])));
    }

    #[tokio::test]
    async fn pause_and_resume() {
        let body = Arc::new(FakeBody::default());
        let n = node(cfg("terminated", false));
        let r = n.run(ctx(json!([1, 4, 2]), body.clone())).await.unwrap();
        let NodeOutcome::Paused(p) = r else { panic!("{r:?}") };
        assert_eq!(p.kind, "human_input");
        // Item 2 was not started after the pause.
        assert_eq!(body.calls.load(Ordering::SeqCst), 2);

        let resumed = ctx(json!([1, 4, 2]), body.clone()).with_resume(Some(Resume { state: p.state, input: None }));
        let r = n.run(resumed).await.unwrap();
        let NodeOutcome::Completed(o) = r else { panic!("{r:?}") };
        assert_eq!(o.outputs.get("output"), Some(&Value::from_json(json!([2, 8, 4]))));
        assert_eq!(body.calls.load(Ordering::SeqCst), 4);
    }
}
