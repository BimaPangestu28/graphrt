//! Errors of the server, and the HTTP status each one answers with.

/// What can go wrong on the server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// An unusable config (the server does not start).
    #[error("config: {0}")]
    Config(String),
    /// A file that cannot be read or written.
    #[error("{0}")]
    Io(String),
    /// A bug or a missing system facility.
    #[error("internal error: {0}")]
    Internal(String),
    /// No valid credentials (401).
    #[error("{0}")]
    Unauthorized(String),
    /// Valid credentials without the right (403).
    #[error("{0}")]
    Forbidden(String),
    /// Nothing there, or nothing the caller may know about (404).
    #[error("{0}")]
    NotFound(String),
    /// A request that cannot be served as sent (400).
    #[error("{0}")]
    BadRequest(String),
    /// A request that conflicts with the current state (409).
    #[error("{0}")]
    Conflict(String),
    /// Too many requests (429).
    #[error("{0}")]
    TooMany(String),
    /// Over a configured quota (403 with code `limit`).
    #[error("{0}")]
    Limit(String),
    /// A plugin service failed or answered with an error (502).
    #[error("{0}")]
    Upstream(String),
}

impl ServerError {
    /// The HTTP status.
    pub fn status(&self) -> u16 {
        match self {
            ServerError::Config(_) | ServerError::Io(_) | ServerError::Internal(_) => 500,
            ServerError::Unauthorized(_) => 401,
            ServerError::Forbidden(_) | ServerError::Limit(_) => 403,
            ServerError::NotFound(_) => 404,
            ServerError::BadRequest(_) => 400,
            ServerError::Conflict(_) => 409,
            ServerError::TooMany(_) => 429,
            ServerError::Upstream(_) => 502,
        }
    }

    /// A short machine readable code.
    pub fn code(&self) -> &'static str {
        match self {
            ServerError::Config(_) | ServerError::Io(_) | ServerError::Internal(_) => "internal",
            ServerError::Unauthorized(_) => "unauthorized",
            ServerError::Forbidden(_) => "forbidden",
            ServerError::Limit(_) => "limit",
            ServerError::NotFound(_) => "not_found",
            ServerError::BadRequest(_) => "bad_request",
            ServerError::Conflict(_) => "conflict",
            ServerError::TooMany(_) => "rate_limited",
            ServerError::Upstream(_) => "service_failed",
        }
    }
}

impl From<graphrt_console::ConsoleError> for ServerError {
    fn from(error: graphrt_console::ConsoleError) -> Self {
        use graphrt_console::ConsoleError as E;
        match error {
            E::Invalid(why) => ServerError::BadRequest(why),
            E::NotFound(why) => ServerError::NotFound(why),
            E::Conflict(why) => ServerError::Conflict(why),
            E::SignIn(why) => ServerError::Unauthorized(why),
            E::Config(why) => ServerError::Config(why),
            other => ServerError::Internal(other.to_string()),
        }
    }
}

/// The result of the server.
pub type ServerResult<T> = std::result::Result<T, ServerError>;
