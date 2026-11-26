//! A model that turns a system prompt and a user message into text.
//!
//! Work that happens outside a run (drafting a skill from a description,
//! suggesting lessons from earlier feedback) needs one plain completion and
//! nothing else of a chat model: no tools, no streaming, no run context. This
//! trait is that much and no more, so the crates that do such work do not
//! depend on a model provider. `graphrt-llm` implements it for any chat model.

use std::fmt;

use futures::future::BoxFuture;

/// One completion request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRequest {
    /// Instructions for the model.
    pub system: String,
    /// The material to work on.
    pub user: String,
    /// Upper bound of the answer, in tokens.
    pub max_tokens: u32,
}

/// The model could not answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TextModelError(pub String);

/// A model that answers one request with text.
pub trait TextModel: Send + Sync + fmt::Debug {
    /// Answer `request`.
    fn complete(&self, request: TextRequest) -> BoxFuture<'_, Result<String, TextModelError>>;
}
