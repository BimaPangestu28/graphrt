//! Server-sent events: engine events as SSE messages, and the `response`
//! events of the answer assembler for streaming callers.

use std::convert::Infallible;

use axum::response::sse::Event as SseEvent;
use futures::{Stream, StreamExt};
use graphrt_core::Event;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::store::{StoredEvent, stored_event_type};

use super::delivery::{AbortOnDrop, EventScope};
use crate::store::live;

/// Events from a broadcast receiver until the terminal event.
pub(super) fn live_events(
    rx: tokio::sync::broadcast::Receiver<live::SharedEvent>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    futures::stream::unfold((rx, false), |(mut rx, finished)| async move {
        if finished {
            return None;
        }
        match rx.recv().await {
            Ok(event) => {
                let terminal = event.is_terminal();
                Some((Ok(sse_event(&event)), (rx, terminal)))
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                let data = serde_json::json!({ "type": "lagged", "skipped": skipped });
                Some((Ok(SseEvent::default().event("lagged").data(data.to_string())), (rx, false)))
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
        }
    })
}

/// The SSE stream of a streaming caller: the run's events within `scope`,
/// each preceded by the `response` events of the answer text it completes.
/// Holding `guard` aborts the run when the caller disconnects early.
pub(super) fn sse_stream(
    subscription: live::Subscription,
    mut assembler: ResponseAssembler,
    guard: AbortOnDrop,
    scope: EventScope,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    let backlog = futures::stream::iter(subscription.backlog);
    let events =
        if subscription.done { backlog.boxed() } else { backlog.chain(skipping_lags(subscription.live)).boxed() };
    let mut chunks = Vec::new();
    until_terminal(events).flat_map(move |event| {
        let _keep_alive = &guard;
        let mut out = Vec::new();
        assembler.push_into(&event, &mut chunks);
        out.extend(chunks.drain(..).map(|chunk| Ok(response_sse(&chunk))));
        if let Some(message) = scoped_sse_event(&event, scope) {
            out.push(Ok(message));
        }
        futures::stream::iter(out)
    })
}

/// Live events of a run. A streaming caller that cannot keep up skips events,
/// like any lagging listener.
fn skipping_lags(
    rx: tokio::sync::broadcast::Receiver<live::SharedEvent>,
) -> impl Stream<Item = live::SharedEvent> + Send + 'static {
    futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => return Some((event, rx)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

/// `events` up to and including the terminal event. The broadcast channel
/// itself stays open while the run is tracked, so waiting for it to close
/// would hang.
fn until_terminal(
    events: impl Stream<Item = live::SharedEvent> + Send + Unpin + 'static,
) -> impl Stream<Item = live::SharedEvent> + Send + 'static {
    futures::stream::unfold((events, false), |(mut events, finished)| async move {
        if finished {
            return None;
        }
        let event = events.next().await?;
        let terminal = event.is_terminal();
        Some((event, (events, terminal)))
    })
}

/// A `response` SSE message: a piece of the ordered answer text.
fn response_sse(chunk: &graphrt_core::response::ResponseChunk) -> SseEvent {
    let data = serde_json::json!({ "node_id": chunk.node_id, "text": chunk.text, "is_final": chunk.is_final });
    SseEvent::default().event("response").data(data.to_string())
}

/// The SSE message for `event` within `scope`; `None` when the caller does not see it.
fn scoped_sse_event(event: &Event, scope: EventScope) -> Option<SseEvent> {
    match scope {
        EventScope::Full => Some(sse_event(event)),
        #[cfg(feature = "jwt")]
        EventScope::Answer => crate::user_auth::events::sse_event(event),
    }
}

/// A live event; unlike stored ones, `run_paused` keeps its snapshot here.
pub(super) fn sse_event(event: &Event) -> SseEvent {
    let text = serde_json::to_string(event).unwrap_or_else(|_| "null".to_owned());
    match serde_json::value::RawValue::from_string(text) {
        Ok(json) => stored_sse(&json),
        Err(_) => SseEvent::default().event("event").data("null"),
    }
}

/// One SSE message per event, named after the event's `type`.
pub(super) fn stored_sse(event: &StoredEvent) -> SseEvent {
    let name = stored_event_type(event).unwrap_or("event");
    SseEvent::default().event(name).data(event.get())
}
