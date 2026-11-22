//! Provider and deploy errors, classified so the gateway can react without
//! parsing messages (plan §6, conformance C9).

use std::fmt;
use std::time::Duration;

/// What kind of failure a provider call hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderErrorKind {
    /// No capacity (quota, region full, Fargate capacity). The gateway falls
    /// back: another replica / region, or `stop` → cold start.
    Capacity,
    /// HTTP 429 or a provider throttling code. Retried with jittered backoff.
    RateLimited,
    /// 401/403 after the one refresh. Not retried.
    Auth,
    /// The resource does not exist (deleted out of band).
    NotFound,
    /// Network error, timeout, 5xx, 409 conflict. Retried with backoff.
    Transient,
    /// Anything else (bad request, invalid config). Not retried.
    Fatal,
}

impl ProviderErrorKind {
    /// Whether the shared retry loop retries this kind.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Transient)
    }

    /// Stable lowercase name for logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Capacity => "capacity",
            Self::RateLimited => "rate_limited",
            Self::Auth => "auth",
            Self::NotFound => "not_found",
            Self::Transient => "transient",
            Self::Fatal => "fatal",
        }
    }
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A failed run-time provider call. The message never contains credentials:
/// response bodies are truncated and headers are never included.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct ProviderError {
    /// Classification.
    pub kind: ProviderErrorKind,
    /// Human-readable detail.
    pub message: String,
    /// Server-provided `Retry-After`, honoured by the retry loop.
    pub retry_after: Option<Duration>,
}

impl ProviderError {
    /// Builds an error of `kind`.
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), retry_after: None }
    }

    /// Attaches a `Retry-After` hint.
    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    /// Shorthand constructors.
    pub fn capacity(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Capacity, message)
    }
    /// See [`ProviderErrorKind::RateLimited`].
    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::RateLimited, message)
    }
    /// See [`ProviderErrorKind::Auth`].
    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Auth, message)
    }
    /// See [`ProviderErrorKind::NotFound`].
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::NotFound, message)
    }
    /// See [`ProviderErrorKind::Transient`].
    pub fn transient(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Transient, message)
    }
    /// See [`ProviderErrorKind::Fatal`].
    pub fn fatal(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Fatal, message)
    }

    /// Classifies an HTTP status of a provider API. Providers refine the
    /// result with their own error codes (e.g. a 403 `RESOURCE_EXHAUSTED` is
    /// `Capacity`).
    pub fn kind_for_status(status: u16) -> ProviderErrorKind {
        match status {
            401 | 403 => ProviderErrorKind::Auth,
            404 | 410 => ProviderErrorKind::NotFound,
            429 => ProviderErrorKind::RateLimited,
            408 | 409 | 425 | 500..=599 => ProviderErrorKind::Transient,
            _ => ProviderErrorKind::Fatal,
        }
    }
}

/// What kind of failure `DeployDriver::apply` hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeployErrorKind {
    /// The spec is invalid for this target (e.g. async runs on Lambda).
    Invalid,
    /// The target does not support a requested feature.
    Unsupported,
    /// The provider API failed.
    Provider(ProviderErrorKind),
}

/// A failed deploy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("deploy failed ({kind:?}): {message}")]
pub struct DeployError {
    /// Classification.
    pub kind: DeployErrorKind,
    /// Human-readable detail.
    pub message: String,
}

impl DeployError {
    /// An invalid spec.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self { kind: DeployErrorKind::Invalid, message: message.into() }
    }
    /// An unsupported feature.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self { kind: DeployErrorKind::Unsupported, message: message.into() }
    }
}

impl From<ProviderError> for DeployError {
    fn from(error: ProviderError) -> Self {
        Self { kind: DeployErrorKind::Provider(error.kind), message: error.message }
    }
}
