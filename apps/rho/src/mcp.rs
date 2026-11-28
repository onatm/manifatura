//! MCP servers for the chat, from `[mcp]` in `config.toml`. A server is a URL (streamable HTTP) or a program
//! (stdio):
//!
//! ```toml
//! [mcp.exa]
//! url = "https://mcp.exa.ai/mcp"
//! env_headers = { x-api-key = "EXA_API_KEY" }   # optional: without a key, Exa's free rate limit
//! eager = true                                  # offer its tools directly, rather than through find_tools
//!
//! [mcp.fs]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
//! ```
//!
//! Servers start while rho connects. One that does not start is reported and left out. Their tools ask before
//! each call unless a rule under `[rules]` allows them (`mcp__exa__*`).

use std::sync::Arc;

use anyhow::Result;
use manifatura::Tool;
use manifatura::mcp::{McpServer, ServerConfig};

/// A server that started, and its tools.
pub(crate) struct Started {
    pub(crate) name: String,
    pub(crate) server: Arc<McpServer>,
    pub(crate) tools: Vec<Tool>,
}

/// Starts every server at once. Each comes back started, or with what went wrong.
pub(crate) async fn start(servers: Vec<(String, ServerConfig)>) -> Vec<Result<Started, String>> {
    let starting = servers.into_iter().map(|(name, config)| async move {
        let server = McpServer::start(name.clone(), config).await?;
        let tools = server.tools().await?;
        Ok(Started {
            name,
            server,
            tools,
        })
    });
    futures_util::future::join_all(starting).await
}
