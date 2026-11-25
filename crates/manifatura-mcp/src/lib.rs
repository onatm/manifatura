//! MCP servers' tools for manifatura. A server runs as a local program over stdio, or is
//! reached at a URL over streamable HTTP. Its tools become deferred tools named
//! `<server>__<tool>`, kept out of the prompt: add `kit::catalog()` so the model can find them
//! (`find_tools`) and run them (`run_tool`), or set `eager` to offer a server's tools directly.
//!
//! ```no_run
//! # async fn demo() -> Result<(), String> {
//! use manifatura_mcp::{McpServer, ServerConfig};
//!
//! // A hosted server, with its key from the environment.
//! let exa = McpServer::start("exa", ServerConfig::http("https://mcp.exa.ai/mcp")
//!     .env_header("x-api-key", "EXA_API_KEY")).await?;
//! // A local one.
//! let fs = McpServer::start("fs", ServerConfig::stdio("npx")
//!     .args(["-y", "@modelcontextprotocol/server-filesystem", "."])).await?;
//! let tools = [exa.tools().await?, fs.tools().await?].concat();
//! # Ok(()) }
//! ```
//!
//! The protocol is [`rmcp`]'s, the official Rust SDK. Local servers run as the user, outside
//! any [`Env`](manifatura_agent::Env) sandbox: they are programs the user configured. A
//! server's process stops, or its HTTP session ends, when the last handle is dropped.

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use manifatura_agent::{Tool, ToolCx, ToolHandler, ToolResult, ToolSpec, async_trait};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::process::Command;

pub use rmcp;

const START_TIMEOUT: Duration = Duration::from_secs(30);
const STDERR_LINES: usize = 20;

/// How to reach a server: a `command` to run, or a `url`. It is `Deserialize`, so it can come
/// from an app's config file:
///
/// ```toml
/// [servers.exa]
/// url = "https://mcp.exa.ai/mcp"
/// env_headers = { x-api-key = "EXA_API_KEY" }
///
/// [servers.fs]
/// command = "npx"
/// args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The program to run, found on `PATH` unless it is a path. Spoken to over stdio.
    pub command: Option<String>,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Added to the environment the program inherits.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The directory the program runs in; the app's own when `None`.
    pub cwd: Option<PathBuf>,
    /// The server's streamable HTTP endpoint, instead of a `command`.
    pub url: Option<String>,
    /// Headers sent with every HTTP request.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Headers sent with every HTTP request, each set to the value of the named environment
    /// variable, so keys stay out of config files.
    #[serde(default)]
    pub env_headers: BTreeMap<String, String>,
    /// Every tool of this server should ask before each call. Read by the app, which adds
    /// the approval hook ([`McpServer::config`]); the tools themselves carry no policy.
    #[serde(default)]
    pub approve: bool,
    /// Offer the tools in the prompt instead of deferring them to the catalog.
    #[serde(default)]
    pub eager: bool,
    /// Seconds a call may take (default 120).
    pub timeout: Option<u64>,
}

impl ServerConfig {
    /// A server run with `command`, over stdio.
    pub fn stdio(command: impl Into<String>) -> Self {
        Self {
            command: Some(command.into()),
            ..Self::default()
        }
    }

    /// A server at `url`, over streamable HTTP.
    pub fn http(url: impl Into<String>) -> Self {
        Self {
            url: Some(url.into()),
            ..Self::default()
        }
    }

    /// Sets the program's arguments, replacing any.
    pub fn args<S: Into<String>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Adds a header to every HTTP request.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Adds a header set to the value of the environment variable `var`.
    pub fn env_header(mut self, name: impl Into<String>, var: impl Into<String>) -> Self {
        self.env_headers.insert(name.into(), var.into());
        self
    }

    /// Offers the tools in the prompt instead of deferring them.
    pub fn eager(mut self, eager: bool) -> Self {
        self.eager = eager;
        self
    }
}

/// A connected server.
pub struct McpServer {
    name: String,
    config: ServerConfig,
    client: RunningService<RoleClient, ClientConfig>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("name", &self.name)
            .field("command", &self.config.command)
            .field("url", &self.config.url)
            .finish_non_exhaustive()
    }
}

fn client_config() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("manifatura", env!("CARGO_PKG_VERSION")),
    )
}

fn http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .build()
                .expect("HTTP client")
        })
        .clone()
}

impl McpServer {
    /// Starts the server, or connects to its URL, and completes the `initialize` handshake.
    pub async fn start(name: impl Into<String>, config: ServerConfig) -> Result<Arc<Self>, String> {
        let name = name.into();
        let stderr: Arc<Mutex<Vec<String>>> = Arc::default();
        let client = match (&config.command, &config.url) {
            (Some(command), None) => {
                let mut cmd = Command::new(command);
                cmd.args(&config.args).envs(&config.env);
                if let Some(d) = &config.cwd {
                    cmd.current_dir(d);
                }
                let (proc, pipe) = TokioChildProcess::builder(cmd)
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("mcp {name}: cannot start `{command}`: {e}"))?;
                if let Some(pipe) = pipe {
                    let lines = stderr.clone();
                    tokio::spawn(async move {
                        let mut pipe = BufReader::new(pipe).lines();
                        while let Ok(Some(l)) = pipe.next_line().await {
                            let mut lines = lines.lock().unwrap();
                            if lines.len() == STDERR_LINES {
                                lines.remove(0);
                            }
                            lines.push(l);
                        }
                    });
                }
                handshake(&name, client_config().serve(proc), &stderr).await?
            }
            (None, Some(url)) => {
                let headers = headers(&name, &config)?;
                let transport = StreamableHttpClientTransport::with_client(
                    http_client(),
                    StreamableHttpClientTransportConfig::with_uri(url.as_str())
                        .custom_headers(headers),
                );
                handshake(&name, client_config().serve(transport), &stderr).await?
            }
            (Some(_), Some(_)) => {
                return Err(format!("mcp {name}: give a command or a url, not both"));
            }
            (None, None) => return Err(format!("mcp {name}: give a command or a url")),
        };
        Ok(Arc::new(Self {
            name,
            config,
            client,
            stderr,
        }))
    }

    /// Speaks to a server over any byte stream (a socket, or a pipe in tests).
    pub async fn connect(
        name: impl Into<String>,
        config: ServerConfig,
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Result<Arc<Self>, String> {
        let name = name.into();
        let stderr = Arc::default();
        let client = handshake(&name, client_config().serve((reader, writer)), &stderr).await?;
        Ok(Arc::new(Self {
            name,
            config,
            client,
            stderr,
        }))
    }

    /// How the server was configured.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// The name it was started under, which prefixes its tools' names.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `serverInfo` from the handshake (`name`, `version`).
    pub fn info(&self) -> Value {
        self.client
            .peer_info()
            .and_then(|i| serde_json::to_value(&i.server_info).ok())
            .unwrap_or(Value::Null)
    }

    /// The server's session, for requests beyond tools: resources, prompts and the rest.
    pub fn peer(&self) -> &rmcp::Peer<RoleClient> {
        self.client.peer()
    }

    fn fail(&self, what: &str) -> String {
        fail(&self.name, what, &self.stderr)
    }

    /// The server's tools, as deferred `<server>__<tool>` tools (unless `eager`).
    pub async fn tools(self: &Arc<Self>) -> Result<Vec<Tool>, String> {
        let listed = self
            .client
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| self.fail(&format!("tools/list: {e}")))?;
        Ok(listed
            .into_iter()
            .map(|t| {
                let mut description = t.description.as_deref().unwrap_or_default().to_string();
                if description.len() > 1000 {
                    description.truncate(description.floor_char_boundary(1000));
                    description.push('…');
                }
                Tool::from_handler(
                    ToolSpec {
                        name: tool_name(&self.name, &t.name),
                        description,
                        input_schema: Value::Object((*t.input_schema).clone()),
                    },
                    Remote {
                        server: self.clone(),
                        tool: t.name.to_string(),
                    },
                )
                .with_deferred(!self.config.eager)
            })
            .collect())
    }

    async fn call(&self, tool: &str, input: Value, cx: &ToolCx) -> ToolResult {
        let mut params = CallToolRequestParams::new(tool.to_string());
        params.arguments = match input {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return ToolResult::error(format!("{tool}: input must be an object, not {other}"));
            }
        };
        let timeout = Duration::from_secs(self.config.timeout.unwrap_or(120));
        // Dropping the call (on abort or timeout) sends `notifications/cancelled`.
        let r = tokio::select! {
            r = self.client.peer().call_tool(params) => r.map_err(|e| e.to_string()),
            _ = cx.abort().aborted() => Err("aborted".into()),
            _ = tokio::time::sleep(timeout) => Err(format!("no answer within {} s", timeout.as_secs())),
        };
        match r.and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string())) {
            Ok(result) => {
                let text = content_text(&result);
                if result["isError"].as_bool() == Some(true) {
                    ToolResult::error(text)
                } else {
                    ToolResult::ok(text)
                }
            }
            Err(e) => ToolResult::error(self.fail(&format!("{tool}: {e}"))),
        }
    }
}

async fn handshake<E: std::fmt::Display>(
    name: &str,
    serve: impl Future<Output = Result<RunningService<RoleClient, ClientConfig>, E>>,
    stderr: &Mutex<Vec<String>>,
) -> Result<RunningService<RoleClient, ClientConfig>, String> {
    match tokio::time::timeout(START_TIMEOUT, serve).await {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(e)) => Err(fail(name, &format!("initialize: {e}"), stderr)),
        Err(_) => Err(fail(name, "did not answer initialize within 30 s", stderr)),
    }
}

fn headers(
    name: &str,
    config: &ServerConfig,
) -> Result<HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>, String> {
    let from_env = config.env_headers.iter().map(|(h, var)| {
        std::env::var(var)
            .map(|v| (h.clone(), v))
            .map_err(|_| format!("mcp {name}: environment variable {var} is not set"))
    });
    let literal = config
        .headers
        .iter()
        .map(|(h, v)| Ok((h.clone(), v.clone())));
    literal
        .chain(from_env)
        .map(|kv| {
            let (h, v) = kv?;
            let bad = |e: &dyn std::fmt::Display| format!("mcp {name}: header {h}: {e}");
            Ok((
                reqwest::header::HeaderName::try_from(h.as_str()).map_err(|e| bad(&e))?,
                reqwest::header::HeaderValue::try_from(v.as_str()).map_err(|e| bad(&e))?,
            ))
        })
        .collect()
}

fn fail(name: &str, what: &str, stderr: &Mutex<Vec<String>>) -> String {
    let stderr = stderr.lock().unwrap();
    let tail = stderr
        .iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    if tail.is_empty() {
        format!("mcp {name}: {what}")
    } else {
        format!("mcp {name}: {what} (stderr: {tail})")
    }
}

/// `<server>__<tool>`, keeping to the characters model APIs accept in tool names.
fn tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut name = format!("{}__{}", clean(server), clean(tool));
    name.truncate(64);
    name
}

/// The text of a `tools/call` result: text parts, structured content, and markers for the rest.
fn content_text(result: &Value) -> String {
    let mut parts = Vec::new();
    for c in result["content"].as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("text") => parts.push(c["text"].as_str().unwrap_or_default().to_string()),
            Some("resource") => {
                let r = &c["resource"];
                match r["text"].as_str() {
                    Some(t) => parts.push(t.to_string()),
                    None => parts.push(format!("[resource {}]", r["uri"].as_str().unwrap_or("?"))),
                }
            }
            Some("resource_link") => {
                parts.push(format!("[resource {}]", c["uri"].as_str().unwrap_or("?")))
            }
            Some(other) => parts.push(format!(
                "[{other}{}]",
                c["mimeType"]
                    .as_str()
                    .map(|m| format!(" {m}"))
                    .unwrap_or_default()
            )),
            None => {}
        }
    }
    if parts.is_empty()
        && let Some(s) = result.get("structuredContent")
    {
        return s.to_string();
    }
    if parts.is_empty() {
        return "ok".into();
    }
    parts.join("\n")
}

struct Remote {
    server: Arc<McpServer>,
    tool: String,
}

#[async_trait]
impl ToolHandler for Remote {
    async fn call(&self, input: Value, cx: ToolCx) -> ToolResult {
        self.server.call(&self.tool, input, &cx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_and_content() {
        assert_eq!(tool_name("git hub", "list.repos"), "git_hub__list_repos");
        let r = json!({ "content": [
            { "type": "text", "text": "one" },
            { "type": "image", "mimeType": "image/png", "data": "…" },
            { "type": "resource", "resource": { "uri": "file:///a", "text": "two" } },
        ]});
        assert_eq!(content_text(&r), "one\n[image image/png]\ntwo");
        assert_eq!(
            content_text(&json!({ "content": [], "structuredContent": { "n": 1 } })),
            "{\"n\":1}"
        );
    }

    #[test]
    fn config_from_toml_shapes() {
        let c: ServerConfig = serde_json::from_value(json!({
            "url": "https://mcp.exa.ai/mcp", "env_headers": { "x-api-key": "EXA_API_KEY" }, "eager": true
        }))
        .unwrap();
        assert_eq!(
            c,
            ServerConfig::http("https://mcp.exa.ai/mcp")
                .env_header("x-api-key", "EXA_API_KEY")
                .eager(true)
        );
        let c: ServerConfig =
            serde_json::from_value(json!({ "command": "npx", "args": ["-y", "x"] })).unwrap();
        assert_eq!(c, ServerConfig::stdio("npx").args(["-y", "x"]));
    }

    #[tokio::test]
    async fn needs_one_transport() {
        let e = McpServer::start("x", ServerConfig::default())
            .await
            .unwrap_err();
        assert_eq!(e, "mcp x: give a command or a url");
        let e = McpServer::start(
            "x",
            ServerConfig::http("http://localhost:1").env_header("k", "MANIFATURA_TEST_UNSET"),
        )
        .await
        .unwrap_err();
        assert_eq!(
            e,
            "mcp x: environment variable MANIFATURA_TEST_UNSET is not set"
        );
    }

    /// `cargo test -p manifatura-mcp -- --ignored`: Exa's hosted server, keyless.
    #[tokio::test]
    #[ignore = "network"]
    async fn exa_over_http() {
        let s = McpServer::start("exa", ServerConfig::http("https://mcp.exa.ai/mcp"))
            .await
            .unwrap();
        let tools = s.tools().await.unwrap();
        let search = tools
            .iter()
            .find(|t| t.name() == "exa__web_search_exa")
            .expect("search tool");
        let r = search
            .call(
                json!({ "query": "Rust programming language", "numResults": 1 }),
                ToolCx::detached("c"),
            )
            .await;
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.to_lowercase().contains("rust"));
    }
}

#[cfg(test)]
mod protocol {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    /// A server with two tools, `echo` and `fail`, and one page cursor.
    async fn serve(
        reader: impl AsyncRead + Unpin,
        mut writer: impl AsyncWrite + Unpin,
        log: Arc<Mutex<Vec<Value>>>,
    ) {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: Value = serde_json::from_str(&line).unwrap();
            log.lock().unwrap().push(msg.clone());
            let id = msg["id"].clone();
            let result = match msg["method"].as_str().unwrap_or_default() {
                "initialize" => {
                    json!({ "protocolVersion": msg["params"]["protocolVersion"], "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mock", "version": "1" } })
                }
                "tools/list" if msg["params"]["cursor"].is_null() => json!({
                    "tools": [{ "name": "echo", "description": "Echo text",
                        "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } } }],
                    "nextCursor": "p2" }),
                "tools/list" => json!({ "tools": [{ "name": "fail", "description": "Always fails",
                    "inputSchema": { "type": "object" } }] }),
                "tools/call" if msg["params"]["name"] == "echo" => json!({
                    "content": [{ "type": "text", "text": msg["params"]["arguments"]["text"] }] }),
                "tools/call" => {
                    json!({ "content": [{ "type": "text", "text": "nope" }], "isError": true })
                }
                _ => continue,
            };
            let mut out =
                serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                    .unwrap();
            out.push(b'\n');
            writer.write_all(&out).await.unwrap();
        }
    }

    #[tokio::test]
    async fn lists_and_calls_tools() {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (sr, sw) = tokio::io::split(server);
        let log: Arc<Mutex<Vec<Value>>> = Arc::default();
        tokio::spawn(serve(sr, sw, log.clone()));
        let (cr, cw) = tokio::io::split(client);
        let s = McpServer::connect("demo", ServerConfig::default(), cr, cw)
            .await
            .unwrap();
        assert_eq!(s.info()["name"], "mock");
        let tools = s.tools().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["demo__echo", "demo__fail"]);
        assert!(tools.iter().all(|t| t.is_deferred()));
        let r = tools[0]
            .call(json!({ "text": "hi" }), ToolCx::detached("c1"))
            .await;
        assert_eq!((r.output.as_str(), r.is_error), ("hi", false));
        let r = tools[1].call(json!({}), ToolCx::detached("c2")).await;
        assert_eq!((r.output.as_str(), r.is_error), ("nope", true));
        let methods: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|m| m["method"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/list",
                "tools/call",
                "tools/call"
            ]
        );
    }
}
