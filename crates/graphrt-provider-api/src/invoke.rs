//! Platform-native transport for gateway → agent requests.
//!
//! Most targets are reached over HTTP at an [`crate::EndpointAddress`]. Some
//! are not (AWS Lambda: `InvokeWithResponseStream`, signed with SigV4, at an
//! address like `lambda://<function>:<qualifier>`). Such providers implement
//! [`crate::Provider::invoke`]; the gateway then sends the signed protocol
//! request (`POST /internal/v1/runs`, ...) through it and reads the agent's
//! response bytes (the SSE stream) from [`InvokeResponse::body`].

use std::pin::Pin;

use futures::Stream;

use crate::error::ProviderError;

/// A gateway → agent request, already signed (`Graphrt-Sig` among `headers`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokeRequest {
    /// `GET`, `POST`...
    pub method: String,
    /// Path with query, e.g. `/internal/v1/runs`.
    pub path: String,
    /// Headers (lowercase names).
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

/// Response body bytes as they arrive (never buffered whole: SSE).
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, ProviderError>> + Send>>;

/// The agent's answer.
pub struct InvokeResponse {
    /// HTTP status the agent answered with.
    pub status: u16,
    /// Body bytes.
    pub body: ByteStream,
}

impl std::fmt::Debug for InvokeResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("InvokeResponse").field("status", &self.status).finish_non_exhaustive()
    }
}
