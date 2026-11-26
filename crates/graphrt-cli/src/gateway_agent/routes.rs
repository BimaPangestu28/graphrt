//! Handlers of `/internal/v1` (see the [parent module](super)).

// Helpers return the error response a handler answers with; boxing it would
// only add an allocation on the refusal path.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::future::BoxFuture;
use graphrt_core::engine::{ResumeFrom, RunInput};
use graphrt_core::store::{RunRecord, now_ms, validate_run_id};

use super::body::RunBody;
use super::json;
use super::reclaim::ReclaimGuard;
use super::{AssemblerSeed, GatewayAgent, GatewayRequest, PROTOCOL_VERSION, Verified, protocol_error};
use crate::serve::{AppState, RunRequest, to_values};
use crate::sse_ids::Cursor;
use crate::store::live::ActiveRun;

/// Route a verified `/internal/v1/*` request to its endpoint.
///
/// Each endpoint's future is boxed on its own: awaited inline, they would make
/// one state machine holding every endpoint (by far the largest function of the
/// feature), while separately they cost a fraction of that.
pub(super) fn dispatch(
    state: Arc<AppState>,
    agent: Arc<GatewayAgent>,
    verified: Verified,
) -> BoxFuture<'static, Response> {
    let Verified { parts, body, request } = verified;
    let endpoint = parts.uri.path().strip_prefix("/internal/v1/").unwrap_or_default();
    let segments: Vec<&str> = endpoint.split('/').collect();
    let method = &parts.method;
    let done = |response: Response| -> BoxFuture<'static, Response> { Box::pin(std::future::ready(response)) };
    match segments.as_slice() {
        ["hello"] if method == Method::GET => done(hello(&state, &agent)),
        ["warm"] if method == Method::POST => done(warm(&state)),
        ["runs"] if method == Method::POST => match super::body::run(&body) {
            Ok(run) => {
                let trace = traceparent(&parts.headers);
                Box::pin(async move { start_run(&state, &agent, &request, trace, run).await })
            }
            Err(message) => done(bad_request(message)),
        },
        ["runs", run_id, "stream"] if method == Method::GET => {
            let cursor = crate::sse_ids::cursor_of(&parts.headers)
                .unwrap_or_else(|| Cursor::from_index(query_from(parts.uri.query())));
            done(reattach(&state, &agent, &request, run_id, cursor))
        }
        ["runs", run_id, "abort"] if method == Method::POST => done(abort(&state, &agent, &request, run_id)),
        ["drain"] if method == Method::POST => {
            let deadline_ms = match super::body::drain_deadline_ms(&body) {
                Ok(deadline_ms) => deadline_ms,
                Err(message) => return done(bad_request(message)),
            };
            done(drain(&state, deadline_ms))
        }
        ["stop"] if method == Method::POST => done(stop(&state)),
        ["orphans"] if method == Method::GET => {
            let orphans = agent.orphan_list().iter().map(super::reclaim::Orphan::to_json).collect::<Vec<_>>();
            done(json::response(StatusCode::OK, json::Object::new().raw("orphans", &json::array(orphans)).finish()))
        }
        ["orphans", run_id] if method == Method::DELETE => done(release_orphan(&agent, &request, run_id)),
        _ => done(protocol_error(StatusCode::NOT_FOUND, "not_found", "no such gateway endpoint")),
    }
}

fn bad_request(message: &str) -> Response {
    protocol_error(StatusCode::BAD_REQUEST, "bad_request", message)
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
    let workflows = state.served_workflows().map(|(name, w)| {
        json::Object::new().str("name", name).str("fingerprint", w.engine.graph().fingerprint()).finish()
    });
    let body = json::Object::new()
        .num("protocol", PROTOCOL_VERSION.into())
        .str("version", env!("CARGO_PKG_VERSION"))
        .str("deployment", agent.deployment())
        .raw("workflows", &json::array(workflows))
        .num("max_active_runs", state.max_active_runs() as u64)
        .num("active_runs", state.active_runs() as u64)
        .raw("warm_plugins", &plugin_names(state))
        .bool("draining", state.is_draining())
        .finish();
    json::response(StatusCode::OK, body)
}

/// `POST /internal/v1/warm`: a warm hint (the gateway expects runs soon).
/// Starts the plugin processes that are not up (a pool stopped for idleness,
/// a plugin never used yet) and answers at once with the plugins already warm.
fn warm(state: &AppState) -> Response {
    state.prewarm_plugins();
    json::response(StatusCode::ACCEPTED, json::Object::new().raw("warm_plugins", &plugin_names(state)).finish())
}

/// `warm_plugins` of `hello` and `warm`: a JSON array of names.
fn plugin_names(state: &AppState) -> String {
    let plugins = state.warm_plugins().into_iter().map(|name| {
        let mut text = String::new();
        json::push_str(&mut text, &name);
        text
    });
    json::array(plugins)
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

/// `POST /internal/v1/runs`: start or resume run `Graphrt-Run-Id` at epoch
/// `Graphrt-Epoch` and stream it. A request for the execution already running
/// at that epoch attaches to it; another epoch is refused.
async fn start_run(
    state: &Arc<AppState>,
    agent: &Arc<GatewayAgent>,
    request: &GatewayRequest,
    trace: Option<String>,
    body: RunBody,
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
        // A retried start of the execution already here attaches to it; once it
        // finished it is never executed twice. Its outcome reached the gateway on
        // a stream, or is held as an orphan when every stream broke before it.
        Some(execution) if execution.epoch == epoch => {
            if active {
                return attached_stream(state, agent, &run_id, epoch, Cursor::ALL).unwrap_or_else(|refused| refused);
            }
            return finished();
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
    preempt_after_ms: Option<u64>,
}

fn prepare(state: &AppState, run_id: &str, epoch: u64, body: RunBody) -> Result<Prepared, Response> {
    let Some(workflow) = state.served_workflow(&body.workflow) else {
        return Err(protocol_error(StatusCode::NOT_FOUND, "unknown_workflow", "unknown workflow"));
    };
    if body.resume.as_ref().is_some_and(|resume| resume.snapshot.run_id != run_id) {
        return Err(bad_request("snapshot of another run"));
    }
    let fingerprint = workflow.engine.graph().fingerprint();
    let expected = body.resume.as_ref().map(|r| r.snapshot.graph_fingerprint.as_str()).or(body.fingerprint.as_deref());
    if expected.is_some_and(|expected| expected != fingerprint) {
        return Err(protocol_error(StatusCode::CONFLICT, "fingerprint_mismatch", "another workflow version"));
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
    context.apply_context(&mut record).map_err(|m| bad_request(&m))?;
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
        let mut from = ResumeFrom::new(resume.snapshot, to_values(resume.inputs));
        for (selector, secret) in resume.secrets {
            from = from.secret(selector, secret);
        }
        input.resume = Some(from);
    }
    Ok(Prepared {
        record,
        input,
        workflow: body.workflow,
        base_index: body.event_index,
        preempt_after_ms: body.preempt_after_ms,
    })
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
    let Prepared { record, input, workflow, base_index, preempt_after_ms } = prepared;
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
    if let Some(after_ms) = preempt_after_ms {
        arm_request_cap(Arc::clone(state), Arc::clone(agent), run_id.clone(), epoch, after_ms);
    }
    // Attached once by `begin`; the stream below owns that attachment.
    stream_execution(agent, active, epoch, Cursor::ALL).unwrap_or_else(|refused| refused)
}

/// Reason data of a preemption before the platform's request cap.
pub const REQUEST_CAP_REASON: &str = "request_cap";

/// Preempts execution `epoch` of `run_id` after `after_ms` if it still runs
/// here, so a capped platform request ends with a resumable snapshot.
fn arm_request_cap(state: Arc<AppState>, agent: Arc<GatewayAgent>, run_id: String, epoch: u64, after_ms: u64) {
    super::spawn_task(
        &tokio::runtime::Handle::current(),
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(after_ms)).await;
            let current = agent.execution(&run_id).map(|execution| execution.epoch);
            if current == Some(epoch)
                && let Some(run) = state.active_run(&run_id)
            {
                run.handle().preempt(REQUEST_CAP_REASON);
            }
        }),
    );
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
    stream_execution(agent, active, epoch, cursor)
}

/// SSE of an attached execution: `hello`, then events and response text from
/// `cursor`, with global ids. The stream holds the attachment; when it is
/// dropped before the run finished, the reclaim grace starts.
fn stream_execution(
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
    let guard = ReclaimGuard::new(Arc::clone(agent), Arc::clone(&active), run_id, epoch);
    let events = crate::serve::sse_stream(
        events,
        execution.assembler_seed.assembler(),
        first_index,
        cursor,
        Box::new(guard),
        crate::serve::EventScope::Full,
    );
    Ok(Sse::new(events).keep_alive(KeepAlive::default()).into_response())
}

/// `409 finished`: the execution ended; see the orphans for an outcome no
/// stream delivered.
fn finished() -> Response {
    protocol_error(StatusCode::CONFLICT, "finished", "execution finished")
}

/// `GET /internal/v1/runs/{id}/stream?from=N` (or `Last-Event-ID`): reattach to
/// a running execution; its events and response text are streamed from the
/// cursor. A finished one is `409 finished`.
fn reattach(
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
    if state.active_run(run_id).is_none() {
        return finished();
    }
    attached_stream(state, agent, run_id, execution.epoch, cursor).unwrap_or_else(|refused| refused)
}

/// `POST /internal/v1/runs/{id}/abort` (at `Graphrt-Epoch`).
fn abort(state: &AppState, agent: &GatewayAgent, request: &GatewayRequest, run_id: &str) -> Response {
    let current = agent.execution(run_id).map(|e| e.epoch);
    match (state.active_run(run_id), current) {
        (Some(run), Some(epoch)) if request.epoch == Some(epoch) => {
            run.handle().abort("aborted by the gateway");
            json::response(
                StatusCode::ACCEPTED,
                json::Object::new().str("run_id", run_id).str("status", "aborting").finish(),
            )
        }
        (Some(_), Some(_)) => protocol_error(StatusCode::CONFLICT, "epoch_conflict", "the run is at another epoch"),
        _ => protocol_error(StatusCode::NOT_FOUND, "not_found", "no active execution of this run"),
    }
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
    super::spawn_task(
        &tokio::runtime::Handle::current(),
        Box::pin(async move {
            let report =
                crate::drain::drain(&draining, Duration::from_millis(deadline_ms), "gateway drain", None).await;
            eprintln!("{report}");
        }),
    );
    let body = json::Object::new()
        .bool("draining", true)
        .num("active_runs", active as u64)
        .num("deadline_ms", deadline_ms)
        .finish();
    json::response(StatusCode::ACCEPTED, body)
}

/// `POST /internal/v1/stop`: drain and exit exactly as on `SIGTERM` (how the
/// local-process provider stops an agent on Windows, which has no signal to
/// send). Answers `202` at once.
fn stop(state: &Arc<AppState>) -> Response {
    state.request_stop();
    json::response(StatusCode::ACCEPTED, json::Object::new().bool("stopping", true).finish())
}

/// `DELETE /internal/v1/orphans/{id}` (at `Graphrt-Epoch`): the gateway took the run over.
fn release_orphan(agent: &GatewayAgent, request: &GatewayRequest, run_id: &str) -> Response {
    match request.epoch {
        Some(epoch) if agent.release_orphan(run_id, epoch) => StatusCode::NO_CONTENT.into_response(),
        _ => protocol_error(StatusCode::NOT_FOUND, "not_found", "no orphan of this run at this epoch"),
    }
}
