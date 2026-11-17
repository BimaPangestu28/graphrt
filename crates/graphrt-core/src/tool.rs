//! Tool contract shared by the `tool` node, the `agent` node and tool
//! providers (native Rust tools, MCP servers, HTTP tools...).

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::value::Value;

/// Description of a callable tool, exposed to models for tool calling.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Unique name within a [`ToolRuntime`] (e.g. `search` or `github.create_issue`).
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: serde_json::Value,
}

/// Result of a tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    /// Text handed back to a model.
    pub text: String,
    /// Structured result, if the tool returns one.
    pub data: Value,
    /// Whether the tool reported an error (the agent loop may still continue).
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), data: Value::Null, is_error: false }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), data: Value::Null, is_error: true }
    }
}

/// Error calling a tool (transport, unknown tool...).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tool `{tool}`: {message}")]
pub struct ToolError {
    pub tool: String,
    pub message: String,
}

/// A source of tools.
pub trait ToolRuntime: Send + Sync + fmt::Debug {
    /// Tools currently available.
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>>;
    /// Invoke a tool with a JSON arguments object.
    fn call<'a>(&'a self, name: &'a str, arguments: serde_json::Value) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

/// Service registered in [`crate::node::Services`]: named tool providers
/// (e.g. `builtin`, `mcp:github`). Nodes reference tools as `provider/tool`.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    providers: indexmap::IndexMap<String, Arc<dyn ToolRuntime>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.providers.keys()).finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, provider: impl Into<String>, runtime: Arc<dyn ToolRuntime>) -> &mut Self {
        self.providers.insert(provider.into(), runtime);
        self
    }

    pub fn get(&self, provider: &str) -> Option<&Arc<dyn ToolRuntime>> {
        self.providers.get(provider)
    }

    pub fn providers(&self) -> impl Iterator<Item = (&str, &Arc<dyn ToolRuntime>)> {
        self.providers.iter().map(|(k, v)| (k.as_str(), v))
    }
}
