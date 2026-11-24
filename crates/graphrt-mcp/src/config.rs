//! MCP server configuration.
//!
//! Parsing is strict: unknown keys (typically typos such as `alowed_tools`) are
//! rejected, because a silently ignored allow-list would expose every tool of a
//! server. The camelCase spellings common in MCP client configs
//! (`allowedTools`, `timeoutMs`, `passEnv`, `inheritEnv`, `showStderr`,
//! `bearerToken`) are accepted as aliases.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Default per-call timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Host variables passed to stdio servers when `inherit_env` is false
/// (shared with plugin processes; see [`graphrt_core::child_env`]).
pub use graphrt_core::child_env::DEFAULT_PASS_ENV;

/// One MCP server.
///
/// ```json
/// { "name": "fs", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/data"] }
/// { "name": "remote", "url": "https://mcp.example.com/mcp", "bearer_token": "..." }
/// ```
///
/// Exactly one of `command` (stdio) or `url` (Streamable HTTP) must be present, and
/// transport-specific keys must match the chosen transport.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawServer")]
pub struct McpServerConfig {
    /// Provider name used to reference the server's tools (`<name>` and `mcp:<name>`).
    /// Optional when the config comes from a named map ([`McpServers`]).
    pub name: String,
    #[serde(flatten)]
    pub transport: McpTransport,
    /// Timeout for each request (connect handshake, list, call), in milliseconds.
    /// Defaults to [`DEFAULT_TIMEOUT_MS`].
    pub timeout_ms: u64,
    /// When set, only these tools are listed and callable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
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
#[serde(deny_unknown_fields)]
pub struct StdioConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Explicit environment variables for the child.
    #[serde(default)]
    pub env: IndexMap<String, String>,
    /// Names of host variables copied into the child's environment
    /// (in addition to [`DEFAULT_PASS_ENV`]).
    #[serde(default, alias = "passEnv")]
    pub pass_env: Vec<String>,
    /// Inherit the full host environment. Off by default so host secrets are
    /// not leaked to third-party servers.
    #[serde(default, alias = "inheritEnv")]
    pub inherit_env: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Forward the server's stderr to this process's stderr (default: discarded).
    #[serde(default, alias = "showStderr")]
    pub show_stderr: bool,
}

/// Streamable HTTP server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub url: String,
    /// Extra headers sent with every request.
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    /// Bearer token for the `Authorization` header (without the `Bearer ` prefix).
    #[serde(default, alias = "bearerToken")]
    pub bearer_token: Option<String>,
}

/// Every key a server entry may contain, used to validate configs strictly.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    #[serde(default)]
    name: String,
    /// Optional explicit transport kind, as written by many MCP clients.
    #[serde(default, rename = "type", alias = "transport")]
    kind: Option<String>,
    #[serde(default, alias = "timeoutMs")]
    timeout_ms: Option<u64>,
    #[serde(default, alias = "allowedTools")]
    allowed_tools: Option<Vec<String>>,
    // stdio
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<IndexMap<String, String>>,
    #[serde(default, alias = "passEnv")]
    pass_env: Option<Vec<String>>,
    #[serde(default, alias = "inheritEnv")]
    inherit_env: Option<bool>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, alias = "showStderr")]
    show_stderr: Option<bool>,
    // http
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<IndexMap<String, String>>,
    #[serde(default, alias = "bearerToken")]
    bearer_token: Option<String>,
}

impl TryFrom<RawServer> for McpServerConfig {
    type Error = String;

    fn try_from(raw: RawServer) -> Result<Self, String> {
        let label = if raw.name.is_empty() { "MCP server".to_owned() } else { format!("MCP server `{}`", raw.name) };
        let transport = match (raw.command, raw.url) {
            (Some(_), Some(_)) => {
                return Err(format!("{label}: set either `command` (stdio) or `url` (http), not both"));
            }
            (None, None) => return Err(format!("{label}: needs `command` (stdio) or `url` (http)")),
            (Some(command), None) => {
                if let Some(kind) = raw.kind.as_deref().filter(|k| !k.eq_ignore_ascii_case("stdio")) {
                    return Err(format!("{label}: `type: {kind}` does not match a stdio `command`"));
                }
                let http_keys = [("headers", raw.headers.is_some()), ("bearer_token", raw.bearer_token.is_some())];
                reject_foreign_keys(&label, "stdio", &http_keys)?;
                McpTransport::Stdio(StdioConfig {
                    command,
                    args: raw.args.unwrap_or_default(),
                    env: raw.env.unwrap_or_default(),
                    pass_env: raw.pass_env.unwrap_or_default(),
                    inherit_env: raw.inherit_env.unwrap_or(false),
                    cwd: raw.cwd,
                    show_stderr: raw.show_stderr.unwrap_or(false),
                })
            }
            (None, Some(url)) => {
                let http_kinds = ["http", "streamable-http", "streamable_http", "streamablehttp"];
                if let Some(kind) =
                    raw.kind.as_deref().filter(|k| !http_kinds.iter().any(|h| h.eq_ignore_ascii_case(k)))
                {
                    return Err(format!("{label}: `type: {kind}` does not match an http `url`"));
                }
                let stdio_keys = [
                    ("args", raw.args.is_some()),
                    ("env", raw.env.is_some()),
                    ("pass_env", raw.pass_env.is_some()),
                    ("inherit_env", raw.inherit_env.is_some()),
                    ("cwd", raw.cwd.is_some()),
                    ("show_stderr", raw.show_stderr.is_some()),
                ];
                reject_foreign_keys(&label, "http", &stdio_keys)?;
                McpTransport::Http(HttpConfig {
                    url,
                    headers: raw.headers.unwrap_or_default(),
                    bearer_token: raw.bearer_token,
                })
            }
        };
        Ok(McpServerConfig {
            name: raw.name,
            transport,
            timeout_ms: raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            allowed_tools: raw.allowed_tools,
        })
    }
}

fn reject_foreign_keys(label: &str, transport: &str, keys: &[(&str, bool)]) -> Result<(), String> {
    match keys.iter().find(|(_, present)| *present) {
        Some((key, _)) => Err(format!("{label}: `{key}` does not apply to {transport} servers")),
        None => Ok(()),
    }
}

/// A named collection of servers, e.g. a config file:
///
/// ```json
/// { "mcpServers": { "fs": { "command": "npx", "args": ["..."] } } }
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    fn parse(value: serde_json::Value) -> Result<McpServerConfig, String> {
        serde_json::from_value::<McpServerConfig>(value).map_err(|e| e.to_string())
    }

    #[test]
    fn parses_stdio_and_http() {
        let stdio = parse(serde_json::json!({
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

        let http = parse(serde_json::json!({
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
    fn accepts_camel_case_aliases() {
        let cfg = parse(serde_json::json!({
            "name": "fs", "type": "stdio", "command": "srv", "allowedTools": ["read"], "timeoutMs": 7,
            "passEnv": ["X"], "inheritEnv": true, "showStderr": true
        }))
        .unwrap();
        assert_eq!(cfg.allowed_tools, Some(vec!["read".to_owned()]));
        assert_eq!(cfg.timeout_ms, 7);
        assert!(!cfg.allows("write"));
        match &cfg.transport {
            McpTransport::Stdio(s) => assert!(s.inherit_env && s.show_stderr && s.pass_env == ["X"]),
            other => panic!("unexpected {other:?}"),
        }
        let http =
            parse(serde_json::json!({"url": "https://x", "type": "streamable-http", "bearerToken": "t"})).unwrap();
        assert!(matches!(&http.transport, McpTransport::Http(h) if h.bearer_token.as_deref() == Some("t")));
    }

    #[test]
    fn rejects_typos_and_mixed_transports() {
        let typo = parse(serde_json::json!({"command": "srv", "alowed_tools": ["a"]})).unwrap_err();
        assert!(typo.contains("alowed_tools"), "{typo}");
        let both = parse(serde_json::json!({"command": "srv", "url": "https://x"})).unwrap_err();
        assert!(both.contains("not both"), "{both}");
        let foreign = parse(serde_json::json!({"url": "https://x", "args": ["a"]})).unwrap_err();
        assert!(foreign.contains("`args`"), "{foreign}");
        let kind = parse(serde_json::json!({"command": "srv", "type": "http"})).unwrap_err();
        assert!(kind.contains("type"), "{kind}");
    }

    #[test]
    fn rejects_config_without_transport() {
        assert!(parse(serde_json::json!({"name": "x"})).is_err());
    }

    #[test]
    fn serialization_round_trips() {
        let cfg = parse(serde_json::json!({"name": "fs", "command": "srv", "allowedTools": ["a"]})).unwrap();
        let text = serde_json::to_value(&cfg).unwrap();
        assert_eq!(parse(text).unwrap(), cfg);
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
        assert!(serde_json::from_value::<McpServers>(serde_json::json!({"mcpServer": {}})).is_err());
    }
}
