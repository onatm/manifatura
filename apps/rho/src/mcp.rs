//! MCP servers for the chat, from `mcp.toml` in rho's config directory. A server is a URL (streamable HTTP)
//! or a program (stdio):
//!
//! ```toml
//! [servers.exa]
//! url = "https://mcp.exa.ai/mcp"
//! env_headers = { x-api-key = "EXA_API_KEY" }   # optional: without a key, Exa's free rate limit
//! eager = true                                  # offer its tools directly, rather than through find_tools
//!
//! [servers.fs]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
//! ```
//!
//! Servers start while rho connects. One that does not start is reported and left out. Their tools ask before
//! each call unless a rule in `rules.toml` allows them (`mcp__exa__*`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use manifatura::Tool;
use manifatura::mcp::{McpServer, ServerConfig};

/// `mcp.toml`: one table per server.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    servers: BTreeMap<String, ServerConfig>,
}

/// The servers configured in `path`; none when it does not exist.
pub(crate) fn configured(path: &Path) -> Result<Vec<(String, ServerConfig)>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let file: File =
        toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    Ok(file.servers.into_iter().collect())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_read_from_their_tables() {
        let dir = std::env::temp_dir().join(format!("rho-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.toml");
        std::fs::write(
            &path,
            "[servers.exa]\nurl = \"https://mcp.exa.ai/mcp\"\nenv_headers = { x-api-key = \"EXA_API_KEY\" }\neager = true\n\n\
             [servers.fs]\ncommand = \"npx\"\nargs = [\"-y\", \"server-filesystem\"]\n",
        )
        .unwrap();
        let servers = configured(&path).unwrap();
        assert_eq!(servers.len(), 2);
        let (name, config) = &servers[0];
        assert_eq!(
            (name.as_str(), config.url.as_deref(), config.eager),
            ("exa", Some("https://mcp.exa.ai/mcp"), true)
        );
        assert_eq!(config.env_headers["x-api-key"], "EXA_API_KEY");
        let (name, config) = &servers[1];
        assert_eq!(
            (name.as_str(), config.command.as_deref(), config.args.len()),
            ("fs", Some("npx"), 2)
        );
        assert!(configured(&dir.join("missing.toml")).unwrap().is_empty());
        std::fs::write(&path, "[servers.x]\ncommnd = \"typo\"\n").unwrap();
        assert!(
            configured(&path).is_err(),
            "a misspelt key is an error, not a server without a command"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
