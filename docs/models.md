# Models

The loop talks to models through one trait, `Provider`. `manifatura::models` has a provider for OpenAI-compatible servers and a registry that routes `provider/model-id` names to them; anything else is a `Provider` of your own.

## The registry

`models::from_env()` registers [llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server` as `llamacpp`, at `LLAMACPP_URL` (default `http://localhost:8080`). The default model is `MANIFATURA_MODEL`.

A model is named `provider/model-id`, so `llamacpp/qwen3.5-9b` goes to llama.cpp as `qwen3.5-9b`, and `vllm/qwen3.5-32b` to a server registered as `vllm`. The model comes from, in order: the session (`Session::set_model`), the agent (`Agent::model`), the runtime (`RuntimeBuilder::model`), then the provider's default.

Other OpenAI-compatible servers (vLLM, hosted APIs) register by name, in code or from an app's config file:

```rust
use manifatura::models::{Models, OpenAi};

let models = Models::new()
    .with("vllm", OpenAi::compatible("http://gpu-box:8000/v1"))
    .with("groq", OpenAi::compatible("https://api.groq.com/openai/v1").key(groq_key))
    .with_default("vllm/qwen3.5-32b");
```

```rust
use manifatura::models::{self, ProviderConfig};

let config: ProviderConfig = toml::from_str(
    r#"
    kind = "openai"
    base_url = "http://gpu-box:8000/v1"
    api_key_env = "VLLM_KEY"
    context_window = 32768
    "#,
)?;
let models = models::from_env()?.configure("vllm", &config)?;
```

The context window is what the loop reports in events and what `kit::compaction::auto` measures against. The registry assumes 8,192 tokens for `llamacpp` and knows nothing of other servers: set it to the server's real window (llama.cpp's `-c`) with `context_window` or `Models::with_info`.

## Errors

A failed model call reaches a `ModelError` hook, then, if no hook retries it, fails the turn with `Error::Model`. Its `kind` says what happened: `ContextOverflow`, `Auth`, `RateLimited`, `Server`, `Network`, `InvalidRequest` or `Other`. The OpenAI-compatible provider retries network errors, 429s and 5xx responses a few times before a reply starts, honouring `retry-after`; past that, a hook decides:

```rust
let agent = Agent::new("…").on(|e: hook::ModelError, _cx| async move {
    let transient = matches!(e.error.kind, ErrorKind::RateLimited | ErrorKind::Server | ErrorKind::Network);
    if transient && e.attempt < 3 {
        tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(e.attempt as u32))).await;
        return hook::Recover::Retry;
    }
    hook::Recover::Fail
});
```

## Writing a provider

A provider streams a reply to a `ModelRequest`: the model name, the system prompt, the messages, the tools offered, an output limit, a reasoning effort, and whether the model may call the tools (`tool_choice`: `ToolChoice::Auto` by default, `ToolChoice::Off` for text only with the tools still declared). A provider that cannot keep a model from calling tools may ignore it. To make a request yourself, use `ModelRequest::new(model, messages)` and the setters for what the call uses: `system`, `tools`, `max_tokens`, `reasoning`, `tool_choice`. The stream yields text and reasoning as it comes, and ends with exactly one `StreamEvent::Done` carrying the whole reply. Failures are reported in `Done`, as `StopReason::Error`, never as a broken stream.

```rust
use manifatura::{
    Abort, BoxStream, Message, ModelInfo, ModelRequest, Provider, Response, StopReason,
    StreamEvent, Usage,
};

/// Repeats the last message back.
struct Echo;

impl Provider for Echo {
    fn stream(&self, req: ModelRequest, _abort: Abort) -> BoxStream<'static, StreamEvent> {
        let text = req.messages.last().map(Message::text).unwrap_or_default();
        let done = Response {
            message: Message::assistant(text.clone()),
            stop: StopReason::Stop,
            usage: Usage::default(),
        };
        Box::pin(futures::stream::iter([StreamEvent::TextDelta(text), StreamEvent::Done(done)]))
    }

    fn default_model(&self) -> Option<String> {
        Some("echo".into())
    }

    fn model_info(&self, _model: &str) -> Option<ModelInfo> {
        Some(ModelInfo { context_window: 8_192, max_output: 1_024 })
    }
}
```

- **Tool calls** are `Content::ToolCall { id, name, input }` blocks in the reply's message, with `StopReason::ToolUse`. A `StreamEvent::ToolCallStarted` lets a UI show the call before its input has streamed.
- **Reasoning** goes in `Content::Thinking` blocks, and streams as `ReasoningDelta`.
- **Usage** should count what the model processed. When it reports nothing, the loop estimates the context at about 3.5 characters per token.
- **Abort**: when `abort` fires, end the stream soon with `StopReason::Aborted`, keeping what was generated.
- **A full context** is `Response::error(ErrorKind::ContextOverflow, message)`, so `kit::compaction::on_overflow` can compact and retry.

A provider can run its model anywhere, including in the same process. [rustling](https://github.com/onatm/rustling) does that: its `chat` command's provider sends requests to an inference engine thread and maps the engine's events to `StreamEvent`s, with no HTTP in between.

Tests use `manifatura::testing::ScriptedProvider` (`testing` feature), which replies from a script and records every request.
