//! The conversations API, and how a run request joins a conversation.
//!
//! | Method | Path | Body | Answer |
//! | --- | --- | --- | --- |
//! | POST | `/v1/conversations` | `{workflow, input?, title?}` | `201` + the conversation |
//! | GET | `/v1/conversations` | query `workflow`, `user`, `limit`, `cursor` | `{conversations, next_cursor}`, most recently used first |
//! | GET | `/v1/conversations/{id}` | – | the conversation with its `messages` |
//! | DELETE | `/v1/conversations/{id}` | – | `204` |
//!
//! Every route takes the API key or a verified user token. A user reaches only
//! their own conversations: another user's answers `404`, like an unknown id.

// Handlers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use super::{Conversation, ConversationError, Conversations};
use crate::http_input::{JsonBody, QueryParams};
use crate::serve::{AppState, RunRequest, error_response};

/// Conversations per page of a listing, and the most a caller may ask for.
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 200;
/// Longest title a caller may set, in bytes.
const MAX_TITLE_BYTES: usize = 300;

/// The conversation routes; they authenticate themselves.
pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/conversations", get(list).post(create))
        .route("/v1/conversations/{id}", get(get_one).delete(delete_one))
}

/// Body of `POST /v1/conversations`.
#[derive(Debug, Deserialize)]
struct CreateRequest {
    workflow: String,
    /// The start input that takes each message. Default: `sys.query`.
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

/// The conversations of a server that keeps some.
fn enabled(state: &AppState) -> Result<&Arc<Conversations>, Response> {
    state.conversations().ok_or_else(|| {
        error_response(StatusCode::NOT_FOUND, "this server keeps no conversations (start it with --conversations)")
    })
}

/// A store failure as a response. I/O errors name server paths: logged, not sent.
fn store_error(error: &ConversationError) -> Response {
    match error {
        ConversationError::InvalidId => error_response(StatusCode::NOT_FOUND, "unknown conversation"),
        ConversationError::Io(_) | ConversationError::Corrupt(_) => {
            eprintln!("conversation store error: {error}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "conversation store error (details in the server log)")
        }
    }
}

/// A conversation as the API shows it. `owner` is shown to API-key callers only.
fn summary(conversation: &Conversation, to_key: bool) -> serde_json::Value {
    let mut json = serde_json::json!({
        "id": conversation.id,
        "workflow": conversation.workflow,
        "input": conversation.input,
        "title": conversation.title,
        "created_ms": conversation.created_ms,
        "updated_ms": conversation.updated_ms,
        "message_count": conversation.messages.len(),
    });
    if to_key && let (Some(object), Some(owner)) = (json.as_object_mut(), &conversation.owner) {
        object.insert("owner".into(), owner.as_str().into());
    }
    if to_key && let (Some(object), Some(channel)) = (json.as_object_mut(), &conversation.channel) {
        object.insert("channel".into(), serde_json::to_value(channel).unwrap_or_default());
    }
    json
}

/// The conversation `id` if `user` may reach it; `404` otherwise.
async fn reachable(conversations: &Conversations, id: &str, user: Option<&str>) -> Result<Conversation, Response> {
    match conversations.store().get(id).await {
        Ok(Some(conversation)) if conversation.is_reachable_by(user) => Ok(conversation),
        Ok(_) => Err(error_response(StatusCode::NOT_FOUND, "unknown conversation")),
        Err(e) => Err(store_error(&e)),
    }
}

/// `POST /v1/conversations`.
async fn create(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let conversations = match enabled(&state) {
        Ok(conversations) => conversations,
        Err(refused) => return refused,
    };
    // Authorized before the body is parsed, as on every route.
    let caller = match state.authenticate_user_or_key_unmetered(request.headers()).await {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    let JsonBody(body) =
        match <JsonBody<CreateRequest> as axum::extract::FromRequest<()>>::from_request(request, &()).await {
            Ok(body) => body,
            Err(refused) => return refused,
        };
    if let Some(refused) = state.refuse_workflow_to(&body.workflow, caller.user()) {
        return refused;
    }
    if let Some(input) = &body.input
        && !state.start_inputs(&body.workflow).is_some_and(|inputs| inputs.contains(input))
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("workflow `{}` declares no start input `{input}`", body.workflow),
        );
    }
    if body.title.as_ref().is_some_and(|title| title.len() > MAX_TITLE_BYTES) {
        return error_response(StatusCode::BAD_REQUEST, &format!("`title` must be at most {MAX_TITLE_BYTES} bytes"));
    }
    let owner = caller.user().map(str::to_owned);
    match conversations.create(owner, &body.workflow, body.input, body.title).await {
        Ok(conversation) => {
            (StatusCode::CREATED, Json(summary(&conversation, caller.user().is_none()))).into_response()
        }
        Err(e) => store_error(&e),
    }
}

/// `GET /v1/conversations`.
async fn list(State(state): State<Arc<AppState>>, RawQuery(raw): RawQuery, headers: HeaderMap) -> Response {
    let conversations = match enabled(&state) {
        Ok(conversations) => conversations,
        Err(refused) => return refused,
    };
    let caller = match state.authenticate_user_or_key_unmetered(&headers).await {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    let params = QueryParams::parse(raw.as_deref());
    let limit = match params.number("limit") {
        Ok(limit) => limit.map_or(DEFAULT_PAGE, |limit| usize::try_from(limit).unwrap_or(MAX_PAGE)),
        Err(refused) => return refused,
    };
    let limit = limit.clamp(1, MAX_PAGE);
    let (workflow, cursor) = (params.string("workflow"), params.string("cursor"));
    // A user lists their own; the key lists all, or those of `user=`.
    let owner_filter = params.string("user");
    let all = match conversations.store().all().await {
        Ok(all) => all,
        Err(e) => return store_error(&e),
    };
    let visible = all.into_iter().filter(|conversation| match caller.user() {
        Some(user) => conversation.owner.as_deref() == Some(user),
        None => owner_filter.as_ref().is_none_or(|owner| conversation.owner.as_ref() == Some(owner)),
    });
    let mut matching: Vec<Conversation> =
        visible.filter(|c| workflow.as_ref().is_none_or(|workflow| &c.workflow == workflow)).collect();
    if let Some(cursor) = cursor
        && let Some(position) = matching.iter().position(|conversation| conversation.id == cursor)
    {
        matching.drain(..=position);
    }
    let next_cursor = (matching.len() > limit).then(|| matching.get(limit - 1).map(|last| last.id.clone())).flatten();
    matching.truncate(limit);
    let to_key = caller.user().is_none();
    let items: Vec<serde_json::Value> = matching.iter().map(|conversation| summary(conversation, to_key)).collect();
    Json(serde_json::json!({ "conversations": items, "next_cursor": next_cursor })).into_response()
}

/// `GET /v1/conversations/{id}`.
async fn get_one(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>, headers: HeaderMap) -> Response {
    let conversations = match enabled(&state) {
        Ok(conversations) => conversations,
        Err(refused) => return refused,
    };
    let caller = match state.authenticate_user_or_key_unmetered(&headers).await {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    let conversation = match reachable(conversations, &id, caller.user()).await {
        Ok(conversation) => conversation,
        Err(refused) => return refused,
    };
    let mut json = summary(&conversation, caller.user().is_none());
    if let Some(object) = json.as_object_mut() {
        object.insert("messages".into(), serde_json::to_value(&conversation.messages).unwrap_or_default());
    }
    Json(json).into_response()
}

/// `DELETE /v1/conversations/{id}`.
async fn delete_one(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>, headers: HeaderMap) -> Response {
    let conversations = match enabled(&state) {
        Ok(conversations) => conversations,
        Err(refused) => return refused,
    };
    let caller = match state.authenticate_user_or_key_unmetered(&headers).await {
        Ok(caller) => caller,
        Err(refused) => return refused,
    };
    if let Err(refused) = reachable(conversations, &id, caller.user()).await {
        return refused;
    }
    match conversations.store().delete(&id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => store_error(&e),
    }
}

/// Join the run `run_id` of `workflow` to the conversation its request names
/// (`conversation_id`): the stored turns become `sys.history`, the stored
/// variables fill the `conversation` namespace, and the message is remembered
/// until the run ends. Does nothing for a request that names none.
///
/// @param user - The verified end user of the request, `None` for the API key.
/// @throws Response - `400` when the server keeps no conversations, the request
///   also sends `sys.history`, or it has no message; `404` for a conversation the
///   caller cannot reach; `409` for one of another workflow.
pub(crate) async fn join_run(
    state: &AppState,
    workflow: &str,
    run_id: &str,
    user: Option<&str>,
    body: &mut RunRequest,
) -> Result<(), Response> {
    let Some(id) = body.conversation_id.clone() else { return Ok(()) };
    let refuse = |message: &str| error_response(StatusCode::BAD_REQUEST, message);
    let Some(conversations) = state.conversations() else {
        return Err(refuse("`conversation_id` needs a server that keeps conversations (--conversations)"));
    };
    if body.sys.contains_key("history") {
        return Err(refuse("send `conversation_id` or `sys.history`, not both"));
    }
    if body.session_id.as_ref().is_some_and(|session| session != &id) {
        return Err(refuse("`session_id` of a run in a conversation is the conversation's id"));
    }
    let conversation = reachable(conversations, &id, user).await?;
    if conversation.workflow != workflow {
        let message = format!("conversation `{id}` belongs to workflow `{}`", conversation.workflow);
        return Err(error_response(StatusCode::CONFLICT, &message));
    }
    let message = match &conversation.input {
        Some(input) => body.inputs.get(input).and_then(serde_json::Value::as_str),
        None => body.sys.get("query").and_then(serde_json::Value::as_str),
    };
    let Some(message) = message.map(str::to_owned) else {
        let place = conversation.input.as_ref().map_or("sys.query".to_owned(), |input| format!("inputs.{input}"));
        return Err(refuse(&format!("a run in this conversation needs its message as text in `{place}`")));
    };
    body.sys.insert("history".into(), conversation.history());
    body.sys.insert("conversation_id".into(), id.as_str().into());
    for (name, value) in &conversation.variables {
        // What the request sets wins over what the last run left.
        body.conversation.entry(name.clone()).or_insert_with(|| value.clone());
    }
    body.session_id = Some(id.clone());
    match conversations.begin_turn(&id, run_id, &message).await {
        Ok(Some(_)) => Ok(()),
        // Deleted between the two store calls.
        Ok(None) => Err(error_response(StatusCode::NOT_FOUND, "unknown conversation")),
        Err(e) => Err(store_error(&e)),
    }
}
