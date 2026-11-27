//! `POST /v1/workflows/{name}/runs`: start a run.

use std::sync::Arc;

use crate::http_input::JsonBody;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_core::Value;
use graphrt_core::engine::RunInput;
use graphrt_core::store::{RunRecord, now_ms};
use indexmap::IndexMap;
use serde::Deserialize;

use super::auth::AuthenticatedUser;
use super::delivery::{Delivery, Reply};
use super::errors::{error_response, store_error};
use super::execute::{execute, to_values, traceparent};
use super::state::AppState;

/// Body of `POST /v1/workflows/{name}/runs`.
#[derive(Debug, Default, Deserialize)]
pub struct RunRequest {
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub sys: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub conversation: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub stream: bool,
    /// Return `202` with the run id immediately; follow the run via `GET /v1/runs/{id}`
    /// and `/events`.
    #[serde(default, rename = "async")]
    pub run_async: bool,
    /// Caller labels, stored on the run record and stamped on its spans
    /// (`langfuse.trace.metadata.<key>`, `graphrt.run.metadata.<key>`).
    #[serde(default)]
    pub metadata: IndexMap<String, String>,
    /// Session / conversation id (`session.id`, `gen_ai.conversation.id`, `langfuse.session.id`).
    #[serde(default)]
    pub session_id: Option<String>,
    /// End-user id (`user.id`, `langfuse.user.id`).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Tags (`langfuse.trace.tags`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Workspace files attached to the run, by the `reference` an upload
    /// returned (`workspace:...`). They become `sys.files`, as file values a
    /// model node can take as vision input. Needs `--workspace-api`.
    #[cfg(feature = "files")]
    #[serde(default)]
    pub files: Vec<String>,
    /// A conversation the server keeps (`--conversations`): its turns become
    /// `sys.history`, and this run's message and answer are added when it succeeds.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// A token and cost budget for this run, below the server's
    /// (`--max-tokens-per-run`, `--max-cost-per-run`): the lower applies.
    /// Builds with model nodes only.
    #[cfg(feature = "llm")]
    #[serde(default)]
    pub limits: RunLimits,
}

/// `limits` of a run request.
#[cfg(feature = "llm")]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunLimits {
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    /// In micro units of the price unit of the providers file.
    #[serde(default)]
    pub max_cost_micros: Option<u64>,
}

/// Limits on the caller context of a run request.
const MAX_METADATA_ENTRIES: usize = 64;
const MAX_TAGS: usize = 32;
const MAX_LABEL_BYTES: usize = 256;
const MAX_METADATA_VALUE_BYTES: usize = 2048;

impl RunRequest {
    /// Validate and copy the caller context onto `record`.
    ///
    /// @throws String - A message for a `400` response when a field is too large.
    pub(crate) fn apply_context(&self, record: &mut RunRecord) -> Result<(), String> {
        if self.metadata.len() > MAX_METADATA_ENTRIES {
            return Err(format!("`metadata` has more than {MAX_METADATA_ENTRIES} entries"));
        }
        if self.tags.len() > MAX_TAGS {
            return Err(format!("`tags` has more than {MAX_TAGS} entries"));
        }
        let label_ok = |text: &str| !text.is_empty() && text.len() <= MAX_LABEL_BYTES;
        if let Some(key) = self.metadata.keys().find(|k| !label_ok(k)) {
            return Err(format!("`metadata` key `{key}` must be 1-{MAX_LABEL_BYTES} bytes"));
        }
        if self.metadata.values().any(|v| v.len() > MAX_METADATA_VALUE_BYTES) {
            return Err(format!("`metadata` values must be at most {MAX_METADATA_VALUE_BYTES} bytes"));
        }
        let ids = [("session_id", &self.session_id), ("user_id", &self.user_id)];
        if let Some((name, _)) = ids.iter().find(|(_, id)| id.as_deref().is_some_and(|id| !label_ok(id))) {
            return Err(format!("`{name}` must be 1-{MAX_LABEL_BYTES} bytes"));
        }
        if self.tags.iter().any(|t| !label_ok(t)) {
            return Err(format!("`tags` entries must be 1-{MAX_LABEL_BYTES} bytes"));
        }
        record.metadata.clone_from(&self.metadata);
        record.session_id.clone_from(&self.session_id);
        record.user_id.clone_from(&self.user_id);
        record.tags.clone_from(&self.tags);
        Ok(())
    }
}

/// `POST /v1/workflows/{name}/runs`.
pub(super) async fn start_run(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    user: Option<axum::Extension<AuthenticatedUser>>,
    headers: axum::http::HeaderMap,
    JsonBody(mut body): JsonBody<RunRequest>,
) -> Response {
    let Some(workflow) = state.workflows.get(&name) else {
        return error_response(StatusCode::NOT_FOUND, &format!("unknown workflow `{name}`"));
    };
    if let Some(busy) = state.refuse_if_busy() {
        return busy;
    }
    let run_id = state.new_run_id();
    let mut record = RunRecord::running(&run_id, &name, workflow.engine.fingerprint(), now_ms());
    // Validate the run context before the body's inputs are moved out of it.
    if let Err(message) = body.apply_context(&mut record) {
        return error_response(StatusCode::BAD_REQUEST, &message);
    }
    // The page context of a web chat: checked, never read as identity.
    if let Err(message) = super::check_page_context(&body.sys) {
        return error_response(StatusCode::BAD_REQUEST, &message);
    }
    let verified_user = user.map(|axum::Extension(AuthenticatedUser(subject))| subject);
    if let Err(refused) = join_conversation(&state, &name, &run_id, verified_user.as_deref(), &mut body).await {
        return refused;
    }
    record.session_id.clone_from(&body.session_id);
    let mut inputs = to_values(std::mem::take(&mut body.inputs));
    if let Some(bindings) = state.header_inputs.get(&name)
        && let Err(refused) = bindings.apply_to_inputs(&headers, &mut inputs)
    {
        return refused.into_response();
    }
    #[cfg(feature = "files")]
    let attached = match attached_files(&state, &body.files, verified_user.as_deref()).await {
        Ok(attached) => attached,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, &message),
    };
    if let Some(subject) = &verified_user {
        // A verified end user (`--verify-jwt`) is the run's user, whatever the body said.
        record.user_id = Some(subject.clone());
        record.user_verified = true;
    }
    if let Err(e) = state.store.create(record.clone()).await {
        return store_error(&e);
    }
    let reply = Reply {
        delivery: Delivery::of(body.stream, body.run_async),
        scope: state.event_scope(verified_user.is_some()),
    };
    let mut input = RunInput::new();
    input.run_id = Some(run_id);
    input.inputs = inputs;
    input.sys = run_sys(body.sys, verified_user);
    #[cfg(feature = "files")]
    if let Some(files) = attached {
        input.sys.insert("files".to_owned(), files);
    }
    input.conversation = to_values(body.conversation);
    input.env = workflow.env.clone();
    #[cfg(feature = "llm")]
    {
        input.max_total_tokens = body.limits.max_total_tokens;
        input.max_cost_micros = body.limits.max_cost_micros;
    }
    execute(&state, &workflow.engine, record, input, reply, traceparent(&headers)).await
}

/// Join the run to the conversation its request names (`conversation_id`).
///
/// @throws Response - See [`crate::conversations::join_run`]; `400` when this
///   build keeps no conversations.
#[allow(clippy::result_large_err)]
async fn join_conversation(
    state: &AppState,
    workflow: &str,
    run_id: &str,
    user: Option<&str>,
    body: &mut RunRequest,
) -> Result<(), Response> {
    #[cfg(feature = "conversations")]
    return crate::conversations::join_run(state, workflow, run_id, user, body).await;
    #[cfg(not(feature = "conversations"))]
    {
        let _ = (state, workflow, run_id, user);
        match body.conversation_id {
            Some(_) => Err(error_response(
                StatusCode::BAD_REQUEST,
                "`conversation_id` is not supported by this build (cargo feature `conversations`)",
            )),
            None => Ok(()),
        }
    }
}

/// The file values of the workspace files a run request attached, checked
/// against what its caller may reach: a verified end user only their own
/// directory. `None` when the request attached nothing.
///
/// @throws String - A message for a `400` response.
#[cfg(feature = "files")]
async fn attached_files(
    state: &AppState,
    references: &[String],
    verified_user: Option<&str>,
) -> Result<Option<Value>, String> {
    if references.is_empty() {
        return Ok(None);
    }
    let Some(api) = state.file_api.as_deref() else {
        return Err("`files` needs the workspace API (`--workspace-api`)".to_owned());
    };
    let caller = match verified_user {
        #[cfg(feature = "jwt")]
        Some(subject) => super::auth::Caller::User(subject.to_owned()),
        #[cfg(not(feature = "jwt"))]
        Some(_) => super::auth::Caller::ApiKey,
        None => super::auth::Caller::ApiKey,
    };
    api.attachments(&caller, references).await.map(Some)
}

/// The run's `sys` values: the caller's, with a verified end user's `sub` as
/// `user_id`, which wins over anything the client put there.
fn run_sys(sys: serde_json::Map<String, serde_json::Value>, verified_user: Option<String>) -> IndexMap<String, Value> {
    let mut values = to_values(sys);
    if let Some(subject) = verified_user {
        values.insert("user_id".to_owned(), Value::String(subject.into()));
    }
    values
}
