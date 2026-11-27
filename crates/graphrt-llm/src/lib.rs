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
//! # #[cfg(feature = "http")]
//! let models = graphrt_llm::ModelRegistry::from_env()?;
//! # #[cfg(not(feature = "http"))]
//! # let models = graphrt_llm::ModelRegistry::new();
//! let services = graphrt_core::Services::new().with(models);
//! # let _ = (registry, services);
//! # Ok(())
//! # }
//! ```

pub mod credential_config;
pub mod credentials;
pub mod error;
/// Scripted model for tests and offline examples (feature `mock`, on by default).
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod nodes;
pub mod providers;
pub mod registry;
/// Record / replay provider for deterministic runs (feature `replay`).
#[cfg(feature = "replay")]
pub mod replay;
pub mod sse;
/// Plain completions for work outside a run (feature `text-model`).
#[cfg(feature = "text-model")]
pub mod text_model;
pub mod types;
#[cfg(feature = "typesafe")]
pub mod typesafe;

#[cfg(feature = "http")]
mod http;
#[cfg(feature = "http")]
pub use http::prewarm_http_client;

#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(feature = "bedrock")]
pub mod bedrock;
#[cfg(feature = "gemini")]
pub mod gemini;
#[cfg(feature = "openai")]
pub mod openai;

pub use error::LlmError;
pub use registry::ModelRegistry;
pub use types::{
    ChatDelta, ChatModel, ChatRequest, ChatResponse, ChatStream, ContentPart, FinishReason, InvalidToolArguments,
    Message, ResponseFormat, Role, ToolCall, ToolChoice, collect,
};

/// Register the `llm`, `question-classifier`, `parameter-extractor` and `agent` node kinds.
pub fn register(registry: &mut graphrt_core::Registry) {
    registry.register_lazily_described("llm", nodes::descriptors::llm, nodes::llm::factory);
    registry.register_lazily_described(
        "question-classifier",
        nodes::descriptors::question_classifier,
        nodes::classifier::factory,
    );
    registry.register_lazily_described(
        "parameter-extractor",
        nodes::descriptors::parameter_extractor,
        nodes::extractor::factory,
    );
    registry.register_lazily_described("agent", nodes::descriptors::agent, nodes::agent::factory);
    #[cfg(feature = "typesafe")]
    registry.register_lazily_described("system-one", nodes::descriptors::system_one, nodes::system_one::factory);
}
