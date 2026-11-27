//! Errors of the lesson store.

use std::path::PathBuf;

/// Why a lesson could not be read, stored or changed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A file or directory could not be read or written.
    #[error("{}: {message}", path.display())]
    Io {
        /// The path the operation failed on.
        path: PathBuf,
        /// The operating system's message.
        message: String,
    },
    /// An agent name that cannot be used as a store directory.
    #[error("`{0}` is not a valid agent name: use 1 to 128 letters, digits, `.`, `_` and `-`, not starting with a dot")]
    InvalidAgent(String),
    /// A user id that is empty, too long or holds invisible characters.
    #[error("the user id is not valid: use 1 to 256 visible characters")]
    InvalidUser,
    /// Lesson text that is empty, too long, or not safe to place in a prompt.
    #[error("the lesson text is not valid: {0}")]
    InvalidText(String),
    /// No lesson with that id for this agent.
    #[error("agent `{agent}` has no lesson `{id}`")]
    NotFound {
        /// The agent.
        agent: String,
        /// The id that was asked for.
        id: String,
    },
    /// The scope already holds as many lessons as the policy allows.
    #[error("this agent already has {limit} lessons for that scope. Retire or delete some first")]
    Full {
        /// The policy's limit.
        limit: usize,
    },
    /// Too many proposals are waiting for a decision.
    #[error("{limit} proposals are already waiting for a decision. Approve or reject some first")]
    TooManyProposals {
        /// The policy's limit.
        limit: usize,
    },
    /// The change is not possible in the lesson's current state.
    #[error("{0}")]
    NotAllowed(String),
    /// A skill could not be drafted from lessons.
    #[error("{0}")]
    SkillDraft(String),
    /// A store file cannot be understood.
    #[error("the lesson store is damaged: {0}")]
    Damaged(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, error: &std::io::Error) -> Self {
        Self::Io { path: path.into(), message: error.to_string() }
    }
}
