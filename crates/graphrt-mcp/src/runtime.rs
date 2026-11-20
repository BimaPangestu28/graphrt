//! [`McpToolRuntime`]: a [`ToolRuntime`] backed by one MCP server connection.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::tool::{ToolCallContext, ToolError, ToolOutput, ToolRuntime, ToolSpec};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::IntoTransport;
use rmcp::{RoleClient, ServiceExt};
use tokio::sync::RwLock;

use crate::config::{McpServerConfig, McpTransport};
use crate::content::{tool_output, tool_spec};

/// Error establishing an MCP connection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    #[error("invalid MCP configuration: {0}")]
    Config(String),
    #[error("tool provider `{0}` is already registered")]
    DuplicateProvider(String),
    #[error("MCP server `{server}`: failed to start process: {source}")]
    Spawn {
        server: String,
        #[source]
        source: std::io::Error,
    },
    #[error("MCP server `{server}`: initialization failed: {message}")]
    Initialize { server: String, message: String },
    #[error("MCP server `{server}`: timed out after {timeout_ms} ms")]
    Timeout { server: String, timeout_ms: u64 },
    #[error("MCP server `{server}`: invalid header `{header}`")]
    InvalidHeader { server: String, header: String },
    #[error("MCP server `{server}`: the `{transport}` transport is not enabled in this build")]
    TransportDisabled { server: String, transport: &'static str },
}

type Client = RunningService<RoleClient, ()>;

/// Tools of a single connected MCP server.
///
/// The tool list is fetched lazily and cached; call [`McpToolRuntime::refresh`]
/// to re-fetch it. Every request is bounded by the configured timeout.
pub struct McpToolRuntime {
    name: String,
    client: Client,
    timeout: Duration,
    allowed_tools: Option<Vec<String>>,
    cache: RwLock<Option<Arc<Vec<ToolSpec>>>>,
}

impl fmt::Debug for McpToolRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpToolRuntime")
            .field("name", &self.name)
            .field("timeout", &self.timeout)
            .field("allowed_tools", &self.allowed_tools)
            .finish_non_exhaustive()
    }
}

impl McpToolRuntime {
    /// Connect to the server described by `config` and complete the MCP handshake.
    pub async fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        match &config.transport {
            McpTransport::Stdio(stdio) => Self::connect_stdio(config, stdio).await,
            McpTransport::Http(http) => Self::connect_http(config, http).await,
        }
    }

    #[cfg(feature = "stdio")]
    async fn connect_stdio(config: &McpServerConfig, stdio: &crate::config::StdioConfig) -> Result<Self, McpError> {
        use std::process::Stdio;

        use crate::config::DEFAULT_PASS_ENV;

        let mut cmd = tokio::process::Command::new(&stdio.command);
        cmd.args(&stdio.args);
        if !stdio.inherit_env {
            cmd.env_clear();
            let names = DEFAULT_PASS_ENV.iter().copied().chain(stdio.pass_env.iter().map(String::as_str));
            for name in names {
                if let Some(value) = std::env::var_os(name) {
                    cmd.env(name, value);
                }
            }
        }
        cmd.envs(&stdio.env);
        if let Some(dir) = &stdio.cwd {
            cmd.current_dir(dir);
        }
        let stderr = if stdio.show_stderr { Stdio::inherit() } else { Stdio::null() };
        let (transport, _stderr) = rmcp::transport::TokioChildProcess::builder(cmd)
            .stderr(stderr)
            .spawn()
            .map_err(|source| McpError::Spawn { server: config.name.clone(), source })?;
        Self::from_transport(config, transport).await
    }

    #[cfg(not(feature = "stdio"))]
    async fn connect_stdio(config: &McpServerConfig, _stdio: &crate::config::StdioConfig) -> Result<Self, McpError> {
        Err(McpError::TransportDisabled { server: config.name.clone(), transport: "stdio" })
    }

    #[cfg(feature = "http")]
    async fn connect_http(config: &McpServerConfig, http_cfg: &crate::config::HttpConfig) -> Result<Self, McpError> {
        use std::collections::HashMap;

        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

        // reqwest is built without a bundled crypto provider; install ring once.
        // An error only means a provider is already installed, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut headers = HashMap::with_capacity(http_cfg.headers.len());
        for (name, value) in &http_cfg.headers {
            let invalid = || McpError::InvalidHeader { server: config.name.clone(), header: name.clone() };
            let header_name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
            let header_value = http::HeaderValue::from_str(value).map_err(|_| invalid())?;
            headers.insert(header_name, header_value);
        }
        let mut transport_cfg =
            StreamableHttpClientTransportConfig::with_uri(http_cfg.url.as_str()).custom_headers(headers);
        if let Some(token) = &http_cfg.bearer_token {
            transport_cfg = transport_cfg.auth_header(token.clone());
        }
        let transport = StreamableHttpClientTransport::from_config(transport_cfg);
        Self::from_transport(config, transport).await
    }

    #[cfg(not(feature = "http"))]
    async fn connect_http(config: &McpServerConfig, _http: &crate::config::HttpConfig) -> Result<Self, McpError> {
        Err(McpError::TransportDisabled { server: config.name.clone(), transport: "http" })
    }

    /// Complete the MCP handshake over any rmcp client transport (stdio,
    /// Streamable HTTP, an in-memory duplex stream, ...). The transport part of
    /// `config` is ignored; its name, timeout and allow-list are used.
    pub async fn from_transport<T, E, A>(config: &McpServerConfig, transport: T) -> Result<Self, McpError>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let timeout = Duration::from_millis(config.timeout_ms);
        let client = with_timeout(timeout, ().serve(transport))
            .await
            .ok_or_else(|| McpError::Timeout { server: config.name.clone(), timeout_ms: config.timeout_ms })?
            .map_err(|e| McpError::Initialize { server: config.name.clone(), message: e.to_string() })?;
        Ok(Self {
            name: config.name.clone(),
            client,
            timeout,
            allowed_tools: config.allowed_tools.clone(),
            cache: RwLock::new(None),
        })
    }

    /// Server name (provider id).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Re-fetch the tool list from the server and update the cache.
    pub async fn refresh(&self) -> Result<Vec<ToolSpec>, ToolError> {
        let tools = with_timeout(self.timeout, self.client.list_all_tools())
            .await
            .ok_or_else(|| self.error("*", format!("listing tools timed out after {} ms", self.timeout.as_millis())))?
            .map_err(|e| self.error("*", format!("listing tools failed: {e}")))?;
        let specs: Vec<ToolSpec> = tools.iter().filter(|t| self.allows(&t.name)).map(tool_spec).collect();
        *self.cache.write().await = Some(Arc::new(specs.clone()));
        Ok(specs)
    }

    /// Close the connection (and stop a child process server).
    pub async fn close(self) {
        // A cancellation error only means the connection was already gone.
        let _ = self.client.cancel().await;
    }

    fn allows(&self, tool: &str) -> bool {
        self.allowed_tools.as_ref().is_none_or(|list| list.iter().any(|t| t == tool))
    }

    fn error(&self, tool: &str, message: impl Into<String>) -> ToolError {
        ToolError { tool: format!("{}/{tool}", self.name), message: message.into() }
    }

    async fn list_cached(&self) -> Result<Vec<ToolSpec>, ToolError> {
        if let Some(cached) = self.cache.read().await.as_ref() {
            return Ok(cached.as_ref().clone());
        }
        self.refresh().await
    }

    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolOutput, ToolError> {
        if !self.allows(name) {
            return Err(self.error(name, "tool is not in the allowed list"));
        }
        let mut params = CallToolRequestParams::new(name.to_owned());
        match arguments {
            serde_json::Value::Object(map) => params = params.with_arguments(map),
            serde_json::Value::Null => {}
            _ => return Err(self.error(name, "arguments must be a JSON object")),
        }
        let result = with_timeout(self.timeout, self.client.call_tool(params))
            .await
            .ok_or_else(|| self.error(name, format!("call timed out after {} ms", self.timeout.as_millis())))?
            .map_err(|e| self.error(name, e.to_string()))?;
        Ok(tool_output(result))
    }
}

impl ToolRuntime for McpToolRuntime {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        Box::pin(self.list_cached())
    }

    /// MCP has no per-call secret channel; the context is never forwarded
    /// (`accepts_secrets` stays `false`, so callers never fill it).
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
        _context: &'a ToolCallContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(self.call_tool(name, arguments))
    }
}

/// `None` when `fut` does not finish within `timeout`.
async fn with_timeout<F: Future>(timeout: Duration, fut: F) -> Option<F::Output> {
    tokio::time::timeout(timeout, fut).await.ok()
}
