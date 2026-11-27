# Approvals

An approval is a question to a person before a tool call runs. `kit::approve` asks through a callback, for an app where the question and the answer happen in one place ([hooks](hooks.md#ask-before-a-tool-runs)). This guide covers the two pieces for the rest: `kit::Approvals`, pending requests any client can answer, and `manifatura-policy`, rules deciding which calls ask at all.

## Pending requests

`Approvals` keeps each question as a request with an id. A hook asks and waits; clients see requests as they come and answer them by id. The first answer wins, so several clients (a terminal, a web page, a second person) can show the same question.

```rust
use manifatura::kit::{Answer, ApprovalEvent, Approvals};

let approvals = Approvals::new();
let agent = Agent::new("…")
    .tools(tools::all())
    .hooks(approvals.gate(|call| call.name == "bash", |call| call.name.clone()));

// In a client:
let mut events = approvals.subscribe();
for r in approvals.pending() { show(&r); }   // what was asked before it connected
while let Ok(e) = events.recv().await {
    match e {
        ApprovalEvent::Requested(r) => show(&r),
        ApprovalEvent::Answered { id, .. } | ApprovalEvent::Cancelled { id, .. } => hide(&id),
        _ => {}
    }
}
approvals.answer(&id, Answer::Allow); // when the person chooses
```

A `Request` carries the session, the call, its `scope` (what "always" allows for the rest of the session), an optional `reason` (why it asks) and an optional `remember` (what "allow and remember" keeps beyond the session). The answers:

| Answer | Means |
| --- | --- |
| `Allow` | This call. |
| `AllowAlways` | This call, and later ones in the same scope, for the rest of the session. |
| `AllowAndRemember` | This call, and what the request offers to remember (a rule), from now on; where it offers nothing, as `AllowAlways`. |
| `Deny(reason)` | Refuse it; the reason goes back to the model. |

When the turn is aborted, the request is cancelled and the call denied. Requests live in memory: a process that stops leaves none, and the turns that waited are interrupted. Each answer is recorded in the session as a custom `approval` record, which the model never sees. "Always" answers live in session state under `approval.`, which [subagents](subagents.md) share with their parent.

## Policy

The `policy` feature (the `manifatura-policy` crate) decides which calls run, which ask and which are refused, from rules:

```toml
allow = ["exec_command(git commit)", "mcp__exa__*"]
ask = ["mcp__github__*"]
deny = ["exec_command(git push --force)", "exec_command(rm -rf)"]
```

A rule names a tool (`*` matches any run of characters) and, for a tool that runs shell commands, a command prefix in parentheses. `bash` and `exec_command` are known command tools (`Policy::command_tool` adds others). A command line is split with tree-sitter-bash into the simple commands it runs, through `&&`, pipes, subshells, conditionals and loops, without wrappers such as `timeout` or `VAR=value`, and each is checked:

1. A `deny` rule matching the tool or any of the commands, even one inside `$(…)`, denies.
2. Otherwise an `ask` rule asks.
3. An `allow` rule for the tool allows. For a command line, every command must be allowed by a rule or only read (`ls`, `rg`, `git status`, `sed -n '1,20p'`, … without the arguments that make them write), and the line must hold nothing that is only known when it runs: a command substitution, a function, a program named by a variable.
4. Otherwise the defaults decide, in the order given, then the overall default (ask, unless set).

```rust
use manifatura::policy::{Decision, Policy, Rules};

let rules: Rules = toml::from_str(&std::fs::read_to_string(".myapp/rules.toml")?)?;
let policy = Policy::new()
    .rules(&rules)?
    .when(|call, _| manifatura::exec::is_escalation(call), Decision::Ask)
    .tool_default("exec_command", Decision::Allow)   // sandboxed: no need to ask
    .tool_default("read", Decision::Allow)
    .default(Decision::Ask)
    .on_remember(|rule| save_rule(rule));            // keep what "allow and remember" adds
let agent = Agent::new("…").hooks(policy.hooks(&approvals));
```

`Policy::hooks` runs the policy as a `ToolCall` hook and asks through `Approvals`. A request about a command offers the rules that would allow the whole line next time, one per command (`exec_command(git add), exec_command(git commit)`, with the subcommand for programs that have them), and none when the line holds a program that would allow anything (`rm`, `sudo`, `bash`, `python`, `curl`, …) or cannot be read. The reason shown is the call's `justification` field when it has one (as an `exec_command` asking to leave the sandbox does), or the rule that asks. A rule remembered in one session holds in every session sharing the policy.

Rules are not a security boundary: a program can be renamed or reached through a script. Keep commands in a [sandbox](tools.md#a-sandbox) and let rules decide what may leave it.
