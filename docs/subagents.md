# Subagents

A subagent is an agent that another agent hands a self-contained task to. It works in a session of its own and only its final reply comes back, so what it reads, runs and tries along the way stays out of the caller's context. Use one to answer a question that takes many searches and reads, or to carry out one well-defined part of a larger change. The `manifatura-subagents` crate, `manifatura::subagents` behind the `subagents` feature (off by default), gives the model a `task` tool that starts one.

## Defining subagents

A `Subagent` has a name, a description the caller's model chooses by, and the agent it runs:

```rust
use manifatura::subagents::{Subagent, Subagents};

// An agent of its own: its instructions say what it does, and it has only its own tools.
let reviewer = Subagent::new(
    "reviewer",
    "Reviews a change for bugs and reports them with path:line.",
    Agent::new(REVIEWER).tools(manifatura::tools::by_name(&["read", "grep", "glob"])?),
);

// The calling session's own agent, with a brief before the task.
let worker = Subagent::like_parent("worker", "Carries out one well-defined part of a change.")
    .brief("You are doing one part of a larger change for another agent. Do only your part, then say what you changed.")
    .max_steps(40);

tools.push(Subagents::new([reviewer, worker]).tool());
```

`Subagent::like_parent` runs the session's own agent: the same instructions, tools, model settings and hooks. Its requests then start with the same system prompt and tool declarations as the parent's, so a provider that caches processed prompts serves the child from the parent's cache, and the child's approvals, compaction and other hooks work as they do for the parent. The brief tells it what its role is; it goes before the task in the child's first message.

A subagent can also be written as Markdown, the same way as a [skill](skills.md): frontmatter with a `name`, a `description` and optionally `tools`, `model`, `effort` and `max_steps`, then the brief.

```markdown
---
name: explore
description: Answers one question about the code by searching and reading it.
tools: read, grep, glob
---
You are exploring the code for another agent. Report what you found with path:line.
```

`tools` is a list or one line of names separated by commas. `model` and `effort` choose what the child runs on; without them, a child like its parent runs on the model and effort its parent session uses at the time, including a change made with `Session::set_model` or `set_reasoning`. `Subagent::with` applies such settings kept elsewhere, such as an app's config.

```rust
let def = manifatura::subagents::Definition::parse(include_str!("agents/explore.md"))?;
let explore = Subagent::from_definition(def); // like its parent, with the body as its brief
```

## The task tool

`Subagents::tool()` is the `task` tool. Its description lists the subagents, and its input is:

| Field | |
| --- | --- |
| `agent` | The subagent's name, an enum of them |
| `prompt` | The complete task |
| `description` | A few words on the task, for the user; optional |

The child does not see the caller's conversation, so the tool's description tells the model to put everything the child needs in the prompt. Its guideline, which [`SystemPrompt::tools`](system-prompts.md) adds to the prompt, says when to delegate. Several task calls in one reply run in parallel; `Subagents::sequential()` runs them one after another, for a provider that serves one request at a time.

The result is the child's last reply, cut at 16 KB (`max_result_bytes`). A child that fails, for example by running out of steps, returns an error with its last reply, so the caller can decide what to do next. The tool [keeps its results](tools.md#flags): compaction leaves reports whole.

## Child sessions

Each call opens a child session in the same runtime, so it is kept in the same store. Its id is the parent's followed by the subagent's name and a short random part (`1a2b…-c3d4.explore-9f8e7d6c`), its header names the subagent, and its first entry is a custom record of kind `subagent` holding the parent's id and the call's id. `manifatura::subagents::parent(&entries)` reads it, so a session picker can leave child sessions out or show them under their parent.

Aborting the caller's turn aborts the child.

## Starting one from the app

An app can start a subagent itself, for example from a `/explore` command, without waiting for the model to choose it:

```rust
let report = subagents.run(&session, "explore", question, &abort, |line| show(line)).await;
```

`run` does what a task call does, as a child of `session`: the same child session, shared approvals and depth, `on_event`, and the lines a task call logs go to the closure. `session` must be idle. The result is what the task call would have returned; nothing is added to the parent's conversation, so the app decides how the parent hears of it, such as by sending the report with the user's next prompt.

## Nesting

A child of `like_parent` has the `task` tool too. `Subagents::max_depth(n)` says how deep subagents nest: at depth `n` a task call is refused with "do this task yourself". The default, 1, lets only top-level sessions start subagents. The child's depth is in its state under `subagents.depth`, and its subagent's name under `subagents.agent` (`AGENT_KEY`).

## Limiting a child's tools

`Subagent::tools(names)` lists the only tools the child may call, by the names the model calls them:

```rust
let explore = Subagent::like_parent("explore", "Answers one question about the code.")
    .tools(["read", "grep", "glob"]);
```

A call to any other tool is refused, and the model is told which it may use. The refusal comes first among the child's hooks, so an approval hook never asks about a call that will be refused. The child is still offered all of its agent's tools: a child like its parent keeps the parent's prompt, and with it the prompt cache. To keep the other tools out of the prompt as well, give an agent of its own (`Subagent::new`) only these. A `*` in a name matches any run of characters, so `mcp__github__*` allows one server's tools. A deferred tool run through `run_tool` counts as the tool it runs, and `find_tools` is allowed whenever a name starts with `mcp__`.

## Step limits

Every child's model calls are capped, at 50 by default (`DEFAULT_MAX_STEPS`; `Subagents::default_max_steps` changes it, `Subagent::max_steps(n)` sets one child's), and the last of them is kept for its reply:

- From halfway on, each request ends with a note saying which step it is (`Step 11 of 20. Keep the last step for your reply.`).
- On step `n` the request asks for no tool calls (`ToolChoice::Off`), with the tools still declared, and ends with a note telling the child to write its reply from what it has found and say what it did not get to.

So a child that runs out of steps still reports, instead of stopping mid-search. The notes are added to those requests only; the child's session keeps none of them. A provider that cannot keep the model from calling tools may still get a call on the last step; the turn then ends without a reply, and the task returns an error with the child's last text.

## Approvals and state

A child's tool calls go through its agent's hooks, so a `kit::approve` hook asks before them as it does for the caller. State keys starting with `approval.` are shared: they are copied into the child when it starts, and what the child sets of them is copied back when it ends. An "always" answer given to the parent therefore holds in the child, and one given to the child holds afterwards in the parent. `Subagents::share_state(prefixes)` chooses other prefixes, or none.

## Showing what a child does

Each tool call the child starts appears in the caller's events as an `action.log` line under the task call, such as `grep {"pattern":"fn main"}`. For more, `Subagents::on_event(f)` calls `f` with every event of every child, along with the parent's id, the task call's id, the subagent's name and the child's id.
