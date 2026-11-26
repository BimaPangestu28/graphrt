//! `/v1/skills`: manage the skill store of this server (cargo feature `skills`).
//!
//! The same [`SkillStore`] backs the `skills` of the served `agent` nodes, so
//! a skill installed or turned off here applies to the next run.
//!
//! | Method | Path | Body | Response |
//! | --- | --- | --- | --- |
//! | GET | `/v1/skills` | – | `{skills: [SkillEntry]}` |
//! | GET | `/v1/skills/{name}` | – | `SkillDetails` |
//! | POST | `/v1/skills/drafts` | `{name, description, body, requested_tools?}` | `SkillDraft`, checked and scanned, nothing installed |
//! | POST | `/v1/skills` | `{draft: {...}, grants?, accept_findings?, replace?}` | `201` + `SkillEntry` (origin `authored`) |
//! | PATCH | `/v1/skills/{name}` | `{enabled?, grants?}` | `SkillEntry` |
//! | DELETE | `/v1/skills/{name}` | – | `204` |
//!
//! Every route needs the API key. A skill is written here as text and goes
//! through the same validation, scan and grant rules as one installed from a
//! directory. Installing a package from a catalog is not offered over HTTP.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_skill::{Error as SkillError, Grant, InstallOptions, SkillDraft, SkillStore};
use serde::Deserialize;
use serde_json::json;

use crate::http_input::JsonBody;
use crate::serve::{AppState, error_response};

/// A skill written by the caller.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftBody {
    name: String,
    description: String,
    body: String,
    #[serde(default)]
    requested_tools: Vec<String>,
}

/// Body of `POST /v1/skills`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallBody {
    draft: DraftBody,
    #[serde(default)]
    grants: Vec<Grant>,
    #[serde(default)]
    accept_findings: bool,
    #[serde(default)]
    replace: bool,
}

/// Body of `PATCH /v1/skills/{name}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPatch {
    enabled: Option<bool>,
    grants: Option<Vec<Grant>>,
}

fn store(state: &AppState) -> Result<&Arc<SkillStore>, Response> {
    state.skill_store().ok_or_else(|| error_response(StatusCode::NOT_FOUND, "this server has no skill store"))
}

/// Map a skill store error to an HTTP response. File paths stay in the log.
fn skill_error(error: &SkillError) -> Response {
    let status = match error {
        SkillError::NotFound(_) | SkillError::NoSuchFile { .. } => StatusCode::NOT_FOUND,
        SkillError::AlreadyInstalled(_) | SkillError::Blocked { .. } | SkillError::Tampered { .. } => {
            StatusCode::CONFLICT
        }
        SkillError::Io { .. } | SkillError::Index(_) => {
            eprintln!("skill store error: {error}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "skill store error (details in the server log)");
        }
        _ => StatusCode::BAD_REQUEST,
    };
    if let SkillError::Blocked { findings, .. } = error {
        let body = json!({ "error": error.to_string(), "findings": findings });
        return (status, Json(body)).into_response();
    }
    error_response(status, &error.to_string())
}

fn draft_of(body: &DraftBody) -> Result<SkillDraft, Response> {
    SkillDraft::new(&body.name, &body.description, &body.body, &body.requested_tools).map_err(|e| skill_error(&e))
}

/// `GET /v1/skills`
pub async fn list_skills(State(state): State<Arc<AppState>>) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    match store.list() {
        Ok(skills) => Json(json!({ "skills": skills })).into_response(),
        Err(error) => skill_error(&error),
    }
}

/// `GET /v1/skills/{name}`
pub async fn get_skill(State(state): State<Arc<AppState>>, UrlPath(name): UrlPath<String>) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    match store.inspect(&name) {
        Ok(details) => Json(details).into_response(),
        Err(error) => skill_error(&error),
    }
}

/// `POST /v1/skills/drafts`: check and scan a skill without installing it.
pub async fn check_draft(State(state): State<Arc<AppState>>, JsonBody(body): JsonBody<DraftBody>) -> Response {
    if let Err(refused) = store(&state) {
        return refused;
    }
    match draft_of(&body) {
        Ok(draft) => Json(draft).into_response(),
        Err(refused) => refused,
    }
}

/// `POST /v1/skills`: install a skill written by the caller.
pub async fn install_skill(State(state): State<Arc<AppState>>, JsonBody(body): JsonBody<InstallBody>) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let draft = match draft_of(&body.draft) {
        Ok(draft) => draft,
        Err(refused) => return refused,
    };
    let options = InstallOptions {
        grants: body.grants,
        accept_findings: body.accept_findings,
        replace: body.replace,
        ..InstallOptions::default()
    };
    match store.install_draft(&draft, options) {
        Ok(entry) => (StatusCode::CREATED, Json(entry)).into_response(),
        Err(error) => skill_error(&error),
    }
}

/// `PATCH /v1/skills/{name}`: turn a skill on or off, or replace its grants.
pub async fn patch_skill(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    JsonBody(patch): JsonBody<SkillPatch>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    if patch.enabled.is_none() && patch.grants.is_none() {
        return error_response(StatusCode::BAD_REQUEST, "pass `enabled`, `grants` or both");
    }
    // Grants first: a refused grant must not leave the skill half changed.
    let mut result = store.get(&name);
    if let (Ok(_), Some(grants)) = (&result, patch.grants) {
        result = store.set_grants(&name, grants);
    }
    if let (Ok(_), Some(enabled)) = (&result, patch.enabled) {
        result = store.set_enabled(&name, enabled);
    }
    match result {
        Ok(entry) => Json(entry).into_response(),
        Err(error) => skill_error(&error),
    }
}

/// `DELETE /v1/skills/{name}`
pub async fn delete_skill(State(state): State<Arc<AppState>>, UrlPath(name): UrlPath<String>) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    match store.remove(&name) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => skill_error(&error),
    }
}
