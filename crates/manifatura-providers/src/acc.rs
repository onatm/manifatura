//! Accumulates streamed blocks into the final assistant message.

use manifatura_agent::{
    Content, ErrorKind, Message, ProviderError, Response, Role, StopReason, TEXT_INPUT, Usage,
};
use serde_json::Value;

pub(crate) enum Block {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    Tool {
        id: String,
        name: String,
        /// JSON arguments, or the raw input of a free-form call.
        args: String,
        /// A free-form call: `args` is text, which the call carries as `{"input": text}`.
        text: bool,
    },
}

#[derive(Default)]
pub(crate) struct Acc {
    pub blocks: Vec<Block>,
    pub usage: Usage,
    pub stop: Option<StopReason>,
}

impl Acc {
    /// Appends text to the last block if it is text, else starts a text block.
    pub fn text(&mut self, t: &str) {
        match self.blocks.last_mut() {
            Some(Block::Text(s)) => s.push_str(t),
            _ => self.blocks.push(Block::Text(t.to_string())),
        }
    }

    pub fn thinking(&mut self, t: &str) {
        match self.blocks.last_mut() {
            Some(Block::Thinking { text, .. }) => text.push_str(t),
            _ => self.blocks.push(Block::Thinking {
                text: t.to_string(),
                signature: None,
            }),
        }
    }

    pub fn has_tools(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::Tool { .. }))
    }

    /// The final response. `end` overrides the stop reason (abort, network failure); with no
    /// stop reason at all, the stream ended early.
    pub fn finish(self, end: Option<StopReason>) -> Response {
        let stop = end.or(self.stop).unwrap_or_else(|| {
            StopReason::Error(ProviderError {
                kind: ErrorKind::Network,
                message: "the stream ended before the response finished".into(),
            })
        });
        let content = self
            .blocks
            .into_iter()
            .filter_map(|b| match b {
                Block::Text(text) if !text.is_empty() => Some(Content::Text { text }),
                Block::Text(_) => None,
                Block::Thinking { text, signature } => (!text.is_empty() || signature.is_some())
                    .then_some(Content::Thinking { text, signature }),
                Block::Tool {
                    id,
                    name,
                    args,
                    text,
                } => Some(Content::ToolCall {
                    id,
                    name,
                    input: if text {
                        serde_json::json!({ TEXT_INPUT: args })
                    } else {
                        parse_args(&args)
                    },
                }),
            })
            .collect();
        Response {
            message: Message::new(Role::Assistant, content),
            stop,
            usage: self.usage,
        }
    }
}

/// Empty arguments mean `{}`. Invalid JSON is kept as a string, which the loop reports back
/// to the model as a tool error.
fn parse_args(args: &str) -> Value {
    if args.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(args).unwrap_or_else(|_| Value::String(args.to_string()))
}
