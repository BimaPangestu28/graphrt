//! `GET /v1/runs/{run_id}/events`: stored events, then the live tail of a run
//! that still executes. A reconnecting SSE client resumes after its
//! `Last-Event-ID`.
//!
//! A verified user follows their own runs here and sees what `--user-events`
//! lets them see ([`EventScope`]). With `?responses=true` (always for a user
//! who sees the answer only) the stream also carries the answer text as
//! `response` events:
//!
//! - of the execution in progress, exactly as the caller that started it gets
//!   them, with the same ids (cargo feature `run-follow`);
//! - of a run that is not executing, as one `response` event with the stored
//!   answer just before the run's last event, when the stream starts at the
//!   run's first event.
//!
//! Stored events of earlier executions of a run (before a pause) come without
//! response text: a client that needs it reads `answer` of the run record.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use graphrt_core::store::{StoredEvent, event_to_stored};
use serde::Deserialize;

use super::auth::AuthenticatedUser;
use super::delivery::EventScope;
use super::errors::store_error;
use super::sse::{Numbered, lagged_sse, numbered, scoped_sse_event, scoped_stored_sse};
use super::state::AppState;
use crate::sse_ids::Cursor;
use crate::store::live::{self, ActiveRun};

/// Query of `GET /v1/runs/{id}/events`.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub from: u64,
    /// `false` returns stored events as a JSON array instead of SSE.
    pub stream: Option<bool>,
    /// `true` adds `response` events (the answer text) to the SSE stream.
    pub responses: Option<bool>,
}

impl EventsQuery {
    /// Reads `from` and `stream` from a raw query string.
    ///
    /// # Errors
    /// `400` naming the field that is not a number / boolean.
    #[allow(clippy::result_large_err)]
    pub fn from_query(raw: Option<&str>) -> Result<Self, Response> {
        let params = crate::http_input::QueryParams::parse(raw);
        Ok(Self {
            from: params.number("from")?.unwrap_or(0),
            stream: params.boolean("stream")?,
            responses: params.boolean("responses")?,
        })
    }
}

/// `GET /v1/runs/{run_id}/events`.
pub(super) async fn run_events(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    RawQuery(raw): RawQuery,
    user: Option<axum::Extension<AuthenticatedUser>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let query = match EventsQuery::from_query(raw.as_deref()) {
        Ok(query) => query,
        Err(refused) => return refused,
    };
    // A reconnecting SSE client names the last item it saw; the later of it and `?from=` wins.
    let cursor = match crate::sse_ids::cursor_of(&headers) {
        Some(cursor) if cursor.start >= query.from => cursor,
        _ => Cursor::from_index(query.from),
    };
    let scope = state.event_scope(user.is_some());
    let follow = Follow {
        cursor,
        json: query.stream == Some(false),
        // A caller that sees the answer only has nothing else to read it from.
        responses: query.responses == Some(true) || scope != EventScope::Full,
        scope,
    };
    events_response(&state, &run_id, follow).await
}

/// How a caller follows a run.
#[derive(Clone, Copy, Debug)]
struct Follow {
    /// Where the caller's stream starts.
    cursor: Cursor,
    /// A JSON array instead of SSE.
    json: bool,
    /// Send `response` events too.
    responses: bool,
    scope: EventScope,
}

/// The live part of a run that still executes: its listener's subscription,
/// and the index of the subscription's first event in the run's log.
struct LiveTail {
    run: Arc<ActiveRun>,
    first_index: u64,
    subscription: live::Subscription,
}

/// Stored events of `run_id` from the cursor on, followed by the live tail
/// while the run executes, each with its index as SSE `id:` (or a JSON array).
async fn events_response(state: &AppState, run_id: &str, follow: Follow) -> Response {
    let from = follow.cursor.start;
    // Take the live subscription first: events the driver stores after this point
    // reach us through it, and the stored part is cut at `stored_before`.
    let tail =
        state.active(run_id).map(|run| LiveTail { first_index: run.stored_before, subscription: run.subscribe(), run });
    let mut stored = match state.store.events(run_id, from).await {
        Ok(events) => events,
        Err(e) => return store_error(&e),
    };
    if let Some(tail) = &tail {
        let keep = usize::try_from(tail.first_index.saturating_sub(from)).unwrap_or(usize::MAX);
        stored.truncate(keep);
    }
    if follow.json {
        return events_json(stored, tail, from, follow.scope);
    }
    // The answer of a run that is not executing, for a stream from its first event.
    let stored_answer = if follow.responses && tail.is_none() && follow.cursor == Cursor::ALL {
        stored_answer(state, run_id).await
    } else {
        None
    };
    events_sse(stored, tail, follow, stored_answer)
}

/// The stored answer text of `run_id`, when it has one.
async fn stored_answer(state: &AppState, run_id: &str) -> Option<String> {
    #[cfg(feature = "run-follow")]
    return state
        .store
        .get(run_id)
        .await
        .ok()
        .flatten()
        .map(|record| record.answer)
        .filter(|answer| !answer.is_empty());
    #[cfg(not(feature = "run-follow"))]
    {
        let _ = (state, run_id);
        None
    }
}

/// A stored event as JSON within `scope`; `None` when the caller does not see it.
fn scoped_json(event: StoredEvent, scope: EventScope) -> Option<serde_json::Value> {
    match scope {
        EventScope::Full => serde_json::from_str(event.get()).ok(),
        #[cfg(feature = "jwt")]
        EventScope::Answer => crate::user_auth::events::stored_event(&event).map(|(_, json)| json),
    }
}

/// The stored events, then the backlog of a run that still executes (from
/// index `from` on), as one JSON array.
fn events_json(mut stored: Vec<StoredEvent>, tail: Option<LiveTail>, from: u64, scope: EventScope) -> Response {
    if let Some(tail) = tail {
        let skip = usize::try_from(from.saturating_sub(tail.first_index)).unwrap_or(usize::MAX);
        stored.extend(tail.subscription.backlog.iter().skip(skip).map(|event| event_to_stored(event)));
    }
    if scope == EventScope::Full {
        // The stored JSON texts as they are: nothing is parsed.
        return Json(stored).into_response();
    }
    let visible: Vec<serde_json::Value> = stored.into_iter().filter_map(|event| scoped_json(event, scope)).collect();
    Json(visible).into_response()
}

/// The stored events (the first at the cursor's index) as SSE, then, for a run
/// that still executes, its events from there on until the terminal one.
fn events_sse(
    stored: Vec<StoredEvent>,
    tail: Option<LiveTail>,
    follow: Follow,
    #[cfg_attr(not(feature = "run-follow"), allow(unused_variables))] stored_answer: Option<String>,
) -> Response {
    let from = follow.cursor.start;
    let scope = follow.scope;
    let last = stored.len().saturating_sub(1);
    let mut replay = Vec::with_capacity(stored.len() + 1);
    for ((position, json), index) in stored.iter().enumerate().zip(from..) {
        #[cfg(feature = "run-follow")]
        if position == last
            && let Some(answer) = &stored_answer
        {
            replay.push(super::sse::stored_answer_sse(answer, index));
        }
        #[cfg(not(feature = "run-follow"))]
        let _ = (position, last);
        if let Some(message) = scoped_stored_sse(json, index, scope) {
            replay.push(message);
        }
    }
    let replay = futures::stream::iter(replay.into_iter().map(Ok::<_, Infallible>));
    let Some(tail) = tail else { return Sse::new(replay.boxed()).into_response() };
    #[cfg(feature = "run-follow")]
    if follow.responses
        && !tail.subscription.backlog_truncated
        && let Some(assembler) = tail.run.new_assembler()
    {
        // The same stream a streaming run start sends, from this caller's cursor:
        // the assembler sees the execution from its first event, so the text
        // and its ids are the ones the starting caller got.
        let events = live::follow(tail.run, tail.subscription);
        let guard = Box::new(super::delivery::FollowOnly);
        let live = super::sse::sse_stream(events, assembler, tail.first_index, follow.cursor, guard, scope);
        return Sse::new(replay.chain(live)).keep_alive(KeepAlive::default()).into_response();
    }
    let live = numbered(live::follow(tail.run, tail.subscription), tail.first_index).filter_map(move |item| {
        futures::future::ready(match item {
            Numbered::Event(event, index) if index >= from => scoped_sse_event(&event, index, scope).map(Ok),
            Numbered::Event(..) => None,
            Numbered::Lagged(skipped) => Some(Ok(lagged_sse(skipped))),
        })
    });
    Sse::new(replay.chain(live)).keep_alive(KeepAlive::default()).into_response()
}
