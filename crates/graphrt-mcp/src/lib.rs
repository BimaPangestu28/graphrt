//! # graphrt-mcp
//!
//! Exposes the tools of [Model Context Protocol](https://modelcontextprotocol.io)
//! servers to graphrt's `tool` and `agent` nodes.
//!
//! ```no_run
//! # async fn demo() -> Result<(), graphrt_mcp::McpError> {
//! use graphrt_mcp::{McpServers, connect_all};
//!
//! let servers: McpServers = serde_json::from_str(r#"{
//!     "mcpServers": {
//!         "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/data"] },
//!         "remote": { "url": "https://mcp.example.com/mcp", "bearer_token": "..." }
//!     }
//! }"#).unwrap_or_default();
//! let tools = connect_all(&servers.into_configs()).await?;
//! let services = graphrt_core::Services::new().with(tools);
//! # let _ = services;
//! # Ok(()) }
//! ```
//!
//! Each server is registered in the [`ToolRegistry`] under both `<name>` and
//! `mcp:<name>`, so a `tool` node can use `provider_id: mcp:fs`.
//!
//! Security defaults: stdio servers start with an empty environment except for
//! a few launcher essentials ([`config::DEFAULT_PASS_ENV`]) plus explicitly
//! listed variables; every request has a timeout; `allowed_tools` restricts
//! which tools are exposed.

pub mod config;
pub mod content;
pub mod runtime;

use std::sync::Arc;

use graphrt_core::tool::ToolRegistry;

pub use config::{HttpConfig, McpServerConfig, McpServers, McpTransport, StdioConfig};
pub use runtime::{McpError, McpToolRuntime};

/// Prefix of the alias provider name for MCP servers.
pub const PROVIDER_PREFIX: &str = "mcp:";

/// Register a connected server under `<name>` and `mcp:<name>`.
pub fn register(registry: &mut ToolRegistry, runtime: McpToolRuntime) {
    let name = runtime.name().to_owned();
    let runtime = Arc::new(runtime);
    registry.add(format!("{PROVIDER_PREFIX}{name}"), runtime.clone());
    registry.add(name, runtime);
}

/// Connect to every server concurrently and build a [`ToolRegistry`].
/// Fails on the first server that cannot be connected.
pub async fn connect_all(configs: &[McpServerConfig]) -> Result<ToolRegistry, McpError> {
    let connections = futures::future::join_all(configs.iter().map(McpToolRuntime::connect)).await;
    let mut registry = ToolRegistry::new();
    for connection in connections {
        register(&mut registry, connection?);
    }
    Ok(registry)
}
