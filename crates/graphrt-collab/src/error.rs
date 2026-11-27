//! Errors of the collaboration crate.

/// What can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum CollabError {
    /// An update or state vector that cannot be decoded or applied.
    #[error("invalid update: {0}")]
    Update(String),
    /// Content the project refuses (too deep, too large, wrong shape).
    #[error("invalid project: {0}")]
    Invalid(String),
    /// A protocol message that cannot be read.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// The result of this crate.
pub type Result<T, E = CollabError> = std::result::Result<T, E>;
