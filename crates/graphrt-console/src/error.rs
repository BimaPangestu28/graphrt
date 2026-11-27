//! The errors of the console, each with the HTTP status it answers with.

use axum::http::StatusCode;

/// Something the console refused or could not do.
#[derive(Debug, thiserror::Error)]
pub enum ConsoleError {
    /// The console config is not usable.
    #[error("console config: {0}")]
    Config(String),
    /// A file of the console data directory cannot be read or written.
    #[error("{0}")]
    Io(String),
    /// A request that is not valid (a name, a value, a format).
    #[error("{0}")]
    Invalid(String),
    /// Something the request names does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The request conflicts with what is there (the last owner, a missing key).
    #[error("{0}")]
    Conflict(String),
    /// Encryption or decryption failed (a wrong or missing key).
    #[error("{0}")]
    Crypto(String),
    /// The identity provider failed or refused.
    #[error("sign in: {0}")]
    SignIn(String),
}

impl ConsoleError {
    /// The HTTP status a request that failed with this error answers with.
    pub fn status(&self) -> StatusCode {
        match self {
            ConsoleError::Config(_) | ConsoleError::Io(_) | ConsoleError::Crypto(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            ConsoleError::Invalid(_) => StatusCode::BAD_REQUEST,
            ConsoleError::NotFound(_) => StatusCode::NOT_FOUND,
            ConsoleError::Conflict(_) => StatusCode::CONFLICT,
            ConsoleError::SignIn(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

/// `Result` of the console.
pub type Result<T, E = ConsoleError> = std::result::Result<T, E>;
