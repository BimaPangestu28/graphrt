//! How a caller receives its run: the delivery mode, the event scope, and the
//! JSON result of a blocking request.

use std::sync::Arc;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_core::Event;

use super::errors::error_response;
use crate::store::live::{self, ActiveRun};

/// How the caller wants the result of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Delivery {
    /// Wait and return the final JSON result.
    Blocking,
    /// Server-sent events.
    Stream,
    /// `202` immediately.
    Detached,
}

impl Delivery {
    pub(super) fn of(stream: bool, run_async: bool) -> Self {
        if run_async {
            Delivery::Detached
        } else if stream {
            Delivery::Stream
        } else {
            Delivery::Blocking
        }
    }
}

/// How much of a run the caller receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventScope {
    /// Every engine event and the full result (API-key callers, `--user-events all`).
    Full,
    /// The answer stream, the final result and pause prompts (end users by default).
    #[cfg(feature = "jwt")]
    Answer,
}

/// How the caller receives a run: the delivery mode and the event scope.
#[derive(Clone, Copy, Debug)]
pub(super) struct Reply {
    pub(super) delivery: Delivery,
    pub(super) scope: EventScope,
}

/// Aborts the run when a synchronous caller (blocking or streaming) goes away before
/// the run finished. Detached runs never get one.
pub(super) struct AbortOnDrop(pub(super) Arc<ActiveRun>);

/// Kept alive by a run's SSE response for as long as the client is connected.
/// What happens when it is dropped is the guard's business: [`AbortOnDrop`]
/// aborts the run, the gateway's reclaim guard holds it as an orphan.
pub(crate) trait StreamGuard: Send + 'static {
    /// The run's terminal event was handed to the response.
    fn terminal_sent(&self) {}
}

impl StreamGuard for AbortOnDrop {}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if !self.0.is_done() {
            self.0.handle().abort("client disconnected");
        }
    }
}

/// The JSON body returned to blocking callers.
pub(super) fn result_json(run_id: &str, outcome: live::Outcome, scope: EventScope) -> Response {
    let answer = outcome.answer;
    let body = match outcome.terminal {
        Some(Event::RunSucceeded { outputs, usage, handled_errors, elapsed_ms }) => serde_json::json!({
            "run_id": run_id, "status": "succeeded", "outputs": outputs, "answer": answer,
            "usage": usage, "handled_errors": handled_errors, "elapsed_ms": elapsed_ms,
        }),
        Some(Event::RunFailed { error, error_type, usage, elapsed_ms }) => serde_json::json!({
            "run_id": run_id, "status": "failed", "error": error, "error_type": error_type,
            "usage": usage, "elapsed_ms": elapsed_ms,
        }),
        Some(Event::RunAborted { reason, elapsed_ms }) => serde_json::json!({
            "run_id": run_id, "status": "aborted", "reason": reason, "elapsed_ms": elapsed_ms,
        }),
        Some(Event::RunPaused { reasons, snapshot }) => serde_json::json!({
            "run_id": run_id, "status": "paused", "reasons": reasons, "snapshot": snapshot, "answer": answer,
        }),
        _ => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "run ended without a terminal event"),
    };
    match scope {
        EventScope::Full => Json(body).into_response(),
        #[cfg(feature = "jwt")]
        EventScope::Answer => Json(crate::user_auth::events::result(body)).into_response(),
    }
}
