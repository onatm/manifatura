# manifatura

[![CI](https://github.com/onatm/manifatura/actions/workflows/ci.yml/badge.svg)](https://github.com/onatm/manifatura/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/manifatura.svg)](https://crates.io/crates/manifatura)
[![docs.rs](https://img.shields.io/docsrs/manifatura)](https://docs.rs/manifatura)

manifatura is a small Rust library for building agents into your own programs: a loop, typed hooks, and sessions kept as a tree. You bring the model, the UI and the policy; manifatura runs the loop between them and stays out of the way.

The loop decides nothing a hook could decide. Approvals, compaction, tool discovery and input repair are hooks you add, and the ones in `kit` are plain functions you can read, copy or replace. Agents are Rust values built in your `main.rs`: there is no directory format, no server and no CLI of its own.

```rust
use manifatura::{Agent, Event, Runtime, SessionId, ToolCall, kit, providers, tools};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = Agent::new("You are a careful coding assistant.")
        .tools(tools::all()) // read, edit, grep, glob, bash
        .hooks(kit::approve(["edit", "bash"], ask));
    let rt = Runtime::builder()
        .provider(providers::from_env()?) // e.g. MANIFATURA_MODEL=llamacpp/qwen3.5-9b
        .env(tools::Local::new(".")?)
        .build();

    let session = rt.session(&agent, SessionId::new());
    let mut turn = session.prompt("Why does the build fail?").await?;
    while let Some(event) = turn.next_event().await {
        if let Event::TextDelta { text, .. } = event {
            print!("{text}");
        }
    }
    turn.wait().await?;
    Ok(())
}

/// Your UI: a terminal prompt, a button in a chat, a policy.
async fn ask(call: ToolCall) -> kit::Answer {
    println!("\nallowing {} {}", call.name, call.input);
    kit::Answer::Allow
}
```

## Features

- **Typed hooks.** Fifteen points in the loop, each an event type with its own return value: `agent.on(|e: hook::ToolCall, cx| async { … })`. A hook can rewrite a call, deny it, answer it itself, change the tools for the next model call, or keep the turn going.
- **Sessions as a tree.** Every message, state change and compaction is an entry with a parent, appended to a store. Resume a session, branch from any entry, or read the file with `jq`.
- **Compaction when you ask.** Nothing summarises by itself. Call `Session::compact`, or add `kit::compaction::on_overflow` to compact when the model reports a full context, or `kit::compaction::auto` past a share of the window.
- **Any model.** `providers` speaks to llama.cpp's server, and to any other OpenAI-compatible server you register. Any other model is a `Provider`: one function that streams a reply, which is enough to run a model in your own process.
- **Tools.** Your own tools from a closure and a `JsonSchema` input, or from a program; the basic `read`, `edit`, `grep`, `glob` and `bash`; MCP servers' tools, over stdio or HTTP. Tools act through an `Env`, so the same tools run locally, in a sandbox or on another machine.
- **Deferred tools.** Tools marked deferred stay out of the prompt, and `kit::catalog` lets the model search for them and run them. Useful with MCP servers that bring dozens of tools.
- **`AGENTS.md` context.** `kit::context` finds `AGENTS.md` (or `CLAUDE.md`) files from the repository root down, and builds a system prompt from them and the tools' own guidelines.
- **Skills.** `skills` loads [Agent Skills](https://agentskills.io) from `.agents/skills`, your own directories or the binary itself. The prompt lists them, the model loads one with the `skill` tool when a task calls for it, and your UI can start one with `/name`.
- **Subagents.** `subagents` gives the model a `task` tool that hands a self-contained task to another agent, or to a copy of itself, in a child session, and returns only its report. A subagent can be limited to some tools and a number of steps, keeping its last step for the report; it can be written as Markdown, and your app can start one itself, as with a `/explore` command. Approvals are shared with the caller, and subagents nest as deep as you allow.
- **Small.** About 11,000 lines across nine crates, a core that depends on `serde`, `tokio` and little else, and a facade that puts each block behind a feature.

## Install

```sh
cargo add manifatura
cargo add tokio --features rt-multi-thread,macros
```

manifatura needs Rust 1.91 or newer. `Local` and the `bash` tool run on Unix: Linux and macOS.

| Feature | Default | What it adds |
| --- | --- | --- |
| `kit` | yes | `manifatura::kit`: approvals, compaction, the catalog, input repair, `AGENTS.md` context |
| `providers-openai` | yes | `manifatura::providers`: llama.cpp and other OpenAI-compatible servers |
| `providers-responses` | no | `manifatura::providers::responses`: OpenAI's Responses API, with pluggable credentials |
| `auth` | no | `manifatura::auth`: signing in to a model vendor, such as OpenAI's Sign in with ChatGPT |
| `skills` | yes | `manifatura::skills`: Agent Skills, a catalog for the prompt and the `skill` tool |
| `store-jsonl` | yes | `manifatura::store`: sessions as JSON lines |
| `tools` | yes | `manifatura::tools`: `read`, `edit`, `apply_patch`, `grep`, `glob`, `bash` and `Local` |
| `subagents` | no | `manifatura::subagents`: the `task` tool and child sessions |
| `snapshots` | no | `manifatura::snapshots`: the files as each turn found them, to restore later |
| `policy` | no | `manifatura::policy`: allow, ask and deny rules for tools and shell command prefixes |
| `sandbox` | no | `manifatura::sandbox`: an env whose commands run in a sandbox (macOS Seatbelt, Linux bubblewrap), writing only to the workspace, without the network |
| `exec` | no | `manifatura::exec`: `exec_command` and `write_stdin`, commands that keep running, and terminals for `Local` |
| `mcp` | no | `manifatura::mcp`: MCP servers' tools, over stdio or HTTP |
| `testing` | no | `manifatura::testing`: a scripted provider for tests |

`manifatura::prelude` brings in the names most programs use, and follows the features: with `skills`, `SystemPrompt` gains `skills(&set)`.

## Quick start

The examples talk to [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server`, at `LLAMACPP_URL` (default `http://localhost:8080`):

```sh
llama-server -m qwen3.5-9b.gguf -c 32768 --jinja &
git clone https://github.com/onatm/manifatura && cd manifatura
export MANIFATURA_MODEL=llamacpp/qwen3.5-9b
cargo run --example hello
cargo run --example typed-tool     # a typed tool
cargo run --example dynamic-agent  # a Step hook adds a tool
cargo run --example hooks          # approvals, input, a context meter
```

`MANIFATURA_MODEL` is `provider/model-id`. Any other OpenAI-compatible server registers under a name of its own with `Models::with` or `Models::configure`.

## How it works

An **agent** is instructions, tools and hooks. A **runtime** holds the provider, the store, the env and hooks for every session. A **session** is one conversation; each prompt starts a **turn**, which streams events while it runs:

```text
prompt(text)
  Input            → the text, changed, or dropped
  TurnStart
  loop:
    Step           → instructions and tools for this model call
    Context        → the messages sent (not persisted)
    Request        → the final request
    the model streams its reply
    ModelError     → Retry | Fail
    Response       → the reply, then recorded
    StepEnd        (usage, context size)
    no tool calls  → leave the loop
    ToolCall       → Run | Deny | Result, for each call
    the calls run, in parallel unless a tool is sequential
    ToolResult     → each result, then recorded
  TurnEnd          → Done | FollowUp
```

Hooks registered on the runtime run before the agent's own, each in the order added. A bundle of behaviour is a function that returns `Hooks`, and `kit` is a set of them. The [hooks guide](docs/hooks.md) has the full table and recipes.

| Crate | What it is |
| --- | --- |
| [`manifatura`](https://docs.rs/manifatura) | The facade: the core, with each block behind a feature |
| [`manifatura-agent`](https://docs.rs/manifatura-agent) | The loop, typed hooks, sessions, and the `Provider`, `Env` and `Store` traits |
| [`manifatura-kit`](https://docs.rs/manifatura-kit) | Hook bundles and `AGENTS.md` context |
| [`manifatura-skills`](https://docs.rs/manifatura-skills) | Agent Skills: discovery, the prompt catalog and the `skill` tool |
| [`manifatura-subagents`](https://docs.rs/manifatura-subagents) | Subagents: the `task` tool and child sessions |
| [`manifatura-providers`](https://docs.rs/manifatura-providers) | llama.cpp and other OpenAI-compatible providers, and OpenAI's Responses API, streamed over SSE |
| [`manifatura-auth`](https://docs.rs/manifatura-auth) | Signing in to a model vendor: OpenAI's Sign in with ChatGPT |
| [`manifatura-store`](https://docs.rs/manifatura-store) | Sessions as JSON lines |
| [`manifatura-exec`](https://docs.rs/manifatura-exec) | `exec_command` and `write_stdin`: commands that keep running in the background |
| [`manifatura-snapshots`](https://docs.rs/manifatura-snapshots) | Workspace snapshots per turn, in a git repository of their own |
| [`manifatura-policy`](https://docs.rs/manifatura-policy) | Approval rules for tools and shell commands, split with tree-sitter-bash |
| [`manifatura-sandbox`](https://docs.rs/manifatura-sandbox) | A sandboxed env: commands under macOS Seatbelt or Linux bubblewrap |
| [`manifatura-tools`](https://docs.rs/manifatura-tools) | `read`, `edit`, `apply_patch`, `grep`, `glob`, `bash`, script tools and `Local` |
| [`manifatura-mcp`](https://docs.rs/manifatura-mcp) | MCP servers' tools, over stdio or streamable HTTP |

## Documentation

The guides are on [manifatura.dev](https://manifatura.dev), and in [`docs/`](docs):

- [Getting started](docs/getting-started.md)
- [Agents and sessions](docs/sessions.md): turns, events, steering, resuming, branching, compaction
- [Tools](docs/tools.md): your own, the basic ones, commands that keep running, MCP servers, deferred tools, environments
- [Hooks](docs/hooks.md): every point in the loop, and recipes
- [Approvals](docs/approvals.md): pending requests any client answers, and rules for which calls ask (`policy` feature)
- [Models](docs/models.md): providers, and writing your own
- [System prompts](docs/system-prompts.md): `AGENTS.md` files, tool guidelines, keeping the prompt cache warm
- [Skills](docs/skills.md): Agent Skills from `.agents/skills` and your own, loaded when a task calls for them (`skills` feature)
- [Subagents](docs/subagents.md): handing a task to another agent in a child session (`subagents` feature)
- [API reference](https://docs.rs/manifatura) on docs.rs

## Used by

- [rustling](https://github.com/onatm/rustling): an inference engine for Apple silicon whose `chat` command is a manifatura agent, with the model running in the same process through its own `Provider`, skills built into the binary or kept in a project's `.rustling/skills`, and an `explore` subagent that the model or `/explore` starts to search the code.

## Status

manifatura is young: 0.x releases may change the API. It has no durability: a session survives a restart, but a turn that was running when the process died does not, and tool calls it left without results are marked as interrupted. The core, kit and store reach files, processes and the clock only through `Env`, `Store` and `Provider`, so they can be hosted in WebAssembly later.

## Acknowledgements

manifatura borrows freely:

- [pi](https://github.com/badlogic/pi-mono): the loop, lifecycle events, `transformContext` (here the `Context` hook), the session tree and its compaction rules.
- [Flue](https://github.com/withastro/flue): a narrow environment for tools, capabilities that change per call, and a shell that gets only the environment variables it is given.
- [eve](https://github.com/vercel/eve): the event names and the deferred tool catalog.

## Licence

Licensed under the [MIT licence](LICENSE). Unless you state otherwise, any contribution you submit for inclusion in this project is licensed as above, without any additional terms or conditions.
