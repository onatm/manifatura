# Getting started

This guide builds a small agent in a new binary: one that answers, then one with a tool, then one whose sessions outlive the process.

You need Rust 1.91 or newer and a model behind [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server`:

```sh
llama-server -m qwen3.5-9b.gguf -c 32768 --jinja
```

Any other OpenAI-compatible server works too, registered by name (see [models](models.md)).

## A new project

```sh
cargo new my-agent && cd my-agent
cargo add manifatura
cargo add tokio --features rt-multi-thread,macros
cargo add serde --features derive
cargo add schemars
```

## An agent that answers

```rust
use manifatura::{Agent, Event, Runtime, SessionId, providers};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = Agent::new("You are concise.");
    let rt = Runtime::builder().provider(providers::from_env()?).build();

    let session = rt.session(&agent, SessionId::new());
    let mut turn = session.prompt("Why is the sky blue?").await?;
    while let Some(event) = turn.next_event().await {
        if let Event::TextDelta { text, .. } = event {
            print!("{text}");
        }
    }
    let reply = turn.wait().await?;
    println!("\n[{} tokens in, {} out]", reply.usage.input, reply.usage.output);
    Ok(())
}
```

```sh
MANIFATURA_MODEL=llamacpp/qwen3.5-9b cargo run
```

`providers::from_env()` registers llama.cpp as `llamacpp`, at `LLAMACPP_URL` (default `http://localhost:8080`), and takes the default model from `MANIFATURA_MODEL`. An agent can name its own with `Agent::model("llamacpp/qwen3.5-9b")`.

`prompt` queues a turn and returns at once. `next_event` streams what happens in it, and `wait` returns the final reply, or the error the turn failed with. Turns of one session run one after another.

## A tool

A tool is a name, a description for the model, and a closure. With a typed input, the schema comes from `JsonSchema` and the input is checked before the closure runs:

```rust
use manifatura::{Agent, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct Weather {
    /// The city, e.g. "Lisbon".
    city: String,
}

let weather = Tool::new("weather", "The weather in a city now.").run(|i: Weather, _cx| async move {
    match i.city.as_str() {
        "Lisbon" => Ok("sunny, 24°C".to_string()),
        city => Err(ToolError::msg(format!("no weather station in {city}"))),
    }
});

let agent = Agent::new("You answer questions about the weather. Never guess it.").tool(&weather);
```

An error goes back to the model as a failed tool result, so it can try again. The [tools guide](tools.md) covers the basic tools, tools from programs, MCP servers and where tools run.

## Sessions on disk

By default sessions live in memory. A `JsonlStore` keeps each one in a file, so a session can be resumed by its id:

```rust
use manifatura::store::JsonlStore;
use manifatura::{Runtime, SessionId, providers};

let rt = Runtime::builder()
    .provider(providers::from_env()?)
    .store(JsonlStore::new("sessions")?)
    .build();

let id = SessionId::from("support-1042");
let session = rt.session(&agent, id); // loads support-1042.jsonl if it exists
```

Each line of the file is one entry: a message, a state change, a compaction. `jq` reads them:

```sh
jq -c 'select(.type == "message") | .message' sessions/support-1042.jsonl
```

## Next

- [Agents and sessions](sessions.md): what a turn does, steering, branching and compaction.
- [Hooks](hooks.md): approvals, guards, tools that appear when they are needed.
- [System prompts](system-prompts.md): `AGENTS.md` files and tool guidelines.
