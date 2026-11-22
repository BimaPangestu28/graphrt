//! Bearer authentication of the API: the API key (`--api-key-env`) and, with
//! `--verify-jwt`, end-user tokens on the routes end users may call.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use super::errors::error_response;
use super::state::AppState;

/// The verified end user of a request (`sub` of its JWT), set by `--verify-jwt`.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser(pub String);

impl AppState {
    /// Authorize a request a user may make about their own runs (feedback): a
    /// valid end-user JWT (with `--verify-jwt`; returns its subject for the
    /// handler's ownership check), else the API key (returns `None`).
    ///
    /// @throws Response - `401` for an invalid token or key, `429` over the user's rate limit.
    #[cfg(feature = "scores")]
    pub(crate) async fn authorize_user_or_key(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<Option<String>, Response> {
        #[cfg(feature = "jwt")]
        if let Some(user_auth) = &self.user_auth
            && let crate::user_auth::Admission::User(user) = user_auth.admit(headers).await?
        {
            return Ok(Some(user.subject));
        }
        if self.authorized(headers) {
            Ok(None)
        } else {
            Err(error_response(StatusCode::UNAUTHORIZED, "missing or invalid bearer token"))
        }
    }

    /// Whether `headers` carry the API key (always true when none is set).
    pub(crate) fn authorized(&self, headers: &axum::http::HeaderMap) -> bool {
        let Some(expected) = &self.api_key else { return true };
        let provided = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        constant_time_eq(provided.as_bytes(), expected.as_bytes())
    }
}

/// Authorize `POST /v1/workflows/{name}/runs`: a valid end-user JWT (with
/// `--verify-jwt`), else the API key. A present but invalid token is refused.
pub(super) async fn authorize_start(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    request: Request,
    next: Next,
) -> Response {
    #[cfg(feature = "jwt")]
    if let Some(user_auth) = &state.user_auth {
        let mut request = request;
        match user_auth.admit(request.headers()).await {
            Err(refused) => return refused,
            // A user token starts only workflows built for end users: those that
            // take the verified token as an input.
            Ok(crate::user_auth::Admission::User(_)) if !state.takes_header_input(&name, user_auth.input()) => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    &format!("workflow `{name}` does not accept user tokens (no `{}` input)", user_auth.input()),
                );
            }
            Ok(crate::user_auth::Admission::User(user)) => {
                request.extensions_mut().insert(AuthenticatedUser(user.subject));
                return next.run(request).await;
            }
            Ok(crate::user_auth::Admission::NoToken) => return require_auth(State(state), request, next).await,
        }
    }
    #[cfg(not(feature = "jwt"))]
    let _ = name;
    require_auth(State(state), request, next).await
}

pub(super) async fn require_auth(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    if state.authorized(request.headers()) {
        next.run(request).await
    } else {
        error_response(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
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
