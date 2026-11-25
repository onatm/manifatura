use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{Message, Usage};

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Outcome {
    /// The model finished.
    Completed,
    /// The turn failed; `turn.completed` says why.
    Failed,
    /// The turn was aborted.
    Aborted,
}

/// Live session events, following eve's vocabulary. Deltas exist only in memory; messages are
/// also in the session records.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum Event {
    /// A turn began.
    #[serde(rename = "turn.started")]
    TurnStarted {
        /// The turn's id.
        turn: String,
    },
    /// A model call is about to be made.
    #[serde(rename = "step.started")]
    StepStarted {
        /// The turn's id.
        turn: String,
        /// The model call's number in the turn, from 1.
        step: usize,
        /// Estimated input tokens for this step's request.
        context_tokens: u64,
        /// The model's context window in tokens; `None` when it is not known.
        context_window: Option<u32>,
    },
    /// A compaction was written; `turn` is empty outside turns.
    #[serde(rename = "context.compacted")]
    Compacted {
        /// The turn's id, or empty.
        turn: String,
        /// Estimated context tokens before the compaction.
        tokens_before: u64,
        /// Estimated context tokens after it.
        tokens_after: u64,
    },
    /// More of the answer's text, as it streams.
    #[serde(rename = "text.delta")]
    TextDelta {
        /// The turn's id.
        turn: String,
        /// The new text.
        text: String,
    },
    /// More of the model's reasoning, as it streams.
    #[serde(rename = "reasoning.delta")]
    ReasoningDelta {
        /// The turn's id.
        turn: String,
        /// The new text.
        text: String,
    },
    /// A message was recorded: the model's reply, or a batch of tool results.
    #[serde(rename = "message.appended")]
    MessageAppended {
        /// The turn's id.
        turn: String,
        /// The message, as recorded.
        message: Message,
        /// The model call's usage, for a reply; `None` for tool results.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// A tool call starts running.
    #[serde(rename = "action.started")]
    ActionStarted {
        /// The turn's id.
        turn: String,
        /// The call's id.
        call_id: String,
        /// The tool's name.
        name: String,
        /// The call's input, after `ToolCall` hooks.
        input: Value,
    },
    /// A line a running tool logged ([`ToolCx::log`](crate::ToolCx::log)). Sent to
    /// subscribers only, not to `Event` hooks.
    #[serde(rename = "action.log")]
    ActionLog {
        /// The turn's id.
        turn: String,
        /// The call's id.
        call_id: String,
        /// What the tool logged.
        text: String,
    },
    /// A tool call finished, or was denied.
    #[serde(rename = "action.result")]
    ActionResult {
        /// The turn's id.
        turn: String,
        /// The call's id.
        call_id: String,
        /// The tool's name.
        name: String,
        /// The result the model gets, after `ToolResult` hooks.
        output: String,
        /// Whether the call failed or was denied.
        is_error: bool,
    },
    /// A turn ended: the last event of every turn.
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        /// The turn's id.
        turn: String,
        /// How it ended.
        outcome: Outcome,
        /// Why it failed; `None` unless the outcome is [`Outcome::Failed`].
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// The tokens every model call of the turn used, added up.
        usage: Usage,
    },
}

impl Event {
    /// The id of the turn the event belongs to; empty for a compaction outside turns.
    pub fn turn(&self) -> &str {
        match self {
            Event::TurnStarted { turn }
            | Event::StepStarted { turn, .. }
            | Event::Compacted { turn, .. }
            | Event::TextDelta { turn, .. }
            | Event::ReasoningDelta { turn, .. }
            | Event::MessageAppended { turn, .. }
            | Event::ActionStarted { turn, .. }
            | Event::ActionLog { turn, .. }
            | Event::ActionResult { turn, .. }
            | Event::TurnCompleted { turn, .. } => turn,
        }
    }
}
