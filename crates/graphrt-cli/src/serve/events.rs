//! `GET /v1/runs/{run_id}/events`: stored events, then the live tail of a run
//! that still executes. A reconnecting SSE client resumes after its
//! `Last-Event-ID`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use graphrt_core::store::{StoredEvent, event_to_stored};
use serde::Deserialize;

use super::errors::store_error;
use super::sse::{Numbered, lagged_sse, numbered, sse_event, stored_sse};
use super::state::AppState;
use crate::store::live::{self, ActiveRun};

/// Query of `GET /v1/runs/{id}/events`.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub from: u64,
    /// `false` returns stored events as a JSON array instead of SSE.
    pub stream: Option<bool>,
}

/// `GET /v1/runs/{run_id}/events`.
pub(super) async fn run_events(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    Query(query): Query<EventsQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    // A reconnecting SSE client names the last event it saw.
    let from = query.from.max(crate::sse_ids::resume_index_of(&headers).unwrap_or(0));
    events_response(&state, &run_id, from, query.stream == Some(false)).await
}

/// The live part of a run that still executes: its listener's subscription,
/// and the index of the subscription's first event in the run's log.
struct LiveTail {
    run: Arc<ActiveRun>,
    first_index: u64,
    subscription: live::Subscription,
}

/// Stored events of `run_id` from index `from`, followed by the live tail while
/// the run executes, each with its index as SSE `id:` (or a JSON array when
/// `json`).
async fn events_response(state: &AppState, run_id: &str, from: u64, json: bool) -> Response {
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
    if json {
        return events_json(stored, tail, from);
    }
    events_sse(stored, tail, from)
}

/// The stored events, then the backlog of a run that still executes (from
/// index `from` on), as one JSON array.
fn events_json(mut stored: Vec<StoredEvent>, tail: Option<LiveTail>, from: u64) -> Response {
    if let Some(tail) = tail {
        let skip = usize::try_from(from.saturating_sub(tail.first_index)).unwrap_or(usize::MAX);
        stored.extend(tail.subscription.backlog.iter().skip(skip).map(|event| event_to_stored(event)));
    }
    Json(stored).into_response()
}

/// The stored events (the first at index `from`) as SSE, then, for a run that
/// still executes, its events from `from` on until the terminal one.
fn events_sse(stored: Vec<StoredEvent>, tail: Option<LiveTail>, from: u64) -> Response {
    let replay = stored.into_iter().zip(from..).map(move |(json, index)| Ok::<_, Infallible>(stored_sse(&json, index)));
    let replay = futures::stream::iter(replay);
    let Some(tail) = tail else { return Sse::new(replay.boxed()).into_response() };
    let live = numbered(live::follow(tail.run, tail.subscription), tail.first_index).filter_map(move |item| {
        futures::future::ready(match item {
            Numbered::Event(event, index) => (index >= from).then(|| Ok(sse_event(&event, index))),
            Numbered::Lagged(skipped) => Some(Ok(lagged_sse(skipped))),
        })
    });
    Sse::new(replay.chain(live)).keep_alive(KeepAlive::default()).into_response()
}
