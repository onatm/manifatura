# System prompts

An agent's instructions are a string, and how it is built is up to the app. `kit::context` builds one the way coding agents do: a base text, guidelines from the agent's tools, the project's `AGENTS.md` files, and a few facts.

## `AGENTS.md` files

[`AGENTS.md`](https://agents.md) is a convention for instructions an agent should follow in a project: how to build it, how to run its tests, what not to touch. `ContextFiles` finds them:

```rust
use manifatura::kit::context::ContextFiles;

let files = ContextFiles::new()
    .user_file(home.join(".config/my-agent/AGENTS.md")) // the user's own, read first
    .home(&home)                                         // where the search stops outside a repository
    .load(&env)
    .await;
```

It reads, in order:

1. the user's own file, when the app names one;
2. in each directory from the repository root (the nearest ancestor with a `.git`) down to the working directory, `AGENTS.md`, or `CLAUDE.md` where there is no `AGENTS.md`.

Outside a repository the search starts at the home directory the app gives, when the working directory is under it, and otherwise looks in the working directory alone. Files are read through the env, so discovery works wherever the env points; empty files and repeats are skipped, and each file is capped at 64 KiB (`max_bytes`). `names([...])` changes the file names looked for.

## Building the prompt

```rust
use manifatura::kit::context::SystemPrompt;

let tools = tools::by_name(&["read", "edit", "bash"])?;
let instructions = SystemPrompt::new("You are a coding agent in a terminal.")
    .tools(&tools)                                   // their guidelines
    .guideline("When the task is done, say briefly what you changed.")
    .context_files(&files)
    .cwd(env.cwd())
    .date("2026-11-24")
    .build();
let agent = Agent::new(instructions).tools(tools);
```

The prompt comes out in this order:

```text
You are a coding agent in a terminal.

Guidelines:
- Read files with `read` (not cat); page through large files with offset and limit.
- …
- When the task is done, say briefly what you changed.

# Project context

Instructions from context files. Follow them; more specific (deeper) files take precedence.

## /home/me/shop/AGENTS.md

<its text>

Working directory: /home/me/shop
Current date: 2026-11-24
```

Tools carry their own guideline (`Tool::new(…).guideline("…")`), so the prompt describes exactly the tools the agent has; deferred tools add none, as the model does not see them. `section(text)` adds a block of the app's own, and `fact(label, value)` another line at the end. The app passes the date, so nothing in `kit` reads the clock.

## Keeping the prompt cache warm

Providers cache the start of a prompt: a request whose system prompt and earlier messages match the previous one's is cheaper and faster. Changing the system prompt or the tools mid-session throws that away.

`kit::system_delta()` keeps the first system prompt of a session fixed. When a later step's instructions or tools differ, the model is told what changed in a note, and the cached prefix stays valid. What it declared is kept in session state, so a resumed session continues from there. Add it after every hook that changes the instructions or tools.

The same reasoning applies to anything put in the prompt: a date with the time, or a counter, changes it on every request.
