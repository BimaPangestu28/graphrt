//! `/v1/skills`: manage the skill store of this server (cargo feature `skills`).
//!
//! The same [`SkillStore`] backs the `skills` of the served `agent` nodes, so
//! a skill installed or turned off here applies to the next run.
//!
//! | Method | Path | Body | Response |
//! | --- | --- | --- | --- |
//! | GET | `/v1/skills` | – | `{skills: [SkillEntry]}`, plus `bundled` when a served bundle brought skills |
//! | GET | `/v1/skills/{name}` | – | `SkillDetails` |
//! | POST | `/v1/skills/drafts` | `{name, description, body, requested_tools?}` | `SkillDraft`, checked and scanned, nothing installed |
//! | POST | `/v1/skills/drafts/model` | `{model, describe? \| conversation?, tools?}` | `SkillDraft` written by a model, nothing installed |
//! | POST | `/v1/skills` | `{draft: {...}, grants?, accept_findings?, replace?}` | `201` + `SkillEntry` (origin `authored`) |
//! | POST | `/v1/skills/catalog/install` | `{skill, grants?, accept_findings?, replace?}` | `201` + `SkillEntry` (origin `catalog`, verified) |
//! | PATCH | `/v1/skills/{name}` | `{enabled?, grants?}` | `SkillEntry` |
//! | DELETE | `/v1/skills/{name}` | – | `204` |
//!
//! Every route is an owner action: it needs the API key, a user token is not
//! enough, and a server that listens beyond loopback without a key refuses
//! them all (see `refuse_closed_owner_api` in `serve::auth`).
//!
//! A skill written here as text, a draft a model wrote and a skill from the
//! catalog all go through the same validation, scan and grant rules as one
//! installed from a directory.
//!
//! **Drafting with a model** installs nothing. The answer is a draft to read,
//! edit and post to `POST /v1/skills`. The model is one of this server's
//! configured providers, and the source is passed to it as quoted material.
//!
//! **Installing from the catalog** uses the catalog this server was started
//! with (`$GRAPHRT_PLUGIN_INDEX`, `$GRAPHRT_PLUGIN_INDEX_KEY`, else the
//! graphrt-plugins catalog and its built-in key). A request cannot name an
//! index or a key, and there is no unsigned mode: the index signature is
//! checked before the index is parsed, exactly as `graphrt skill install`
//! does it.
//!
//! **Skills of a bundle.** A served `.grt` bundle that brings skills runs on
//! its own private store. Those skills are listed under `bundled` and cannot
//! be changed here: the bundle pins them. They are not in this server's store
//! and no other workflow can reach them.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_skill::{
    DraftRequest, DraftSource, Error as SkillError, Grant, InstallOptions, SkillDraft, SkillStore, Turn, draft_skill,
};
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
    /// What an earlier check found. Accepted so that a draft can be posted
    /// back as it was returned, and ignored: the scan runs again.
    #[serde(default, rename = "findings")]
    _findings: serde::de::IgnoredAny,
}

/// Body of `POST /v1/skills/drafts/model`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDraftBody {
    /// `provider/name`, a provider this server has configured.
    model: String,
    /// What the skill should do, in plain language.
    describe: Option<String>,
    /// A conversation that shows how the task is done.
    conversation: Option<Vec<TurnBody>>,
    /// Tools the draft may ask for. Asking grants nothing.
    #[serde(default)]
    tools: Vec<String>,
}

/// One turn of [`ModelDraftBody::conversation`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnBody {
    #[serde(default = "default_role")]
    role: String,
    content: String,
}

fn default_role() -> String {
    "user".to_owned()
}

/// Body of `POST /v1/skills/catalog/install`. It has no field for an index, a
/// key or an unsigned mode on purpose: unknown fields are refused.
#[cfg(feature = "plugins")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogInstallBody {
    /// `name` or `name@1.2.3`.
    skill: String,
    #[serde(default)]
    grants: Vec<Grant>,
    #[serde(default)]
    accept_findings: bool,
    #[serde(default)]
    replace: bool,
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

/// `GET /v1/skills`: this server's skills, and read only, the skills each
/// served bundle brought with it.
pub async fn list_skills(State(state): State<Arc<AppState>>) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let skills = match store.list() {
        Ok(skills) => skills,
        Err(error) => return skill_error(&error),
    };
    let mut bundled = Vec::new();
    for (workflow, store) in state.bundled_skills() {
        match store.list() {
            Ok(skills) => bundled.push(json!({ "workflow": workflow, "skills": skills })),
            Err(error) => return skill_error(&error),
        }
    }
    if bundled.is_empty() {
        return Json(json!({ "skills": skills })).into_response();
    }
    Json(json!({ "skills": skills, "bundled": bundled })).into_response()
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

/// `POST /v1/skills/drafts/model`: ask one of this server's models to write a
/// skill from a description or a conversation. Nothing is installed: the
/// answer is a draft for a person to read, and `POST /v1/skills` installs it.
pub async fn draft_with_model(
    State(state): State<Arc<AppState>>,
    JsonBody(body): JsonBody<ModelDraftBody>,
) -> Response {
    if let Err(refused) = store(&state) {
        return refused;
    }
    let source = match (body.describe, body.conversation) {
        (Some(text), None) => DraftSource::Description(text),
        (None, Some(turns)) => DraftSource::Conversation(
            turns.into_iter().map(|turn| Turn { role: turn.role, text: turn.content }).collect(),
        ),
        _ => return error_response(StatusCode::BAD_REQUEST, "pass exactly one of `describe` or `conversation`"),
    };
    let model = match state.text_model(&body.model) {
        Ok(model) => model,
        Err(refused) => return refused,
    };
    let request = DraftRequest { source, available_tools: body.tools };
    match tokio::time::timeout(crate::serve::MODEL_ANSWER_TIMEOUT, draft_skill(&model, &request)).await {
        Ok(Ok(draft)) => Json(draft).into_response(),
        // The model failed or its answer is not a skill: the caller's request was fine.
        Ok(Err(SkillError::Draft(reason))) => {
            error_response(StatusCode::BAD_GATEWAY, &format!("no draft was written: {reason}"))
        }
        Ok(Err(error)) => skill_error(&error),
        Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, "the model did not answer in time"),
    }
}

/// `POST /v1/skills/catalog/install`: install a skill from the signed catalog
/// this server was started with.
#[cfg(feature = "plugins")]
pub async fn install_from_catalog(
    State(state): State<Arc<AppState>>,
    JsonBody(body): JsonBody<CatalogInstallBody>,
) -> Response {
    use crate::plugin_catalog::skills::{CatalogSkillError, checked_origin, fetch_catalog_skill};
    let store = match store(&state) {
        Ok(store) => Arc::clone(store),
        Err(refused) => return refused,
    };
    let Some(spec) = crate::plugin_catalog::index::parse_plugin_spec(&body.skill)
        .filter(|spec| graphrt_skill::is_valid_name(&spec.name))
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "`skill` must be a catalog skill name (`name` or `name@1.2.3`)",
        );
    };
    let catalog = state.skill_catalog().clone();
    // Reading the catalog blocks (file and network I/O with their own timeouts).
    let installed = tokio::task::spawn_blocking(move || {
        let fetched = fetch_catalog_skill(&spec, catalog.source()).map_err(|error| match error {
            CatalogSkillError::NotListed(error) => error_response(StatusCode::NOT_FOUND, &error.message),
            CatalogSkillError::Refused(error) => catalog_refusal(&error.message),
        })?;
        // The archive must hold the skill the signed index describes.
        let origin = checked_origin(&store, &fetched).map_err(|error| catalog_refusal(&error.message))?;
        let options = InstallOptions {
            grants: body.grants,
            accept_findings: body.accept_findings,
            replace: body.replace,
            origin,
        };
        store.install(&fetched.dir, options).map_err(|error| skill_error(&error))
    })
    .await;
    match installed {
        Ok(Ok(entry)) => (StatusCode::CREATED, Json(entry)).into_response(),
        Ok(Err(refused)) => refused,
        Err(error) => {
            eprintln!("skill catalog install failed: {error}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "the install did not finish (details in the server log)")
        }
    }
}

/// The catalog could not be trusted or read, or the archive is not what the
/// signed index describes. Nothing was installed.
#[cfg(feature = "plugins")]
fn catalog_refusal(reason: &str) -> Response {
    error_response(StatusCode::BAD_GATEWAY, &format!("nothing was installed: {reason}"))
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
