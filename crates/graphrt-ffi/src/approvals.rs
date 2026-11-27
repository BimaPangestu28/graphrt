//! What a paused run asks, and the answers that resume it.
//!
//! A run pauses when an `agent` node holds tool calls for approval
//! (`require_approval`) or a `human-input` node waits for a form. A
//! [`PausedRun`] lists those requests as typed records and collects the
//! answers; [`crate::WorkflowHandle::resume_paused`] resumes the run with them.
//! [`PausedRun::to_json`] is what an app persists to resume later, even after
//! a restart ([`PausedRun::from_json`]).

use std::sync::{Mutex, PoisonError};

use graphrt_core::Value;
use graphrt_core::event::PauseReason;
use graphrt_core::snapshot::Snapshot;

use crate::GraphrtError;

/// What a request asks for.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RequestKind {
    /// An `agent` node holds tool calls until someone approves or rejects them.
    ToolApproval,
    /// A `human-input` node waits for a form: an action and field values.
    HumanInput,
    /// A pause of another kind (`data_json` holds the details).
    Other { name: String },
}

/// A tool call waiting for approval.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PendingToolCall {
    /// The call's id, for [`PausedRun::decide_tool_call`].
    pub id: String,
    /// The tool as the model sees it.
    pub name: String,
    /// The arguments, as JSON (secret values masked).
    pub arguments_json: String,
}

/// A field of a `human-input` form.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FormField {
    pub name: String,
    pub label: String,
    /// `text`, `paragraph`, `number`, `select`, `file`... as the node declares it.
    pub field_type: String,
    pub required: bool,
    /// The default value as JSON, if any.
    pub default_json: Option<String>,
}

/// A button of a `human-input` form.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FormAction {
    pub id: String,
    pub title: String,
}

/// One node of a paused run that waits for an answer.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ApprovalRequest {
    /// The key to answer under: the node id, or `<frame>/<node_id>` inside containers.
    pub key: String,
    pub node_id: String,
    pub kind: RequestKind,
    /// The text shown to the person (human input).
    pub prompt: Option<String>,
    /// Form fields (human input).
    pub fields: Vec<FormField>,
    /// Form buttons (human input).
    pub actions: Vec<FormAction>,
    /// Calls waiting for approval (tool approval).
    pub tool_calls: Vec<PendingToolCall>,
    /// How long the request may wait, when the node sets a limit.
    pub timeout_ms: Option<u64>,
    /// A UI card to show with the request, as JSON, when the node has one.
    pub card_json: Option<String>,
    /// The raw pause data, for kinds this record does not model.
    pub data_json: String,
}

fn str_at(value: &Value, path: &[&str]) -> Option<String> {
    value.get_path(path).as_ref().and_then(Value::as_str).map(str::to_owned)
}

fn array_at(value: &Value, key: &str) -> Vec<serde_json::Value> {
    match value.get_path(&[key]).map(|v| v.to_json()) {
        Some(serde_json::Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

fn json_str(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned()
}

impl ApprovalRequest {
    /// The request of one pause reason; `None` for a pause nobody answers
    /// (paused from outside with `RunHandle.pause`).
    pub(crate) fn of(reason: &PauseReason) -> Option<Self> {
        let node_id = reason.node_id.as_deref()?;
        let key = if reason.frame.is_empty() { node_id.to_owned() } else { format!("{}/{node_id}", reason.frame) };
        let kind = match reason.kind.as_str() {
            "tool_approval" => RequestKind::ToolApproval,
            "human_input" => RequestKind::HumanInput,
            other => RequestKind::Other { name: other.to_owned() },
        };
        let data = &reason.data;
        let tool_calls = array_at(data, "calls")
            .iter()
            .map(|call| PendingToolCall {
                id: json_str(call, "id"),
                name: json_str(call, "name"),
                arguments_json: call.get("arguments").map(|a| a.to_string()).unwrap_or_else(|| "{}".to_owned()),
            })
            .collect();
        let fields = array_at(data, "fields")
            .iter()
            .map(|field| FormField {
                name: json_str(field, "name"),
                label: json_str(field, "label"),
                field_type: json_str(field, "type"),
                required: field.get("required").and_then(serde_json::Value::as_bool).unwrap_or(false),
                default_json: field.get("default").filter(|d| !d.is_null()).map(|d| d.to_string()),
            })
            .collect();
        let actions = array_at(data, "actions")
            .iter()
            .map(|action| FormAction { id: json_str(action, "id"), title: json_str(action, "title") })
            .collect();
        let timeout_ms = data
            .get_path(&["timeout_ms"])
            .as_ref()
            .and_then(Value::as_i64)
            .and_then(|t| u64::try_from(t).ok())
            .filter(|t| *t > 0);
        let card_json = data.get_path(&["card"]).filter(|c| !c.is_null()).map(|c| c.to_json().to_string());
        Some(Self {
            key,
            node_id: node_id.to_owned(),
            kind,
            prompt: str_at(data, &["prompt"]),
            fields,
            actions,
            tool_calls,
            timeout_ms,
            card_json,
            data_json: data.to_json().to_string(),
        })
    }
}

/// A paused run: its requests, the answers given so far and its snapshot.
#[derive(Debug, uniffi::Object)]
pub struct PausedRun {
    snapshot: Snapshot,
    reasons: Vec<PauseReason>,
    requests: Vec<ApprovalRequest>,
    answers: Mutex<serde_json::Map<String, serde_json::Value>>,
}

impl PausedRun {
    pub(crate) fn new(snapshot: Snapshot, reasons: Vec<PauseReason>) -> Self {
        let requests = reasons.iter().filter_map(ApprovalRequest::of).collect();
        Self { snapshot, reasons, requests, answers: Mutex::new(serde_json::Map::new()) }
    }

    pub(crate) fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub(crate) fn inputs(&self) -> serde_json::Map<String, serde_json::Value> {
        self.answers.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn request(&self, key: &str) -> Result<&ApprovalRequest, GraphrtError> {
        self.requests.iter().find(|r| r.key == key).ok_or_else(|| {
            let keys: Vec<&str> = self.requests.iter().map(|r| r.key.as_str()).collect();
            GraphrtError::input(format!("no request `{key}` in this paused run (requests: {keys:?})"))
        })
    }

    fn set(&self, key: &str, answer: serde_json::Value) {
        self.answers.lock().unwrap_or_else(PoisonError::into_inner).insert(key.to_owned(), answer);
    }

    /// Answer a form with the defaults of its fields and the action `action`.
    fn answer_form(
        &self,
        request: &ApprovalRequest,
        action: &str,
        values: serde_json::Value,
    ) -> Result<(), GraphrtError> {
        if !request.actions.iter().any(|a| a.id == action) {
            let ids: Vec<&str> = request.actions.iter().map(|a| a.id.as_str()).collect();
            return Err(GraphrtError::input(format!(
                "request `{}` has no action `{action}` (actions: {ids:?})",
                request.key
            )));
        }
        let mut merged = serde_json::Map::new();
        for field in &request.fields {
            if let Some(default) = field.default_json.as_deref().and_then(|d| serde_json::from_str(d).ok()) {
                merged.insert(field.name.clone(), default);
            }
        }
        match values {
            serde_json::Value::Object(given) => merged.extend(given),
            serde_json::Value::Null => {}
            _ => return Err(GraphrtError::input("values must be a JSON object")),
        }
        self.set(&request.key, serde_json::json!({ "action": action, "values": merged }));
        Ok(())
    }

    fn decide(&self, key: &str, approved: bool, reason: Option<String>) -> Result<(), GraphrtError> {
        let request = self.request(key)?;
        match &request.kind {
            RequestKind::ToolApproval => {
                let mut answer = serde_json::json!({ "approved": approved });
                if let (Some(reason), Some(fields)) = (reason, answer.as_object_mut()) {
                    fields.insert("reason".to_owned(), serde_json::Value::from(reason));
                }
                self.set(key, answer);
                Ok(())
            }
            RequestKind::HumanInput => {
                let action = if approved { "approve" } else { "reject" };
                self.answer_form(request, action, serde_json::Value::Null)
            }
            RequestKind::Other { name } => {
                Err(GraphrtError::input(format!("request `{key}` is a `{name}` pause; answer it with set_input")))
            }
        }
    }
}

#[uniffi::export]
impl PausedRun {
    /// A paused run from the result JSON of a run that ended with status
    /// `paused` (what `RunHandle.waitJson` and `EventListener.onFinish` return),
    /// or from what [`PausedRun::to_json`] returned.
    #[uniffi::constructor]
    pub fn from_json(json: String) -> Result<std::sync::Arc<Self>, GraphrtError> {
        #[derive(serde::Deserialize)]
        struct Paused {
            status: String,
            #[serde(default)]
            reasons: Vec<PauseReason>,
            snapshot: Option<Snapshot>,
        }
        let paused: Paused =
            serde_json::from_str(&json).map_err(|e| GraphrtError::input(format!("paused run: {e}")))?;
        if paused.status != "paused" {
            return Err(GraphrtError::input(format!("the run is `{}`, not paused", paused.status)));
        }
        let snapshot = paused.snapshot.ok_or_else(|| GraphrtError::input("paused run: no snapshot"))?;
        Ok(std::sync::Arc::new(Self::new(snapshot, paused.reasons)))
    }

    /// The paused run as JSON (requests and snapshot, not the answers), to store
    /// and read back with [`PausedRun::from_json`]. It holds the run's variables
    /// (never secrets): keep it in the app's private storage.
    pub fn to_json(&self) -> String {
        serde_json::json!({ "status": "paused", "reasons": self.reasons, "snapshot": self.snapshot }).to_string()
    }

    /// Run id of the paused run.
    pub fn run_id(&self) -> String {
        self.snapshot.run_id.clone()
    }

    /// The snapshot alone, for `WorkflowHandle.resume`.
    pub fn snapshot_json(&self) -> String {
        self.snapshot.to_json().unwrap_or_default()
    }

    /// Every request of the run, in the order the nodes paused.
    pub fn requests(&self) -> Vec<ApprovalRequest> {
        self.requests.clone()
    }

    /// Keys of the requests that have no answer yet. Resuming with requests
    /// unanswered keeps those nodes paused.
    pub fn unanswered(&self) -> Vec<String> {
        let answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        self.requests.iter().filter(|r| !answers.contains_key(&r.key)).map(|r| r.key.clone()).collect()
    }

    /// Approve request `key`: every held tool call runs, or a form is answered
    /// with its `approve` action and the fields' defaults.
    pub fn approve(&self, key: String, reason: Option<String>) -> Result<(), GraphrtError> {
        self.decide(&key, true, reason)
    }

    /// Reject request `key`: the model is told the calls were not approved (and
    /// why, with `reason`), or a form is answered with its `reject` action.
    pub fn reject(&self, key: String, reason: Option<String>) -> Result<(), GraphrtError> {
        self.decide(&key, false, reason)
    }

    /// Approve or reject one held tool call of request `key`. Calls without a
    /// decision keep the node paused.
    pub fn decide_tool_call(&self, key: String, call_id: String, approved: bool) -> Result<(), GraphrtError> {
        let request = self.request(&key)?;
        if request.kind != RequestKind::ToolApproval {
            return Err(GraphrtError::input(format!("request `{key}` holds no tool calls")));
        }
        if !request.tool_calls.iter().any(|c| c.id == call_id) {
            return Err(GraphrtError::input(format!("request `{key}` holds no call `{call_id}`")));
        }
        let mut answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = answers.entry(key).or_insert_with(|| serde_json::json!({}));
        if !entry.is_object() {
            *entry = serde_json::json!({});
        }
        if let Some(fields) = entry.as_object_mut() {
            let per_call = fields.entry("approvals").or_insert_with(|| serde_json::json!({}));
            if let Some(per_call) = per_call.as_object_mut() {
                per_call.insert(call_id, serde_json::Value::Bool(approved));
            }
        }
        Ok(())
    }

    /// Answer the form of request `key` with `action` (one of its actions) and
    /// field values as a JSON object; fields left out get their defaults.
    pub fn answer(&self, key: String, action: String, values_json: String) -> Result<(), GraphrtError> {
        let request = self.request(&key)?;
        if request.kind != RequestKind::HumanInput {
            return Err(GraphrtError::input(format!("request `{key}` is not a form")));
        }
        let values = if values_json.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&values_json).map_err(|e| GraphrtError::input(format!("values: {e}")))?
        };
        self.answer_form(request, &action, values)
    }

    /// Set the resume input of request `key` as raw JSON, for pause kinds the
    /// helpers above do not cover.
    pub fn set_input(&self, key: String, input_json: String) -> Result<(), GraphrtError> {
        self.request(&key)?;
        let input: serde_json::Value =
            serde_json::from_str(&input_json).map_err(|e| GraphrtError::input(format!("input: {e}")))?;
        self.set(&key, input);
        Ok(())
    }

    /// The resume inputs collected so far, as JSON (what `WorkflowHandle.resume` takes).
    pub fn inputs_json(&self) -> String {
        serde_json::Value::Object(self.inputs()).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphrt_core::event::FrameId;
    use serde_json::json;

    fn reason(node: &str, kind: &str, data: serde_json::Value) -> PauseReason {
        PauseReason {
            frame: FrameId::from(""),
            node_id: Some(node.to_owned()),
            kind: kind.to_owned(),
            data: Value::from_json(data),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            format: graphrt_core::snapshot::SNAPSHOT_FORMAT,
            run_id: "run-1".into(),
            graph_fingerprint: String::new(),
            elapsed_ms: 0,
            steps: 0,
            usage: graphrt_core::event::Usage::default(),
            handled_errors: 0,
            root: graphrt_core::snapshot::FrameSnapshot::default(),
        }
    }

    fn paused(reasons: &[PauseReason]) -> PausedRun {
        PausedRun::new(snapshot(), reasons.to_vec())
    }

    fn external() -> PauseReason {
        PauseReason { frame: FrameId::from(""), node_id: None, kind: "external".into(), data: Value::Null }
    }

    fn tool_pause() -> PauseReason {
        reason(
            "assistant",
            "tool_approval",
            json!({"calls": [
                {"id": "c1", "name": "device__send", "arguments": {"to": "Ada"}},
                {"id": "c2", "name": "device__call", "arguments": {}},
            ]}),
        )
    }

    fn form_pause() -> PauseReason {
        reason(
            "review",
            "human_input",
            json!({
                "prompt": "Ship it?",
                "fields": [{"name": "note", "label": "Note", "type": "text", "required": false, "default": "ok"}],
                "actions": [{"id": "approve", "title": "Approve"}, {"id": "reject", "title": "Reject"}],
                "timeout_ms": 60000,
            }),
        )
    }

    #[test]
    fn requests_are_typed() {
        let run = paused(&[tool_pause(), form_pause(), external()]);
        let requests = run.requests();
        assert_eq!(requests.len(), 2, "an external pause asks nobody");
        let tools = &requests[0];
        assert_eq!((tools.key.as_str(), &tools.kind), ("assistant", &RequestKind::ToolApproval));
        assert_eq!(tools.tool_calls[0].id, "c1");
        assert_eq!(tools.tool_calls[0].arguments_json, r#"{"to":"Ada"}"#);
        let form = &requests[1];
        assert_eq!(form.kind, RequestKind::HumanInput);
        assert_eq!(form.prompt.as_deref(), Some("Ship it?"));
        assert_eq!(form.fields[0].default_json.as_deref(), Some(r#""ok""#));
        assert_eq!(form.actions[1].id, "reject");
        assert_eq!(form.timeout_ms, Some(60_000));
        assert_eq!(run.unanswered(), ["assistant", "review"]);
    }

    #[test]
    fn approve_and_reject_build_the_inputs_each_node_reads() -> Result<(), GraphrtError> {
        let run = paused(&[tool_pause(), form_pause()]);
        run.reject("assistant".into(), Some("not now".into()))?;
        run.approve("review".into(), None)?;
        let inputs: serde_json::Value =
            serde_json::from_str(&run.inputs_json()).map_err(|e| GraphrtError::input(e.to_string()))?;
        assert_eq!(
            inputs,
            json!({
                "assistant": {"approved": false, "reason": "not now"},
                "review": {"action": "approve", "values": {"note": "ok"}},
            })
        );
        assert!(run.unanswered().is_empty());
        Ok(())
    }

    #[test]
    fn per_call_decisions_and_form_answers() -> Result<(), GraphrtError> {
        let run = paused(&[tool_pause(), form_pause()]);
        run.decide_tool_call("assistant".into(), "c1".into(), true)?;
        run.decide_tool_call("assistant".into(), "c2".into(), false)?;
        run.answer("review".into(), "reject".into(), r#"{"note": "too risky"}"#.into())?;
        let inputs: serde_json::Value =
            serde_json::from_str(&run.inputs_json()).map_err(|e| GraphrtError::input(e.to_string()))?;
        assert_eq!(inputs["assistant"], json!({"approvals": {"c1": true, "c2": false}}));
        assert_eq!(inputs["review"], json!({"action": "reject", "values": {"note": "too risky"}}));
        Ok(())
    }

    #[test]
    fn wrong_answers_are_refused() {
        let run = paused(&[tool_pause(), form_pause()]);
        assert!(run.approve("nope".into(), None).is_err());
        assert!(run.decide_tool_call("assistant".into(), "c9".into(), true).is_err());
        assert!(run.decide_tool_call("review".into(), "c1".into(), true).is_err());
        assert!(run.answer("review".into(), "publish".into(), "{}".into()).is_err());
        assert!(run.answer("review".into(), "approve".into(), "[1]".into()).is_err());
        assert!(run.answer("assistant".into(), "approve".into(), "{}".into()).is_err());
        assert!(run.inputs().is_empty());
    }

    #[test]
    fn a_paused_run_survives_a_round_trip() -> Result<(), GraphrtError> {
        let run = paused(&[tool_pause()]);
        let back = PausedRun::from_json(run.to_json())?;
        assert_eq!(back.requests(), run.requests());
        assert_eq!(back.run_id(), "run-1");
        Ok(())
    }

    #[test]
    fn a_result_that_is_not_paused_is_refused() {
        assert!(PausedRun::from_json(r#"{"status":"succeeded","answer":""}"#.into()).is_err());
        assert!(PausedRun::from_json(r#"{"status":"paused"}"#.into()).is_err());
        assert!(PausedRun::from_json("{".into()).is_err());
    }
}
