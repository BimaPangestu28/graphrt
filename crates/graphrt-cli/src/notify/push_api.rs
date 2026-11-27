//! The Web Push API of `graphrt serve --push`.
//!
//! | Method | Path | Body | Answer |
//! | --- | --- | --- | --- |
//! | GET | `/v1/push` | – | `{public_key}`: what a browser subscribes with (`applicationServerKey`) |
//! | POST | `/v1/push/subscriptions` | `{endpoint, keys: {p256dh, auth}}` | `201`; the browser is notified from now on |
//! | DELETE | `/v1/push/subscriptions` | `{endpoint}` | `204` |
//! | POST | `/v1/push/test` | `{endpoint}` | `202`; a test notification is on its way to that browser |
//!
//! All take the API key or a device token ([`crate::devices`]). The body of a
//! subscription is what `PushSubscription.toJSON()` gives. A device removes
//! and tests only its own subscriptions.

// Handlers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Extension, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use super::webpush::{PushError, Subscription, WebPush};
use crate::http_input::JsonBody;
use crate::serve::{AppState, Operator, error_response};

/// The push routes, behind their own authorization.
pub(crate) fn routes(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/push", get(info))
        .route("/v1/push/subscriptions", post(subscribe).delete(unsubscribe))
        .route("/v1/push/test", post(test))
        .route_layer(middleware::from_fn_with_state(Arc::clone(state), authorize))
}

/// Who asks: the paired device, or `None` for the holder of the API key.
#[derive(Clone, Debug)]
struct Asking(Option<String>);

#[derive(Debug, Deserialize)]
struct Keys {
    p256dh: String,
    auth: String,
}

/// Body of `POST /v1/push/subscriptions`.
#[derive(Debug, Deserialize)]
struct SubscribeRequest {
    endpoint: String,
    keys: Keys,
}

/// Body of `DELETE /v1/push/subscriptions` and `POST /v1/push/test`.
#[derive(Debug, Deserialize)]
struct EndpointRequest {
    endpoint: String,
}

/// Web Push of a server that has it.
fn push_of(state: &AppState) -> Result<&Arc<WebPush>, Response> {
    state.push().ok_or_else(|| {
        error_response(StatusCode::NOT_FOUND, "this server sends no push notifications (start it with --push)")
    })
}

/// The API key or a device token. Runs before a request's body is read, and
/// tells the handlers which device asks.
async fn authorize(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Response {
    let asking = push_of(&state).and_then(|_| state.authenticate_operator(request.headers(), None));
    match asking {
        Ok(Operator::ApiKey) => request.extensions_mut().insert(Asking(None)),
        Ok(Operator::Device(device)) => request.extensions_mut().insert(Asking(Some(device.id))),
        Err(refused) => return refused,
    };
    next.run(request).await
}

fn refused(error: &PushError) -> Response {
    let status = match error {
        PushError::Invalid(_) => StatusCode::BAD_REQUEST,
        PushError::TooMany => StatusCode::CONFLICT,
        PushError::Io(_) | PushError::Crypto => {
            eprintln!("push error: {error}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "push error (details in the server log)");
        }
    };
    error_response(status, &error.to_string())
}

/// `GET /v1/push`.
async fn info(State(state): State<Arc<AppState>>) -> Response {
    match push_of(&state) {
        Ok(push) => Json(serde_json::json!({ "public_key": push.public_key() })).into_response(),
        Err(refused) => refused,
    }
}

/// `POST /v1/push/subscriptions`.
async fn subscribe(
    State(state): State<Arc<AppState>>,
    Extension(Asking(device_id)): Extension<Asking>,
    JsonBody(body): JsonBody<SubscribeRequest>,
) -> Response {
    let push = match push_of(&state) {
        Ok(push) => push,
        Err(refused) => return refused,
    };
    let subscription = Subscription {
        endpoint: body.endpoint,
        p256dh: body.keys.p256dh,
        auth: body.keys.auth,
        device_id,
        created_ms: 0,
    };
    match push.subscribe(subscription) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => refused(&error),
    }
}

/// `DELETE /v1/push/subscriptions`.
async fn unsubscribe(
    State(state): State<Arc<AppState>>,
    Extension(Asking(device_id)): Extension<Asking>,
    JsonBody(body): JsonBody<EndpointRequest>,
) -> Response {
    let push = match push_of(&state) {
        Ok(push) => push,
        Err(refused) => return refused,
    };
    match push.unsubscribe(&body.endpoint, device_id.as_deref()) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "unknown subscription"),
        Err(error) => refused(&error),
    }
}

/// `POST /v1/push/test`: one notification to the browser that asks, so a
/// person sees that notifications reach their device.
async fn test(
    State(state): State<Arc<AppState>>,
    Extension(Asking(device_id)): Extension<Asking>,
    JsonBody(body): JsonBody<EndpointRequest>,
) -> Response {
    let push = match push_of(&state) {
        Ok(push) => push,
        Err(refused) => return refused,
    };
    match push.send_test(&body.endpoint, device_id.as_deref()).await {
        Ok(true) => StatusCode::ACCEPTED.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "unknown subscription"),
        Err(why) => error_response(StatusCode::BAD_GATEWAY, &format!("the notification was not sent: {why}")),
    }
}
