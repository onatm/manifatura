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
  (commands that keep running in the background), `update_plan`, skills from `.agents/skills`, and MCP servers.
- **Sandbox**: every command runs in it (macOS Seatbelt, Linux bubblewrap): it writes only in the workspace,
  the scratch directory and `/tmp`, never to `.git`, `.agents` or `.rho`, and has no network beyond localhost.
  rho's own writes follow the same rules.
- **Approvals**: nothing asks, except a command the model wants to run outside the sandbox (with its reason),
  typing into one that runs there, and MCP tools. Each question offers once, the session, "always" (a rule
  like `exec_command(git commit)`, saved to your rules), or deny.

## Files

| Path | What |
| --- | --- |
| `~/.config/rho/config.toml` | `model`, `effort` (off, low, medium, high), `context_window` |
| `~/.config/rho/rules.toml` | `allow`, `ask` and `deny` rules, such as `exec_command(npm install)` or `mcp__exa__*` |
| `.rho/rules.toml` | A project's rules: `ask` and `deny` only |
| `~/.config/rho/mcp.toml` | MCP servers (`[servers.<name>]` with `url` or `command`) |
| `~/.config/rho/AGENTS.md`, `AGENTS.md` | Instructions, with the project's from its root down |
| `~/.local/share/rho/sessions/<directory>` | Sessions, one JSON-lines file each |

In the chat, `/help` lists the commands: `/model`, `/effort`, `/ps` and `/kill` for background commands, `/rules`,
`/compact`, `/resume` and more. Enter while the agent works steers it before its next step; Esc stops it.
