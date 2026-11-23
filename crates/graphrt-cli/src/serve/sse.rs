//! Server-sent events: engine events as SSE messages, and the `response`
//! events of the answer assembler for streaming callers.
//!
//! Every message about a real event carries an `id:` from [`crate::sse_ids`]
//! (its index in the run's event log), so a reconnecting client can name the
//! last event it saw (`Last-Event-ID`) and get exactly the rest.

use std::convert::Infallible;

use axum::response::sse::Event as SseEvent;
use futures::{Stream, StreamExt};
use graphrt_core::Event;
use graphrt_core::response::{ResponseAssembler, ResponseChunk};
use graphrt_core::store::{StoredEvent, stored_event_type};

use super::delivery::{EventScope, StreamGuard};
use crate::sse_ids::{Cursor, chunk_id, event_id};
use crate::store::live;

/// A [`live::Followed`] item with the event's index in the run's log.
pub(super) enum Numbered {
    Event(live::SharedEvent, u64),
    Lagged(u64),
}

/// Number the events of [`live::follow`], whose first event has index
/// `first_index`; events reported lagged keep their indices (a gap in the ids).
pub(super) fn numbered(
    events: impl Stream<Item = live::Followed> + Send + 'static,
    first_index: u64,
) -> impl Stream<Item = Numbered> + Send + 'static {
    events.scan(first_index, |next, followed| {
        let item = match followed {
            live::Followed::Event(event) => {
                *next += 1;
                Numbered::Event(event, *next - 1)
            }
            live::Followed::Lagged(skipped) => {
                *next += skipped;
                Numbered::Lagged(skipped)
            }
        };
        futures::future::ready(Some(item))
    })
}

/// The SSE response of a streaming run start or resume: response text and
/// engine events (from [`live::follow`], whose first event has index
/// `first_index`) with their `id:`s until the terminal one, skipping what
/// `cursor` excludes. The assembler sees every event, so the text after a
/// cursor is exactly what an uninterrupted stream would have sent. `guard`
/// lives as long as the stream: [`super::delivery::AbortOnDrop`] aborts the
/// run when the caller disconnects early.
pub(crate) fn sse_stream(
    events: impl Stream<Item = live::Followed> + Send + 'static,
    mut assembler: ResponseAssembler,
    first_index: u64,
    cursor: Cursor,
    // Boxed: one copy of this stream in the binary for every kind of guard.
    guard: Box<dyn StreamGuard>,
    scope: EventScope,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    // `follow` ends right after the terminal event and refills a lagging caller
    // from the run's feed, so the answer text stays complete. The first event
    // comes from the guard, so every caller gets one and the same stream type.
    let first = futures::stream::iter(guard.first_event().map(Ok));
    let mut chunks = Vec::new();
    first.chain(numbered(events, first_index).flat_map(move |item| {
        let (event, index) = match item {
            Numbered::Event(event, index) => (event, index),
            Numbered::Lagged(skipped) => return futures::stream::iter(vec![Ok(lagged_sse(skipped))]),
        };
        let mut out = Vec::new();
        assembler.push_into(&event, &mut chunks);
        for (k, chunk) in chunks.drain(..).enumerate() {
            if cursor.admits(index, Some(k)) {
                out.push(Ok(response_sse(&chunk, index, k)));
            }
        }
        if cursor.admits(index, None) {
            if let Some(message) = scoped_sse_event(&event, index, scope) {
                out.push(Ok(message));
            }
            if event.is_terminal() {
                guard.terminal_sent();
            }
        }
        futures::stream::iter(out)
    }))
}

/// The `lagged` SSE event: `skipped` events are gone for this listener (it fell
/// further behind than the in-memory feed keeps). Fetch them with `?from=` later.
/// It carries no `id:`, so `Last-Event-ID` keeps naming the last real event.
pub(super) fn lagged_sse(skipped: u64) -> SseEvent {
    let data = serde_json::json!({ "type": "lagged", "skipped": skipped });
    SseEvent::default().event("lagged").data(data.to_string())
}

/// A `response` SSE message: the `k`-th piece of the ordered answer text
/// assembled from the event at `index`.
fn response_sse(chunk: &ResponseChunk, index: u64, k: usize) -> SseEvent {
    let data = serde_json::json!({ "node_id": chunk.node_id, "text": chunk.text, "is_final": chunk.is_final });
    SseEvent::default().event("response").id(chunk_id(index, k)).data(data.to_string())
}

/// The SSE message for `event` (at `index`) within `scope`; `None` when the
/// caller does not see it.
fn scoped_sse_event(event: &Event, index: u64, scope: EventScope) -> Option<SseEvent> {
    match scope {
        EventScope::Full => Some(sse_event(event, index)),
        #[cfg(feature = "jwt")]
        EventScope::Answer => crate::user_auth::events::sse_event(event).map(|m| m.id(event_id(index))),
    }
}

/// A live event with its index as `id:`; unlike stored ones, `run_paused` keeps
/// its snapshot here.
pub(super) fn sse_event(event: &Event, index: u64) -> SseEvent {
    let text = serde_json::to_string(event).unwrap_or_else(|_| "null".to_owned());
    match serde_json::value::RawValue::from_string(text) {
        Ok(json) => stored_sse(&json, index),
        Err(_) => SseEvent::default().event("event").id(event_id(index)).data("null"),
    }
}

/// One SSE message per event, named after the event's `type`, with its index as `id:`.
pub(super) fn stored_sse(event: &StoredEvent, index: u64) -> SseEvent {
    let name = stored_event_type(event).unwrap_or("event");
    SseEvent::default().event(name).id(event_id(index)).data(event.get())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use axum::response::IntoResponse;
    use axum::response::sse::Sse;
    use graphrt_core::engine::{Limits, RunInput};
    use graphrt_core::store::RunRecord;

    use super::super::build::{build_state, discover};
    use super::super::delivery::{AbortOnDrop, EventScope};
    use super::super::execute::{launch, new_assembler};
    use super::sse_stream;
    use crate::store::live;

    /// Property: a stream started at any cursor (after any engine event, or in the
    /// middle of an event's response text) sends exactly the rest of what an
    /// uninterrupted stream sends: same ids, same names, same response text.
    #[tokio::test]
    async fn a_stream_from_any_cursor_is_the_exact_rest_of_the_full_stream() {
        use http_body_util::BodyExt;
        let dir = std::env::temp_dir().join(format!("graphrt-cursor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("chat.yaml"),
            "version: \"1\"\nname: chat\ngraph:\n  nodes:\n    - { id: start, type: start, variables: [] }\n\
             \x20   - { id: a, type: answer, answer: \"one \" }\n    - { id: b, type: answer, answer: \"two\" }\n\
             \x20 edges:\n    - { source: start, target: a }\n    - { source: a, target: b }\n",
        )
        .unwrap();
        let files = discover(&dir).unwrap();
        let options = crate::setup::ServiceOptions {
            plugins: crate::extensions::PluginOptions { no_default: true, ..Default::default() },
            ..crate::setup::ServiceOptions::default()
        };
        let state = std::sync::Arc::new(build_state(&files, &options, Limits::default(), None).await.unwrap());
        // (id, event name, data unless it holds timings).
        let stream = |cursor: crate::sse_ids::Cursor| {
            let state = std::sync::Arc::clone(&state);
            async move {
                let workflow = state.workflows.get("chat").unwrap();
                let record = RunRecord::running(state.new_run_id(), "chat", workflow.engine.graph().fingerprint(), 1);
                state.store.create(record.clone()).await.unwrap();
                let input = RunInput::new();
                let assembler = new_assembler(&workflow.engine, &input);
                let active = launch(&state, &workflow.engine, record, input, None);
                let events = live::follow(Arc::clone(&active), active.subscribe());
                let sse =
                    Sse::new(sse_stream(events, assembler, 7, cursor, Box::new(AbortOnDrop(active)), EventScope::Full));
                let body = sse.into_response().into_body().collect().await.unwrap().to_bytes();
                let text = String::from_utf8(body.to_vec()).unwrap();
                text.split("\n\n")
                    .filter(|block| block.contains("event:"))
                    .map(|block| {
                        let field = |name: &str| {
                            block
                                .lines()
                                .find_map(|l| l.strip_prefix(name))
                                .map(|v| v.trim().to_owned())
                                .unwrap_or_default()
                        };
                        let data = if field("event:") == "response" { field("data:") } else { String::new() };
                        (field("id:"), field("event:"), data)
                    })
                    .collect::<Vec<_>>()
            }
        };
        let full = stream(crate::sse_ids::Cursor::ALL).await;
        assert!(full.iter().any(|(id, ..)| id.contains('.')), "the stream has response text: {full:?}");
        assert_eq!(full.first().map(|(id, ..)| id.as_str()), Some("7"), "ids start at the first index");
        for (at, (last, ..)) in full.iter().enumerate() {
            let rest = stream(crate::sse_ids::Cursor::after(last).unwrap()).await;
            assert_eq!(rest, full[at + 1..], "after {last}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
