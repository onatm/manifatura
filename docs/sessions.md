# Agents and sessions

## The pieces

| Type | What it is |
| --- | --- |
| `Agent` | Instructions, tools, hooks, and optionally a model, reasoning effort, output limit, step limit and env. A value: clone it, build it per request, keep a few. |
| `Runtime` | The provider, the store, the default model and env, and hooks for every session. Cheap to clone. |
| `Session` | One conversation, opened with `rt.session(&agent, id)`. Opening an id that is already open returns that session, with the agent it was opened with. |
| `Turn` | One prompt and everything it causes, until the model is done. |

```rust
let agent = Agent::new("You are a support agent for an online shop.")
    .name("support")              // recorded in the session's header
    .model("llamacpp/qwen3.5-9b")
    .reasoning(Effort::Low)
    .max_tokens(4_096)            // per reply
    .max_steps(30)                // model calls per turn
    .tools([&lookup, &refund]);
```

## Turns and events

`Session::prompt` queues a turn and returns its handle. A turn's events arrive in order:

| Event | When |
| --- | --- |
| `TurnStarted` | The turn began. |
| `StepStarted` | A model call is about to be made, with the estimated context size. |
| `ReasoningDelta`, `TextDelta` | Reasoning and answer text as they stream. |
| `MessageAppended` | A reply or a batch of tool results was recorded. |
| `ActionStarted`, `ActionLog`, `ActionResult` | A tool call started, logged a line, finished. |
| `Compacted` | A compaction was written. |
| `TurnCompleted` | The turn ended: completed, failed (with the reason) or aborted. |

`turn.next_event()` yields only that turn's events. `session.subscribe()` gives every event of the session, for a UI that outlives single turns. `turn.abort()` stops a turn at the next safe point: a streaming reply is cut off, running tools see their abort signal, and what finished is kept.

`turn.wait()` returns a `Reply` (the final text, message and usage) or an `Error`:

| Error | Meaning |
| --- | --- |
| `Error::Model(e)` | The model call failed and no hook retried it; `e.kind` says why (`ContextOverflow`, `Auth`, `RateLimited`, …). |
| `Error::Failed(reason)` | A hook failed the turn, it ran out of steps, or no model was configured. |
| `Error::Aborted` | The turn was aborted. |
| `Error::Store(e)` | The store failed. |

## Steering and follow-ups

While a turn runs, `session.steer(text)` adds a user message before the next model call, and `session.follow_up(text)` adds one for when the model would otherwise finish. Both return `false`, and drop the text, when no turn is running; start a new turn with `prompt` then.

## State

Sessions carry a small key–value state, recorded like everything else and restored on resume. Tools and hooks read and write it; so can the app:

```rust
session.set_state("tier", "gold").await?;
let tier: Option<String> = session.state("tier").await?;
session.set_state("tier", serde_json::Value::Null).await?; // removes it
```

Writes from tools and hooks are applied at the loop's next safe point, never between a tool call and its result.

## The session tree

A session is an append-only list of entries, each with an id and a parent. The conversation is the path from the newest entry back to the first. That makes branching cheap:

```rust
let entries = session.entries().await?;
let earlier = entries[4].id.clone();
session.branch(Some(earlier)).await?; // the next prompt continues from there
```

The other branch stays in the store. Model changes (`set_model`, `set_reasoning`) and labels (`set_label`) are recorded too. `append_custom(kind, data)` records anything of the app's own, which never reaches the model.

When a session is opened, its history is read from the store. Tool calls left without results by a process that died get an "interrupted" result, so the conversation is valid for the model again.

Stores implement `Store`: `JsonlStore` writes one file per session, `MemoryStore` keeps them in memory. `rt.store().list()` lists sessions, with the agent named in each and when it last changed.

### Going back

`Session::path` is the current branch, from the first entry to the one the next message follows. To go back to before a message, branch to that message's parent: `session.branch(entry.parent_id.clone())`.

The files can go back too. With the `snapshots` feature (the `manifatura-snapshots` crate), a hook snapshots the working directory as each turn starts, as a git tree in a repository of its own (never the project's `.git`; what `.gitignore` leaves out is left out), and records it in the session:

```rust
use manifatura::snapshots::{Snapshots, before};

let snapshots = Snapshots::new(data_dir.join("snapshots.git"));
let agent = Agent::new("…").hooks(snapshots.hooks());

// Later: the files as they were before message `id`, and the conversation too.
let path = session.path().await?;
if let Some(tree) = before(&path, &id) {
    let restored = snapshots.restore(env, &tree).await?; // files changed since written back, new ones removed
}
let parent = path.iter().find(|e| e.id == id).and_then(|e| e.parent_id.clone());
session.branch(parent).await?;
```

`Snapshots::changes` lists what differs from a snapshot, for a preview. Snapshots run `git` through the env; for a repository in a directory a sandbox keeps commands from writing to, use `Snapshots::unsandboxed(true)` on an env that allows it.

## Compaction

Nothing compacts by itself. When the context grows too long, a compaction summarises older messages and keeps recent ones:

```rust
use manifatura::kit::Summarizer;

let summarizer = Arc::new(Summarizer::default()); // keeps the last 20k tokens
let report = session.compact(summarizer, Some("the refund decision".into())).await?;
println!("{} → {} tokens", report.tokens_before, report.tokens_after);
```

The cut never separates a tool result from its call, and the previous summary is handed to the next one. The session keeps every message; the summary only changes what the model sees.

Hooks in `kit::compaction` decide when, if you would rather not, or keep the requests small without summarising:

| Hook | What it does |
| --- | --- |
| `on_overflow(compactor)` | When the provider reports a context overflow, compacts once and retries the call. |
| `auto(threshold, compactor)` | Compacts before the next call once the context passes `threshold` of the window (e.g. `0.8`). |
| `drop_old_results()` | Leaves the results of turns before the previous one out of what is sent, as a one-line stub, without touching the session. Short results, errors and tools that keep their results go whole. A turn past 70% of the window drops its own oldest results down to 50%, in one batch, so the start of each request stays the same and the prompt cache keeps working. `DropResults` sets the turns kept, the size kept and the shares. |
| `trim_results(keep_recent, max_chars)` | Shortens old, long tool results in what is sent, without touching the session. |

Used together, `drop_old_results` goes first and a summary is the fallback: keep its ceiling under `auto`'s threshold, so results are dropped before a summary is asked for, and `auto` measures the requests as sent, after the drops.

`Summarizer::small()` keeps 6k recent tokens, for small context windows. A `BeforeCompact` hook can change the plan, supply its own summary or cancel.
