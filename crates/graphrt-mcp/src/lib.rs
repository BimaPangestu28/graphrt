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

/// Register a connected server under `<name>` and `mcp:<name>`, replacing any
/// provider already registered under those names. Prefer [`try_register`], which
/// refuses to shadow an existing provider.
pub fn register(registry: &mut ToolRegistry, runtime: McpToolRuntime) {
    let name = runtime.name().to_owned();
    let runtime = Arc::new(runtime);
    registry.add(format!("{PROVIDER_PREFIX}{name}"), runtime.clone());
    registry.add(name, runtime);
}

/// Register a connected server under `<name>` and `mcp:<name>`. Fails if either name
/// is already taken, so a server can never silently replace another provider
/// (for example one named `builtin`).
pub fn try_register(registry: &mut ToolRegistry, runtime: McpToolRuntime) -> Result<(), McpError> {
    let name = runtime.name().to_owned();
    validate_name(&name)?;
    let alias = format!("{PROVIDER_PREFIX}{name}");
    for provider in [&name, &alias] {
        if registry.get(provider).is_some() {
            return Err(McpError::DuplicateProvider(provider.clone()));
        }
    }
    register(registry, runtime);
    Ok(())
}

/// Check server names before connecting: non-empty, no `/` (tools are referenced as
/// `provider/tool`), and unique.
pub fn validate_configs(configs: &[McpServerConfig]) -> Result<(), McpError> {
    let mut seen = std::collections::HashSet::new();
    for config in configs {
        validate_name(&config.name)?;
        if !seen.insert(config.name.as_str()) {
            return Err(McpError::Config(format!("duplicate MCP server name `{}`", config.name)));
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), McpError> {
    if name.trim().is_empty() {
        return Err(McpError::Config("MCP server name must not be empty".to_owned()));
    }
    if name.contains('/') {
        return Err(McpError::Config(format!("MCP server name `{name}` must not contain `/`")));
    }
    Ok(())
}

/// Connect to every server concurrently and build a [`ToolRegistry`].
/// Names are validated first; fails on the first server that cannot be connected.
pub async fn connect_all(configs: &[McpServerConfig]) -> Result<ToolRegistry, McpError> {
    validate_configs(configs)?;
    let connections = futures::future::join_all(configs.iter().map(McpToolRuntime::connect)).await;
    let mut registry = ToolRegistry::new();
    for connection in connections {
        try_register(&mut registry, connection?)?;
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str) -> McpServerConfig {
        serde_json::from_value(serde_json::json!({"name": name, "command": "srv"})).unwrap()
    }

    #[test]
    fn config_names_are_validated() {
        assert!(validate_configs(&[cfg("a"), cfg("b")]).is_ok());
        assert!(matches!(validate_configs(&[cfg("a"), cfg("a")]), Err(McpError::Config(m)) if m.contains("duplicate")));
        assert!(validate_configs(&[cfg("")]).is_err());
        assert!(validate_configs(&[cfg("a/b")]).is_err());
    }

    #[tokio::test]
    async fn connect_all_rejects_duplicates_before_spawning() {
        // `srv` does not exist: a spawn would fail with a different error.
        let err = connect_all(&[cfg("dup"), cfg("dup")]).await.unwrap_err();
        assert!(matches!(err, McpError::Config(_)), "{err}");
    }
}
