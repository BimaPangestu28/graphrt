//! Dynamic credentials for Streamable HTTP servers.
//!
//! A static `bearer_token` cannot outlive its expiry: an OAuth access token
//! that expires halfway through a run would fail every later call. A
//! [`TokenProvider`] hands out the current access token instead. It is asked
//! before every HTTP request (so a token refreshed by the host between requests
//! is picked up, including on reconnects), and when the server rejects a token
//! (HTTP 401) it is asked once for a new one and the request is retried once.
//!
//! The provider owns the whole credential lifecycle (OAuth discovery, sign-in,
//! storage, refresh): this crate only sends what it is given, so it needs no
//! OAuth dependency and works with any token source.
//!
//! ```no_run
//! # use std::sync::Arc;
//! use futures::future::BoxFuture;
//! use graphrt_mcp::auth::{TokenError, TokenProvider, TokenSource};
//!
//! struct Fixed(String);
//! impl TokenProvider for Fixed {
//!     fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>> {
//!         Box::pin(async move { Ok(self.0.clone()) })
//!     }
//!     fn refresh(&self, _rejected: String) -> BoxFuture<'_, Result<String, TokenError>> {
//!         Box::pin(async { Err(TokenError::SignInRequired("token was rejected".into())) })
//!     }
//! }
//! let source = TokenSource::new(Arc::new(Fixed("token".into())));
//! # let _ = source;
//! ```

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

/// Why a [`TokenProvider`] has no token to give.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TokenError {
    /// The user has to sign in (again): there is no token, or the refresh
    /// token was rejected. Retrying without user action will not help.
    #[error("sign-in required: {0}")]
    SignInRequired(String),
    /// The token could not be obtained right now (network, credential store, ...).
    #[error("access token unavailable: {0}")]
    Unavailable(String),
}

/// Supplies the access token sent as `Authorization: Bearer <token>`.
pub trait TokenProvider: Send + Sync + 'static {
    /// The token to send now. Called before every request; implementations
    /// should cache the token and refresh it when it is (about to be) expired.
    fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>>;

    /// The server rejected `rejected` with HTTP 401. Return a new token (the
    /// request is retried once with it), or an error to fail the request.
    /// Returning `rejected` again also fails the request without a retry.
    fn refresh(&self, rejected: String) -> BoxFuture<'_, Result<String, TokenError>>;
}

/// A shared [`TokenProvider`], as stored in [`crate::HttpConfig`].
///
/// Two sources are equal only when they are the same provider instance.
#[derive(Clone)]
pub struct TokenSource(Arc<dyn TokenProvider>);

impl TokenSource {
    pub fn new(provider: Arc<dyn TokenProvider>) -> Self {
        Self(provider)
    }

    /// The provider.
    pub fn provider(&self) -> &Arc<dyn TokenProvider> {
        &self.0
    }
}

impl fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenSource(..)")
    }
}

impl PartialEq for TokenSource {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(feature = "http")]
pub(crate) use client::TokenAuthClient;

#[cfg(feature = "http")]
mod client {
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::Arc;

    use futures::stream::BoxStream;
    use http::{HeaderName, HeaderValue};
    use rmcp::model::ClientJsonRpcMessage;
    use rmcp::transport::streamable_http_client::{
        SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    };
    use sse_stream::Sse;

    use super::{TokenError, TokenSource};

    type SseStream = BoxStream<'static, Result<Sse, SseError>>;

    /// An rmcp HTTP client that authenticates every request with a
    /// [`TokenSource`] and retries once with a refreshed token after a 401.
    /// The transport's own (static) auth header is ignored.
    #[derive(Clone)]
    pub(crate) struct TokenAuthClient<C> {
        inner: C,
        source: TokenSource,
    }

    impl<C> TokenAuthClient<C>
    where
        C: StreamableHttpClient + Send + Sync,
    {
        pub(crate) fn new(inner: C, source: TokenSource) -> Self {
            Self { inner, source }
        }

        /// Runs `call` with the current token; on a 401, asks for a new token
        /// and runs it once more.
        async fn with_token<T, F, Fut>(&self, call: F) -> Result<T, StreamableHttpError<C::Error>>
        where
            F: Fn(String) -> Fut,
            Fut: Future<Output = Result<T, StreamableHttpError<C::Error>>>,
        {
            let provider = self.source.provider();
            let token = provider.access_token().await.map_err(token_failure)?;
            match call(token.clone()).await {
                Err(error) if is_unauthorized(&error) => {
                    let fresh = provider.refresh(token.clone()).await.map_err(token_failure)?;
                    if fresh == token {
                        return Err(error);
                    }
                    call(fresh).await
                }
                result => result,
            }
        }
    }

    /// A token failure surfaced through the transport. rmcp's error type is
    /// generic over the inner client's error, so it travels as an I/O error
    /// that keeps the provider's message.
    fn token_failure<E: std::error::Error + Send + Sync + 'static>(error: TokenError) -> StreamableHttpError<E> {
        StreamableHttpError::Io(std::io::Error::other(error))
    }

    /// A 401: with a `WWW-Authenticate` challenge (`AuthRequired`) or without
    /// one (rmcp reports the bare status in the message).
    ///
    /// The last case matches rmcp's message text (`HTTP 401 …`, rmcp 3.10): check
    /// it when upgrading rmcp (`recognizes_every_shape_of_a_401` covers it).
    fn is_unauthorized<E: std::error::Error + Send + Sync + 'static>(error: &StreamableHttpError<E>) -> bool {
        match error {
            StreamableHttpError::AuthRequired(_) => true,
            StreamableHttpError::HttpStatus(status) => status.status == 401,
            StreamableHttpError::UnexpectedServerResponse(message) => message.starts_with("HTTP 401"),
            _ => false,
        }
    }

    impl<C> StreamableHttpClient for TokenAuthClient<C>
    where
        C: StreamableHttpClient + Send + Sync,
    {
        type Error = C::Error;

        async fn post_message(
            &self,
            uri: Arc<str>,
            message: ClientJsonRpcMessage,
            session_id: Option<Arc<str>>,
            _auth_header: Option<String>,
            custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
            self.with_token(|token| {
                self.inner.post_message(
                    uri.clone(),
                    message.clone(),
                    session_id.clone(),
                    Some(token),
                    custom_headers.clone(),
                )
            })
            .await
        }

        async fn post_message_with_max_sse_event_size(
            &self,
            uri: Arc<str>,
            message: ClientJsonRpcMessage,
            session_id: Option<Arc<str>>,
            _auth_header: Option<String>,
            custom_headers: HashMap<HeaderName, HeaderValue>,
            max_sse_event_size: usize,
        ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
            self.with_token(|token| {
                self.inner.post_message_with_max_sse_event_size(
                    uri.clone(),
                    message.clone(),
                    session_id.clone(),
                    Some(token),
                    custom_headers.clone(),
                    max_sse_event_size,
                )
            })
            .await
        }

        async fn delete_session(
            &self,
            uri: Arc<str>,
            session_id: Arc<str>,
            _auth_header: Option<String>,
            custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<(), StreamableHttpError<Self::Error>> {
            self.with_token(|token| {
                self.inner.delete_session(uri.clone(), session_id.clone(), Some(token), custom_headers.clone())
            })
            .await
        }

        async fn get_stream(
            &self,
            uri: Arc<str>,
            session_id: Option<Arc<str>>,
            last_event_id: Option<String>,
            _auth_header: Option<String>,
            custom_headers: HashMap<HeaderName, HeaderValue>,
        ) -> Result<SseStream, StreamableHttpError<Self::Error>> {
            self.with_token(|token| {
                self.inner.get_stream(
                    uri.clone(),
                    session_id.clone(),
                    last_event_id.clone(),
                    Some(token),
                    custom_headers.clone(),
                )
            })
            .await
        }

        async fn get_stream_with_max_sse_event_size(
            &self,
            uri: Arc<str>,
            session_id: Option<Arc<str>>,
            last_event_id: Option<String>,
            _auth_header: Option<String>,
            custom_headers: HashMap<HeaderName, HeaderValue>,
            max_sse_event_size: usize,
        ) -> Result<SseStream, StreamableHttpError<Self::Error>> {
            self.with_token(|token| {
                self.inner.get_stream_with_max_sse_event_size(
                    uri.clone(),
                    session_id.clone(),
                    last_event_id.clone(),
                    Some(token),
                    custom_headers.clone(),
                    max_sse_event_size,
                )
            })
            .await
        }
    }

    #[cfg(test)]
    mod tests {
        use std::borrow::Cow;

        use super::*;

        #[test]
        fn recognizes_every_shape_of_a_401() {
            type E = StreamableHttpError<std::io::Error>;
            let challenge =
                E::AuthRequired(rmcp::transport::streamable_http_client::AuthRequiredError::new("Bearer".into()));
            assert!(is_unauthorized(&challenge));
            assert!(is_unauthorized(&E::UnexpectedServerResponse(Cow::Borrowed("HTTP 401 Unauthorized: no"))));
            assert!(!is_unauthorized(&E::UnexpectedServerResponse(Cow::Borrowed("HTTP 500: boom"))));
            assert!(!is_unauthorized(&E::UnexpectedEndOfStream));
        }

        #[test]
        fn token_failures_keep_the_provider_message() {
            let error: StreamableHttpError<std::io::Error> =
                token_failure(TokenError::SignInRequired("refresh token revoked".into()));
            assert!(error.to_string().contains("sign-in required: refresh token revoked"), "{error}");
        }
    }
}
