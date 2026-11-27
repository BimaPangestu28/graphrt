//! What a verified end user sees of a run they started (`--user-events
//! answer`, the default).
//!
//! Engine events carry far more than a chat client needs: node inputs and
//! outputs, retrieved documents, tool results, rendered prompts (content
//! progress when capture is on) and the pause snapshot with every node's
//! state. A user token holder is the least trusted caller of the API, so it
//! gets only:
//!
//! - the `response` events (the ordered answer text), produced separately by
//!   the serve handler;
//! - the run-level events: `run_started`, `run_succeeded` (outputs of the end
//!   node, usage), `run_failed` (`error_type` and usage, not the error text,
//!   which can quote URLs and upstream bodies), `run_aborted`, and `run_paused`
//!   with its reasons (the human-input forms to act on) but no snapshot.
//!
//! The blocking JSON result is cut down the same way. `--user-events all`
//! (operators only) restores full events.

use axum::response::sse::Event as SseEvent;
use graphrt_core::Event;
use graphrt_core::store::event_to_json;

/// Fields of the blocking result (see [`crate::serve`]) a user receives.
const RESULT_FIELDS: &[&str] =
    &["run_id", "status", "outputs", "answer", "usage", "reasons", "error_type", "reason", "elapsed_ms"];

/// The SSE message a user receives for `event`, or `None` for an internal event.
pub(crate) fn sse_event(event: &Event) -> Option<SseEvent> {
    let name = run_event_name(event)?;
    // `event_to_json` drops the snapshot of `run_paused`.
    let mut json = event_to_json(event);
    if let Some(object) = json.as_object_mut() {
        object.remove("error");
    }
    Some(SseEvent::default().event(name).data(json.to_string()))
}

/// The blocking JSON result restricted to what a user receives.
pub(crate) fn result(mut body: serde_json::Value) -> serde_json::Value {
    if let Some(object) = body.as_object_mut() {
        object.retain(|key, _| RESULT_FIELDS.contains(&key.as_str()));
    }
    body
}

/// Fields of a run record (`GET /v1/runs/{id}`) a user receives of their own run.
const RECORD_FIELDS: &[&str] = &[
    "run_id",
    "workflow",
    "status",
    "created_ms",
    "updated_ms",
    "outputs",
    "answer",
    "usage",
    "error_type",
    "event_count",
    "active",
    "pause_reasons",
    "resume_inputs",
    "session_id",
];

/// Names of the stored events a user receives.
const RUN_EVENTS: &[&str] = &["run_started", "run_succeeded", "run_failed", "run_aborted", "run_paused"];

/// A run record restricted to what a user receives: no error text, inputs,
/// trace ids or metadata.
pub(crate) fn record(mut body: serde_json::Value) -> serde_json::Value {
    if let Some(object) = body.as_object_mut() {
        object.retain(|key, _| RECORD_FIELDS.contains(&key.as_str()));
    }
    body
}

/// A stored event as a user receives it (its name and JSON), or `None` for an
/// internal event. Stored events never hold the pause snapshot.
pub(crate) fn stored_event(event: &graphrt_core::store::StoredEvent) -> Option<(&'static str, serde_json::Value)> {
    let kind = graphrt_core::store::stored_event_type(event)?;
    let name = RUN_EVENTS.iter().copied().find(|name| *name == kind)?;
    let mut json: serde_json::Value = serde_json::from_slice(event.get().as_bytes()).ok()?;
    if let Some(object) = json.as_object_mut() {
        object.remove("error");
    }
    Some((name, json))
}

/// SSE name of the run-level events a user receives.
fn run_event_name(event: &Event) -> Option<&'static str> {
    Some(match event {
        Event::RunStarted { .. } => "run_started",
        Event::RunSucceeded { .. } => "run_succeeded",
        Event::RunFailed { .. } => "run_failed",
        Event::RunAborted { .. } => "run_aborted",
        Event::RunPaused { .. } => "run_paused",
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use graphrt_core::Usage;
    use serde_json::json;

    use super::*;

    #[test]
    fn only_run_level_events_reach_users() {
        let chunk = Event::NodeChunk {
            frame: "".into(),
            node_id: "llm".into(),
            exec_id: "e".into(),
            field: "text".into(),
            chunk: "raw".into(),
        };
        assert!(sse_event(&chunk).is_none());
        let failed = Event::RunFailed {
            error: "http 500 from http://internal:9000/secret-path".into(),
            error_type: "http".into(),
            usage: Usage::default(),
            elapsed_ms: 3,
        };
        assert!(run_event_name(&failed).is_some());
        assert!(sse_event(&failed).is_some());
    }

    #[test]
    fn stored_events_and_records_are_cut_down_the_same_way() {
        let raw = |text: &str| serde_json::value::RawValue::from_string(text.to_owned()).unwrap();
        let failed = raw(r#"{"type":"run_failed","error":"http 500 from http://internal:9000","error_type":"http"}"#);
        let (name, json) = stored_event(&failed).unwrap();
        assert_eq!(name, "run_failed");
        assert_eq!(json, json!({ "type": "run_failed", "error_type": "http" }));
        assert!(stored_event(&raw(r#"{"type":"node_succeeded","outputs":{"secret":"x"}}"#)).is_none());
        assert!(stored_event(&raw("[]")).is_none());
        let kept = record(json!({
            "run_id": "r", "status": "failed", "error": "internal detail", "error_type": "http",
            "inputs": {"q": "x"}, "metadata": {"tenant": "acme"}, "trace_id": "t", "user_id": "u",
        }));
        assert_eq!(kept, json!({ "run_id": "r", "status": "failed", "error_type": "http" }));
    }

    #[test]
    fn results_keep_only_user_fields() {
        let body = json!({
            "run_id": "r", "status": "paused", "answer": "Hi", "reasons": [],
            "snapshot": {"root": {}}, "error": "internal detail", "handled_errors": 1,
        });
        let kept = result(body);
        assert_eq!(kept, json!({ "run_id": "r", "status": "paused", "answer": "Hi", "reasons": [] }));
    }
}
