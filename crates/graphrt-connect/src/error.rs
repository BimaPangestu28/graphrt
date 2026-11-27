//! Errors of connectors and syncs, with a stable kind and a message a
//! business user can act on. Messages never contain a credential.

/// Why listing, fetching or syncing a source failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// The source configuration is wrong (missing field, bad URL, bad JSONPath).
    #[error("{0}")]
    Config(String),
    /// The credential is missing, expired or was refused (HTTP 401).
    #[error("{0}")]
    Auth(String),
    /// The credential is valid but has no access to this item (HTTP 403).
    #[error("{0}")]
    Forbidden(String),
    /// The folder, page or address does not exist (HTTP 404).
    #[error("{0}")]
    NotFound(String),
    /// The address is not allowed: a private network address, a host outside
    /// the allow list, or a path robots.txt disallows.
    #[error("{0}")]
    Denied(String),
    /// The service asked to slow down (HTTP 429) and retries ran out.
    #[error("{0}")]
    RateLimited(String),
    /// The response is larger than the configured limit.
    #[error("{0}")]
    TooLarge(String),
    /// Any other HTTP status.
    #[error("{message}")]
    Http { status: u16, message: String },
    /// The service could not be reached or the connection broke (timeouts included).
    #[error("{0}")]
    Network(String),
    /// The response could not be understood.
    #[error("{0}")]
    Decode(String),
    /// Writing to the knowledge base or to the sync state failed.
    #[error("{0}")]
    Store(String),
    /// Another process (or another sync in this one) is syncing the source.
    #[error("{0}")]
    Busy(String),
}

impl ConnectError {
    /// Stable machine readable kind (`auth`, `not_found`, ...), for UIs that
    /// translate the message and for metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Auth(_) => "auth",
            Self::Forbidden(_) => "forbidden",
            Self::NotFound(_) => "not_found",
            Self::Denied(_) => "denied",
            Self::RateLimited(_) => "rate_limited",
            Self::TooLarge(_) => "too_large",
            Self::Http { .. } => "http",
            Self::Network(_) => "network",
            Self::Decode(_) => "decode",
            Self::Store(_) => "store",
            Self::Busy(_) => "busy",
        }
    }

    /// Whether trying again later may succeed (network trouble, rate limits,
    /// server errors), as opposed to a problem someone has to fix.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) | Self::RateLimited(_) | Self::Busy(_) => true,
            Self::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }

    /// The error for an HTTP status of `service` (a name such as "Google Drive").
    pub fn from_status(service: &str, status: u16, detail: &str) -> Self {
        let detail = if detail.is_empty() { String::new() } else { format!(" ({detail})") };
        match status {
            401 => Self::Auth(format!("{service} did not accept the sign-in; connect the account again{detail}")),
            403 => Self::Forbidden(format!("{service} refused access; check that the account can open it{detail}")),
            404 => {
                Self::NotFound(format!("{service} could not find it; check the address or that it was shared{detail}"))
            }
            429 => Self::RateLimited(format!("{service} asked to slow down; the next sync tries again{detail}")),
            _ => Self::Http { status, message: format!("{service} answered with HTTP {status}{detail}") },
        }
    }
}

/// Result alias of this crate.
pub type Result<T, E = ConnectError> = std::result::Result<T, E>;
