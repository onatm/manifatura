# Changelog

All notable changes are recorded here. The project follows [Semantic Versioning](https://semver.org); before 1.0, a minor version may change the API.

## 0.3.0

- `manifatura-subagents`, new, as `manifatura::subagents` behind the `subagents` feature (off by default): a `task` tool that runs a subagent in a child session and returns its report. A subagent is an agent of its own or the calling session's agent with a brief (`Subagent::like_parent`), so it reuses the parent's prompt cache; it can also be written as Markdown (`Definition`). Approvals are shared both ways, nesting is limited (`max_depth`, 1 by default), aborting the parent aborts the child, and child events reach `action.log` and `Subagents::on_event`. `Subagents::run` starts a subagent from the app, as a task call would. `Subagent::tools` (or `tools:` in Markdown) limits the tools a child may call, keeping its prompt; `Subagent::max_steps` caps its model calls and keeps the last for its report.
- `manifatura-agent`: `ToolCx::agent`, the agent of the session a tool runs in, and `ToolCx::state_map`, every state key as the tool sees it; `Session::agent` and `Session::runtime`; `Agent::hooks_first`, for hooks that must decide before the others; `ModelRequest::tool_choice` (`ToolChoice::Auto` by default, or `ToolChoice::Off` for no tool calls), which `manifatura-models` sends as `tool_choice: "none"`. `ModelRequest` is `#[non_exhaustive]`: build one with `ModelRequest::new(model, messages)` and the setters `system`, `tools`, `max_tokens`, `reasoning` and `tool_choice`, so a call without tools never mentions them and later fields break no caller. Its fields stay public for providers and hooks.

## 0.2.1

- The README describes skills, the `skills` feature, `manifatura-skills` and the prelude, so the crates.io page matches 0.2.0.

## 0.2.0

- `manifatura-skills`, new, as `manifatura::skills` behind the `skills` feature (on by default): Agent Skills. Skills are found in `.agents/skills` (project and home), other directories and built into the app, with project skills taking precedence; `SystemPrompt::skills` (from `SystemPromptExt`) lists them in the prompt, the `skill` tool loads one, and `SkillSet::activation` serves `/name` commands.
- `manifatura`: `prelude`, the names most programs use, following the features.
- `manifatura-agent`: `ToolBuilder::keep_results` marks a tool whose results stay relevant for the rest of the task (`Tool::keeps_results`, `Tool::with_keep_results`).
- `manifatura-kit`: `compaction::trim_results` leaves the results of such tools whole. `SystemPrompt::section` ignores empty text.

## 0.1.0

The first release.

- `manifatura-agent`: the loop, fifteen typed hook points, sessions as an append-only tree with branching and compaction, steering and follow-ups, and the `Provider`, `Env` and `Store` traits.
- `manifatura-kit`: approvals with scoped "always" answers, compaction hooks and a summariser, a deferred tool catalog, input repair, `AGENTS.md` discovery, a system prompt built from tool guidelines, and the system delta for warm prompt caches.
- `manifatura-models`: llama.cpp and any other OpenAI-compatible server behind a `provider/model-id` registry.
- `manifatura-store`: sessions as JSON lines, repaired after a crash mid-write.
- `manifatura-tools`: `read`, `edit` (which also creates files), `grep` and `glob` (respecting `.gitignore`), and `bash` with background jobs; `Local`; script tools.
- `manifatura-mcp`: MCP servers' tools, over stdio or streamable HTTP.
- `manifatura`: the facade, with each block behind a feature.
