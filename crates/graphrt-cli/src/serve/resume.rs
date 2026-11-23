//! `POST /v1/runs/{run_id}/resume`: continue a paused run; and at startup,
//! the runs a previous shutdown drained ([`resume_drained_runs`]).

use std::sync::Arc;

use crate::http_input::JsonBody;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_core::Snapshot;
use graphrt_core::engine::{ResumeFrom, RunInput};
use graphrt_core::store::{RunFilter, RunRecord, RunStatus, StoreError, now_ms};
use indexmap::IndexMap;
use serde::Deserialize;

use super::delivery::{Delivery, EventScope, Reply};
use super::errors::{error_response, store_error};
use super::execute::{execute, launch, to_values, traceparent};
use super::state::{AppState, ServedWorkflow};

/// Body of `POST /v1/runs/{run_id}/resume`.
#[derive(Debug, Deserialize)]
pub struct ResumeRequest {
    /// Snapshot from a paused response. Optional: when omitted, the server uses the
    /// snapshot it stored when the run paused.
    #[serde(default)]
    pub snapshot: Option<Snapshot>,
    /// Input for paused nodes keyed by node id (or `<frame>/<node_id>`).
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Secrets (other than `env.*`) to re-supply, keyed by `namespace.name`.
    /// Snapshots never contain secret values.
    #[serde(default)]
    pub secrets: IndexMap<String, String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, rename = "async")]
    pub run_async: bool,
}

/// `POST /v1/runs/{run_id}/resume`.
///
/// Three phases: checks that need no store access, the claim (the run becomes
/// `running`), then everything that can still refuse after the claim. The last
/// phase is one fallible function whose refusals all pass through a single
/// [`release_claim`], so no early return can leave a run stuck as `running`
/// until the next restart.
pub(super) async fn resume_run(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    headers: axum::http::HeaderMap,
    JsonBody(body): JsonBody<ResumeRequest>,
) -> Response {
    if let Some(refused) = refuse_resume(&state, &run_id, body.snapshot.as_ref()) {
        return refused;
    }
    let record = match claim_run(&state, &run_id, body.snapshot.as_ref()).await {
        Ok(record) => record,
        Err(refused) => return refused,
    };
    let delivery = Delivery::of(body.stream, body.run_async);
    let (workflow, input) = match prepare_resume(&state, &record, body, &headers).await {
        Ok(prepared) => prepared,
        Err(refused) => {
            release_claim(&state, record).await;
            return refused;
        }
    };
    // Resume takes the API key only, so its caller gets every event.
    let reply = Reply { delivery, scope: EventScope::Full };
    execute(&state, &workflow.engine, record, input, reply, traceparent(&headers)).await
}

/// Refusals that need no store access: the run executes already, the server
/// is at `--max-active-runs`, or a client snapshot belongs to another run.
fn refuse_resume(state: &AppState, run_id: &str, client_snapshot: Option<&Snapshot>) -> Option<Response> {
    if state.active(run_id).is_some() {
        return Some(error_response(StatusCode::CONFLICT, "run is already executing"));
    }
    if let Some(busy) = state.refuse_if_busy() {
        return Some(busy);
    }
    if client_snapshot.is_some_and(|snapshot| snapshot.run_id != run_id) {
        return Some(error_response(StatusCode::BAD_REQUEST, "snapshot belongs to a different run"));
    }
    None
}

/// Mark the run `running` for this resume. A client-held snapshot also
/// resumes runs this server never recorded: those get a new record.
///
/// @throws Response - The store's error response, or `404` for a client
///   snapshot of an unknown run that no served workflow matches.
async fn claim_run(state: &AppState, run_id: &str, client_snapshot: Option<&Snapshot>) -> Result<RunRecord, Response> {
    match state.store.claim_resume(run_id, now_ms()).await {
        Ok(record) => Ok(record),
        Err(e) => match (e, client_snapshot) {
            (StoreError::NotFound(_), Some(snapshot)) => record_unknown_run(state, run_id, snapshot).await,
            (e, _) => Err(store_error(&e)),
        },
    }
}

/// Create a `running` record for a client snapshot of a run this server has no
/// record of, under the served workflow the snapshot was taken from.
///
/// @throws Response - `404` when no served workflow matches, or the store's error response.
async fn record_unknown_run(state: &AppState, run_id: &str, snapshot: &Snapshot) -> Result<RunRecord, Response> {
    let Some((name, _)) = state.workflow_with_fingerprint(&snapshot.graph_fingerprint) else {
        return Err(error_response(StatusCode::NOT_FOUND, "no served workflow matches this snapshot"));
    };
    let record = RunRecord::running(run_id, name, &snapshot.graph_fingerprint, now_ms());
    state.store.create(record.clone()).await.map_err(|e| store_error(&e))?;
    Ok(record)
}

/// Everything a claimed resume still needs: the snapshot (the client's, else
/// the stored one), the served workflow, and the header-mapped secrets.
///
/// @throws Response - `409` without a snapshot, `404` when the workflow is not
///   served, or the header inputs' refusal. The caller releases the claim.
async fn prepare_resume<'a>(
    state: &'a AppState,
    record: &RunRecord,
    body: ResumeRequest,
    headers: &axum::http::HeaderMap,
) -> Result<(&'a ServedWorkflow, RunInput), Response> {
    let snapshot = match body.snapshot {
        Some(snapshot) => snapshot,
        None => load_stored_snapshot(state, &record.run_id).await?,
    };
    let Some((name, workflow)) = state
        .workflows
        .get_key_value(&record.workflow)
        .or_else(|| state.workflow_with_fingerprint(&snapshot.graph_fingerprint))
    else {
        return Err(error_response(StatusCode::NOT_FOUND, &format!("workflow `{}` is not served", record.workflow)));
    };
    let mut secrets = body.secrets;
    if let Some(bindings) = state.header_inputs.get(name) {
        bindings.apply_to_resume(headers, &mut secrets).map_err(IntoResponse::into_response)?;
    }
    let mut resume = ResumeFrom::new(snapshot, to_values(body.inputs));
    for (selector, secret) in secrets {
        resume = resume.secret(selector, secret);
    }
    let mut input = RunInput::new();
    input.run_id = Some(record.run_id.clone());
    input.env = workflow.env.clone();
    input.resume = Some(resume);
    Ok((workflow, input))
}

/// The snapshot the server stored when the run paused.
///
/// @throws Response - `409` when there is none (or it cannot be read).
async fn load_stored_snapshot(state: &AppState, run_id: &str) -> Result<Snapshot, Response> {
    match state.store.load_snapshot(run_id).await {
        Ok(Some(snapshot)) => Ok(snapshot),
        Ok(None) | Err(_) => Err(error_response(
            StatusCode::CONFLICT,
            "no stored snapshot for this run; pass the `snapshot` from the paused response",
        )),
    }
}

/// Undo a resume claim so a refused resume leaves the run visible as paused. The
/// write is fenced by the claim's epoch, so it never undoes a later claim.
async fn release_claim(state: &AppState, mut record: RunRecord) {
    record.status = RunStatus::Paused;
    let run_id = record.run_id.clone();
    let epoch = record.epoch;
    if let Err(e) = state.store.put_if(record, epoch).await {
        eprintln!("warning: run {run_id}: cannot restore paused status: {e}");
    }
}

/// At startup: resume the async and webhook runs a previous shutdown drained
/// ([`crate::drain::is_auto_resumable`]) in the background. Runs whose workflow
/// changed, or that need secrets only their caller has, stay paused for a
/// resume over the API. Returns how many were resumed.
pub async fn resume_drained_runs(state: Arc<AppState>) -> usize {
    let mut resumed = 0;
    let mut cursor = None;
    loop {
        let filter = RunFilter { status: Some(RunStatus::Paused), limit: 100, cursor, ..RunFilter::default() };
        let page = match state.store.list(filter).await {
            Ok(page) => page,
            Err(e) => {
                eprintln!("warning: cannot list drained runs to resume: {e}");
                return resumed;
            }
        };
        for record in page.runs.iter().filter(|r| crate::drain::is_auto_resumable(r)) {
            if state.refuse_if_busy().is_some() {
                eprintln!("warning: not resuming further drained runs: the server is busy or draining");
                return resumed;
            }
            match resume_drained(&state, &record.run_id).await {
                Ok(()) => resumed += 1,
                Err(why) => eprintln!("run {}: drained run left paused: {why}", record.run_id),
            }
        }
        cursor = match page.next_cursor {
            Some(next) => Some(next),
            None => break,
        };
    }
    if resumed > 0 {
        eprintln!("resumed {resumed} run(s) drained by the previous shutdown");
    }
    resumed
}

/// Claim and resume one drained run as a detached run. Like an API resume,
/// every refusal after the claim passes through one [`release_claim`].
///
/// @throws String - Why the run stays paused.
async fn resume_drained(state: &Arc<AppState>, run_id: &str) -> Result<(), String> {
    let record = state.store.claim_resume(run_id, now_ms()).await.map_err(|e| e.to_string())?;
    let (workflow, input) = match prepare_drained_resume(state, &record).await {
        Ok(prepared) => prepared,
        Err(why) => {
            release_claim(state, record).await;
            return Err(why);
        }
    };
    let active = launch(state, &workflow.engine, record, input, None);
    active.set_detached();
    Ok(())
}

/// The workflow and input of a claimed drained run: its stored snapshot, on
/// the unchanged served workflow, without caller-supplied secrets.
///
/// @throws String - Why the run cannot resume by itself. The caller releases the claim.
async fn prepare_drained_resume<'a>(
    state: &'a AppState,
    record: &RunRecord,
) -> Result<(&'a ServedWorkflow, RunInput), String> {
    let snapshot = match state.store.load_snapshot(&record.run_id).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) | Err(_) => return Err("no stored snapshot".to_owned()),
    };
    let workflow =
        state.workflows.get(&record.workflow).filter(|w| w.engine.graph().fingerprint() == snapshot.graph_fingerprint);
    let Some(workflow) = workflow else {
        return Err("its workflow is not served or changed since".to_owned());
    };
    if crate::drain::needs_caller_secrets(&snapshot) {
        return Err("it needs secrets only its caller can supply; resume it over the API".to_owned());
    }
    let mut input = RunInput::new();
    input.run_id = Some(record.run_id.clone());
    input.env = workflow.env.clone();
    input.resume = Some(ResumeFrom::new(snapshot, IndexMap::new()));
    Ok((workflow, input))
}
