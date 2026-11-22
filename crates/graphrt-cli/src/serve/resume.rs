//! `POST /v1/runs/{run_id}/resume`: continue a paused run.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_core::Snapshot;
use graphrt_core::engine::{ResumeFrom, RunInput};
use graphrt_core::store::{RunRecord, RunStatus, StoreError, now_ms};
use indexmap::IndexMap;
use serde::Deserialize;

use super::delivery::{Delivery, EventScope, Reply};
use super::errors::{error_response, store_error};
use super::execute::{execute, to_values, traceparent};
use super::state::AppState;

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

pub(super) async fn resume_run(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ResumeRequest>,
) -> Response {
    if state.active(&run_id).is_some() {
        return error_response(StatusCode::CONFLICT, "run is already executing");
    }
    if let Some(busy) = state.refuse_if_busy() {
        return busy;
    }
    let (record, snapshot) = match body.snapshot {
        // Resume from the server's own records.
        None => {
            let record = match state.store.claim_resume(&run_id, now_ms()).await {
                Ok(r) => r,
                Err(e) => return store_error(&e),
            };
            match state.store.load_snapshot(&run_id).await {
                Ok(Some(snapshot)) => (record, snapshot),
                Ok(None) | Err(_) => {
                    release_claim(&state, record).await;
                    return error_response(
                        StatusCode::CONFLICT,
                        "no stored snapshot for this run; pass the `snapshot` from the paused response",
                    );
                }
            }
        }
        // Client-held snapshot (also works for runs this server never recorded).
        Some(snapshot) => {
            if snapshot.run_id != run_id {
                return error_response(StatusCode::BAD_REQUEST, "snapshot belongs to a different run");
            }
            let record = match state.store.claim_resume(&run_id, now_ms()).await {
                Ok(r) => r,
                Err(StoreError::NotFound(_)) => {
                    let Some((name, _)) = state
                        .workflows
                        .iter()
                        .find(|(_, w)| w.engine.graph().fingerprint() == snapshot.graph_fingerprint)
                    else {
                        return error_response(StatusCode::NOT_FOUND, "no served workflow matches this snapshot");
                    };
                    let record = RunRecord::running(&run_id, name, &snapshot.graph_fingerprint, now_ms());
                    if let Err(e) = state.store.create(record.clone()).await {
                        return store_error(&e);
                    }
                    record
                }
                Err(e) => return store_error(&e),
            };
            (record, snapshot)
        }
    };
    let workflow = state
        .workflows
        .get_key_value(&record.workflow)
        .or_else(|| state.workflows.iter().find(|(_, w)| w.engine.graph().fingerprint() == snapshot.graph_fingerprint));
    let Some((name, workflow)) = workflow else {
        let message = format!("workflow `{}` is not served", record.workflow);
        // The claim above marked the run `running`; without a release it would stay so until a restart.
        release_claim(&state, record).await;
        return error_response(StatusCode::NOT_FOUND, &message);
    };
    let mut secrets = body.secrets;
    if let Some(bindings) = state.header_inputs.get(name)
        && let Err(refused) = bindings.apply_to_resume(&headers, &mut secrets)
    {
        release_claim(&state, record).await;
        return refused.into_response();
    }
    let mut input = RunInput::new();
    input.run_id = Some(run_id);
    input.env = workflow.env.clone();
    let mut resume = ResumeFrom::new(snapshot, to_values(body.inputs));
    for (selector, secret) in secrets {
        resume = resume.secret(selector, secret);
    }
    input.resume = Some(resume);
    // Resume takes the API key only, so its caller gets every event.
    let reply = Reply { delivery: Delivery::of(body.stream, body.run_async), scope: EventScope::Full };
    execute(&state, &workflow.engine, record, input, reply, traceparent(&headers)).await
}

/// Undo a resume claim so a refused resume leaves the run visible as paused.
async fn release_claim(state: &AppState, mut record: RunRecord) {
    record.status = RunStatus::Paused;
    let run_id = record.run_id.clone();
    if let Err(e) = state.store.put(record).await {
        eprintln!("warning: run {run_id}: cannot restore paused status: {e}");
    }
}
