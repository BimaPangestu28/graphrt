//! The approvals inbox (`graphrt serve --approvals`, cargo feature `approvals`):
//! every run that waits for a person, across workflows, and a way to answer.
//!
//! | Method | Path | Body | Answer |
//! | --- | --- | --- | --- |
//! | GET | `/v1/approvals` | query `workflow`, `limit`, `cursor` | `{approvals, next_cursor}`, most recently paused first |
//! | POST | `/v1/approvals/{run_id}` | `{inputs}` | `202` + where to follow the run |
//!
//! Both take the API key, or a device token ([`crate::devices`]) with the scope
//! `approvals:resolve`.
//!
//! An entry is a paused run with its `requests`: one per node that waits, with
//! the `key` to answer under, the pause `kind` (`human_input`,
//! `tool_approval`...), the pause `data` (prompt, fields and actions, or the
//! tool calls) and `expires_ms` when the request has a time limit. Runs paused
//! by a shutdown drain wait for no person and are not listed.
//!
//! Resolving resumes the run in the background with `inputs` keyed like the
//! requests, exactly as `POST /v1/runs/{id}/resume` with `async: true` and the
//! stored snapshot would. A resolve can never send a snapshot or secrets.
//!
//! With `--expire-approvals` the server also carries out what a `human-input`
//! node asks for when its time limit passes (`on_expire`): it answers with the
//! named action and the fields' defaults, or stops the run.

// Handlers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use graphrt_core::Value;
use graphrt_core::event::PauseReason;
use graphrt_core::store::{RunFilter, RunRecord, RunStatus, now_ms};
use serde::Deserialize;

use crate::devices::Scope;
use crate::http_input::{JsonBody, QueryParams};
use crate::serve::{AppState, error_response, store_error};

/// Entries per page of the inbox, and the most a caller may ask for.
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 200;
/// How often expired requests are looked for.
const EXPIRY_INTERVAL: Duration = Duration::from_secs(30);
/// Why a run stopped by an expired request ended.
pub const EXPIRED_REASON: &str = "expired: nobody answered the request for input in time";

/// The approvals inbox of a server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Approvals {
    /// Carry out `on_expire` of requests whose time limit passed (`--expire-approvals`).
    pub expire: bool,
}

impl Approvals {
    /// One line for the startup log.
    pub fn describe(&self) -> String {
        let expiry = if self.expire { ", requests past their time limit are answered or stopped" } else { "" };
        format!("approvals inbox under /v1/approvals{expiry}")
    }
}

/// The inbox routes, behind their own authorization.
pub(crate) fn routes(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/approvals", get(list))
        .route("/v1/approvals/{run_id}", post(resolve))
        .route_layer(middleware::from_fn_with_state(Arc::clone(state), authorize))
}

/// One node of a paused run that waits for a person.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The key of this request in resume inputs: the node id, or `<frame>/<node_id>` inside containers.
    pub key: String,
    pub node_id: String,
    pub kind: String,
    pub data: Value,
    /// When the request's time limit passes, if it has one.
    pub expires_ms: Option<u64>,
}

impl Request {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "key": self.key,
            "node_id": self.node_id,
            "kind": self.kind,
            "data": self.data.to_json(),
            "expires_ms": self.expires_ms,
        })
    }

    /// What the node asks to happen when the request expires: the inputs to
    /// answer with, or `None` to stop the run.
    fn answer_on_expire(&self) -> Option<serde_json::Value> {
        let on_expire = self.data.get_path(&["on_expire"])?;
        if on_expire.get_path(&["kind"]).as_ref().and_then(Value::as_str) != Some("answer") {
            return None;
        }
        let action = on_expire.get_path(&["action"]).as_ref().and_then(Value::as_str)?.to_owned();
        // The fields' defaults, as a person picking the action without typing would send.
        let mut values = serde_json::Map::new();
        if let Some(serde_json::Value::Array(fields)) = self.data.get_path(&["fields"]).map(|fields| fields.to_json()) {
            for field in fields {
                if let (Some(name), Some(default)) =
                    (field.get("name").and_then(serde_json::Value::as_str), field.get("default"))
                    && !default.is_null()
                {
                    values.insert(name.to_owned(), default.clone());
                }
            }
        }
        Some(serde_json::json!({ "action": action, "values": values }))
    }
}

/// The request of one pause reason; `None` for a pause no person answers (a drain).
fn request_of(reason: &PauseReason, paused_ms: u64) -> Option<Request> {
    let node_id = reason.node_id.as_deref()?;
    let key = if reason.frame.is_empty() { node_id.to_owned() } else { format!("{}/{node_id}", reason.frame) };
    let timeout_ms = reason
        .data
        .get_path(&["timeout_ms"])
        .as_ref()
        .and_then(Value::as_i64)
        .and_then(|timeout| u64::try_from(timeout).ok())
        .filter(|timeout| *timeout > 0);
    Some(Request {
        key,
        node_id: node_id.to_owned(),
        kind: reason.kind.clone(),
        data: reason.data.clone(),
        expires_ms: timeout_ms.map(|timeout| paused_ms.saturating_add(timeout)),
    })
}

/// What `record` waits for; empty unless it is paused for a person.
pub fn requests_of(record: &RunRecord) -> Vec<Request> {
    if record.status != RunStatus::Paused {
        return Vec::new();
    }
    record.pause_reasons.iter().filter_map(|reason| request_of(reason, record.updated_ms)).collect()
}

fn entry_json(record: &RunRecord, requests: &[Request]) -> serde_json::Value {
    serde_json::json!({
        "run_id": record.run_id,
        "workflow": record.workflow,
        "paused_ms": record.updated_ms,
        "user_id": record.user_id,
        "session_id": record.session_id,
        "requests": requests.iter().map(Request::json).collect::<Vec<_>>(),
    })
}

/// The inbox of a server that has one.
fn enabled(state: &AppState) -> Result<Approvals, Response> {
    state.approvals().ok_or_else(|| {
        error_response(StatusCode::NOT_FOUND, "this server has no approvals inbox (start it with --approvals)")
    })
}

/// The API key, or a device token that resolves approvals. Runs before a
/// request's body is read, so a caller without a credential learns nothing else.
async fn authorize(State(state): State<Arc<AppState>>, request: axum::extract::Request, next: Next) -> Response {
    let allowed =
        enabled(&state).and_then(|_| state.authenticate_operator(request.headers(), Some(Scope::ApprovalsResolve)));
    match allowed {
        Ok(_) => next.run(request).await,
        Err(refused) => refused,
    }
}

/// `GET /v1/approvals`.
async fn list(State(state): State<Arc<AppState>>, RawQuery(raw): RawQuery) -> Response {
    let params = QueryParams::parse(raw.as_deref());
    let limit = match params.number("limit") {
        Ok(limit) => limit.map_or(DEFAULT_PAGE, |limit| usize::try_from(limit).unwrap_or(MAX_PAGE)),
        Err(refused) => return refused,
    };
    let filter = RunFilter {
        workflow: params.string("workflow"),
        status: Some(RunStatus::Paused),
        limit: limit.clamp(1, MAX_PAGE),
        cursor: params.string("cursor"),
    };
    match state.store().list(filter).await {
        Ok(page) => {
            let approvals: Vec<serde_json::Value> = page
                .runs
                .iter()
                .filter_map(|record| {
                    let requests = requests_of(record);
                    (!requests.is_empty()).then(|| entry_json(record, &requests))
                })
                .collect();
            Json(serde_json::json!({ "approvals": approvals, "next_cursor": page.next_cursor })).into_response()
        }
        Err(e) => store_error(&e),
    }
}

/// Body of `POST /v1/approvals/{run_id}`.
#[derive(Debug, Deserialize)]
struct ResolveRequest {
    /// The answers, keyed like the entry's `requests`.
    inputs: serde_json::Map<String, serde_json::Value>,
}

/// `POST /v1/approvals/{run_id}`: answer what the run waits for and let it continue.
async fn resolve(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    JsonBody(body): JsonBody<ResolveRequest>,
) -> Response {
    let record = match state.store().get(&run_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "unknown run"),
        Err(e) => return store_error(&e),
    };
    let requests = requests_of(&record);
    if requests.is_empty() {
        return error_response(
            StatusCode::CONFLICT,
            "this run waits for no answer (it was answered already, or it ended)",
        );
    }
    if body.inputs.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "`inputs` is empty: answer at least one request");
    }
    // An answer under a key nothing waits for would be dropped and the run
    // would pause again as if nobody had answered.
    if let Some(unknown) = body.inputs.keys().find(|key| !requests.iter().any(|request| &request.key == *key)) {
        let waiting: Vec<&str> = requests.iter().map(|request| request.key.as_str()).collect();
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("`inputs.{unknown}`: the run waits for no such request (waiting: {})", waiting.join(", ")),
        );
    }
    crate::serve::resume_detached(&state, &run_id, body.inputs).await
}

/// Carry out `on_expire` of every request whose time limit passed by `now`.
/// Returns how many runs were answered or stopped.
pub async fn expire_due(state: &Arc<AppState>, now: u64) -> usize {
    let mut expired = 0;
    let mut cursor = None;
    loop {
        let filter = RunFilter { status: Some(RunStatus::Paused), limit: MAX_PAGE, cursor, ..RunFilter::default() };
        let page = match state.store().list(filter).await {
            Ok(page) => page,
            Err(e) => {
                eprintln!("warning: cannot list paused runs to expire: {e}");
                return expired;
            }
        };
        for record in &page.runs {
            let requests = requests_of(record);
            let due: Vec<&Request> =
                requests.iter().filter(|request| request.expires_ms.is_some_and(|expires| expires <= now)).collect();
            if due.is_empty() {
                continue;
            }
            match expire_run(state, &record.run_id, &due).await {
                Ok(()) => expired += 1,
                Err(why) => eprintln!("run {}: expired request left waiting: {why}", record.run_id),
            }
        }
        cursor = match page.next_cursor {
            Some(next) => Some(next),
            None => break,
        };
    }
    expired
}

/// One run with expired requests: stop it when one of them says so, else
/// answer each as its node asks.
///
/// @throws String - Why the run still waits.
async fn expire_run(state: &Arc<AppState>, run_id: &str, due: &[&Request]) -> Result<(), String> {
    let mut inputs = serde_json::Map::new();
    for request in due {
        match request.answer_on_expire() {
            Some(answer) => inputs.insert(request.key.clone(), answer),
            None => {
                return crate::serve::stop_paused(state, run_id, EXPIRED_REASON).await.map_err(|e| e.to_string());
            }
        };
    }
    let answer = crate::serve::resume_detached(state, run_id, inputs).await;
    if answer.status().is_success() { Ok(()) } else { Err(format!("the resume answered {}", answer.status())) }
}

/// Look for expired requests every [`EXPIRY_INTERVAL`] until the server stops.
/// The task holds the state weakly, so it never keeps a server alive.
pub fn spawn_expiry(state: &Arc<AppState>) {
    let weak = Arc::downgrade(state);
    graphrt_core::task::spawn(async move {
        loop {
            tokio::time::sleep(EXPIRY_INTERVAL).await;
            let Some(state) = weak.upgrade() else { return };
            if state.is_draining() {
                return;
            }
            let expired = expire_due(&state, now_ms()).await;
            if expired > 0 {
                eprintln!("approvals: {expired} run(s) with an expired request were answered or stopped");
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn reason(kind: &str, node: Option<&str>, frame: &str, data: serde_json::Value) -> PauseReason {
        PauseReason {
            frame: Arc::from(frame),
            node_id: node.map(str::to_owned),
            kind: kind.to_owned(),
            data: Value::from_json(data),
        }
    }

    fn paused(reasons: Vec<PauseReason>) -> RunRecord {
        let mut record = RunRecord::running("run-1", "refunds", "fp", 1_000);
        record.status = RunStatus::Paused;
        record.updated_ms = 5_000;
        record.pause_reasons = reasons;
        record
    }

    #[test]
    fn requests_name_their_key_and_when_they_expire() {
        let record = paused(vec![
            reason("human_input", Some("ask"), "", json!({ "prompt": "Refund?", "timeout_ms": 60_000 })),
            reason("tool_approval", Some("agent"), "loop#2", json!({ "calls": [] })),
            reason("drained", None, "", json!({ "resume": "auto" })),
        ]);
        let requests = requests_of(&record);
        assert_eq!(requests.len(), 2, "a drain waits for no person");
        assert_eq!((requests[0].key.as_str(), requests[0].expires_ms), ("ask", Some(65_000)));
        assert_eq!((requests[1].key.as_str(), requests[1].expires_ms), ("loop#2/agent", None));
        let entry = entry_json(&record, &requests);
        assert_eq!(entry["paused_ms"], 5_000);
        assert_eq!(entry["requests"][0]["data"]["prompt"], "Refund?");
    }

    #[test]
    fn only_paused_runs_wait() {
        let mut record = paused(vec![reason("human_input", Some("ask"), "", json!({}))]);
        record.status = RunStatus::Running;
        assert!(requests_of(&record).is_empty());
    }

    #[test]
    fn an_expired_request_answers_with_the_named_action_and_the_defaults_or_stops() {
        let data = json!({
            "timeout_ms": 1,
            "on_expire": { "kind": "answer", "action": "reject" },
            "fields": [
                { "name": "note", "default": "no answer in time" },
                { "name": "amount", "default": null },
            ],
        });
        let record = paused(vec![reason("human_input", Some("ask"), "", data)]);
        let answer = requests_of(&record)[0].answer_on_expire().unwrap();
        assert_eq!(answer, json!({ "action": "reject", "values": { "note": "no answer in time" } }));
        for stop in
            [json!({ "on_expire": { "kind": "stop" } }), json!({}), json!({ "on_expire": { "kind": "answer" } })]
        {
            let record = paused(vec![reason("human_input", Some("ask"), "", stop)]);
            assert_eq!(requests_of(&record)[0].answer_on_expire(), None);
        }
    }
}
