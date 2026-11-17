//! MCP server configuration.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Default per-call timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Host variables passed to stdio servers when `inherit_env` is false.
/// Most launchers (`npx`, `uvx`, `docker`) need these to start at all.
pub const DEFAULT_PASS_ENV: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR", "SYSTEMROOT", "USERPROFILE", "APPDATA"];

/// One MCP server.
///
/// ```json
/// { "name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/data"] }
/// { "name": "remote", "url": "https://mcp.example.com/mcp", "bearer_token": "..." }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Provider name used to reference the server's tools (`<name>` and `mcp:<name>`).
    /// Optional when the config comes from a named map ([`McpServers`]).
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub transport: McpTransport,
    /// Timeout for each request (connect handshake, list, call), in milliseconds.
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// When set, only these tools are listed and callable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// How to reach an MCP server. Chosen by the presence of `command` or `url`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpTransport {
    /// Spawn a child process speaking MCP over stdin/stdout.
    Stdio(StdioConfig),
    /// Connect to a Streamable HTTP endpoint.
    Http(HttpConfig),
}

/// Child-process server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StdioConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Explicit environment variables for the child.
    #[serde(default)]
    pub env: IndexMap<String, String>,
    /// Names of host variables copied into the child's environment
    /// (in addition to [`DEFAULT_PASS_ENV`]).
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// Inherit the full host environment. Off by default so host secrets are
    /// not leaked to third-party servers.
    #[serde(default)]
    pub inherit_env: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Forward the server's stderr to this process's stderr (default: discarded).
    #[serde(default)]
    pub show_stderr: bool,
}

/// Streamable HTTP server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpConfig {
    pub url: String,
    /// Extra headers sent with every request.
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    /// Bearer token for the `Authorization` header (without the `Bearer ` prefix).
    #[serde(default)]
    pub bearer_token: Option<String>,
}

/// A named collection of servers, e.g. a config file:
///
/// ```json
/// { "mcpServers": { "fs": { "command": "npx", "args": ["..."] } } }
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct McpServers {
    #[serde(alias = "mcpServers", alias = "mcp_servers")]
    pub servers: IndexMap<String, McpServerConfig>,
}

impl McpServers {
    /// Server configs with `name` filled from the map keys.
    pub fn into_configs(self) -> Vec<McpServerConfig> {
        self.servers
            .into_iter()
            .map(|(key, mut cfg)| {
                if cfg.name.is_empty() {
                    cfg.name = key;
                }
                cfg
            })
            .collect()
    }
}

impl McpServerConfig {
    /// Whether `tool` passes the allow-list.
    pub fn allows(&self, tool: &str) -> bool {
        self.allowed_tools.as_ref().is_none_or(|list| list.iter().any(|t| t == tool))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stdio_and_http() {
        let stdio: McpServerConfig = serde_json::from_value(serde_json::json!({
            "name": "fs", "command": "npx", "args": ["-y", "srv"], "env": {"A": "1"}, "pass_env": ["GITHUB_TOKEN"]
        }))
        .unwrap();
        match &stdio.transport {
            McpTransport::Stdio(s) => {
                assert_eq!(s.command, "npx");
                assert_eq!(s.args, ["-y", "srv"]);
                assert!(!s.inherit_env);
                assert_eq!(s.pass_env, ["GITHUB_TOKEN"]);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(stdio.timeout_ms, DEFAULT_TIMEOUT_MS);

        let http: McpServerConfig = serde_json::from_value(serde_json::json!({
            "name": "r", "url": "https://x/mcp", "bearer_token": "t", "timeout_ms": 5,
            "allowed_tools": ["a"]
        }))
        .unwrap();
        assert!(matches!(&http.transport, McpTransport::Http(h) if h.bearer_token.as_deref() == Some("t")));
        assert!(http.allows("a"));
        assert!(!http.allows("b"));
        assert!(stdio.allows("anything"));
    }

    #[test]
    fn rejects_config_without_transport() {
        assert!(serde_json::from_value::<McpServerConfig>(serde_json::json!({"name": "x"})).is_err());
    }

    #[test]
    fn named_map_fills_names() {
        let servers: McpServers = serde_json::from_value(serde_json::json!({
            "mcpServers": {"fs": {"command": "srv"}, "web": {"name": "custom", "url": "http://h"}}
        }))
        .unwrap();
        let cfgs = servers.into_configs();
        assert_eq!(cfgs[0].name, "fs");
        assert_eq!(cfgs[1].name, "custom");
    }
}
