//! Bearer authentication of the API: the API key (`--api-key-env`) and, with
//! `--verify-jwt`, end-user tokens on the routes end users may call (run
//! starts and feedback).

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

impl AppState {
    /// Authenticate a request that accepts an end-user token in place of the
    /// API key: a verified token (`--verify-jwt`), else the API key. A present
    /// but invalid token is refused even next to a valid key, so a bad token
    /// never reaches a tool through the key's broader access.
    ///
    /// @throws Response - `401` for an invalid token or key, `429` over the user's rate limit.
    pub(crate) async fn authenticate_user_or_key(&self, headers: &HeaderMap) -> Result<Caller, Response> {
        #[cfg(feature = "jwt")]
        if let Some(user_auth) = &self.user_auth
            && let crate::user_auth::Admission::User(user) = user_auth.admit(headers).await?
        {
            return Ok(Caller::User(user.subject));
        }
        match self.refuse_without_api_key(headers) {
            Some(refused) => Err(refused),
            None => Ok(Caller::ApiKey),
        }
    }

    /// `401` unless `headers` carry the API key (`None` too when no key is set).
    pub(crate) fn refuse_without_api_key(&self, headers: &HeaderMap) -> Option<Response> {
        let expected = self.api_key.as_ref()?;
        let provided = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            return None;
        }
        Some(error_response(StatusCode::UNAUTHORIZED, "missing or invalid bearer token"))
    }

    /// `403` unless workflow `name` is built for end users. A user token starts
    /// only workflows that take the verified token as an input (`--verify-jwt
    /// INPUT` mapped by `--input-header`), so internal workflows stay key-only.
    #[cfg(feature = "jwt")]
    fn refuse_user_token(&self, name: &str) -> Option<Response> {
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
            if let Some(refused) = state.refuse_user_token(&name) {
                return refused;
            }
            let mut request = request;
            request.extensions_mut().insert(AuthenticatedUser(subject));
            next.run(request).await
        }
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
