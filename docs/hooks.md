# Hooks

The loop runs the model and the tools and records what happens; everything else is a hook. A hook is an async closure whose argument's type picks the point in the loop it runs at:

```rust
use manifatura::{Agent, hook};

let agent = Agent::new("You are a coding agent.").on(|e: hook::ToolCall, _cx| async move {
    let command = e.call.input["command"].as_str().unwrap_or_default();
    if e.call.name == "bash" && command.contains("rm -rf") {
        return hook::Decision::Deny("not here: delete files one at a time".into());
    }
    hook::Decision::Run(e)
});
```

The event comes by value and the hook returns what the loop uses next, so a hook can hold the event across any `await`: a prompt on a terminal, a request to a policy service.

## Points in the loop

| Event | Gets | Returns | With several hooks |
| --- | --- | --- | --- |
| `SessionStart` | whether the session existed | `()` | all run |
| `Input` | the prompt's text | `Option<Input>`: `None` drops it, and the turn ends without a model call | chained; `None` stops |
| `TurnStart` | | `()` | all run |
| `Step` | the instructions and tools for the next model call | `Step` | chained |
| `Context` | the messages for this call | `Context`: changes apply to this call only | chained |
| `Request` | the request as it will be sent | `Request` | chained |
| `ModelError` | the provider's error and the attempt | `Recover::Retry` or `Recover::Fail` | the first `Retry` wins |
| `Response` | the reply, its usage and stop reason | `Response`, then recorded | chained |
| `StepEnd` | usage, estimated context tokens, the window | `()` | all run |
| `ToolCall` | the call and the tool it names | `Decision`: `Run(call)`, `Deny(reason)` or `Result(result)` | chained; `Deny` and `Result` stop |
| `ToolResult` | the call and its result | `ToolResult`, then recorded | chained |
| `TurnEnd` | the final reply and the turn's usage | `Next::Done` or `Next::FollowUp(text)` | the first `FollowUp` wins |
| `BeforeCompact` | the compaction plan | `Proceed(plan)`, `Summary(text)` or `Cancel` | chained; `Summary` and `Cancel` stop |
| `Compacted` | the summary and the token counts | `()` | all run |
| `Event` | each live event, except `action.log` | `()` | all run, inline |

Hooks on the runtime (`RuntimeBuilder::on`) run before the agent's (`Agent::on`), each in the order added. `Event` hooks run inside the loop: keep them quick, and use `Session::subscribe` for slow consumers.

## What a hook can do

The second argument, `HookCx`, reads the session and asks the loop for changes:

| Method | What it does |
| --- | --- |
| `session_id()`, `turn_id()`, `step()` | Where the hook runs. |
| `tools()`, `tool(name)` | The tools of the current step. |
| `state(key)`, `set_state(key, value)`, `clear_state(key)` | The session's state. |
| `note(text)` | A note the model sees with its next call. |
| `steer(text)`, `follow_up(text)` | A user message for the next call, or for when the model would finish. |
| `compact(compactor, focus)` | Compacts before the next model call. |
| `custom(kind, data)` | Records data of the app's own, never sent to the model. |
| `fail(reason)` | Fails the turn. |
| `env()`, `abort()`, `status()`, `entries()` | The env, the abort signal, the model and context use, the session's entries. |

Changes are applied at the loop's next safe point: before a model call or at the end of the turn, never between a tool call and its result.

## Bundles

A bundle of behaviour is a function returning `Hooks`, added with `.hooks(…)` on an agent or a runtime:

```rust
use manifatura::{Hooks, hook};

/// Logs every tool call and its result.
fn audit() -> Hooks {
    Hooks::new()
        .on(|e: hook::ToolCall, cx| async move {
            println!("[{}] {} {}", cx.session_id(), e.call.name, e.call.input);
            hook::Decision::Run(e)
        })
        .on(|e: hook::ToolResult, _cx| async move {
            println!("  → {} bytes{}", e.result.output.len(), if e.result.is_error { ", failed" } else { "" });
            e
        })
}
```

`kit` is a set of such bundles:

| Bundle | What it does |
| --- | --- |
| `kit::approve(tools, ask)` | Asks before calls of the named tools. "Always" allows the tool for the session. |
| `kit::approve_when(check, ask)` | Asks before calls `check` picks out, such as dangerous commands. "Always" allows only the same call again. |
| `kit::approve_by(check, scope, ask)` | The same, with "always" covering whatever `scope` says: a tool, a command, a server. |
| `kit::Approvals::gate(check, scope)` | The same, asking through pending requests any client can answer (see [approvals](approvals.md)). |
| `kit::catalog()` | Deferred tools behind `find_tools` and `run_tool` (see [tools](tools.md)). |
| `kit::repair()` | Fixes inputs small models get wrong: `"30"` for a number, objects sent as JSON strings, a bare value for a one-field tool. |
| `kit::compaction::on_overflow(c)`, `auto(t, c)`, `drop_old_results()`, `trim_results(n, chars)` | Compaction on overflow, past a threshold, older turns' tool results left out, and shorter old tool results (see [sessions](sessions.md)). |
| `kit::system_delta()` | Keeps the first system prompt and sends later changes as notes (see [system prompts](system-prompts.md)). |

Order matters where bundles look at the same calls. Add `catalog` before bundles that look at tool names, so they see the real tool rather than `run_tool`, and `system_delta` last.

## Recipes

### Ask before a tool runs

`ask` is your UI. While it waits, an aborted turn denies the call.

```rust
use manifatura::{ToolCall, kit};

let agent = Agent::new("…")
    .tools(tools::all())
    .hooks(kit::approve(["edit", "bash"], |call: ToolCall| async move {
        match my_ui::confirm(&call.name, &call.input).await {
            my_ui::Choice::Yes => kit::Answer::Allow,
            my_ui::Choice::Always => kit::Answer::AllowAlways,
            my_ui::Choice::No(reason) => kit::Answer::Deny(reason),
        }
    }));
```

For questions answered somewhere other than where the hook runs (a web page, another person, a client that reconnects), use `kit::Approvals`, and for rules about which tools and commands may run, `manifatura-policy`: see [approvals](approvals.md).

### A tool that appears when it is needed

A `Step` hook runs before every model call, so the tools can follow the session's state:

```rust
let agent = Agent::new("You handle support. Refunds need an escalated case.")
    .tool(&escalate) // sets the "escalated" state
    .on(move |step: hook::Step, cx| {
        let refund = refund.clone();
        async move {
            if cx.state("escalated").unwrap_or(false) {
                step.tool(&refund)
            } else {
                step
            }
        }
    });
```

Changing the tools or instructions mid-session invalidates a provider's prompt cache. `kit::system_delta()` avoids that for instructions; see [system prompts](system-prompts.md).

### Answer a command without the model

```rust
let agent = Agent::new("…").on(|e: hook::Input, cx| async move {
    if e.text.trim() != "/budget" {
        return Some(e);
    }
    let spent: u64 = cx.state("spent").unwrap_or(0);
    println!("spent so far: {spent} tokens");
    None
});
```

### A context meter

```rust
let agent = Agent::new("…").on(|e: hook::StepEnd, _cx| async move {
    if let Some(window) = e.context_window {
        eprintln!("[context {}/{window} tokens]", e.context_tokens);
    }
});
```

### Keep going until the tests pass

```rust
let agent = Agent::new("…").on(|e: hook::TurnEnd, cx| async move {
    let tries: u32 = cx.state("tries").unwrap_or(0);
    if tries < 3 && !tests_pass().await {
        cx.set_state("tries", tries + 1);
        return hook::Next::FollowUp("The tests still fail. Fix them.".into());
    }
    hook::Next::Done
});
```
