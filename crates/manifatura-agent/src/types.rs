//! Provider-neutral message and request types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who a message is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A harness notice inside the transcript (tool set or instruction changes). Providers send
    /// it as user-role text so the system prompt prefix stays cacheable.
    System,
    /// The person, or the app speaking for them.
    User,
    /// The model.
    Assistant,
    /// Tool results for the preceding assistant message.
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// One block of a message.
#[non_exhaustive]
pub enum Content {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// The model's reasoning before it answers.
    Thinking {
        /// The reasoning, as the model wrote it.
        text: String,
        /// An opaque token some providers need to accept the reasoning back; `None` when the
        /// provider sends none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// A call the model made.
    ToolCall {
        /// The call's id, which its result refers to.
        id: String,
        /// The tool's name.
        name: String,
        /// The input, as JSON.
        input: Value,
    },
    /// The result of a call, in a [`Role::Tool`] message.
    ToolResult {
        /// The id of the call this answers.
        call_id: String,
        /// What the tool returned, as the model sees it.
        output: String,
        /// Whether the call failed (or was denied).
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    /// Base64-encoded image data.
    Image {
        /// The image's MIME type, e.g. `image/png`.
        media_type: String,
        /// The image, base64-encoded.
        data: String,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A message in the conversation: who it is from and its blocks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who it is from.
    pub role: Role,
    /// Its blocks, in order.
    pub content: Vec<Content>,
}

impl Message {
    /// A message of any role and blocks.
    pub fn new(role: Role, content: Vec<Content>) -> Self {
        Self { role, content }
    }

    /// A user message of one text block.
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![Content::Text { text: text.into() }])
    }

    /// An assistant message of one text block.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, vec![Content::Text { text: text.into() }])
    }

    /// A [`Role::System`] note of one text block.
    pub fn system_note(text: impl Into<String>) -> Self {
        Self::new(Role::System, vec![Content::Text { text: text.into() }])
    }

    /// Concatenated text blocks.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for c in &self.content {
            if let Content::Text { text } = c {
                out.push_str(text);
            }
        }
        out
    }

    /// The calls in this message, in order; empty when it makes none.
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|c| match c {
                Content::ToolCall { id, name, input } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// How providers present a [`Role::System`] note inside the conversation.
pub fn system_note_text(text: &str) -> String {
    format!("<system-notice>\n{text}\n</system-notice>")
}

/// A call the model made, as the loop and hooks handle it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The call's id, unique in its message; its result refers to it.
    pub id: String,
    /// The tool's name.
    pub name: String,
    /// The input, as JSON.
    pub input: Value,
}

/// What the model is told about a tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The name the model calls it by.
    pub name: String,
    /// What it does, written for the model.
    pub description: String,
    /// JSON Schema for the tool input; always an object schema.
    pub input_schema: Value,
}

/// Tokens a model call used, as the provider reports them; zeros when it reports none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens processed afresh, not counting those read from or written to a cache.
    pub input: u64,
    /// Output tokens, reasoning included.
    pub output: u64,
    /// Input tokens read from the provider's prompt cache.
    #[serde(default)]
    pub cache_read: u64,
    /// Input tokens written to the provider's prompt cache.
    #[serde(default)]
    pub cache_write: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
}

/// What the runtime needs to know about a model to manage its context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Input plus output tokens the model accepts.
    pub context_window: u32,
    /// The largest output the model produces.
    pub max_output: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// How much the model reasons before it answers.
#[non_exhaustive]
pub enum Effort {
    /// Ask the model not to reason, where the API allows it.
    Off,
    /// A little.
    Low,
    /// Some.
    Medium,
    /// Much.
    High,
}

impl Effort {
    /// The name OpenAI-style APIs use: `none`, `low`, `medium` or `high`.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Off => "none",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

/// One model call, as a provider receives it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Model id. Through `manifatura-models`' `Models` this is `provider/model-id`; a
    /// concrete provider receives only the model id.
    pub model: String,
    /// The system prompt.
    pub system: String,
    /// The conversation so far, oldest first.
    pub messages: Vec<Message>,
    /// The tools offered for this call; deferred tools are not among them.
    pub tools: Vec<ToolSpec>,
    /// The most output tokens to produce; `None` leaves it to the provider.
    pub max_tokens: Option<u32>,
    /// How much to reason; `None` leaves it to the model.
    pub reasoning: Option<Effort>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// What kind of failure a model call had.
#[non_exhaustive]
pub enum ErrorKind {
    /// The request did not fit the model's context window. A `ModelError` hook may compact and
    /// retry.
    ContextOverflow,
    /// The key is missing, wrong or not allowed this model.
    Auth,
    /// Too many requests; trying later may work.
    RateLimited,
    /// The provider failed on its side.
    Server,
    /// The provider could not be reached, or the connection broke.
    Network,
    /// The provider rejected the request as malformed.
    InvalidRequest,
    /// Anything else.
    Other,
}

/// A failed model call: its kind and the provider's message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderError {
    /// What kind of failure it was.
    pub kind: ErrorKind,
    /// What the provider said, for people.
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Why the model stopped.
#[non_exhaustive]
pub enum StopReason {
    /// It finished its message.
    Stop,
    /// It stopped to have its tool calls run.
    ToolUse,
    /// The output token limit cut the message off.
    Length,
    /// The call was aborted; the message holds what arrived before.
    Aborted,
    /// The call failed.
    Error(ProviderError),
}

/// What a model call produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// The assistant message; empty when the call failed.
    pub message: Message,
    /// Why the model stopped.
    pub stop: StopReason,
    /// The tokens the call used.
    pub usage: Usage,
}

impl Response {
    /// A failed call: an empty message and [`StopReason::Error`].
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            message: Message::new(Role::Assistant, Vec::new()),
            stop: StopReason::Error(ProviderError {
                kind,
                message: message.into(),
            }),
            usage: Usage::default(),
        }
    }
}

/// One item of a provider stream. Every stream ends with exactly one [`StreamEvent::Done`];
/// failures are reported there as [`StopReason::Error`], never as a stream error.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum StreamEvent {
    /// More of the answer's text.
    TextDelta(String),
    /// More of the model's reasoning.
    ReasoningDelta(String),
    /// The model began a tool call; its input follows in the final response.
    ToolCallStarted {
        /// The call's id.
        id: String,
        /// The tool's name.
        name: String,
    },
    /// The call ended, with its complete response.
    Done(Response),
}
