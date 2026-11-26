//! Lesson contract between a lesson store and the nodes that learn.
//!
//! A lesson is one short statement an agent learned from earlier work: a
//! preference, a fact, a procedure or a correction. This module holds only
//! the contract. A host registers a [`Lessons`] service (for example the store
//! of the `graphrt-learn` crate) and the `agent` node asks it for the notes of
//! one [`LessonScope`] when a run starts.
//!
//! A source returns only lessons that were approved, and only those of the
//! scope it is asked for: the lessons of the agent as a whole plus the lessons
//! of the one user the run is for. Nothing here writes a lesson. Learning
//! happens outside the run.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

/// Whose lessons a run reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LessonScope {
    /// The agent the lessons belong to.
    pub agent: String,
    /// The user the run is for. `None` reads only the lessons of the agent as a whole.
    pub user: Option<String>,
}

/// The approved lessons of one scope, ready for a system prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LessonNotes {
    /// The notes section, or an empty string when there is nothing to add.
    pub text: String,
    /// Ids of the lessons in `text`, in order.
    pub ids: Vec<String>,
}

impl LessonNotes {
    /// Whether there is nothing to add to the prompt.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// The lessons of a scope could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LearningError {
    /// Stable error kind, for example `learning_store`.
    pub kind: &'static str,
    /// What went wrong, safe to show to the owner.
    pub message: String,
}

impl LearningError {
    /// An error of `kind` with `message`.
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// A source of approved lessons.
pub trait LessonSource: Send + Sync + fmt::Debug {
    /// The approved lessons `scope` may read.
    fn notes<'a>(&'a self, scope: &'a LessonScope) -> BoxFuture<'a, Result<LessonNotes, LearningError>>;
}

/// Service registered in [`crate::node::Services`]: where nodes find lessons.
#[derive(Clone, Debug)]
pub struct Lessons(pub Arc<dyn LessonSource>);

impl Lessons {
    /// Wrap a lesson source as a service.
    pub fn new(source: Arc<dyn LessonSource>) -> Self {
        Self(source)
    }
}
