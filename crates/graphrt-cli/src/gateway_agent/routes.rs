//! Handlers of `/internal/v1` (see the [parent module](super)).

// Helpers return the error response a handler answers with; boxing it would
// only add an allocation on the refusal path.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::{Method, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use graphrt_core::engine::{ResumeFrom, RunInput};
use graphrt_core::store::{RunRecord, now_ms, validate_run_id};
use graphrt_core::{Snapshot, Value};
use indexmap::IndexMap;
use serde::Deserialize;

use super::reclaim::ReclaimGuard;
use super::{AssemblerSeed, GatewayAgent, GatewayRequest, PROTOCOL_VERSION, Verified, protocol_error};
use crate::serve::{AppState, RunRequest};
use crate::sse_ids::Cursor;
use crate::store::live::ActiveRun;

/// Route a verified `/internal/v1/*` request to its endpoint.
pub(super) async fn dispatch(state: &Arc<AppState>, agent: &Arc<GatewayAgent>, verified: Verified) -> Response {
    let Verified { parts, body, request } = verified;
    let endpoint = parts.uri.path().strip_prefix("/internal/v1/").unwrap_or_default();
    let segments: Vec<&str> = endpoint.split('/').collect();
    let method = &parts.method;
    match segments.as_slice() {
        ["hello"] if method == Method::GET => hello(state, agent),
        ["runs"] if method == Method::POST => match parse_json::<InternalRunRequest>(&body) {
            Ok(run) => start_run(state, agent, &request, traceparent(&parts.headers), run).await,
            Err(refused) => refused,
        },
        ["runs", run_id, "stream"] if method == Method::GET => {
            let cursor = crate::sse_ids::cursor_of(&parts.headers)
                .unwrap_or_else(|| Cursor::from_index(query_from(parts.uri.query())));
            reattach(state, agent, &request, run_id, cursor).await
        }
        ["runs", run_id, "abort"] if method == Method::POST => abort(state, agent, &request, run_id),
        ["drain"] if method == Method::POST => {
            let deadline_ms = if body.is_empty() {
                None
            } else {
                match parse_json::<DrainRequest>(&body) {
                    Ok(drain) => drain.deadline_ms,
                    Err(refused) => return refused,
                }
            };
            drain(state, deadline_ms)
        }
        ["orphans"] if method == Method::GET => {
            Json(serde_json::json!({ "orphans": agent.orphan_list() })).into_response()
        }
        ["orphans", run_id] if method == Method::DELETE => release_orphan(agent, &request, run_id),
        _ => protocol_error(StatusCode::NOT_FOUND, "not_found", "no such gateway endpoint"),
    }
}

/// A JSON request body, or `400 bad_request`. Parsed like axum's `Json`
/// extractor (the public resume endpoint uses it), so the snapshot types'
/// deserializers exist once in the binary, not once per JSON entry point.
fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    Json::<T>::from_bytes(body).map(|Json(value)| value).map_err(|e| {
        protocol_error(StatusCode::BAD_REQUEST, "bad_request", &format!("invalid request body: {}", e.body_text()))
    })
}

/// `from=N` of a query string (0 when absent or invalid).
fn query_from(query: Option<&str>) -> u64 {
    query
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix("from="))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// `GET /internal/v1/hello`.
fn hello(state: &AppState, agent: &GatewayAgent) -> Response {
    let workflows: Vec<serde_json::Value> = state
        .served_workflows()
        .map(|(name, w)| serde_json::json!({ "name": name, "fingerprint": w.engine.graph().fingerprint() }))
        .collect();
    Json(serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "deployment": agent.deployment(),
        "workflows": workflows,
        "max_active_runs": state.max_active_runs(),
        "active_runs": state.active_runs(),
        "warm_plugins": state.warm_plugins(),
        "draining": state.is_draining(),
    }))
    .into_response()
}

/// Body of `POST /internal/v1/runs`: a run request (`inputs`, `sys`,
/// `conversation`, `metadata`, `session_id`, `user_id`, `tags`; the response
/// is always SSE) plus the gateway's fields. The fields are listed rather than
/// `#[serde(flatten)]`ed from [`RunRequest`]: flattening deserializes through a
/// buffered tree and costs far more code.
#[derive(Debug, Deserialize)]
pub struct InternalRunRequest {
    /// Served workflow name.
    pub workflow: String,
    /// Expected graph fingerprint; refused with `409 fingerprint_mismatch` when
    /// this agent serves another version of the workflow.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Deployment version, copied to the run record.
    #[serde(default)]
    pub deployment_version: Option<String>,
    /// Global index of this execution's first event (the gateway's event count),
    /// so SSE ids continue the run's log across executions.
    #[serde(default)]
    pub event_index: u64,
    /// Resume a paused run from its snapshot instead of starting it.
    #[serde(default)]
    pub resume: Option<InternalResume>,
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub sys: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub conversation: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub metadata: IndexMap<String, String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `resume` of [`InternalRunRequest`].
#[derive(Debug, Deserialize)]
pub struct InternalResume {
    pub snapshot: Snapshot,
    /// Input for paused nodes keyed by node id (or `<frame>/<node_id>`).
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Secrets (other than `env.*`) to re-supply, keyed by `namespace.name`.
    #[serde(default)]
    pub secrets: IndexMap<String, String>,
}

fn run_and_epoch(request: &GatewayRequest) -> Result<(String, u64), Response> {
    let (Some(run_id), Some(epoch)) = (request.run_id.clone(), request.epoch) else {
        return Err(protocol_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Graphrt-Run-Id and Graphrt-Epoch headers are required",
        ));
    };
    validate_run_id(&run_id).map_err(|e| protocol_error(StatusCode::BAD_REQUEST, "bad_request", &e.to_string()))?;
    Ok((run_id, epoch))
}

fn to_values(map: serde_json::Map<String, serde_json::Value>) -> IndexMap<String, Value> {
    map.into_iter().map(|(k, v)| (k, Value::from_json(v))).collect()
}

/// `POST /internal/v1/runs`: start or resume run `Graphrt-Run-Id` at epoch
/// `Graphrt-Epoch` and stream it. A request for the execution already running
/// at that epoch attaches to it; another epoch is refused.
async fn start_run(
    state: &Arc<AppState>,
    agent: &Arc<GatewayAgent>,
    request: &GatewayRequest,
    trace: Option<String>,
    body: InternalRunRequest,
) -> Response {
    let (run_id, epoch) = match run_and_epoch(request) {
        Ok(ids) => ids,
        Err(refused) => return refused,
    };
    let active = state.active_run(&run_id).is_some();
    match agent.execution(&run_id) {
        Some(execution) if execution.epoch > epoch => {
            return protocol_error(
                StatusCode::CONFLICT,
                "stale_epoch",
                "a newer epoch of this run was dispatched here",
            );
        }
        // A retried start of the execution already here: attach to it, or replay
        // it once finished, never execute it twice.
        Some(execution) if execution.epoch == epoch => {
            if active {
                return attached_stream(state, agent, &run_id, epoch, Cursor::ALL).unwrap_or_else(|refused| refused);
            }
            return crate::serve::events_response(state, &run_id, 0, execution.base_index, false).await;
        }
        _ if active => {
            return protocol_error(StatusCode::CONFLICT, "epoch_conflict", "the run is executing at another epoch");
        }
        _ => {}
    }
    if state.is_draining() {
        return protocol_error(StatusCode::SERVICE_UNAVAILABLE, "draining", "this agent is draining");
    }
    match prepare(state, &run_id, epoch, body) {
        Ok(prepared) => launch_stream(state, agent, prepared, trace).await,
        Err(refused) => refused,
    }
}

/// A validated run start, ready to launch.
struct Prepared {
    record: RunRecord,
    input: RunInput,
    workflow: String,
    base_index: u64,
}

fn prepare(state: &AppState, run_id: &str, epoch: u64, body: InternalRunRequest) -> Result<Prepared, Response> {
    let Some(workflow) = state.served_workflow(&body.workflow) else {
        return Err(protocol_error(
            StatusCode::NOT_FOUND,
            "unknown_workflow",
            &format!("unknown workflow `{}`", body.workflow),
        ));
    };
    let fingerprint = workflow.engine.graph().fingerprint();
    let expected = body.resume.as_ref().map(|r| r.snapshot.graph_fingerprint.as_str()).or(body.fingerprint.as_deref());
    if expected.is_some_and(|expected| expected != fingerprint) {
        return Err(protocol_error(
            StatusCode::CONFLICT,
            "fingerprint_mismatch",
            "this agent serves another version of the workflow",
        ));
    }
    if let Some(busy) = state.busy_response() {
        return Err(busy);
    }
    let mut record = RunRecord::running(run_id, &body.workflow, fingerprint, now_ms());
    let context = RunRequest {
        metadata: body.metadata,
        session_id: body.session_id,
        user_id: body.user_id,
        tags: body.tags,
        ..RunRequest::default()
    };
    context.apply_context(&mut record).map_err(|m| protocol_error(StatusCode::BAD_REQUEST, "bad_request", &m))?;
    record.deployment = state.gateway_agent().map(|agent| agent.deployment().to_owned());
    record.deployment_version = body.deployment_version;
    record.epoch = epoch;
    let mut input = RunInput::new();
    input.run_id = Some(run_id.to_owned());
    input.inputs = to_values(body.inputs);
    input.sys = to_values(body.sys);
    input.conversation = to_values(body.conversation);
    input.env = workflow.env.clone();
    if let Some(resume) = body.resume {
        if resume.snapshot.run_id != run_id {
            return Err(protocol_error(StatusCode::BAD_REQUEST, "bad_request", "snapshot belongs to a different run"));
        }
        let mut from = ResumeFrom::new(resume.snapshot, to_values(resume.inputs));
        for (selector, secret) in resume.secrets {
            from = from.secret(selector, secret);
        }
        input.resume = Some(from);
    }
    Ok(Prepared { record, input, workflow: body.workflow, base_index: body.event_index })
}

fn traceparent(headers: &axum::http::HeaderMap) -> Option<String> {
    headers.get("traceparent").and_then(|v| v.to_str().ok()).map(str::to_owned)
}

async fn launch_stream(
    state: &Arc<AppState>,
    agent: &Arc<GatewayAgent>,
    prepared: Prepared,
    trace: Option<String>,
) -> Response {
    let Prepared { record, input, workflow, base_index } = prepared;
    let Some(served) = state.served_workflow(&workflow) else {
        return protocol_error(StatusCode::NOT_FOUND, "unknown_workflow", "unknown workflow");
    };
    let run_id = record.run_id.clone();
    let epoch = record.epoch;
    // The agent's store is a cache of its own executions: a new epoch replaces
    // whatever an earlier one left (the gateway holds the run's full log).
    if let Err(e) = state.store().delete(&run_id).await {
        return crate::serve::store_error(&e);
    }
    if let Err(e) = state.store().create(record.clone()).await {
        return crate::serve::store_error(&e);
    }
    let seed = AssemblerSeed {
        graph: Arc::clone(served.engine.graph()),
        resume_root: input.resume.as_ref().map(|r| r.snapshot.root.clone()),
        sys: input.sys.clone(),
    };
    agent.begin(&run_id, epoch, base_index, seed, |id| state.active_run(id).is_some());
    let active = state.launch_run(&served.engine, record, input, trace.as_deref());
    // Attached once by `begin`; the stream below owns that attachment.
    stream_execution(state, agent, active, epoch, Cursor::ALL).unwrap_or_else(|refused| refused)
}

/// Attach one more stream to a running execution.
fn attached_stream(
    state: &Arc<AppState>,
    agent: &Arc<GatewayAgent>,
    run_id: &str,
    epoch: u64,
    cursor: Cursor,
) -> Result<Response, Response> {
    let Some(active) = state.active_run(run_id) else {
        return Err(protocol_error(StatusCode::NOT_FOUND, "not_found", "the execution already finished"));
    };
    if agent.attach(run_id, epoch).is_none() {
        return Err(protocol_error(StatusCode::CONFLICT, "epoch_conflict", "no execution of this run at this epoch"));
    }
    stream_execution(state, agent, active, epoch, cursor)
}

/// SSE of an attached execution: `hello`, then events and response text from
/// `cursor`, with global ids. The stream holds the attachment; when it is
/// dropped before the run finished, the reclaim grace starts.
fn stream_execution(
    state: &Arc<AppState>,
    agent: &Arc<GatewayAgent>,
    active: Arc<ActiveRun>,
    epoch: u64,
    cursor: Cursor,
) -> Result<Response, Response> {
    let run_id = active.run_id.as_str();
    let Some(execution) = agent.execution(run_id).filter(|e| e.epoch == epoch) else {
        agent.detach(run_id, epoch);
        return Err(protocol_error(StatusCode::CONFLICT, "epoch_conflict", "no execution of this run at this epoch"));
    };
    let events = crate::store::live::follow(Arc::clone(&active), active.subscribe());
    let first_index = execution.base_index + active.stored_before;
    let guard = ReclaimGuard::new(Arc::clone(agent), Arc::downgrade(state), Arc::clone(&active), run_id, epoch);
    let hello = serde_json::json!({ "run_id": run_id, "epoch": epoch, "protocol": PROTOCOL_VERSION });
    let hello = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(SseEvent::default().event("hello").data(hello.to_string()))
    });
    let events = crate::serve::sse_stream(
        events,
        execution.assembler_seed.assembler(),
        first_index,
        cursor,
        Box::new(guard),
        crate::serve::EventScope::Full,
    );
    Ok(Sse::new(hello.chain(events)).keep_alive(KeepAlive::default()).into_response())
}

/// `GET /internal/v1/runs/{id}/stream?from=N` (or `Last-Event-ID`): reattach.
/// While the execution runs, its events and response text are streamed from
/// the cursor; once it finished, its stored engine events are.
async fn reattach(
    state: &Arc<AppState>,
    agent: &Arc<GatewayAgent>,
    request: &GatewayRequest,
    run_id: &str,
    cursor: Cursor,
) -> Response {
    let Some(execution) = agent.execution(run_id) else {
        return protocol_error(StatusCode::NOT_FOUND, "not_found", "no execution of this run on this agent");
    };
    if request.epoch != Some(execution.epoch) {
        return protocol_error(StatusCode::CONFLICT, "epoch_conflict", "no execution of this run at this epoch");
    }
    if state.active_run(run_id).is_some() {
        return attached_stream(state, agent, run_id, execution.epoch, cursor).unwrap_or_else(|refused| refused);
    }
    let local_from = cursor.start.saturating_sub(execution.base_index);
    crate::serve::events_response(state, run_id, local_from, execution.base_index, false).await
}

/// `POST /internal/v1/runs/{id}/abort` (at `Graphrt-Epoch`).
fn abort(state: &AppState, agent: &GatewayAgent, request: &GatewayRequest, run_id: &str) -> Response {
    let current = agent.execution(run_id).map(|e| e.epoch);
    match (state.active_run(run_id), current) {
        (Some(run), Some(epoch)) if request.epoch == Some(epoch) => {
            run.handle().abort("aborted by the gateway");
            (StatusCode::ACCEPTED, Json(serde_json::json!({ "run_id": run_id, "status": "aborting" }))).into_response()
        }
        (Some(_), Some(_)) => protocol_error(StatusCode::CONFLICT, "epoch_conflict", "the run is at another epoch"),
        _ => protocol_error(StatusCode::NOT_FOUND, "not_found", "no active execution of this run"),
    }
}

/// Body of `POST /internal/v1/drain`.
#[derive(Debug, Default, Deserialize)]
struct DrainRequest {
    /// How long the gateway waits for this agent, in milliseconds: active runs
    /// pause, and those still running near the deadline are preempted.
    #[serde(default)]
    deadline_ms: Option<u64>,
}

/// Drain deadline when the gateway gives none.
const DEFAULT_DRAIN_MS: u64 = 25_000;

/// `POST /internal/v1/drain`: the shutdown drain of [`crate::drain`], started by
/// the gateway (scale-in): new runs get `503 draining` and every active run
/// pauses (preempted near the deadline), ending its stream with `run_paused`.
/// Answers `202` at once; the drain goes on in the background.
fn drain(state: &Arc<AppState>, deadline_ms: Option<u64>) -> Response {
    let deadline_ms = deadline_ms.unwrap_or(DEFAULT_DRAIN_MS);
    let active = state.active_runs();
    // Refuse new runs before answering, so the gateway sees the drain in effect.
    state.begin_drain();
    let draining = Arc::clone(state);
    tokio::spawn(async move {
        let report = crate::drain::drain(&draining, Duration::from_millis(deadline_ms), "gateway drain", None).await;
        eprintln!("{report}");
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "draining": true, "active_runs": active, "deadline_ms": deadline_ms })),
    )
        .into_response()
}

/// `DELETE /internal/v1/orphans/{id}` (at `Graphrt-Epoch`): the gateway took the run over.
fn release_orphan(agent: &GatewayAgent, request: &GatewayRequest, run_id: &str) -> Response {
    match request.epoch {
        Some(epoch) if agent.release_orphan(run_id, epoch) => StatusCode::NO_CONTENT.into_response(),
        _ => protocol_error(StatusCode::NOT_FOUND, "not_found", "no orphan of this run at this epoch"),
    }
}
