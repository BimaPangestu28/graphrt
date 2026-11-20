//! Errors raised while loading or running an evaluation.

/// An evaluation error.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EvalError {
    /// The eval file, a dataset line or an assertion is invalid.
    #[error("{0}")]
    Invalid(String),
    /// A file could not be read or written.
    #[error("{0}")]
    Io(String),
    /// The system under test could not be run at all (not a failed run: those
    /// are recorded in the test result).
    #[error("{0}")]
    Run(String),
}

impl EvalError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}
