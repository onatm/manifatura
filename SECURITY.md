# Security

Please report vulnerabilities privately, through GitHub's [private vulnerability reporting](https://github.com/onatm/manifatura/security/advisories/new), not in a public issue. Include what you found, how to reproduce it and what it affects. You will get a reply within a week.

Only the latest release is supported.

## What manifatura does and does not protect

- `tools::Local` has no isolation. Commands run as the user running the app, and file tools can reach any path that user can. It passes commands only an allowlisted set of environment variables, but that limits what leaks, not what a command can do. Run agents that act on untrusted input in a sandbox: an `Env` of your own, a container or a VM.
- MCP servers started as programs (over stdio) run as the user, outside any `Env`. Servers reached over HTTP see the tool calls sent to them and the headers you configure, API keys included. Configure only servers you trust.
- Approval hooks (`kit::approve`) are a check in the loop, not a sandbox: a tool that is allowed runs with the env's full access.
- Session files hold the whole conversation, tool output included, in plain text.
