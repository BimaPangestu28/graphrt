//! Model errors and their mapping to node errors.

use graphrt_core::NodeError;

/// Error raised by a model provider.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    /// Invalid or missing credentials / permissions.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// Too many requests. `retry_after_ms` comes from the provider's `retry-after` header.
    #[error("rate limited: {message}")]
    RateLimit { message: String, retry_after_ms: Option<u64> },
    /// The prompt does not fit into the model's context window.
    #[error("context length exceeded: {0}")]
    ContextLength(String),
    /// The provider rejected the request (invalid parameters, unknown model...).
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Provider-side failure (5xx, overloaded).
    #[error("server error ({status}): {message}")]
    Server { status: u16, message: String },
    /// Connection, TLS or timeout failure.
    #[error("network error: {0}")]
    Network(String),
    /// The response could not be decoded.
    #[error("invalid response: {0}")]
    Decode(String),
    /// Local misconfiguration (unknown provider, missing API key...).
    #[error("configuration error: {0}")]
    Config(String),
}

impl LlmError {
    /// Whether retrying the same request may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            LlmError::RateLimit { .. } | LlmError::Server { .. } | LlmError::Network(_) | LlmError::Decode(_)
        )
    }

    /// Stable machine-readable kind.
    pub fn kind(&self) -> &'static str {
        match self {
            LlmError::Auth(_) => "model_auth",
            LlmError::RateLimit { .. } => "model_rate_limit",
            LlmError::ContextLength(_) => "model_context_length",
            LlmError::BadRequest(_) => "model_bad_request",
            LlmError::Server { .. } => "model_server",
            LlmError::Network(_) => "model_network",
            LlmError::Decode(_) => "model_decode",
            LlmError::Config(_) => "model_config",
        }
    }

    /// Classify an HTTP error response.
    pub fn from_status(status: u16, body: &str, retry_after_ms: Option<u64>) -> Self {
        let message = summarize_body(body);
        let lowered = body.to_ascii_lowercase();
        match status {
            401 | 403 => LlmError::Auth(message),
            429 => LlmError::RateLimit { message, retry_after_ms },
            408 => LlmError::Network(message),
            400 | 404 | 413 | 422
                if lowered.contains("context_length")
                    || lowered.contains("context length")
                    || lowered.contains("maximum context")
                    || lowered.contains("prompt is too long")
                    || lowered.contains("too many tokens") =>
            {
                LlmError::ContextLength(message)
            }
            413 => LlmError::ContextLength(message),
            s if s >= 500 => LlmError::Server { status: s, message },
            _ => LlmError::BadRequest(message),
        }
    }
}

/// Extract a readable message from an error body, capped in length.
fn summarize_body(body: &str) -> String {
    const MAX: usize = 1000;
    let parsed: Option<String> = serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| {
        let err = v.get("error").unwrap_or(&v);
        err.get("message").and_then(|m| m.as_str()).map(str::to_owned).or_else(|| err.as_str().map(str::to_owned))
    });
    let text = parsed.unwrap_or_else(|| body.trim().to_owned());
    if text.chars().count() > MAX {
        let mut cut: String = text.chars().take(MAX).collect();
        cut.push('…');
        cut
    } else {
        text
    }
}

impl From<LlmError> for NodeError {
    fn from(e: LlmError) -> Self {
        let kind = e.kind();
        let mut message = e.to_string();
        if let LlmError::RateLimit { retry_after_ms: Some(ms), .. } = &e {
            message.push_str(&format!(" (retry after {ms} ms)"));
        }
        if e.is_retryable() { NodeError::new(kind, message) } else { NodeError::fatal(kind, message) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert!(matches!(LlmError::from_status(401, "", None), LlmError::Auth(_)));
        let rl = LlmError::from_status(429, r#"{"error":{"message":"slow down"}}"#, Some(2000));
        assert_eq!(rl, LlmError::RateLimit { message: "slow down".into(), retry_after_ms: Some(2000) });
        assert!(rl.is_retryable());
        let ctx =
            LlmError::from_status(400, r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#, None);
        assert!(matches!(ctx, LlmError::ContextLength(_)));
        assert!(!ctx.is_retryable());
        assert!(matches!(LlmError::from_status(400, "nope", None), LlmError::BadRequest(_)));
        assert!(matches!(LlmError::from_status(529, "overloaded", None), LlmError::Server { status: 529, .. }));
    }

    #[test]
    fn node_error_retryability() {
        let n: NodeError = LlmError::Server { status: 500, message: "x".into() }.into();
        assert!(n.retryable);
        assert_eq!(n.kind, "model_server");
        let n: NodeError = LlmError::BadRequest("x".into()).into();
        assert!(!n.retryable);
        let n: NodeError = LlmError::RateLimit { message: "x".into(), retry_after_ms: Some(5) }.into();
        assert!(n.message.contains("retry after 5 ms"));
    }
}
