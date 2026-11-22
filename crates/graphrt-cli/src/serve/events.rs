//! `GET /v1/runs/{run_id}/events`: stored events, then the live tail of a run
//! that still executes.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use graphrt_core::store::{StoredEvent, event_to_stored};
use serde::Deserialize;

use super::errors::store_error;
use super::sse::{live_events, sse_event, stored_sse};
use super::state::AppState;
use crate::store::live;

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
) -> Response {
    // Take the live subscription first: events the driver stores after this point
    // reach us through it, and the stored part is cut at `stored_before`.
    let subscription = state.active(&run_id).map(|run| (run.stored_before, run.subscribe()));
    let stored_until = subscription.as_ref().map(|(before, _)| *before);
    let mut stored = match state.store.events(&run_id, query.from).await {
        Ok(events) => events,
        Err(e) => return store_error(&e),
    };
    if let Some(limit) = stored_until {
        let keep = usize::try_from(limit.saturating_sub(query.from)).unwrap_or(usize::MAX);
        stored.truncate(keep);
    }
    let subscription = subscription.map(|(_, sub)| sub);
    if query.stream == Some(false) {
        return events_json(stored, subscription);
    }
    events_sse(stored, subscription)
}

/// The stored events, then the backlog of a run that still executes, as one JSON array.
fn events_json(mut stored: Vec<StoredEvent>, subscription: Option<live::Subscription>) -> Response {
    if let Some(sub) = subscription {
        stored.extend(sub.backlog.iter().map(|event| event_to_stored(event)));
    }
    Json(stored).into_response()
}

/// The stored events as SSE, then (for a run that still executes) its backlog
/// and live events until the terminal one.
fn events_sse(stored: Vec<StoredEvent>, subscription: Option<live::Subscription>) -> Response {
    let replay = futures::stream::iter(stored.into_iter().map(|json| Ok(stored_sse(&json))));
    let Some(sub) = subscription else { return Sse::new(replay.boxed()).into_response() };
    let backlog = futures::stream::iter(sub.backlog.into_iter().map(|e| Ok(sse_event(&e))));
    let live = if sub.done { futures::stream::empty().boxed() } else { live_events(sub.live).boxed() };
    Sse::new(replay.chain(backlog).chain(live)).keep_alive(KeepAlive::default()).into_response()
}
