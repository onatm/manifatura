# rho

A coding agent in the terminal, on your ChatGPT plan, built on manifatura. It lives in this repository while
manifatura grows the pieces it needs; it is not published.

```sh
cargo run -p rho -- login            # sign in with ChatGPT, in the browser
cargo run -p rho                     # a new session in this directory
cargo run -p rho -- --continue       # the latest session here
cargo run -p rho -- --resume [ID]    # one of them, or the list
cargo run -p rho -- models           # the models your plan offers
```

## How it works

- **Tools**: `read`, `grep`, `glob`, `apply_patch` (Codex's patch format), `exec_command` and `write_stdin`
  (commands that keep running in the background), `update_plan`, `task` with an `explore` subagent (read, grep and
  glob only) and the subagents you define, each also a command (`/explore <question>`, `/<name> <task>`), skills (`/skill:<name>`), and MCP servers. A command left
  in the background that exits while rho is idle wakes it to carry on.
- **Sandbox**: every command runs in it (macOS Seatbelt, Linux bubblewrap): it writes only in the workspace,
  the scratch directory and `/tmp`, never to `.git`, `.agents` or `.rho`, and has no network beyond localhost.
  rho's own writes follow the same rules.
- **Approvals**: nothing asks, except a command the model wants to run outside the sandbox (with its reason),
  typing into one that runs there, and MCP tools. Each question offers once, the session, "always" (a rule
  like `exec_command(git commit)`, saved to your rules), or deny.

- **Context**: the session keeps every tool result, but the model is sent only the current turn's and the previous
  one's; older ones become a one-line stub it can act on by calling the tool again. A long turn drops its own oldest
  results in batches past 70% of the window. Only past 85% is a summary written, and when the model still finds the
  request too long, the turn ends with your message back in the composer and the footer's context share in red.
- **Model and effort**: `/models` opens the plan's models: ↑↓ choose one, which starts at its default effort, ←→
  change the effort, Enter uses them. `/effort` opens the current model's efforts alone; `/effort <level>` and
  `/effort default` set one from the composer. A choice holds for this session and becomes where new ones start: rho writes it to
  `model` and `effort` in your config. A session keeps the model and effort it last ran on, whatever your defaults
  become later; `--model` and `--effort` choose for one run without saving. With no `model` in the config, a new
  session starts on the plan's first model at its default level.
- **Rewind**: as each turn starts, rho snapshots the workspace (what `.gitignore` leaves out is left out) in a git
  repository of its own, never your `.git`. `/rewind` goes back to before one of your messages: the code, the
  conversation, or both. A conversation that goes back leaves the screen at that message, with the message back in
  the composer to edit.

## Configuration

One file, `$XDG_CONFIG_HOME/rho/config.toml` (else `~/.config/rho/config.toml`):

```toml
model = "gpt-6.2-sol"     # new sessions start on it; `rho models` lists the plan's
effort = "medium"         # and on this effort, when the model has it
mouse = false             # clicks on the footer and task list; the scroll wheel then goes to rho
theme = "dark"            # dark, light, terminal or one below; unset: dark or light for the terminal
sidebar = false           # start with the sidebar open (Ctrl-B toggles it)
compat = ["claude"]       # also read .claude/skills and ~/.claude/skills

[themes.mine]             # a theme of your own: a base and colours over it
base = "dark"             # text bright dim accent ok warn error info agent band select added removed rule
accent = "#7fb4ff"

[rules]                   # "always allow" adds here
allow = ["exec_command(cargo test)", "mcp__exa__*"]
deny = ["exec_command(git push)"]

[mcp.exa]                 # a server: `url` (streamable HTTP) or `command` and `args` (stdio)
url = "https://mcp.exa.ai/mcp"
eager = true              # its tools offered directly, not through find_tools

[agents.explore]          # over what an agent's file says: tools, mcp, model, effort
effort = "low"
mcp = ["exa"]
```

The `rules.toml` and `mcp.toml` of earlier versions are moved into it on first start, and kept as `.bak`.

## Files

| Path | What |
| --- | --- |
| `~/.config/rho/config.toml` | Everything rho is configured with (below) |
| `.rho/config.toml` | A project's `[rules]` (`ask` and `deny` only) and `[agents]` |
| `~/.config/rho/agents/*.md`, `.rho/agents/*.md` | Subagents of your own, and the project's, for the `task` tool: frontmatter with `name`, `description` and optionally `tools` (`mcp__server__*` for a server's tools), `model` and `effort`, then what it should do. A later one replaces an earlier one of the same name, `explore` included. Each runs on the session's model unless it says, and stops after 50 steps with a report |
| `.agents/skills`, `~/.config/rho/skills`, `~/.agents/skills` | Skills, one folder each with a `SKILL.md`: the project's (from the repository root down; a nearer one wins), then rho's, then those shared with other agents. A skill hides one of the same name below it, with a warning. `/skill:<name> [request]` starts one; `/skills` lists them with their cost in the prompt and sets who sees each in this project (on, name-only, user-only, off; saved under `[skills]` in `.rho/config.toml`) |
| `~/.config/rho/AGENTS.md`, `AGENTS.md` | Instructions, with the project's from its root down |
| `~/.local/share/rho/sessions/<directory>` | Sessions, one JSON-lines file each |
| `~/.local/share/rho/snapshots/<directory>.git` | The workspace as each turn found it |

In the chat, `/help` lists the commands: `/models` and `/effort`, `/tasks` and `/kill` for background commands
and subagents, `/rules`, `/rewind`, `/resume`, `/theme`, `/sidebar` and more. Enter while the agent works steers it before
its next step; Esc stops it.

The transcript marks each thing in the left column: `›` you, `•` an update while rho works (dim), `◇` reads and searches
(grouped while they run back to back), `✎` an edit with its first changed lines, `$` a command, `↳` a subagent, `☰` the
plan, `◆` a skill, `⌁` another tool. The answer is unmarked and brightest. Ctrl-O (or `/tools`) prints this turn's tool
output in full. The footer shows the model, effort and a context meter that turns yellow at 80% and red at 95%.

`/models` is a grid of the plan's models and their efforts, with each model's default `◎` and the session's choice `●`: ↑↓
move to a model at its default, ←→ change the effort, Enter uses them here and for new sessions.

Ctrl-B (or `/sidebar`) opens a sidebar beside the composer on terminals 100 columns or wider: the session's model, effort
and context, the plan, the files changed, the background commands and subagents, the MCP servers and the skills. It is off
unless `sidebar = true`. `/theme <name>` switches the colours and saves the choice; lines already in the scrollback keep
theirs.

The background tasks are the commands left running and the subagents started. ↓ on an empty composer (or `/tasks`, or a click on
"N in background" with `mouse = true`) lists them: Enter on a command shows its output as it runs, `k` stops it, and Enter on a subagent
shows its transcript, followed live. ← → on an empty composer move between the session and its subagents; Esc goes back.
