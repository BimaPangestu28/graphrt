//! The HTTP side of the channels.
//!
//! | Method | Path | Who | What |
//! | --- | --- | --- | --- |
//! | GET | `/v1/channels/{name}/webhook` | Meta | The check of a WhatsApp webhook address (`hub.challenge`) |
//! | POST | `/v1/channels/{name}/webhook` | Telegram, Meta | A delivery, checked by its secret or signature |
//! | GET | `/v1/channels` | API key | The configured channels |
//! | POST | `/v1/channels/{name}/messages` | API key | `{to, text}`: a message of a person of the team |
//! | POST | `/v1/channels/{name}/handoff` | API key | `{to, human}`: hand a customer to a person, or back |
//!
//! A webhook is answered as soon as the delivery is verified and read. The
//! work it starts runs in the background.

// Handlers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use graphrt_channel::ChannelKind;
use serde::Deserialize;

use super::{Bot, Channels, Credentials, Mode};
use crate::http_input::JsonBody;
use crate::serve::{AppState, error_response};

/// The channel routes. Webhooks authenticate by their secret or signature, the
/// rest by the API key.
pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/channels", get(list))
        .route("/v1/channels/{name}/webhook", get(verify).post(deliver))
        .route("/v1/channels/{name}/messages", post(send_message))
        .route("/v1/channels/{name}/handoff", post(handoff))
}

fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "unknown channel")
}

/// The channels and the channel `name` of a server that has it.
fn channel<'a>(state: &'a AppState, name: &str) -> Result<(&'a Arc<Channels>, &'a Arc<Bot>), Response> {
    let channels = state.channels().ok_or_else(not_found)?;
    let bot = channels.bot(name).ok_or_else(not_found)?;
    Ok((channels, bot))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `GET /v1/channels/{name}/webhook`: Meta checks the address of a WhatsApp webhook.
async fn verify(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let Ok((_, bot)) = channel(&state, &name) else { return not_found() };
    #[cfg(feature = "channel-whatsapp")]
    if let Credentials::WhatsApp { verify_token, .. } = &bot.config.credentials {
        let params = crate::http_input::QueryParams::parse(raw.as_deref());
        let (mode, token, challenge) =
            (params.string("hub.mode"), params.string("hub.verify_token"), params.string("hub.challenge"));
        return match graphrt_channel::whatsapp::verify_subscription(
            mode.as_deref(),
            token.as_deref(),
            challenge.as_deref(),
            verify_token,
        ) {
            Ok(challenge) => challenge.into_response(),
            Err(_) => error_response(StatusCode::FORBIDDEN, "the verify token does not match"),
        };
    }
    let _ = (bot, raw);
    error_response(StatusCode::METHOD_NOT_ALLOWED, "this channel takes deliveries by POST only")
}

/// `POST /v1/channels/{name}/webhook`: a delivery.
async fn deliver(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok((channels, bot)) = channel(&state, &name) else { return not_found() };
    // The same answer for a missing and a wrong credential: a caller learns nothing from it.
    let refused = || error_response(StatusCode::UNAUTHORIZED, "the delivery is not signed by the channel");
    let malformed = || error_response(StatusCode::BAD_REQUEST, "the delivery is not JSON");
    match &bot.config.credentials {
        #[cfg(feature = "channel-telegram")]
        Credentials::Telegram { webhook_secret, .. } => {
            let (Mode::Webhook, Some(secret)) = (bot.config.mode, webhook_secret) else {
                return error_response(StatusCode::NOT_FOUND, "this channel polls: it takes no deliveries");
            };
            let given = header(&headers, graphrt_channel::telegram::SECRET_TOKEN_HEADER);
            if !graphrt_channel::telegram::verify_secret_token(given, secret) {
                return refused();
            }
            let Ok(update) = serde_json::from_slice::<serde_json::Value>(&body) else { return malformed() };
            channels.receive(bot, graphrt_channel::telegram::parse_update(&update).event);
        }
        #[cfg(feature = "channel-whatsapp")]
        Credentials::WhatsApp { app_secret, phone_number_id, .. } => {
            let signature = header(&headers, graphrt_channel::whatsapp::SIGNATURE_HEADER);
            if graphrt_channel::whatsapp::verify_signature(&body, signature, app_secret).is_err() {
                return refused();
            }
            let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else { return malformed() };
            for event in graphrt_channel::whatsapp::parse_webhook(&payload) {
                // One app can serve several numbers: only this channel's are its own.
                if &event.phone_number_id == phone_number_id {
                    channels.receive(bot, event.event);
                }
            }
        }
        #[allow(unreachable_patterns)]
        _ => return not_found(),
    }
    let _ = Mode::Webhook;
    "ok".into_response()
}

/// `GET /v1/channels`.
async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(refused) = state.refuse_without_api_key(&headers) {
        return refused;
    }
    let bots = state.channels().map(|channels| channels.bots.as_slice()).unwrap_or_default();
    let channels: Vec<serde_json::Value> = bots
        .iter()
        .map(|bot| {
            let config = &bot.config;
            let webhook =
                (config.kind != ChannelKind::Telegram || config.mode == Mode::Webhook).then(|| bot.webhook_path());
            serde_json::json!({
                "name": config.name,
                "kind": config.kind.as_str(),
                "workflow": config.workflow,
                "mode": if webhook.is_some() { "webhook" } else { "polling" },
                "webhook": webhook,
                "groups": config.groups,
                "restricted": !config.allow.is_empty(),
            })
        })
        .collect();
    Json(serde_json::json!({ "channels": channels })).into_response()
}

/// Body of `POST /v1/channels/{name}/messages`.
#[derive(Debug, Deserialize)]
struct MessageRequest {
    /// The customer's chat: a Telegram chat id, a WhatsApp phone number.
    to: String,
    text: String,
}

/// `POST /v1/channels/{name}/messages`: a person of the team writes to a customer.
async fn send_message(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<MessageRequest>,
) -> Response {
    if let Some(refused) = state.refuse_without_api_key(&headers) {
        return refused;
    }
    let (channels, bot) = match channel(&state, &name) {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    if body.text.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "`text` is empty");
    }
    match channels.operator_message(bot, &body.to, &body.text).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(why) => error_response(StatusCode::CONFLICT, &why),
    }
}

/// Body of `POST /v1/channels/{name}/handoff`.
#[derive(Debug, Deserialize)]
struct HandoffRequest {
    to: String,
    /// `true`: a person answers and the workflow stays quiet. `false`: the workflow answers again.
    human: bool,
}

/// `POST /v1/channels/{name}/handoff`.
async fn handoff(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<HandoffRequest>,
) -> Response {
    if let Some(refused) = state.refuse_without_api_key(&headers) {
        return refused;
    }
    let (channels, bot) = match channel(&state, &name) {
        Ok(found) => found,
        Err(refused) => return refused,
    };
    match channels.set_human(bot, &body.to, body.human).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(why) => error_response(StatusCode::NOT_FOUND, &why),
    }
}
