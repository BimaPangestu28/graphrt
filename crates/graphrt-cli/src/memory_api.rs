//! `/v1/memory` and `/v1/me/memory`: what the served agents remember (cargo feature `memory`).
//!
//! The same [`MemoryStore`] backs `long_term_memory` of the served `agent`
//! nodes, so a note approved here is recalled by the next run. Every scope is
//! under the tenant of this server (`--memory-tenant`).
//!
//! | Method | Path | Body or query | Response |
//! | --- | --- | --- | --- |
//! | GET | `/v1/memory/{agent}/notes` | `?user=`, `?status=active\|pending`, `?q=` | `{notes: [Note]}` (no `user`: the agent's own notes) |
//! | POST | `/v1/memory/{agent}/notes` | `{text, user?, kind?, tags?, actor?}` | `201` + `Note` (active, trust `owner`) |
//! | DELETE | `/v1/memory/{agent}/notes/{id}` | `?user=` | `{removed}` |
//! | POST | `/v1/memory/{agent}/notes/{id}/approve` | `?user=` | `Note` |
//! | POST | `/v1/memory/{agent}/notes/{id}/reject` | `?user=` | `{removed: 1}` |
//! | GET | `/v1/memory/{agent}/users/{user}/export` | – | `{notes, lessons}` |
//! | DELETE | `/v1/memory/{agent}/users/{user}` | – | `{notes, lessons}` removed |
//! | GET | `/v1/memory/{agent}/audit` | `?limit=` | `{entries}` |
//! | GET | `/v1/memory/{agent}/graph` | `?user=` | `{entities, relations}` (feature `memory-graph`) |
//! | GET | `/v1/me/memory/{agent}` | `?status=` | the verified user's own notes |
//! | DELETE | `/v1/me/memory/{agent}` | – | erase the verified user's own notes and lessons |
//! | DELETE | `/v1/me/memory/{agent}/{id}` | – | remove one own note |
//!
//! `/v1/memory` is an owner action: it needs the API key, a user token is not
//! enough, and a server that listens beyond loopback without a key refuses
//! it. `/v1/me/memory` needs a verified user token (`--verify-jwt`) and only
//! ever reaches that user's own scope. The audit log records the actor of
//! every change and never the text of a note.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use graphrt_core::memory::{
    ForgetTarget, MemoryBank, MemoryError, MemoryKind, MemoryScope, MemorySource, MemoryStatus, MemoryTrust, NewMemory,
    WritePolicy,
};
use graphrt_memory::{ListFilter, MemoryStore};
use serde::Deserialize;
use serde_json::json;

use crate::http_input::{JsonBody, QueryParams};
use crate::serve::{AppState, error_response};

const DEFAULT_ACTOR: &str = "api";
const DEFAULT_AUDIT: usize = 200;
const MAX_AUDIT: usize = 5_000;

/// The owner routes; the caller adds the API key and the closed-owner-API layers.
pub(crate) fn owner_routes(api: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    let api = api
        .route("/v1/memory/{agent}/notes", get(list).post(add))
        .route("/v1/memory/{agent}/notes/{id}", delete(remove))
        .route("/v1/memory/{agent}/notes/{id}/approve", post(approve))
        .route("/v1/memory/{agent}/notes/{id}/reject", post(reject))
        .route("/v1/memory/{agent}/users/{user}/export", get(export_user))
        .route("/v1/memory/{agent}/users/{user}", delete(erase_user))
        .route("/v1/memory/{agent}/audit", get(audit));
    #[cfg(feature = "memory-graph")]
    let api = api.route("/v1/memory/{agent}/graph", get(graph));
    api
}

/// `GET /v1/memory/{agent}/graph?user=`: the entities and relations of a user,
/// or of the agent itself without `user`.
#[cfg(feature = "memory-graph")]
async fn graph(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let params = QueryParams::parse(raw.as_deref());
    match store.export_user(&scope(&state, &agent, params.string("user"))).await {
        Ok(export) => Json(export.get("graph").cloned().unwrap_or_default()).into_response(),
        Err(e) => memory_error(&e),
    }
}

/// The routes of the verified user; they authenticate themselves.
pub(crate) fn me_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/me/memory/{agent}", get(my_notes).delete(erase_me))
        .route("/v1/me/memory/{agent}/{id}", delete(remove_mine))
}

/// Body of `POST /v1/memory/{agent}/notes`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewNoteBody {
    text: String,
    user: Option<String>,
    #[serde(default)]
    kind: MemoryKind,
    #[serde(default)]
    tags: Vec<String>,
    actor: Option<String>,
}

fn store(state: &AppState) -> Result<&MemoryStore, Response> {
    state.memory_store().ok_or_else(|| error_response(StatusCode::NOT_FOUND, "this server has no memory store"))
}

/// The scope of `user` as a request names it: a scoped id (`channel/shop:42`,
/// `user/<sub>`, `api/<id>`) as it is, any other id as one the owner names (`api/<id>`).
fn scope(state: &AppState, agent: &str, user: Option<String>) -> MemoryScope {
    let user = user.map(|user| graphrt_core::memory::owner_named_user(&user));
    MemoryScope { tenant: state.memory_tenant().map(str::to_owned), agent: agent.to_owned(), user }
}

/// The id lessons know `user` (as a request names it) by: `sys.user_id` of its runs.
#[cfg(feature = "learning")]
fn lesson_user(user: &str) -> String {
    graphrt_core::memory::run_user_id(&graphrt_core::memory::owner_named_user(user)).to_owned()
}

fn owner(actor: Option<&str>) -> MemorySource {
    let actor = actor.filter(|a| !a.trim().is_empty()).unwrap_or(DEFAULT_ACTOR);
    MemorySource { actor: format!("owner:{actor}"), ..MemorySource::default() }
}

/// A memory error as a response. Store failures name server paths: logged, not sent.
fn memory_error(error: &MemoryError) -> Response {
    let status = match error.kind {
        "memory_store" | "memory_corrupt" => {
            eprintln!("memory store error: {error}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "memory store error (details in the server log)");
        }
        "memory_not_found" => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_REQUEST,
    };
    error_response(status, &error.message)
}

fn status_filter(params: &QueryParams) -> Result<Option<MemoryStatus>, Response> {
    match params.string("status").as_deref() {
        None => Ok(None),
        Some("active") => Ok(Some(MemoryStatus::Active)),
        Some("pending") => Ok(Some(MemoryStatus::Pending)),
        Some(other) => {
            Err(error_response(StatusCode::BAD_REQUEST, &format!("`status` is `active` or `pending`, not `{other}`")))
        }
    }
}

/// `GET /v1/memory/{agent}/notes`
async fn list(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let params = QueryParams::parse(raw.as_deref());
    let status = match status_filter(&params) {
        Ok(status) => status,
        Err(refused) => return refused,
    };
    let filter = ListFilter { status, query: params.string("q") };
    match store.list(&scope(&state, &agent, params.string("user")), &filter).await {
        Ok(notes) => Json(json!({ "notes": notes })).into_response(),
        Err(e) => memory_error(&e),
    }
}

/// `POST /v1/memory/{agent}/notes`: a note written by the owner, used at once.
async fn add(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    JsonBody(body): JsonBody<NewNoteBody>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let note = NewMemory::new(body.text, MemoryTrust::Owner, owner(body.actor.as_deref()))
        .kind(body.kind)
        .tags(body.tags)
        .policy(WritePolicy::Open);
    match store.remember(&scope(&state, &agent, body.user), note).await {
        Ok(record) => (StatusCode::CREATED, Json(record)).into_response(),
        Err(e) => memory_error(&e),
    }
}

/// `DELETE /v1/memory/{agent}/notes/{id}`
async fn remove(
    State(state): State<Arc<AppState>>,
    UrlPath((agent, id)): UrlPath<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let params = QueryParams::parse(raw.as_deref());
    let scope = scope(&state, &agent, params.string("user"));
    match store.forget(&scope, &ForgetTarget::Id(id), &owner(params.string("actor").as_deref())).await {
        Ok(0) => error_response(StatusCode::NOT_FOUND, "no such note in this scope"),
        Ok(removed) => Json(json!({ "removed": removed })).into_response(),
        Err(e) => memory_error(&e),
    }
}

async fn decide(state: &AppState, scope: MemoryScope, id: &str, approve: bool, source: MemorySource) -> Response {
    let store = match store(state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    match store.decide(&scope, id, approve, &source).await {
        Ok(None) => error_response(StatusCode::NOT_FOUND, "no such note in this scope"),
        Ok(Some(record)) if approve => Json(record).into_response(),
        Ok(Some(_)) => Json(json!({ "removed": 1 })).into_response(),
        Err(e) => memory_error(&e),
    }
}

/// `POST /v1/memory/{agent}/notes/{id}/approve`
async fn approve(
    State(state): State<Arc<AppState>>,
    UrlPath((agent, id)): UrlPath<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = QueryParams::parse(raw.as_deref());
    let scope = scope(&state, &agent, params.string("user"));
    decide(&state, scope, &id, true, owner(params.string("actor").as_deref())).await
}

/// `POST /v1/memory/{agent}/notes/{id}/reject`: a rejected note is removed.
async fn reject(
    State(state): State<Arc<AppState>>,
    UrlPath((agent, id)): UrlPath<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = QueryParams::parse(raw.as_deref());
    let scope = scope(&state, &agent, params.string("user"));
    decide(&state, scope, &id, false, owner(params.string("actor").as_deref())).await
}

/// The lessons of one user of `agent`: `Ok(None)` when this server keeps no lessons.
///
/// @throws Response - when the lessons cannot be read: an export must not look complete when it is not.
#[cfg(feature = "learning")]
fn user_lessons(state: &AppState, agent: &str, user: &str) -> Result<Option<serde_json::Value>, Response> {
    let Some(lessons) = state.lesson_store() else { return Ok(None) };
    let filter = graphrt_learn::LessonFilter {
        user: graphrt_learn::UserFilter::User(user.to_owned()),
        ..graphrt_learn::LessonFilter::default()
    };
    match lessons.list(agent, &filter) {
        Ok(list) => Ok(Some(serde_json::to_value(list).unwrap_or_default())),
        Err(graphrt_learn::Error::NotFound { .. }) => Ok(Some(serde_json::Value::Array(Vec::new()))),
        Err(e) => {
            eprintln!("lesson store error while exporting a user: {e}");
            Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the lessons could not be read (details in the server log)",
            ))
        }
    }
}

/// `GET /v1/memory/{agent}/users/{user}/export`: everything kept about one user.
async fn export_user(
    State(state): State<Arc<AppState>>,
    UrlPath((agent, user)): UrlPath<(String, String)>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let mut export = match store.export_user(&scope(&state, &agent, Some(user.clone()))).await {
        Ok(export) => export,
        Err(e) => return memory_error(&e),
    };
    #[cfg(feature = "learning")]
    match user_lessons(&state, &agent, &lesson_user(&user)) {
        Ok(Some(lessons)) => {
            if let Some(fields) = export.as_object_mut() {
                fields.insert("lessons".to_owned(), lessons);
            }
        }
        Ok(None) => {}
        Err(refused) => return refused,
    }
    #[cfg(not(feature = "learning"))]
    let _ = &mut export;
    Json(export).into_response()
}

/// Erase one user of `agent`: their notes, and their lessons when this server keeps lessons.
async fn erase(state: &AppState, agent: &str, user: &str, source: &MemorySource) -> Response {
    let store = match store(state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let notes = match store.erase_user(&scope(state, agent, Some(user.to_owned())), source).await {
        Ok(count) => count,
        Err(e) => return memory_error(&e),
    };
    #[cfg(feature = "learning")]
    let lessons = match state.lesson_store() {
        Some(lessons) => match lessons.forget(agent, &graphrt_learn::Forget::User(lesson_user(user)), &source.actor) {
            Ok(removed) => removed,
            // An agent that never learned anything has no lesson file.
            Err(graphrt_learn::Error::NotFound { .. }) => 0,
            Err(e) => {
                eprintln!("lesson store error while erasing a user: {e}");
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the notes were erased, the lessons could not be (details in the server log)",
                );
            }
        },
        None => 0,
    };
    #[cfg(not(feature = "learning"))]
    let lessons = 0;
    Json(json!({ "notes": notes, "lessons": lessons })).into_response()
}

/// `DELETE /v1/memory/{agent}/users/{user}`
async fn erase_user(State(state): State<Arc<AppState>>, UrlPath((agent, user)): UrlPath<(String, String)>) -> Response {
    erase(&state, &agent, &user, &owner(None)).await
}

/// `GET /v1/memory/{agent}/audit`
async fn audit(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let params = QueryParams::parse(raw.as_deref());
    let limit = match params.number("limit") {
        Ok(limit) => limit.map_or(DEFAULT_AUDIT, |n| usize::try_from(n).unwrap_or(MAX_AUDIT)).clamp(1, MAX_AUDIT),
        Err(refused) => return refused,
    };
    match store.audit(state.memory_tenant(), &agent, limit).await {
        Ok(entries) => Json(json!({ "entries": entries })).into_response(),
        Err(e) => memory_error(&e),
    }
}

/// The scoped id of the verified user of a `/v1/me` request for the memory of `agent`. The API
/// key is not a user here, and only agents the served workflows use are
/// reachable: a user cannot make the server create directories for any name.
async fn me(state: &AppState, headers: &HeaderMap, agent: &str) -> Result<String, Response> {
    let caller = state.authenticate_user_or_key_unmetered(headers).await?;
    let user = caller.user().map(str::to_owned).ok_or_else(|| {
        error_response(StatusCode::FORBIDDEN, "/v1/me needs a verified user token; the owner uses /v1/memory")
    })?;
    if !state.memory_agents().iter().any(|known| known == agent) {
        return Err(error_response(StatusCode::NOT_FOUND, "no served agent of that name keeps memory"));
    }
    // Their runs remember under the same scoped id (`user/<sub>`).
    Ok(graphrt_core::memory::UserSource::Verified.scoped(&user))
}

fn me_source(user: &str) -> MemorySource {
    MemorySource { actor: format!("user:{}", short_hash(user)), ..MemorySource::default() }
}

/// A short, stable mark of a user id for the audit log, which holds no ids in clear.
fn short_hash(user: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(user.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// `GET /v1/me/memory/{agent}`
async fn my_notes(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let user = match me(&state, &headers, &agent).await {
        Ok(user) => user,
        Err(refused) => return refused,
    };
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let params = QueryParams::parse(raw.as_deref());
    let status = match status_filter(&params) {
        Ok(status) => status,
        Err(refused) => return refused,
    };
    match store.list(&scope(&state, &agent, Some(user)), &ListFilter { status, query: params.string("q") }).await {
        Ok(notes) => Json(json!({ "notes": notes })).into_response(),
        Err(e) => memory_error(&e),
    }
}

/// `DELETE /v1/me/memory/{agent}`: the user erases everything kept about them.
async fn erase_me(State(state): State<Arc<AppState>>, UrlPath(agent): UrlPath<String>, headers: HeaderMap) -> Response {
    match me(&state, &headers, &agent).await {
        Ok(user) => erase(&state, &agent, &user, &me_source(&user)).await,
        Err(refused) => refused,
    }
}

/// `DELETE /v1/me/memory/{agent}/{id}`
async fn remove_mine(
    State(state): State<Arc<AppState>>,
    UrlPath((agent, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let user = match me(&state, &headers, &agent).await {
        Ok(user) => user,
        Err(refused) => return refused,
    };
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let source = me_source(&user);
    match store.forget(&scope(&state, &agent, Some(user)), &ForgetTarget::Id(id), &source).await {
        Ok(0) => error_response(StatusCode::NOT_FOUND, "no such note"),
        Ok(removed) => Json(json!({ "removed": removed })).into_response(),
        Err(e) => memory_error(&e),
    }
}
