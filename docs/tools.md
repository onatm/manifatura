# Tools

No tool is built in to an agent: it has the tools it is given, its own or the basic ones in `manifatura::tools`, and a `Step` hook can change them before any model call.

## Your own tools

`Tool::new(name, description)` starts a tool; `run` finishes it with a typed closure:

```rust
use manifatura::{Json, Tool, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, JsonSchema)]
struct Lookup {
    /// The order id, e.g. "A-1001".
    order_id: String,
}

#[derive(Serialize)]
struct Order {
    id: String,
    status: String,
}

let lookup = Tool::new("lookup_order", "Look up one order by id.")
    .guideline("Look orders up with `lookup_order`; never guess their status.")
    .run(|i: Lookup, _cx| async move {
        if i.order_id != "A-1001" {
            return Err(ToolError::msg(format!("no order {}", i.order_id)));
        }
        Ok(Json(Order { id: i.order_id, status: "delivered".into() }))
    });
```

- The input schema comes from `JsonSchema`, with subschemas inlined, which small models handle better than references. Doc comments on fields become their descriptions.
- The input is deserialised before the closure runs. A mismatch goes back to the model as an error it can fix.
- The closure returns `Result<T, ToolError>`, where `T` is a `String`, `&str`, `()`, a `serde_json::Value`, `Json(anything Serialize)` or a `ToolResult`. `?` works on any error type.
- `run_json(schema, closure)` takes a hand-written JSON Schema and a `serde_json::Value` input instead.

A finished `Tool` is a cheap handle: clone it into several agents.

### Text tools

Some input is text, not JSON: a patch, a script, a query. `run_text` makes a tool whose input the model writes as free text, and `grammar` can constrain it:

```rust
use manifatura::{Grammar, Tool};

let sql = Tool::new("query", "Run one read-only SQL query.")
    .grammar(Grammar::lark(r#"start: "SELECT " /[^;]+/"#))
    .run_text(|sql: String, _cx| async move { run_query(&sql).await });
```

OpenAI's Responses API offers it as a free-form tool: the model writes the text raw, and follows the grammar while it writes. Other providers offer an object with one string field, `input`, so the tool works with any model. Either way, hooks see the call as `{"input": "…"}`, and the closure gets the text. The grammar shapes what the model writes but is not checked again: a tool still checks its input. `Grammar::regex` takes a regular expression instead of Lark.

### A tool from a program

`ScriptTool` makes a tool of any executable and a JSON Schema. The input arrives on the program's stdin as JSON, its stdout is the result, and a non-zero exit is an error that includes stderr. It runs through the env, like `bash`:

```rust
use manifatura::tools::ScriptTool;
use serde_json::json;

let weather = ScriptTool::new(
    "weather",
    "The weather in a city now.",
    json!({ "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] }),
    vec!["python3".into(), "tools/weather.py".into()],
)
.timeout(std::time::Duration::from_secs(30))
.build();
```

### What a tool can see

The closure's second argument is a `ToolCx`:

| Method | What it gives |
| --- | --- |
| `env()` | The environment to act in (below). |
| `state(key)`, `set_state(key, value)`, `clear_state(key)` | The session's state. |
| `log(text)` | A line shown live as `action.log`, not sent to the model. |
| `abort()` | Fires when the turn is aborted: long work should stop. |
| `call_id()`, `session_id()`, `runtime()` | Where the call comes from. |

A tool that needs a person (a confirmation, a choice) gets a handle to the app's UI in its closure, as any Rust closure would.

### Flags

| Builder | Meaning |
| --- | --- |
| `.sequential()` | Never runs alongside other calls of the same batch. Calls run in parallel otherwise. |
| `.deferred()` | Not offered to the model, but runs when a call names it (see the catalog below). |
| `.keep_results()` | Its results stay relevant for the rest of the task, so `kit::compaction::trim_results` leaves them whole (the `skill` tool of [skills](skills.md) does this). |
| `.guideline(text)` | A line for the system prompt on when and how to use the tool (see [system prompts](system-prompts.md)). |

A tool returning `ToolResult::ok(text).terminate()` ends the turn after its batch, without another model call.

## The basic tools

`manifatura::tools` has these tools, enough for a coding or operations agent:

| Tool | What it does |
| --- | --- |
| `read` | A text file, line-numbered, a page at a time. |
| `edit` | Exact text replacements in a file, or a new file from one edit with an empty `old_text`. |
| `grep` | A regular expression across files, respecting `.gitignore`. |
| `glob` | Files by pattern, newest first, respecting `.gitignore`. |
| `apply_patch` | Adds, updates, moves and deletes files with one patch, in the format of OpenAI's Codex, which OpenAI's models are trained to write. Not in `tools::all()`: give it in place of `edit`. |
| `bash` | A command in the workspace: builds, tests, git. One that runs longer than its wait (30 seconds by default) is left in the background, and its output goes to files the model can read. |

`edit` refuses to change a file that changed since the model read it. `apply_patch` finds its place by the patch's context lines instead (exactly, then ignoring whitespace and typographic punctuation), checks the whole patch before writing any file, and writes nothing if any part fails. Long output keeps its most useful part (the end of a command's output, the first matches of a search) and says where the rest is.

```rust
use manifatura::tools;

let agent = Agent::new("You are a coding agent.").tools(tools::all()); // read, edit, grep, glob, bash

let agent = Agent::new("You are a coding agent.") // for OpenAI's models
    .tools(tools::by_name(&["read", "apply_patch", "grep", "glob", "bash"])?);

let agent = Agent::new("You review code; you change nothing.")
    .tools(tools::by_name(&["read", "grep", "glob"])?);
```

## Environments

Tools act through an `Env`: run a command, read, write, list and remove files. The runtime has one (`RuntimeBuilder::env`), and an agent can have its own (`Agent::env`).

`tools::Local` is this machine, with no isolation: commands run as the current user, in the workspace directory. They get only an allowlisted environment (`PATH`, `HOME`, `USER`, `LANG`, `LC_ALL`, `TERM`, and `TMPDIR` set to a private scratch directory), never the process's own secrets unless named:

```rust
let env = tools::Local::new("/srv/checkouts/shop")?
    .pass_env("GH_TOKEN")               // from this process's environment
    .set_env("CI", "true");
let rt = Runtime::builder().provider(providers::from_env()?).env(env).build();
```

A sandbox, a container or a remote machine is another `Env` implementation; the tools do not change. Paths the model gives resolve against the env's working directory, and `Local` does not stop them leaving it: isolation belongs to the env.

## MCP servers

With the `mcp` feature (the `manifatura-mcp` crate), an MCP server's tools become manifatura tools named `<server>__<tool>`. A server is reached at a URL, over streamable HTTP, or run as a program, over stdio:

```rust
use manifatura::mcp::{McpServer, ServerConfig};

let exa = McpServer::start(
    "exa",
    ServerConfig::http("https://mcp.exa.ai/mcp").env_header("x-api-key", "EXA_API_KEY"),
)
.await?;
let fs = McpServer::start(
    "fs",
    ServerConfig::stdio("npx").args(["-y", "@modelcontextprotocol/server-filesystem", "."]),
)
.await?;
let agent = Agent::new("You research.")
    .tools(exa.tools().await?)
    .tools(fs.tools().await?)
    .hooks(kit::catalog());
```

`ServerConfig` is `Deserialize`, so it can come from an app's config file:

```toml
[servers.exa]
url = "https://mcp.exa.ai/mcp"
env_headers = { x-api-key = "EXA_API_KEY" }   # the header's value comes from the environment
eager = true

[servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
```

An HTTP server takes `headers` and `env_headers`; a program takes `args`, `env` and `cwd`. Both take a `timeout` in seconds, `eager` and `approve`. Programs run as the user, outside any `Env`: they are programs you chose to run. A program stops, and an HTTP session ends, when the last handle to its server is dropped. The protocol is that of [`rmcp`](https://docs.rs/rmcp), the official Rust SDK, and `McpServer::peer` reaches the rest of it: resources, prompts.

## Deferred tools and the catalog

An MCP server can bring dozens of tools, and every tool offered costs prompt tokens on every call. MCP tools are therefore deferred unless their server is `eager`: kept out of the prompt, but runnable.

`kit::catalog()` makes them reachable. While a step has deferred tools, it offers two more:

- `find_tools`, which searches the deferred tools by keywords (BM25 over names, descriptions and input fields) and returns the best matches with their schemas;
- `run_tool`, which runs one by name.

A `run_tool` call is turned into a call of the real tool before later hooks see it, so approvals and events name the real tool. Add `kit::catalog()` before hooks that look at tool names. Any tool can be deferred with `.deferred()` or `tool.with_deferred(true)`.
