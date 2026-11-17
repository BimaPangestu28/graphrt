//! # graphrt-llm
//!
//! Model providers and model-calling nodes for graphrt.
//!
//! - [`ChatModel`]: provider-neutral streaming chat interface.
//! - [`openai`] (feature `openai`): any OpenAI-compatible endpoint.
//! - [`anthropic`] (feature `anthropic`): the Anthropic Messages API.
//! - [`ModelRegistry`]: named providers, registered as a host service.
//! - Nodes: `llm`, `question-classifier`, `parameter-extractor` and `agent`
//!   (a tool-calling loop over [`graphrt_core::tool::ToolRegistry`]).
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut registry = graphrt_core::Registry::with_builtins();
//! graphrt_llm::register(&mut registry);
//! let services = graphrt_core::Services::new().with(graphrt_llm::ModelRegistry::from_env()?);
//! # let _ = (registry, services);
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod mock;
pub mod nodes;
pub mod registry;
pub mod sse;
pub mod types;

#[cfg(feature = "http")]
mod http;

#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(feature = "openai")]
pub mod openai;

pub use error::LlmError;
pub use registry::ModelRegistry;
pub use types::{
    ChatDelta, ChatModel, ChatRequest, ChatResponse, ChatStream, ContentPart, FinishReason, Message, ResponseFormat,
    Role, ToolCall, ToolChoice, collect,
};

/// Register the `llm`, `question-classifier`, `parameter-extractor` and `agent` node kinds.
pub fn register(registry: &mut graphrt_core::Registry) {
    registry.register("llm", nodes::llm::factory);
    registry.register("question-classifier", nodes::classifier::factory);
    registry.register("parameter-extractor", nodes::extractor::factory);
    registry.register("agent", nodes::agent::factory);
}
