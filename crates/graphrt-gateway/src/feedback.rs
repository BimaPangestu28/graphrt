//! `POST /v1/deployments/{d}/runs/{id}/feedback` (cargo feature `feedback`):
//! feedback on a run behind the gateway, forwarded to an agent of the run's
//! version. Behind a feature so the smallest gateway build (`gateway-core`)
//! stays inside its size budget. The release gateway enables it.

// Helpers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_core::store::{RunRecord, now_ms};
use serde_json::{Value as Json, json};

use crate::agent_client::{AgentError, RunScope};
use crate::auth::Principal;
use crate::deployment::Deployment;
use crate::server::{App, error, load_run, parse_object, start_error};

/// `POST .../runs/{id}/feedback`: feedback on a run, recorded by an agent of
/// the run's version (`POST /internal/v1/runs/{id}/feedback`), which learns
/// from it as `graphrt serve` does from its own route. An end user with a
/// verified token rates only a run started with a token of the same user, and
/// is its author whatever `user` says: the agent records their words as their
/// own. With a deployment API key, `user` is a label and the feedback is never
/// a user's own words.
pub(crate) async fn run_feedback(
    app: &App,
    deployment: &Deployment,
    principal: &Principal,
    run_id: &str,
    body: &[u8],
) -> Response {
    let mut feedback = match parse_object(body) {
        Ok(feedback) => feedback,
        Err(response) => return response,
    };
    let record = match load_run(app, deployment, run_id).await {
        Ok(record) => record,
        Err(response) => return response,
    };
    if let Err(refused) = set_feedback_author(&mut feedback, principal, &record) {
        return refused;
    }
    feedback.insert("workflow".into(), Json::from(record.workflow.clone()));
    // Any replica of the run's version: the one that ran it may be gone.
    let version = match deployment.version(record.deployment_version.as_deref().unwrap_or_default(), now_ms()) {
        Ok(version) => version,
        Err(failure) => return start_error(app, failure.into()),
    };
    let Some(key) = deployment.signing_key() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "no_capacity", "the deployment has no gateway key");
    };
    let lease = match version.pool.acquire().await {
        Ok(lease) => lease,
        Err(failure) => return start_error(app, failure.into()),
    };
    let path = format!("/internal/v1/runs/{run_id}/feedback");
    let scope = RunScope { run_id, epoch: None };
    let body = Json::Object(feedback);
    match app.gateway.agents.call_json(&lease.endpoint.address, key, &deployment.name, &path, scope, &body).await {
        Ok(score) => (StatusCode::CREATED, axum::Json(score)).into_response(),
        Err(AgentError::Refused { status, body }) if (400..500).contains(&status) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "error": body }));
            (status, axum::Json(body)).into_response()
        }
        Err(failure) => error(StatusCode::BAD_GATEWAY, "agent_error", &failure.to_string()),
    }
}

/// Who wrote feedback on `record`'s run, as the agent will believe it:
/// `verified` and `user` set over anything the client sent.
///
/// @throws Response - `403` when a verified end user rates a run that was not
///   started with a token of the same user (a `user_id` an API-key caller sent
///   is only a label and makes no run theirs).
fn set_feedback_author(
    feedback: &mut serde_json::Map<String, Json>,
    principal: &Principal,
    record: &RunRecord,
) -> Result<(), Response> {
    let verified = match principal {
        Principal::User { subject, .. } => {
            if !record.user_verified || record.user_id.as_deref() != Some(subject.as_str()) {
                return Err(error(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    "this run does not belong to the authenticated user",
                ));
            }
            feedback.insert("user".into(), Json::from(subject.clone()));
            true
        }
        Principal::ApiKey => false,
    };
    feedback.insert("verified".into(), Json::Bool(verified));
    Ok(())
}
