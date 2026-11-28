//! Provider-neutral message and request types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a text block of the model's is for (OpenAI's Responses API calls it the `phase`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Phase {
    /// An update while it works, before tool calls.
    Commentary,
    /// Its answer for the turn.
    FinalAnswer,
}

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
        /// What the text is for, when the provider says: an update while the model works, or
        /// its final answer. Providers that distinguish them need it back on later requests.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<Phase>,
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

impl Content {
    /// A text block without a phase.
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text {
            text: text.into(),
            phase: None,
        }
    }
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
        Self::new(Role::User, vec![Content::text(text)])
    }

    /// An assistant message of one text block.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, vec![Content::text(text)])
    }

    /// A [`Role::System`] note of one text block.
    pub fn system_note(text: impl Into<String>) -> Self {
        Self::new(Role::System, vec![Content::text(text)])
    }

    /// Concatenated text blocks.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for c in &self.content {
            if let Content::Text { text, .. } = c {
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

/// What the model is told about a tool. Build one with [`ToolSpec::new`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolSpec {
    /// The name the model calls it by.
    pub name: String,
    /// What it does, written for the model.
    pub description: String,
    /// JSON Schema for the tool input; always an object schema.
    pub input_schema: Value,
    /// How the model writes the input: a JSON object (the default) or free text.
    #[serde(default, skip_serializing_if = "ToolFormat::is_json")]
    pub format: ToolFormat,
}

impl ToolSpec {
    /// A tool whose input is a JSON object matching `input_schema`.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            format: ToolFormat::Json,
        }
    }

    /// A tool whose input is free text, constrained by `grammar` if given. Its `input_schema`
    /// is an object with one string field, `input` ([`TEXT_INPUT`]).
    pub fn text(
        name: impl Into<String>,
        description: impl Into<String>,
        grammar: Option<Grammar>,
    ) -> Self {
        Self {
            format: ToolFormat::Text { grammar },
            ..Self::new(name, description, text_input_schema())
        }
    }
}

/// The field a text tool's input arrives in: `{"input": "…"}`.
pub const TEXT_INPUT: &str = "input";

/// The input schema of a text tool: an object with one required string field, `input`.
pub fn text_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": { TEXT_INPUT: { "type": "string" } },
        "required": [TEXT_INPUT],
        "additionalProperties": false,
    })
}

/// How the model writes a tool's input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolFormat {
    /// A JSON object matching the tool's `input_schema`.
    #[default]
    Json,
    /// Free text, such as a patch, optionally constrained by a grammar. A provider that
    /// supports free-form tools (OpenAI's Responses API) lets the model write the text raw;
    /// the others offer the tool's `input_schema`, an object with one string field. Either
    /// way, the call reaches hooks and the tool as `{"input": text}`.
    Text {
        /// The grammar the text must follow, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grammar: Option<Grammar>,
    },
}

impl ToolFormat {
    /// Whether it is [`ToolFormat::Json`].
    pub fn is_json(&self) -> bool {
        *self == ToolFormat::Json
    }

    /// Whether it is [`ToolFormat::Text`].
    pub fn is_text(&self) -> bool {
        matches!(self, ToolFormat::Text { .. })
    }
}

/// A grammar for a text tool's input, which a provider that supports it enforces while the
/// model writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grammar {
    /// The grammar's notation.
    pub syntax: GrammarSyntax,
    /// The grammar itself.
    pub definition: String,
}

impl Grammar {
    /// A [Lark](https://lark-parser.readthedocs.io) grammar.
    pub fn lark(definition: impl Into<String>) -> Self {
        Self {
            syntax: GrammarSyntax::Lark,
            definition: definition.into(),
        }
    }

    /// A regular expression the whole input must match.
    pub fn regex(definition: impl Into<String>) -> Self {
        Self {
            syntax: GrammarSyntax::Regex,
            definition: definition.into(),
        }
    }
}

/// A grammar's notation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GrammarSyntax {
    /// Lark.
    Lark,
    /// A regular expression.
    Regex,
}

impl GrammarSyntax {
    /// The name providers use: `lark` or `regex`.
    pub fn as_str(self) -> &'static str {
        match self {
            GrammarSyntax::Lark => "lark",
            GrammarSyntax::Regex => "regex",
        }
    }
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
    /// More than high, where the model has it.
    #[serde(rename = "xhigh")]
    XHigh,
    /// As much as the model's usual reasoning allows, where it has it.
    Max,
    /// Beyond max, where the model has it.
    Ultra,
}

impl Effort {
    /// Every level, from none to the most.
    pub const ALL: [Effort; 7] = [
        Effort::Off,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
        Effort::Ultra,
    ];

    /// The name OpenAI-style APIs use: `none`, `low`, `medium`, `high`, `xhigh`, `max` or `ultra`.
    /// Not every model has every level; a provider's model list says which it has.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Off => "none",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
            Effort::Ultra => "ultra",
        }
    }

    /// The level named `name`, as [`as_str`](Self::as_str) writes it (`off` for none too), in any case.
    pub fn parse(name: &str) -> Option<Effort> {
        let name = name.trim().to_ascii_lowercase();
        if name == "off" {
            return Some(Effort::Off);
        }
        Effort::ALL.into_iter().find(|e| e.as_str() == name)
    }
}

/// Whether the model may call the tools offered in a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolChoice {
    /// The model decides: what a provider does when it is told nothing.
    #[default]
    Auto,
    /// No tool calls: the model answers in text. The tools are still declared, so a provider's
    /// cached prompt prefix still matches.
    Off,
}

impl ToolChoice {
    /// The name OpenAI-compatible APIs use: `auto` or `none`.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolChoice::Auto => "auto",
            ToolChoice::Off => "none",
        }
    }

    /// Whether it is [`ToolChoice::Auto`], which a provider need not send.
    pub fn is_auto(&self) -> bool {
        *self == ToolChoice::Auto
    }
}

/// One model call, as a provider receives it.
///
/// Build one with [`ModelRequest::new`] and the setters for what the call uses, so a call
/// without tools never mentions them. The fields are public: providers read them, hooks change
/// them, and a value that is already an `Option` (an agent's output limit, say) is assigned to
/// its field directly.
///
/// ```
/// use manifatura_agent::{Effort, Message, ModelRequest};
///
/// let req = ModelRequest::new("local/model", vec![Message::user("Summarise this.")])
///     .system("You write short summaries.")
///     .max_tokens(512)
///     .reasoning(Effort::Off);
/// assert!(req.tools.is_empty());
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelRequest {
    /// Model id. Through `manifatura-providers`' `Models` this is `provider/model-id`; a
    /// concrete provider receives only the model id.
    pub model: String,
    /// The system prompt; empty for none.
    pub system: String,
    /// The conversation so far, oldest first.
    pub messages: Vec<Message>,
    /// The tools offered for this call; deferred tools are not among them.
    pub tools: Vec<ToolSpec>,
    /// The most output tokens to produce; `None` leaves it to the provider.
    pub max_tokens: Option<u32>,
    /// How much to reason; `None` leaves it to the model.
    pub reasoning: Option<Effort>,
    /// Whether the model may call the tools. A [`Request`](crate::hook::Request) hook sets it
    /// for one call; a provider that cannot keep a model from calling tools may ignore it.
    #[serde(default, skip_serializing_if = "ToolChoice::is_auto")]
    pub tool_choice: ToolChoice,
}

impl ModelRequest {
    /// A request for `model` with this conversation: no system prompt, no tools, and output
    /// length and reasoning left to the provider.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            system: String::new(),
            messages,
            tools: Vec::new(),
            max_tokens: None,
            reasoning: None,
            tool_choice: ToolChoice::Auto,
        }
    }

    /// The system prompt.
    pub fn system(mut self, text: impl Into<String>) -> Self {
        self.system = text.into();
        self
    }

    /// The tools offered.
    pub fn tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    /// The most output tokens to produce.
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// How much to reason.
    pub fn reasoning(mut self, effort: Effort) -> Self {
        self.reasoning = Some(effort);
        self
    }

    /// Whether the model may call the tools.
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = choice;
        self
    }
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
    /// A message of the answer began; its text follows as [`StreamEvent::TextDelta`]s. `phase` says what it is
    /// for, when the provider says (OpenAI's Responses API does).
    MessageStarted {
        /// A progress update or the final answer.
        phase: Option<Phase>,
    },
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
