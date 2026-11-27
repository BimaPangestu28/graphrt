//! `subagents` of the `agent` node (cargo feature `subagents`): workflows the
//! owner lists, offered to the model as tools, run as child runs of the same
//! run (`graphrt_core::node::ChildRuns`).
//!
//! The list is the allowlist: a model can only call a listed workflow, and a
//! host only links the workflows it was started with. A child gets the tool
//! arguments as start inputs and the `sys` values that identify the request,
//! nothing else of the parent. It shares the run's step budget, deadline,
//! depth limit, secrets and control signals, and uses the services its host
//! gave its own workflow. A child that pauses
//! (a tool that needs approval, a `human-input` node) pauses the parent, and
//! resuming the run resumes the child where it stopped. See
//! `docs/orchestration-design.md`.

use std::sync::Arc;

use futures::StreamExt;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::event::PauseReason;
use graphrt_core::node::{ChildRequest, ConfigError, SubgraphResult};
use graphrt_core::snapshot::FrameSnapshot;
use graphrt_core::tool::{ToolCallContext, ToolError, ToolOutput, ToolRuntime, ToolSpec};
use graphrt_core::value::Value;
use graphrt_core::{NodeContext, NodeError};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use crate::types::ToolCall;

/// The provider the subagent tools are bound under.
pub(crate) const SUBAGENT_PROVIDER: &str = "subagents";
const DEFAULT_PARALLEL: usize = 4;
const DEFAULT_CALLS: usize = 16;

/// One listed workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) workflow: String,
    description: String,
    /// Its answer becomes the answer of this agent, and the loop ends.
    pub(crate) handoff: bool,
}

/// `subagents` as configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Subagents {
    pub(crate) entries: Vec<Entry>,
    parallel: usize,
    calls: usize,
}

/// Parse `subagents`, `max_parallel_subagents` and `max_subagent_calls`.
pub(crate) fn parse(
    doc: &NodeDoc,
    raw: Option<&Json>,
    parallel: Option<usize>,
    calls: Option<usize>,
) -> Result<Option<Subagents>, ConfigError> {
    let invalid = |why: &str| ConfigError::new(doc, format!("subagents: {why}"));
    let Some(raw) = raw.filter(|raw| !raw.is_null()) else { return Ok(None) };
    let items = raw.as_array().ok_or_else(|| invalid("expected a list of `{workflow, description, handoff?}`"))?;
    let mut entries: Vec<Entry> = Vec::new();
    for item in items {
        let fields = item.as_object().ok_or_else(|| invalid("each entry is `{workflow, description, handoff?}`"))?;
        if let Some(unknown) = fields.keys().find(|k| !matches!(k.as_str(), "workflow" | "description" | "handoff")) {
            return Err(invalid(&format!("unknown field `{unknown}`")));
        }
        let workflow = fields.get("workflow").and_then(Json::as_str).filter(|w| !w.trim().is_empty());
        let workflow = workflow.ok_or_else(|| invalid("`workflow` names a workflow this host serves"))?;
        // The name becomes a tool name and a segment of frame ids.
        if workflow.len() > 64 || !workflow.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(invalid(&format!("`{workflow}`: a workflow name has letters, digits, `_` and `-` only")));
        }
        let description = fields.get("description").and_then(Json::as_str).filter(|d| !d.trim().is_empty());
        let description =
            description.ok_or_else(|| invalid(&format!("`{workflow}` needs a `description` for the model")))?;
        if entries.iter().any(|e| e.workflow == workflow) {
            return Err(invalid(&format!("`{workflow}` is listed twice")));
        }
        let handoff = match fields.get("handoff") {
            None | Some(Json::Null) => false,
            Some(Json::Bool(handoff)) => *handoff,
            Some(_) => return Err(invalid("`handoff` is `true` or `false`")),
        };
        entries.push(Entry { workflow: workflow.to_owned(), description: description.to_owned(), handoff });
    }
    if entries.is_empty() {
        return Ok(None);
    }
    let parallel = parallel.unwrap_or(DEFAULT_PARALLEL);
    let calls = calls.unwrap_or(DEFAULT_CALLS);
    if parallel == 0 || calls == 0 {
        return Err(invalid("`max_parallel_subagents` and `max_subagent_calls` are at least 1"));
    }
    Ok(Some(Subagents { entries, parallel, calls }))
}

/// A child that paused, kept in the agent's state until it is resumed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PausedChild {
    pub(crate) call: ToolCall,
    workflow: String,
    index: usize,
    snapshot: FrameSnapshot,
    reasons: Vec<PauseReason>,
}

/// What the agent keeps about its children across turns and pauses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChildState {
    /// Child runs started so far (it numbers the next one).
    #[serde(default)]
    calls: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) paused: Vec<PausedChild>,
    /// The answer of a handoff: the loop ends with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) handoff: Option<String>,
    /// The run was stopped (aborted, or a limit of the run) while a child ran:
    /// the agent stops too instead of handing the model an error.
    #[serde(skip)]
    pub(crate) stopped: Option<NodeError>,
}

impl ChildState {
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The pause reasons of the children that wait.
    pub(crate) fn reasons(&self) -> Vec<PauseReason> {
        self.paused.iter().flat_map(|c| c.reasons.iter().cloned()).collect()
    }
}

/// The tool specs of `subagents`, one per listed workflow.
///
/// @throws NodeError - `subagents_unavailable` when the host links no
///   workflows, `unknown_workflow` when a listed one is not linked.
pub(crate) fn specs(ctx: &NodeContext, config: &Subagents) -> Result<Vec<ToolSpec>, NodeError> {
    let children = ctx.children.as_ref().ok_or_else(|| {
        NodeError::fatal(
            "subagents_unavailable",
            "subagents: this host links no workflows to the run (serve them in the same `graphrt serve`)",
        )
    })?;
    let linked = children.workflows();
    let mut specs = Vec::new();
    for entry in &config.entries {
        if !linked.contains(&entry.workflow) {
            return Err(NodeError::fatal(
                "unknown_workflow",
                format!("subagents: workflow `{}` is not served by this host", entry.workflow),
            ));
        }
        let parameters = children.inputs_schema(&entry.workflow)?;
        specs.push(ToolSpec { name: entry.workflow.clone(), description: entry.description.clone(), parameters });
    }
    Ok(specs)
}

/// The runtime the subagent tools are bound to. The agent runs them itself
/// (they are child runs, not tool calls), so a call here is a mistake.
#[derive(Debug)]
pub(crate) struct NotDirectly;

impl ToolRuntime for NotDirectly {
    fn list(&self) -> futures::future::BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        _: Json,
        _: &'a ToolCallContext,
    ) -> futures::future::BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move { Ok(ToolOutput::error(format!("error: `{name}` is a workflow the agent runs itself"))) })
    }
}

/// What came of one call.
pub(crate) enum Done {
    /// A result for the model.
    Result(ToolCall, ToolOutput),
    /// No result for the model: the child paused (kept in the state), or the
    /// run stops (`ChildState::stopped`).
    Paused,
}

/// The `sys` values a child sees: the ones that identify the request.
fn child_sys(ctx: &NodeContext) -> IndexMap<String, Value> {
    let mut sys = IndexMap::new();
    for name in ["run_id", "user_id", "conversation_id"] {
        let template = graphrt_core::template::Template::parse(&format!("{{{{#sys.{name}#}}}}"));
        let value = template.render(&ctx.pool);
        if !value.trim().is_empty() {
            sys.insert(name.to_owned(), Value::from(value));
        }
    }
    sys
}

/// The text a child's outputs give the model, `None` when it gave no answer
/// (no outputs, or one empty text).
fn answer_of(outputs: &IndexMap<String, Value>) -> Option<String> {
    for key in ["text", "answer", "result", "output"] {
        if outputs.len() == 1
            && let Some(Value::String(text)) = outputs.get(key)
        {
            return Some(text.to_string()).filter(|text| !text.trim().is_empty());
        }
    }
    if outputs.values().all(|value| matches!(value, Value::Null)) {
        return None;
    }
    let json: serde_json::Map<String, Json> = outputs.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
    Some(Json::Object(json).to_string())
}

impl Subagents {
    fn entry(&self, workflow: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.workflow == workflow)
    }

    /// Run `calls` (each with the workflow it names) as child runs, at most
    /// `max_parallel_subagents` at a time and never more than the run's
    /// concurrency, at most `max_subagent_calls` in a node run. A call to a
    /// workflow that is not listed gets an error result.
    pub(crate) async fn run(
        &self,
        ctx: &NodeContext,
        state: &mut ChildState,
        calls: Vec<(ToolCall, String)>,
    ) -> Vec<Done> {
        let Some(children) = ctx.children.clone() else {
            return calls
                .into_iter()
                .map(|(call, _)| Done::Result(call, ToolOutput::error("error: no workflows are linked to this run")))
                .collect();
        };
        let sys = child_sys(ctx);
        // A state saved without the counter still numbers past its waiting children.
        let past_waiting = state.paused.iter().map(|c| c.index.saturating_add(1)).max().unwrap_or(0);
        state.calls = state.calls.max(past_waiting);
        let mut started = Vec::new();
        // One slot per call, so the results keep the order of the calls.
        let mut done: Vec<Option<Done>> = Vec::with_capacity(calls.len());
        for (call, workflow) in calls {
            let slot = done.len();
            // The list is the allowlist, whatever bound the call.
            if self.entry(&workflow).is_none() {
                let refused = ToolOutput::error(format!("error: `{workflow}` is not a listed workflow"));
                done.push(Some(Done::Result(call, refused)));
                continue;
            }
            if state.calls >= self.calls {
                let message = format!(
                    "error: at most {} workflow calls in one run of this agent: finish with what you have",
                    self.calls
                );
                done.push(Some(Done::Result(call, ToolOutput::error(message))));
                continue;
            }
            let inputs = match call.arguments_json() {
                Ok(fields) => fields.into_iter().map(|(k, v)| (k, Value::from_json(v))).collect(),
                _ => {
                    done.push(Some(Done::Result(call, ToolOutput::error("error: the arguments must be an object"))));
                    continue;
                }
            };
            let index = state.calls;
            state.calls += 1;
            done.push(None);
            started.push((slot, call, workflow, index, inputs));
        }
        let parallel = self.parallel.min(children.max_concurrency()).max(1);
        let results: Vec<(usize, ToolCall, String, usize, SubgraphResult)> = futures::stream::iter(started)
            .map(|(slot, call, workflow, index, inputs)| {
                let children = Arc::clone(&children);
                let sys = sys.clone();
                async move {
                    let request = ChildRequest::start(workflow.clone(), index, inputs).with_sys(sys);
                    let result = children.run(request).await;
                    (slot, call, workflow, index, result)
                }
            })
            // In call order (still `parallel` at once): which handoff wins and the
            // order of the results do not depend on timing.
            .buffered(parallel)
            .collect()
            .await;
        for (slot, call, workflow, index, result) in results {
            let settled = self.settle(ctx, state, call, workflow, index, result);
            if let Some(place) = done.get_mut(slot) {
                *place = Some(settled);
            }
        }
        done.into_iter().flatten().collect()
    }

    /// Resume the children that paused, with the host's input.
    pub(crate) async fn resume(&self, ctx: &NodeContext, state: &mut ChildState) -> Vec<Done> {
        let Some(children) = ctx.children.clone() else { return Vec::new() };
        let mut done = Vec::new();
        let mut waiting = Vec::new();
        for child in std::mem::take(&mut state.paused) {
            // The list is the allowlist on resume too, whatever the saved state names.
            if self.entry(&child.workflow).is_some() {
                waiting.push(child);
            } else {
                let message = format!("error: `{}` is not a listed workflow", child.workflow);
                done.push(Done::Result(child.call, ToolOutput::error(message)));
            }
        }
        let results: Vec<(ToolCall, String, usize, SubgraphResult)> = futures::stream::iter(waiting)
            .map(|child| {
                let children = Arc::clone(&children);
                async move {
                    let request = ChildRequest::resume(child.workflow.clone(), child.index, child.snapshot);
                    let result = children.run(request).await;
                    (child.call, child.workflow, child.index, result)
                }
            })
            .buffered(self.parallel.min(children.max_concurrency()).max(1))
            .collect()
            .await;
        for (call, workflow, index, result) in results {
            done.push(self.settle(ctx, state, call, workflow, index, result));
        }
        done
    }

    fn settle(
        &self,
        ctx: &NodeContext,
        state: &mut ChildState,
        call: ToolCall,
        workflow: String,
        index: usize,
        result: SubgraphResult,
    ) -> Done {
        let scrub = |text: String| match ctx.secret_values() {
            Some(secrets) => secrets.scrub(&text).into_owned(),
            None => text,
        };
        match result {
            SubgraphResult::Completed { outputs, .. } => {
                // No answer is an error for the model, and never a handoff.
                let Some(text) = answer_of(&outputs).map(scrub) else {
                    ctx.progress(
                        "subagent_failed",
                        Value::from_json(json!({"workflow": workflow, "index": index, "kind": "no_answer"})),
                    );
                    return Done::Result(call, ToolOutput::error("error: the workflow finished without an answer"));
                };
                ctx.progress("subagent_finished", Value::from_json(json!({"workflow": workflow, "index": index})));
                if self.entry(&workflow).is_some_and(|e| e.handoff) && state.handoff.is_none() {
                    state.handoff = Some(text.clone());
                }
                Done::Result(call, ToolOutput::text(text))
            }
            // The child ended because the whole run stops: so does this agent.
            SubgraphResult::Failed { error, .. } if ctx.children.as_ref().is_some_and(|c| c.run_stopped()) => {
                state.stopped.get_or_insert(error);
                Done::Paused
            }
            SubgraphResult::Failed { error, .. } => {
                ctx.progress(
                    "subagent_failed",
                    Value::from_json(json!({"workflow": workflow, "index": index, "kind": error.kind})),
                );
                let message = scrub(format!("error ({}): {}", error.kind, error.message));
                Done::Result(call, ToolOutput::error(message))
            }
            SubgraphResult::Paused { snapshot, reasons } => {
                ctx.progress("subagent_paused", Value::from_json(json!({"workflow": workflow, "index": index})));
                state.paused.push(PausedChild { call, workflow, index, snapshot, reasons });
                Done::Paused
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn doc() -> NodeDoc {
        NodeDoc::new("lead", "agent", json!({}))
    }

    #[test]
    fn the_list_is_parsed_strictly() {
        let ok = parse(
            &doc(),
            Some(&json!([{"workflow": "research", "description": "Researches"}, {"workflow": "write", "description": "Writes", "handoff": true}])),
            None,
            Some(3),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ok.entries.len(), 2);
        assert!(ok.entries[1].handoff && ok.parallel == DEFAULT_PARALLEL && ok.calls == 3);
        for bad in [
            json!({"workflow": "x"}),
            json!([{"workflow": "x"}]),
            json!([{"description": "d"}]),
            json!([{"workflow": "x", "description": "d", "extra": 1}]),
            json!([{"workflow": "x", "description": "d"}, {"workflow": "x", "description": "e"}]),
            json!([{"workflow": "a/b", "description": "d"}]),
            json!([{"workflow": "a.b", "description": "d"}]),
            json!([{"workflow": "x".repeat(65), "description": "d"}]),
        ] {
            assert!(parse(&doc(), Some(&bad), None, None).is_err(), "{bad}");
        }
        assert!(parse(&doc(), Some(&json!([{"workflow": "x", "description": "d"}])), Some(0), None).is_err());
        assert!(parse(&doc(), None, None, None).unwrap().is_none());
    }

    #[test]
    fn a_child_answer_is_its_text_or_its_outputs() {
        let mut outputs = IndexMap::new();
        assert_eq!(answer_of(&outputs), None);
        outputs.insert("text".to_owned(), Value::from("  "));
        assert_eq!(answer_of(&outputs), None);
        outputs.insert("text".to_owned(), Value::from("done"));
        assert_eq!(answer_of(&outputs).as_deref(), Some("done"));
        outputs.insert("sources".to_owned(), Value::from(2_i64));
        assert_eq!(answer_of(&outputs).as_deref(), Some(r#"{"text":"done","sources":2}"#));
    }
}
