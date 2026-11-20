//! What one run of the system under test produced, and the views assertions
//! read from it: the output, the tool-call trajectory and retrieval hits.

use graphrt_core::Event;
use graphrt_core::event::Usage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

/// How a run ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Succeeded,
    Failed,
    Aborted,
    Paused,
}

/// Everything recorded about one run.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RunCapture {
    pub run_id: String,
    pub status: RunStatus,
    pub error: Option<String>,
    pub error_type: Option<String>,
    /// The workflow outputs (end node outputs).
    pub outputs: Map<String, Json>,
    /// The streamed answer text (answer nodes), empty when there is none.
    pub answer: String,
    pub usage: Usage,
    /// Wall time of the run in milliseconds, measured by the harness.
    pub latency_ms: u64,
    /// The run's events, without `node_chunk` events (their text is in `answer`
    /// and the node outputs).
    pub events: Vec<Event>,
}

/// One tool call made by an agent node.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolCallRecord {
    /// Node that made the call.
    pub node_id: String,
    pub id: String,
    /// Name the model called (the tool's exposed name).
    pub name: String,
    /// Tool provider and tool name, when the agent's `tool_calls` log has them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool: String,
    pub arguments: Json,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl ToolCallRecord {
    /// Whether `pattern` (with `*` wildcards) names this call: the exposed name,
    /// the tool name or `provider/tool`, as in the agent's `require_approval`.
    pub fn matches(&self, pattern: &str) -> bool {
        use crate::glob::matches;
        matches(pattern, &self.name)
            || (!self.tool.is_empty() && matches(pattern, &self.tool))
            || (!self.provider.is_empty() && matches(pattern, &format!("{}/{}", self.provider, self.tool)))
    }
}

/// One retrieved chunk from a `knowledge-retrieval` node.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RetrievalHit {
    pub node_id: String,
    /// Chunk id.
    pub id: String,
    pub document_id: String,
    pub score: Option<f64>,
    /// The chunk text (judges get it as `contexts`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
}

impl RetrievalHit {
    /// Whether this hit is the labelled document or chunk `label`.
    pub fn is(&self, label: &str) -> bool {
        self.document_id == label || self.id == label
    }
}

impl RunCapture {
    /// Build a capture from a run's events (chunk events are dropped).
    pub fn from_events(events: Vec<Event>, answer: String, latency_ms: u64) -> Self {
        let mut capture = Self { answer, latency_ms, ..Self::default() };
        for event in &events {
            match event {
                Event::RunStarted { run_id, .. } => capture.run_id.clone_from(run_id),
                Event::RunSucceeded { outputs, usage, .. } => {
                    capture.status = RunStatus::Succeeded;
                    capture.outputs = outputs.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
                    capture.usage = *usage;
                }
                Event::RunFailed { error, error_type, usage, .. } => {
                    capture.status = RunStatus::Failed;
                    capture.error = Some(error.clone());
                    capture.error_type = Some(error_type.clone());
                    capture.usage = *usage;
                }
                Event::RunAborted { reason, .. } => {
                    capture.status = RunStatus::Aborted;
                    capture.error = Some(reason.clone());
                    capture.error_type = Some("aborted".into());
                }
                Event::RunPaused { reasons, .. } => {
                    capture.status = RunStatus::Paused;
                    let kinds: Vec<&str> = reasons.iter().map(|r| r.kind.as_str()).collect();
                    capture.error = Some(format!("the run paused ({})", kinds.join(", ")));
                    capture.error_type = Some("paused".into());
                }
                _ => {}
            }
        }
        capture.events = events.into_iter().filter(|e| !matches!(e, Event::NodeChunk { .. })).collect();
        capture
    }

    /// The main output assertions check by default: the answer text when the
    /// workflow streamed one, else the single output value, else the outputs object.
    pub fn output(&self) -> Json {
        if !self.answer.is_empty() {
            return Json::String(self.answer.clone());
        }
        match (self.outputs.len(), self.outputs.values().next()) {
            (1, Some(only)) => only.clone(),
            _ => Json::Object(self.outputs.clone()),
        }
    }

    /// The object paths are resolved against: `output`, `answer`, `outputs`,
    /// `usage`, `latency_ms`, `status`, `error`, plus `vars` from the test.
    pub fn subject_root(&self, vars: &Map<String, Json>) -> Json {
        serde_json::json!({
            "output": self.output(),
            "answer": self.answer,
            "outputs": self.outputs,
            "usage": self.usage,
            "latency_ms": self.latency_ms,
            "status": self.status,
            "error": self.error,
            "vars": vars,
        })
    }

    /// Every tool call in the order the agents announced them (`tool_call`
    /// progress events), enriched with the provider and tool name from the
    /// agent's `tool_calls` output.
    pub fn tool_calls(&self) -> Vec<ToolCallRecord> {
        let mut calls = Vec::new();
        for event in &self.events {
            match event {
                Event::NodeProgress { node_id, name, data, .. } if name == "tool_call" => {
                    let data = data.to_json();
                    calls.push(ToolCallRecord {
                        node_id: node_id.to_string(),
                        id: text(&data, "id"),
                        name: text(&data, "name"),
                        arguments: data.get("arguments").cloned().unwrap_or(Json::Null),
                        ..ToolCallRecord::default()
                    });
                }
                Event::NodeSucceeded { node_id, outputs, .. } => {
                    let Some(log) = outputs.get("tool_calls").map(graphrt_core::Value::to_json) else { continue };
                    for entry in log.as_array().into_iter().flatten() {
                        let name = text(entry, "name");
                        let pending = calls
                            .iter_mut()
                            .find(|c| c.node_id == node_id.as_ref() && c.name == name && c.is_error.is_none());
                        if let Some(call) = pending {
                            call.provider = text(entry, "provider");
                            call.tool = text(entry, "tool");
                            call.is_error = entry.get("is_error").and_then(Json::as_bool);
                        }
                    }
                }
                _ => {}
            }
        }
        calls
    }

    /// Number of agent steps (model turns: `agent_step` progress events).
    pub fn agent_steps(&self) -> usize {
        self.progress_count("agent_step")
    }

    /// Number of node executions that finished (succeeded or failed).
    pub fn node_executions(&self) -> usize {
        self.events.iter().filter(|e| matches!(e, Event::NodeSucceeded { .. } | Event::NodeFailed { .. })).count()
    }

    fn progress_count(&self, wanted: &str) -> usize {
        self.events.iter().filter(|e| matches!(e, Event::NodeProgress { name, .. } if name == wanted)).count()
    }

    /// Hits of every `knowledge-retrieval` node, in rank order (nodes in the
    /// order they finished).
    pub fn retrieval_hits(&self) -> Vec<RetrievalHit> {
        let mut retrieval_execs = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for event in &self.events {
            match event {
                Event::NodeStarted { exec_id, kind, .. } if kind.as_ref() == "knowledge-retrieval" => {
                    retrieval_execs.insert(exec_id.clone());
                }
                Event::NodeSucceeded { node_id, exec_id, outputs, .. } if retrieval_execs.contains(exec_id) => {
                    let Some(result) = outputs.get("result").map(graphrt_core::Value::to_json) else { continue };
                    for hit in result.as_array().into_iter().flatten() {
                        hits.push(RetrievalHit {
                            node_id: node_id.to_string(),
                            id: text(hit, "id"),
                            document_id: text(hit, "document_id"),
                            score: hit.get("score").and_then(Json::as_f64),
                            content: text(hit, "content"),
                        });
                    }
                }
                _ => {}
            }
        }
        hits
    }
}

fn text(value: &Json, key: &str) -> String {
    match value.get(key) {
        Some(Json::String(s)) => s.clone(),
        Some(Json::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}
