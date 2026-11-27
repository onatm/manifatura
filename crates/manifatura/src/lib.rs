//! manifatura: a small library for building agents into your own programs: a loop, typed hooks,
//! and sessions kept as a tree.
//!
//! Guides: <https://manifatura.dev>.
//!
//! The foundation (agent loop, typed hooks, sessions, tools, records) is re-exported at the root.
//! Building blocks sit behind cargo features:
//!
//! | Feature | Module |
//! | --- | --- |
//! | `kit` (default) | [`kit`]: hook bundles (compaction, approvals, catalog, input repair, system delta) |
//! | `providers-openai` (default) | [`providers`]: llama.cpp and other OpenAI-compatible servers (Chat Completions) |
//! | `providers-responses` | [`providers::responses`]: OpenAI's Responses API, with pluggable credentials |
//! | `store-jsonl` (default) | [`store`] |
//! | `tools` (default) | [`tools`]: `read`, `edit`, `grep`, `glob`, `bash` and [`tools::Local`], the env on this machine |
//! | `skills` (default) | [`skills`]: [Agent Skills](https://agentskills.io), a catalog for the prompt and a `skill` tool |
//! | `subagents` | [`subagents`]: a `task` tool that hands a self-contained task to a subagent |
//! | `mcp` | [`mcp`]: MCP servers' tools, over stdio or HTTP |
//! | `exec` | [`exec`]: `exec_command` and `write_stdin`, commands that keep running in the background (with `tools`, terminals for `tools::Local`) |
//! | `policy` | [`policy`]: allow, ask and deny rules for tools and shell command prefixes, asking through [`kit::Approvals`] |
//! | `sandbox` | [`sandbox`]: an env whose commands run in a sandbox (macOS Seatbelt, Linux bubblewrap), writing only to the workspace, without the network |
//! | `auth` | [`auth`]: signing in to a model vendor, such as OpenAI's Sign in with ChatGPT for the user's plan |
//! | `testing` | `testing`: a scripted provider for tests |
//!
//! [`prelude`] holds the names most programs use, following the features.

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub use manifatura_agent::*;

#[cfg(feature = "kit")]
pub use manifatura_kit as kit;

#[cfg(any(feature = "providers-openai", feature = "providers-responses"))]
pub use manifatura_providers as providers;

#[cfg(feature = "store-jsonl")]
pub use manifatura_store as store;

#[cfg(feature = "tools")]
pub use manifatura_tools as tools;

#[cfg(feature = "mcp")]
pub use manifatura_mcp as mcp;

#[cfg(feature = "skills")]
pub use manifatura_skills as skills;

#[cfg(feature = "subagents")]
pub use manifatura_subagents as subagents;

#[cfg(feature = "auth")]
pub use manifatura_auth as auth;

#[cfg(feature = "exec")]
pub use manifatura_exec as exec;

#[cfg(feature = "sandbox")]
pub use manifatura_sandbox as sandbox;

#[cfg(feature = "policy")]
pub use manifatura_policy as policy;

pub use schemars;
pub use serde_json;

/// The names most programs use, in one import: `use manifatura::prelude::*;`. What it brings in
/// follows the features: with `skills`, [`SystemPromptExt`](skills::SystemPromptExt) gives
/// `SystemPrompt` its `skills` method.
pub mod prelude {
    pub use manifatura_agent::{
        Agent, Env, Event, Hooks, Message, Runtime, Session, SessionId, Tool, ToolCx, ToolError,
        ToolResult, hook,
    };

    #[cfg(feature = "kit")]
    pub use manifatura_kit::context::{ContextFiles, SystemPrompt};

    #[cfg(feature = "skills")]
    pub use manifatura_skills::{SkillSet, Skills, SystemPromptExt};
}
