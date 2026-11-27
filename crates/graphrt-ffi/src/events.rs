//! Typed run events for [`RunListener`]: what an app shows (answer text, tool
//! activity, UI blocks, requests for approval, the result) without parsing JSON.
//!
//! Every run started with `WorkflowHandle.start` or `resumePaused` ends with
//! exactly one of [`RunEvent::Paused`] or [`RunEvent::Finished`].

use std::sync::Arc;

use graphrt_core::{Event, Value};

use crate::approvals::PausedRun;

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RunStatus {
    Succeeded,
    Failed,
    Aborted,
    Paused,
}

impl RunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Aborted => "aborted",
            RunStatus::Paused => "paused",
        }
    }

    pub(crate) fn of(terminal: Option<&Event>) -> Self {
        match terminal {
            Some(Event::RunSucceeded { .. }) => RunStatus::Succeeded,
            Some(Event::RunAborted { .. }) => RunStatus::Aborted,
            Some(Event::RunPaused { .. }) => RunStatus::Paused,
            _ => RunStatus::Failed,
        }
    }
}

/// The result of a run that did not pause.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RunResult {
    pub status: RunStatus,
    /// The whole answer text (what the `AnswerChunk` events carried).
    pub answer: String,
    /// The run's outputs (`end` node), as a JSON object.
    pub outputs_json: String,
    /// Why the run failed or was aborted.
    pub error: Option<String>,
    /// Machine readable category of a failure (`timeout`, `model`, `limit_exceeded`...).
    pub error_type: Option<String>,
    /// The same result as `RunHandle.waitJson` returns it.
    pub result_json: String,
}

/// A UI block an agent produced, checked against its schema.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct UiBlock {
    /// `adaptive_card`, `chart`, `table`, `stats` or `diagram`.
    pub kind: String,
    pub title: Option<String>,
    /// The block as JSON (`{"$ui": "...", "version": 1, ...}`).
    pub json: String,
}

/// An event of a run.
#[derive(Clone, Debug, uniffi::Enum)]
pub enum RunEvent {
    Started {
        run_id: String,
        resumed: bool,
    },
    NodeStarted {
        node_id: String,
        kind: String,
        title: String,
    },
    NodeSucceeded {
        node_id: String,
        elapsed_ms: u64,
    },
    /// A node failed; with `handled` its error strategy kept the run going.
    NodeFailed {
        node_id: String,
        error: String,
        error_type: String,
        handled: bool,
    },
    /// An agent called a tool.
    ToolCalled {
        node_id: String,
        call_id: String,
        name: String,
        arguments_json: String,
    },
    /// A tool call finished.
    ToolFinished {
        node_id: String,
        call_id: String,
        name: String,
        is_error: bool,
    },
    /// A held tool call was not approved.
    ToolDenied {
        node_id: String,
        call_id: String,
        name: String,
    },
    /// User facing answer text, in order, as it is produced.
    AnswerChunk {
        text: String,
    },
    /// A UI block in a node's outputs that passed validation (feature `ui`).
    Ui {
        node_id: String,
        block: UiBlock,
    },
    /// A UI block that failed validation and must not be shown.
    UiRejected {
        node_id: String,
        error: String,
    },
    /// The run paused and waits for answers. Terminal.
    Paused {
        run: Arc<PausedRun>,
    },
    /// The run ended. Terminal.
    Finished {
        result: RunResult,
    },
}

/// Receives the typed events of one run. Implemented in the host language.
///
/// Called on a background thread, in order; marshal to the UI thread yourself.
/// Exceptions thrown here are swallowed so they cannot break the run.
#[uniffi::export(with_foreign)]
pub trait RunListener: Send + Sync {
    fn on_event(&self, event: RunEvent);
}

fn json_string(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned()
}

/// The typed events of one engine event, appended to `out`. Answer chunks and
/// the terminal event are produced by the caller, which assembles the answer.
pub(crate) fn translate(event: &Event, out: &mut Vec<RunEvent>) {
    match event {
        Event::RunStarted { run_id, resumed } => {
            out.push(RunEvent::Started { run_id: run_id.clone(), resumed: *resumed })
        }
        Event::NodeStarted { node_id, kind, title, .. } => out.push(RunEvent::NodeStarted {
            node_id: node_id.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
        }),
        Event::NodeSucceeded { node_id, outputs, elapsed_ms, .. } => {
            out.push(RunEvent::NodeSucceeded { node_id: node_id.to_string(), elapsed_ms: *elapsed_ms });
            if let Ok(json) = serde_json::to_value(outputs) {
                ui_events(node_id, &Value::from_json(json), out);
            }
        }
        Event::NodeFailed { node_id, error, error_type, handled, .. } => out.push(RunEvent::NodeFailed {
            node_id: node_id.to_string(),
            error: error.clone(),
            error_type: error_type.clone(),
            handled: *handled,
        }),
        Event::NodeProgress { node_id, name, data, .. } => {
            let data = data.to_json();
            let node_id = node_id.to_string();
            let call_id = json_string(&data, "id");
            let tool = json_string(&data, "name");
            match name.as_str() {
                "tool_call" => out.push(RunEvent::ToolCalled {
                    node_id,
                    call_id,
                    name: tool,
                    arguments_json: data.get("arguments").map(|a| a.to_string()).unwrap_or_else(|| "{}".to_owned()),
                }),
                "tool_result" => out.push(RunEvent::ToolFinished {
                    node_id,
                    call_id,
                    name: tool,
                    is_error: data.get("is_error").and_then(serde_json::Value::as_bool).unwrap_or(false),
                }),
                "tool_denied" => out.push(RunEvent::ToolDenied { node_id, call_id, name: tool }),
                _ => {}
            }
        }
        _ => {}
    }
}

#[cfg(feature = "ui")]
fn ui_events(node_id: &str, outputs: &Value, out: &mut Vec<RunEvent>) {
    for block in graphrt_core::ui::collect_ui_blocks(outputs) {
        match crate::ui::checked(&block.to_json()) {
            Ok(block) => out.push(RunEvent::Ui { node_id: node_id.to_owned(), block }),
            Err(error) => out.push(RunEvent::UiRejected { node_id: node_id.to_owned(), error }),
        }
    }
}

#[cfg(not(feature = "ui"))]
fn ui_events(_node_id: &str, _outputs: &Value, _out: &mut Vec<RunEvent>) {}

/// The terminal typed event of a run.
pub(crate) fn terminal(terminal: Option<&Event>, answer: &str, result_json: String) -> RunEvent {
    if let Some(Event::RunPaused { reasons, snapshot }) = terminal {
        return RunEvent::Paused { run: Arc::new(PausedRun::new((**snapshot).clone(), reasons.clone())) };
    }
    let status = RunStatus::of(terminal);
    let (outputs_json, error, error_type) = match terminal {
        Some(Event::RunSucceeded { outputs, .. }) => {
            (serde_json::to_string(outputs).unwrap_or_else(|_| "{}".to_owned()), None, None)
        }
        Some(Event::RunFailed { error, error_type, .. }) => {
            ("{}".to_owned(), Some(error.clone()), Some(error_type.clone()).filter(|t| !t.is_empty()))
        }
        Some(Event::RunAborted { reason, .. }) => ("{}".to_owned(), Some(reason.clone()), None),
        _ => ("{}".to_owned(), Some("run ended without a terminal event".to_owned()), Some("internal".to_owned())),
    };
    RunEvent::Finished {
        result: RunResult { status, answer: answer.to_owned(), outputs_json, error, error_type, result_json },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn progress(name: &str, data: serde_json::Value) -> Event {
        Event::NodeProgress {
            frame: "".into(),
            node_id: "assistant".into(),
            exec_id: "e1".into(),
            name: name.to_owned(),
            data: Value::from_json(data),
        }
    }

    #[test]
    fn tool_progress_becomes_tool_events() {
        let mut out = Vec::new();
        translate(&progress("tool_call", json!({"id": "c1", "name": "device__battery", "arguments": {}})), &mut out);
        translate(&progress("tool_result", json!({"id": "c1", "name": "device__battery", "is_error": true})), &mut out);
        translate(&progress("tool_denied", json!({"id": "c2", "name": "device__send"})), &mut out);
        translate(&progress("agent_step", json!({})), &mut out);
        assert!(
            matches!(&out[0], RunEvent::ToolCalled { call_id, arguments_json, .. } if call_id == "c1" && arguments_json == "{}")
        );
        assert!(matches!(&out[1], RunEvent::ToolFinished { is_error: true, .. }));
        assert!(matches!(&out[2], RunEvent::ToolDenied { call_id, .. } if call_id == "c2"));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn a_failed_run_carries_its_error() {
        let failed = Event::RunFailed {
            error: "boom".into(),
            error_type: "model".into(),
            usage: graphrt_core::event::Usage::default(),
            elapsed_ms: 1,
        };
        let RunEvent::Finished { result } = terminal(Some(&failed), "partial", "{}".into()) else {
            unreachable!("a failed run finishes");
        };
        assert_eq!(result.status, RunStatus::Failed);
        assert_eq!((result.error.as_deref(), result.error_type.as_deref()), (Some("boom"), Some("model")));
        assert_eq!(result.answer, "partial");
    }
}
