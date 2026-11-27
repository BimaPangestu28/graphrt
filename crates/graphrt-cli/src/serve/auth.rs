//! Bearer authentication of the API: the API key (`--api-key-env`) and, with
//! `--verify-jwt` or chat guests, user tokens on the routes users may call:
//! run starts, feedback, their own runs ([`authorize_run`]) and their own
//! conversations. With `--devices`, a device token reads runs and resolves
//! approvals ([`Operator`]).

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use super::errors::error_response;
use super::state::AppState;

/// The verified end user of a request (`sub` of its JWT), set by `--verify-jwt`.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser(pub String);

/// Who made a request that end users may make with their own token.
#[derive(Debug)]
pub(crate) enum Caller {
    /// The holder of the API key (anyone, when no key is set).
    ApiKey,
    /// A verified end user (`--verify-jwt`): the token's `sub`.
    #[cfg(feature = "jwt")]
    User(String),
}

impl Caller {
    /// The verified end user, if the caller is one.
    #[cfg(feature = "conversations")]
    pub(crate) fn user(&self) -> Option<&str> {
        match self {
            Caller::ApiKey => None,
            #[cfg(feature = "jwt")]
            Caller::User(subject) => Some(subject),
        }
    }
}

/// Who makes an operator request: reading runs, resolving approvals.
#[cfg(feature = "devices")]
#[derive(Debug)]
pub(crate) enum Operator {
    /// The holder of the API key (anyone, when no key is set).
    ApiKey,
    /// A paired device (`--devices`), within its scopes.
    Device(crate::devices::DeviceAccess),
}

/// Whether the user `subject` of a verified token owns `record`'s run: the run
/// was started with that user's token. A `user_id` an API-key caller put in the
/// request body is only a label, so it never makes a run that user's.
#[cfg(feature = "jwt")]
pub(crate) fn owns_run(record: &graphrt_core::store::RunRecord, subject: &str) -> bool {
    record.user_verified && record.user_id.as_deref() == Some(subject)
}

/// The bearer token of `Authorization`, or `""`.
fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
}

impl AppState {
    /// Authenticate a request that accepts an end-user token in place of the
    /// API key: a verified token (`--verify-jwt`, or a chat guest's), else the
    /// API key. A present but invalid token is refused even next to a valid
    /// key, so a bad token never reaches a tool through the key's broader
    /// access. The request counts against the user's rate limit: use it for
    /// requests that start or continue a run.
    ///
    /// @throws Response - `401` for an invalid token or key, `429` over the user's rate limit.
    pub(crate) async fn authenticate_user_or_key(&self, headers: &HeaderMap) -> Result<Caller, Response> {
        self.authenticate(headers, true).await
    }

    /// [`Self::authenticate_user_or_key`] without counting the request against
    /// the user's rate limit, which is a limit on runs: for requests that only
    /// read or stop what the user already started.
    ///
    /// @throws Response - `401` for an invalid token or key.
    pub(crate) async fn authenticate_user_or_key_unmetered(&self, headers: &HeaderMap) -> Result<Caller, Response> {
        self.authenticate(headers, false).await
    }

    async fn authenticate(&self, headers: &HeaderMap, metered: bool) -> Result<Caller, Response> {
        #[cfg(not(feature = "jwt"))]
        let _ = metered;
        #[cfg(feature = "web-chat")]
        if let Some(guests) = self.chat.as_deref().and_then(super::chat::Chat::guests)
            && super::guests::is_guest_token(bearer(headers))
        {
            return guests.admit(bearer(headers), metered).map(Caller::User);
        }
        #[cfg(feature = "jwt")]
        if let Some(user_auth) = &self.user_auth {
            let admission =
                if metered { user_auth.admit(headers).await? } else { user_auth.admit_unmetered(headers).await? };
            if let crate::user_auth::Admission::User(user) = admission {
                #[cfg(feature = "web-chat")]
                if super::guests::is_guest_subject(&user.subject) {
                    // Guest subjects are minted here only: a token of the identity
                    // provider must not name one.
                    return Err(error_response(StatusCode::UNAUTHORIZED, "the token's `sub` is reserved"));
                }
                return Ok(Caller::User(user.subject));
            }
        }
        match self.refuse_without_api_key(headers) {
            Some(refused) => Err(refused),
            None => Ok(Caller::ApiKey),
        }
    }

    /// Authenticate a request that a paired device may make in place of the
    /// API key holder: a valid device token that has `scope` (any scope with
    /// `None`), else the API key. A device token is never tried as the key.
    ///
    /// @throws Response - `401` for an invalid token or key, `403` for a device without `scope`.
    #[cfg(feature = "devices")]
    pub(crate) fn authenticate_operator(
        &self,
        headers: &HeaderMap,
        scope: Option<crate::devices::Scope>,
    ) -> Result<Operator, Response> {
        let token = bearer(headers);
        if !crate::devices::is_device_token(token) {
            return match self.refuse_without_api_key(headers) {
                Some(refused) => Err(refused),
                None => Ok(Operator::ApiKey),
            };
        }
        let Some(devices) = &self.devices else {
            return Err(error_response(StatusCode::UNAUTHORIZED, "this server pairs no devices"));
        };
        let device =
            devices.verify(token).map_err(|error| error_response(StatusCode::UNAUTHORIZED, &error.to_string()))?;
        if let Some(scope) = scope
            && !device.allows(scope)
        {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                &format!("this device token lacks the scope `{}`", scope.name()),
            ));
        }
        Ok(Operator::Device(device))
    }

    /// `401` unless `headers` carry the API key (`None` too when no key is set).
    pub(crate) fn refuse_without_api_key(&self, headers: &HeaderMap) -> Option<Response> {
        let expected = self.api_key.as_ref()?;
        let provided = bearer(headers);
        if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            return None;
        }
        Some(error_response(StatusCode::UNAUTHORIZED, "missing or invalid bearer token"))
    }

    /// `404` for a workflow that is not served, `403` for one `user` (a verified
    /// end user, `None` for the API key) may not start.
    #[cfg(feature = "conversations")]
    pub(crate) fn refuse_workflow_to(&self, name: &str, user: Option<&str>) -> Option<Response> {
        if !self.workflows.contains_key(name) {
            return Some(error_response(StatusCode::NOT_FOUND, &format!("unknown workflow `{name}`")));
        }
        #[cfg(feature = "jwt")]
        if let Some(subject) = user {
            return self.refuse_user_token(name, subject);
        }
        #[cfg(not(feature = "jwt"))]
        let _ = user;
        None
    }

    /// `403` unless workflow `name` is built for end users. A user token starts
    /// only workflows that take the verified token as an input (`--verify-jwt
    /// INPUT` mapped by `--input-header`), so internal workflows stay key-only.
    #[cfg(feature = "jwt")]
    fn refuse_user_token(&self, name: &str, subject: &str) -> Option<Response> {
        #[cfg(feature = "web-chat")]
        if super::guests::is_guest_subject(subject) {
            // A guest starts only the workflows the chat page offers.
            let offered = self.chat.as_deref().is_some_and(|chat| chat.offers(name));
            return (!offered).then(|| {
                error_response(StatusCode::FORBIDDEN, &format!("workflow `{name}` is not offered to chat guests"))
            });
        }
        #[cfg(not(feature = "web-chat"))]
        let _ = subject;
        let input = self.user_auth.as_ref()?.input();
        if self.takes_header_input(name, input) {
            return None;
        }
        Some(error_response(
            StatusCode::FORBIDDEN,
            &format!("workflow `{name}` does not accept user tokens (no `{input}` input)"),
        ))
    }
}

/// Authorize `POST /v1/workflows/{name}/runs`: a valid end-user JWT (with
/// `--verify-jwt`) for workflows built for end users, else the API key.
pub(super) async fn authorize_start(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    request: Request,
    next: Next,
) -> Response {
    #[cfg(not(feature = "jwt"))]
    let _ = name;
    match state.authenticate_user_or_key(request.headers()).await {
        Err(refused) => refused,
        Ok(Caller::ApiKey) => next.run(request).await,
        #[cfg(feature = "jwt")]
        Ok(Caller::User(subject)) => {
            if let Some(refused) = state.refuse_user_token(&name, &subject) {
                return refused;
            }
            let mut request = request;
            request.extensions_mut().insert(AuthenticatedUser(subject));
            next.run(request).await
        }
    }
}

/// Authorize the routes of one run (`GET /v1/runs/{run_id}`, its events, abort
/// and resume): the API key for any run, or a verified user for a run they
/// started. A run of someone else answers `404`, as an unknown one does, so ids
/// cannot be probed. Reading and stopping are not counted against the user's
/// rate limit; a resume, which executes the run again, is.
pub(super) async fn authorize_run(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    request: Request,
    next: Next,
) -> Response {
    // A paired device reads any run, and changes none here: approvals are
    // resolved under `/v1/approvals`.
    #[cfg(feature = "devices")]
    if crate::devices::is_device_token(bearer(request.headers())) {
        if request.method() != axum::http::Method::GET {
            return error_response(
                StatusCode::FORBIDDEN,
                "a device token reads runs: it cannot stop or resume one here (approvals: /v1/approvals)",
            );
        }
        return match state.authenticate_operator(request.headers(), Some(crate::devices::Scope::RunsRead)) {
            Ok(_) => next.run(request).await,
            Err(refused) => refused,
        };
    }
    let resumes = request.uri().path().ends_with("/resume");
    let caller = if resumes {
        state.authenticate_user_or_key(request.headers()).await
    } else {
        state.authenticate_user_or_key_unmetered(request.headers()).await
    };
    #[cfg(not(feature = "jwt"))]
    let _ = run_id;
    match caller {
        Err(refused) => refused,
        Ok(Caller::ApiKey) => next.run(request).await,
        #[cfg(feature = "jwt")]
        Ok(Caller::User(subject)) => {
            if resumes && !state.user_resume {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "users cannot resume runs on this server (the operator has not set --user-resume)",
                );
            }
            match state.store.get(&run_id).await {
                Ok(Some(record)) if owns_run(&record, &subject) => {
                    let mut request = request;
                    request.extensions_mut().insert(AuthenticatedUser(subject));
                    next.run(request).await
                }
                Ok(_) => error_response(StatusCode::NOT_FOUND, "unknown run"),
                Err(e) => super::errors::store_error(&e),
            }
        }
    }
}

/// Authorize `GET /v1/runs`: the API key, or a device token that reads runs.
#[cfg(feature = "devices")]
pub(super) async fn authorize_reader(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    match state.authenticate_operator(request.headers(), Some(crate::devices::Scope::RunsRead)) {
        Ok(_) => next.run(request).await,
        Err(refused) => refused,
    }
}

/// Require the API key.
pub(super) async fn require_auth(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    match state.refuse_without_api_key(request.headers()) {
        Some(refused) => refused,
        None => next.run(request).await,
    }
}

/// Compare without early exit so response timing does not reveal the key prefix.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
