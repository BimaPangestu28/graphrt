//! Stored runs: `GET /v1/runs`, `GET` and `DELETE /v1/runs/{run_id}`, and
//! `POST /v1/runs/{run_id}/abort`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_core::store::{RunFilter, RunRecord, RunStatus};
use serde::Deserialize;

use super::errors::{error_response, store_error};
use super::state::AppState;

pub(super) async fn abort_run(State(state): State<Arc<AppState>>, UrlPath(run_id): UrlPath<String>) -> Response {
    match state.active(&run_id) {
        Some(run) => {
            run.handle().abort("aborted via API");
            (StatusCode::ACCEPTED, Json(serde_json::json!({ "run_id": run_id, "status": "aborting" }))).into_response()
        }
        None => error_response(StatusCode::NOT_FOUND, "no active run with this id"),
    }
}

/// Query of `GET /v1/runs`.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub workflow: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub limit: usize,
    pub cursor: Option<String>,
}

pub(super) async fn list_runs(State(state): State<Arc<AppState>>, Query(query): Query<ListQuery>) -> Response {
    let status = match query.status.as_deref().map(|s| (s, RunStatus::parse(s))) {
        Some((raw, None)) => return error_response(StatusCode::BAD_REQUEST, &format!("unknown status `{raw}`")),
        Some((_, parsed)) => parsed,
        None => None,
    };
    let filter = RunFilter { workflow: query.workflow, status, limit: query.limit, cursor: query.cursor };
    match state.store.list(filter).await {
        Ok(page) => {
            let runs: Vec<serde_json::Value> = page.runs.iter().map(|r| record_json(&state, r)).collect();
            Json(serde_json::json!({ "runs": runs, "next_cursor": page.next_cursor })).into_response()
        }
        Err(e) => store_error(&e),
    }
}

pub(super) async fn get_run(State(state): State<Arc<AppState>>, UrlPath(run_id): UrlPath<String>) -> Response {
    match state.store.get(&run_id).await {
        Ok(Some(record)) => Json(record_json(&state, &record)).into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "unknown run"),
        Err(e) => store_error(&e),
    }
}

pub(super) async fn delete_run(State(state): State<Arc<AppState>>, UrlPath(run_id): UrlPath<String>) -> Response {
    if state.active(&run_id).is_some() {
        return error_response(StatusCode::CONFLICT, "run is executing; abort it first");
    }
    match state.store.delete(&run_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "unknown run"),
        Err(e) => store_error(&e),
    }
}

/// A run record as returned by the API, with the input each paused node expects.
fn record_json(state: &AppState, record: &RunRecord) -> serde_json::Value {
    let mut json = serde_json::to_value(record).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = json.as_object_mut() {
        obj.insert("active".into(), serde_json::Value::Bool(state.active(&record.run_id).is_some()));
        if record.status == RunStatus::Paused {
            let expected: Vec<serde_json::Value> = record
                .pause_reasons
                .iter()
                .filter_map(|reason| {
                    let node = reason.node_id.as_deref()?;
                    // Resume inputs are keyed by node id, or `<frame>/<node_id>` inside containers.
                    let key =
                        if reason.frame.is_empty() { node.to_owned() } else { format!("{}/{node}", reason.frame) };
                    Some(serde_json::json!({ "key": key, "node_id": node, "kind": reason.kind, "data": reason.data }))
                })
                .collect();
            obj.insert("resume_inputs".into(), serde_json::Value::Array(expected));
        }
    }
    json
}
