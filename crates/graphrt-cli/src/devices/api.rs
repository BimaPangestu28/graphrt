//! The device API.
//!
//! | Method | Path | Body | Answer |
//! | --- | --- | --- | --- |
//! | POST | `/v1/devices` | `{name, scopes?, expires_in_days?, url?}` | `201` + the device, its token (shown once) and the pairing payload |
//! | GET | `/v1/devices` | – | `{devices}`, without tokens |
//! | DELETE | `/v1/devices/{id}` | – | `204`; the token stops working at once |
//! | GET | `/v1/devices/me` | – | the device of the token that asks |
//! | DELETE | `/v1/devices/me` | – | `204`; the device unpairs itself |
//!
//! Pairing, listing and revoking take the API key: a device token can never
//! create or list tokens. The two `me` routes take a device token.
//!
//! The pairing payload is `{"v": 1, "url": "<server>", "token": "<token>"}`: what
//! a device needs to reach this server. A host shows it as a QR code. `link`
//! opens the monitoring page with the token in the URL fragment, which is
//! never sent to a server.

// Handlers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use serde::Deserialize;

use super::{DeviceError, Devices, Scope};
use crate::http_input::JsonBody;
use crate::serve::{AppState, Operator, error_response};

/// The longest server URL a pairing may name, in bytes.
const MAX_URL_BYTES: usize = 2000;
/// The most days a token may be asked to live.
const MAX_EXPIRY_DAYS: u32 = 3650;

/// The device routes: pairing, listing and revoking behind the API key, the
/// `me` routes for the device that asks.
pub(crate) fn routes(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    let owner = Router::new()
        .route("/v1/devices", get(list).post(create))
        .route("/v1/devices/{id}", delete(revoke))
        .route_layer(middleware::from_fn_with_state(Arc::clone(state), require_api_key));
    owner.merge(Router::new().route("/v1/devices/me", get(me).delete(unpair)))
}

/// The API key and nothing else: a device token never manages devices. Runs
/// before a request's body is read.
async fn require_api_key(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    match state.refuse_without_api_key(request.headers()) {
        Some(refused) => refused,
        None => next.run(request).await,
    }
}

/// Body of `POST /v1/devices`.
#[derive(Debug, Deserialize)]
struct CreateRequest {
    name: String,
    /// What the token may do. Default: every scope.
    #[serde(default)]
    scopes: Vec<Scope>,
    /// Days until the token stops working. Default: no expiry.
    #[serde(default)]
    expires_in_days: Option<u32>,
    /// The address the device reaches this server at. Default: `--public-url`,
    /// else the `Host` of this request.
    #[serde(default)]
    url: Option<String>,
}

fn devices(state: &AppState) -> Result<&Arc<Devices>, Response> {
    state.devices().ok_or_else(|| {
        error_response(StatusCode::NOT_FOUND, "this server pairs no devices (start it with --devices or --monitor)")
    })
}

fn refused(error: &DeviceError) -> Response {
    let status = match error {
        DeviceError::InvalidToken | DeviceError::Expired => StatusCode::UNAUTHORIZED,
        DeviceError::Invalid(_) => StatusCode::BAD_REQUEST,
        DeviceError::TooMany => StatusCode::CONFLICT,
        DeviceError::Io(_) => {
            // The message names server paths: log it, answer generically.
            eprintln!("device store error: {error}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "device store error (details in the server log)");
        }
    };
    error_response(status, &error.to_string())
}

/// `url` as a server address: `http(s)://host[:port][/prefix]`, without a
/// query, a fragment or credentials, and without a trailing slash.
///
/// @throws String - Why `url` is not one.
pub(crate) fn server_url(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("it must start with https:// or http://")?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() || host.contains('@') {
        return Err("it has no host".to_owned());
    }
    if url.len() > MAX_URL_BYTES || url.contains(['?', '#']) || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("it must be a plain address without a query or a fragment".to_owned());
    }
    Ok(url.to_owned())
}

/// The address a device reaches this server at: what the request names, else
/// `--public-url`, else the `Host` this request came to.
fn pairing_url(state: &AppState, requested: Option<&str>, headers: &HeaderMap) -> Result<String, Response> {
    if let Some(url) = requested {
        return server_url(url).map_err(|why| error_response(StatusCode::BAD_REQUEST, &format!("`url`: {why}")));
    }
    if let Some(url) = state.public_url() {
        return Ok(url.to_owned());
    }
    let host = headers.get(header::HOST).and_then(|value| value.to_str().ok()).unwrap_or_default();
    // A proxy that ends TLS says so. The link is shown to the key holder only.
    let https = headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()) == Some("https");
    server_url(&format!("{}://{host}", if https { "https" } else { "http" })).map_err(|_| {
        error_response(StatusCode::BAD_REQUEST, "send `url`: the address devices reach this server at is not known")
    })
}

/// `POST /v1/devices`: pair a device.
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<CreateRequest>,
) -> Response {
    let devices = match devices(&state) {
        Ok(devices) => devices,
        Err(refused) => return refused,
    };
    if body.expires_in_days.is_some_and(|days| days == 0 || days > MAX_EXPIRY_DAYS) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("`expires_in_days` must be 1 to {MAX_EXPIRY_DAYS} (leave it out for no expiry)"),
        );
    }
    let url = match pairing_url(&state, body.url.as_deref(), &headers) {
        Ok(url) => url,
        Err(refused) => return refused,
    };
    let paired = match devices.create(&body.name, &body.scopes, body.expires_in_days) {
        Ok(paired) => paired,
        Err(error) => return refused(&error),
    };
    let link = state.serves_monitor_page().then(|| format!("{url}/monitor/#pair={}", paired.token));
    let answer = serde_json::json!({
        "device": paired.device.public_json(),
        "token": paired.token,
        "pairing": { "v": 1, "url": url, "token": paired.token },
        "link": link,
    });
    // The only time the token is shown: no cache may keep it.
    (StatusCode::CREATED, [(header::CACHE_CONTROL, "no-store")], Json(answer)).into_response()
}

/// `GET /v1/devices`.
async fn list(State(state): State<Arc<AppState>>) -> Response {
    match devices(&state).map(|devices| devices.list()) {
        Ok(Ok(all)) => {
            let listed: Vec<serde_json::Value> = all.iter().map(super::Device::public_json).collect();
            Json(serde_json::json!({ "devices": listed })).into_response()
        }
        Ok(Err(error)) => refused(&error),
        Err(refused) => refused,
    }
}

/// End device `id`: its token, and what it subscribed to.
fn end_device(state: &AppState, devices: &Devices, id: &str) -> Response {
    match devices.revoke(id) {
        Ok(true) => {
            state.forget_device(id);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => error_response(StatusCode::NOT_FOUND, "unknown device"),
        Err(error) => refused(&error),
    }
}

/// `DELETE /v1/devices/{id}`.
async fn revoke(State(state): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    match devices(&state) {
        Ok(devices) => end_device(&state, devices, &id),
        Err(refused) => refused,
    }
}

/// The device whose token `headers` carry.
fn asking_device(state: &AppState, headers: &HeaderMap) -> Result<String, Response> {
    match state.authenticate_operator(headers, None)? {
        Operator::Device(device) => Ok(device.id),
        Operator::ApiKey => {
            Err(error_response(StatusCode::BAD_REQUEST, "`me` is the device of a device token, not the API key"))
        }
    }
}

/// `GET /v1/devices/me`.
async fn me(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let id = match asking_device(&state, &headers) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    match devices(&state).map(|devices| devices.list()) {
        Ok(Ok(all)) => match all.iter().find(|device| device.id == id) {
            Some(device) => Json(device.public_json()).into_response(),
            None => error_response(StatusCode::NOT_FOUND, "unknown device"),
        },
        Ok(Err(error)) => refused(&error),
        Err(refused) => refused,
    }
}

/// `DELETE /v1/devices/me`.
async fn unpair(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let id = match asking_device(&state, &headers) {
        Ok(id) => id,
        Err(refused) => return refused,
    };
    match devices(&state) {
        Ok(devices) => end_device(&state, devices, &id),
        Err(refused) => refused,
    }
}

#[cfg(test)]
mod tests {
    use super::server_url;

    #[test]
    fn a_server_url_is_a_plain_address() {
        assert_eq!(server_url(" https://agent.example.com/ ").as_deref(), Ok("https://agent.example.com"));
        assert_eq!(server_url("http://192.168.1.5:8080/agent").as_deref(), Ok("http://192.168.1.5:8080/agent"));
        for bad in [
            "",
            "agent.example.com",
            "ftp://agent.example.com",
            "https://",
            "https://user:pw@agent.example.com",
            "https://agent.example.com/?x=1",
            "https://agent.example.com/#frag",
            "https://agent.example.com/a b",
        ] {
            assert!(server_url(bad).is_err(), "{bad}");
        }
    }
}
