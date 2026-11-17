//! List the tools of every MCP server in a config file.
//!
//! ```text
//! cargo run -p graphrt-mcp --example list_tools -- servers.json
//! ```
//!
//! `servers.json`: `{ "mcpServers": { "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] } } }`

use graphrt_core::tool::ToolRuntime;
use graphrt_mcp::{McpServers, McpToolRuntime};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: list_tools <servers.json>")?;
    let servers: McpServers = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    for config in servers.into_configs() {
        let runtime = McpToolRuntime::connect(&config).await?;
        for tool in runtime.list().await? {
            #[allow(clippy::print_stdout)]
            {
                println!("{}/{}: {}", runtime.name(), tool.name, tool.description);
            }
        }
        runtime.close().await;
    }
    Ok(())
}
