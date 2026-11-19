//! Errors raised by retrieval components.

use graphrt_core::NodeError;

/// Error from an embedder, store, reranker or retriever.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RagError {
    /// Invalid configuration (missing dataset, bad URL, unknown provider...).
    #[error("configuration error: {0}")]
    Config(String),
    /// Invalid request input (empty query, dimension mismatch...).
    #[error("invalid input: {0}")]
    Input(String),
    /// Credentials were rejected.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The provider is rate limiting requests.
    #[error("rate limited: {message}")]
    RateLimit { message: String, retry_after_ms: Option<u64> },
    /// Non-success HTTP status not covered by a more specific variant.
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    /// Connection / transport failure.
    #[error("network error: {0}")]
    Network(String),
    /// The response could not be decoded.
    #[error("invalid response: {0}")]
    Decode(String),
    /// Vector store failure.
    #[error("store error: {0}")]
    Store(String),
    /// Unknown dataset or document.
    #[error("not found: {0}")]
    NotFound(String),
    /// Local persistence failure.
    #[error("I/O error: {0}")]
    Io(String),
    /// An in-process model could not be loaded or run (missing or corrupt files,
    /// unsupported architecture, inference failure).
    #[error("embedding model error: {0}")]
    Model(String),
    /// The dataset or store does not support this operation (e.g. listing the
    /// documents of an external retriever).
    #[error("not supported: {0}")]
    Unsupported(String),
}

impl RagError {
    /// Whether retrying the same request may succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            RagError::RateLimit { .. } | RagError::Network(_) => true,
            RagError::Http { status, .. } => *status >= 500 || *status == 408,
            _ => false,
        }
    }

    /// Short machine-readable category.
    pub fn kind(&self) -> &'static str {
        match self {
            RagError::Config(_) => "config",
            RagError::Input(_) => "invalid_input",
            RagError::Auth(_) => "auth",
            RagError::RateLimit { .. } => "rate_limit",
            RagError::Http { .. } => "http_status",
            RagError::Network(_) => "network",
            RagError::Decode(_) => "invalid_response",
            RagError::Store(_) => "store",
            RagError::NotFound(_) => "not_found",
            RagError::Io(_) => "io",
            RagError::Model(_) => "model",
            RagError::Unsupported(_) => "unsupported",
        }
    }
}

impl From<RagError> for NodeError {
    fn from(e: RagError) -> Self {
        let retry_after = match &e {
            RagError::RateLimit { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        };
        let base = if e.is_retryable() {
            NodeError::new(e.kind(), e.to_string())
        } else {
            NodeError::fatal(e.kind(), e.to_string())
        };
        base.with_retry_after(retry_after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_mapping() {
        assert!(RagError::Network("x".into()).is_retryable());
        assert!(RagError::Http { status: 503, message: String::new() }.is_retryable());
        assert!(!RagError::Http { status: 400, message: String::new() }.is_retryable());
        let ne: NodeError = RagError::RateLimit { message: "slow".into(), retry_after_ms: Some(1500) }.into();
        assert!(ne.retryable);
        assert_eq!(ne.retry_after_ms, Some(1500));
        let ne: NodeError = RagError::Auth("bad key".into()).into();
        assert!(!ne.retryable);
    }
}
