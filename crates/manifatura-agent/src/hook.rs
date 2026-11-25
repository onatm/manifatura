//! Hook events: one type per point in the loop.
//!
//! Register a hook with [`Hooks::on`](crate::Hooks::on) (or [`Agent::on`](crate::Agent::on),
//! [`RuntimeBuilder::on`](crate::RuntimeBuilder::on)); the event type in the closure's signature
//! picks the point:
//!
//! ```
//! use manifatura_agent::{Agent, hook};
//!
//! let agent = Agent::new("…").on(|e: hook::ToolCall, _cx| async move {
//!     if e.call.name == "bash" {
//!         return hook::Decision::Deny("no shell here".into());
//!     }
//!     hook::Decision::Run(e)
//! });
//! ```
//!
//! Events are passed by value and each hook returns what to use next:
//!
//! | Event | Returns | With several hooks |
//! | --- | --- | --- |
//! | [`SessionStart`] | `()` | all run |
//! | [`Input`] | `Option<Input>`; `None` drops the input | chained; `None` stops |
//! | [`TurnStart`] | `()` | all run |
//! | [`Step`] | `Step`: the instructions and tools for this model call | chained |
//! | [`Context`] | `Context`: the messages sent this step (not persisted) | chained |
//! | [`Request`] | `Request`: the final model request | chained |
//! | [`ModelError`] | [`Recover`] | the first `Retry` wins |
//! | [`Response`] | `Response`: the assistant message, persisted afterwards | chained |
//! | [`StepEnd`] | `()` | all run |
//! | [`ToolCall`] | [`Decision`] | chained; `Deny` and `Result` stop |
//! | [`ToolResult`] | `ToolResult`, persisted with the batch | chained |
//! | [`TurnEnd`] | [`Next`] | the first `FollowUp` wins |
//! | [`BeforeCompact`] | [`CompactDecision`] | chained; `Summary` and `Cancel` stop |
//! | [`Compacted`] | `()` | all run |
//! | [`Event`] | `()` | all run, inline: keep them quick, or use `Session::subscribe` |

use std::ops::ControlFlow;

use crate::compact::CompactPlan;
use crate::event::Event;
use crate::tool::Tool;
use crate::types::{Message, ModelRequest, ProviderError, StopReason, Usage};

/// A point in the loop that hooks can attach to: one of the types in this module. Sealed:
/// the loop decides where its points are.
pub trait HookEvent: sealed::Sealed + Sized + Send + 'static {
    /// What a hook returns.
    type Out: Send + 'static;

    /// A copy of the event for the next hook, for events where a hook's answer does not carry
    /// one (observers, first-wins events).
    #[doc(hidden)]
    fn keep(&self) -> Option<Self> {
        None
    }

    /// Given one hook's answer, either the event for the next hook or the final answer.
    #[doc(hidden)]
    fn next(out: Self::Out, kept: Option<Self>) -> ControlFlow<Self::Out, Self>;

    /// The answer when every hook has passed (or none is registered).
    #[doc(hidden)]
    fn finish(self) -> Self::Out;
}

mod sealed {
    pub trait Sealed {}
    macro_rules! sealed {
        ($($t:ty),*) => {$(impl Sealed for $t {})*};
    }
    sealed!(
        super::SessionStart,
        super::Input,
        super::TurnStart,
        super::Step,
        super::Context,
        super::Request,
        super::ModelError,
        super::Response,
        super::StepEnd,
        super::ToolCall,
        super::ToolResult,
        super::TurnEnd,
        super::BeforeCompact,
        super::Compacted,
        crate::event::Event
    );
}

macro_rules! transform {
    ($($t:ty),*) => {$(
        impl HookEvent for $t {
            type Out = $t;
            fn next(out: $t, _: Option<$t>) -> ControlFlow<$t, $t> {
                ControlFlow::Continue(out)
            }
            fn finish(self) -> $t {
                self
            }
        }
    )*};
}

macro_rules! observe {
    ($($t:ty),*) => {$(
        impl HookEvent for $t {
            type Out = ();
            fn keep(&self) -> Option<$t> {
                Some(self.clone())
            }
            fn next(_: (), kept: Option<$t>) -> ControlFlow<(), $t> {
                match kept {
                    Some(e) => ControlFlow::Continue(e),
                    None => ControlFlow::Break(()),
                }
            }
            fn finish(self) {}
        }
    )*};
}

/// The session was opened in this process: `resumed` when it already had entries.
#[derive(Clone, Debug)]
pub struct SessionStart {
    /// Whether the session already had entries in its store.
    pub resumed: bool,
}

/// A prompt, before it becomes a user message. Return `None` to drop it (the turn then
/// completes without a model call), or change `text`.
#[derive(Clone, Debug)]
pub struct Input {
    /// The prompt's text.
    pub text: String,
}

impl HookEvent for Input {
    type Out = Option<Input>;
    fn next(out: Option<Input>, _: Option<Input>) -> ControlFlow<Option<Input>, Input> {
        match out {
            Some(i) => ControlFlow::Continue(i),
            None => ControlFlow::Break(None),
        }
    }
    fn finish(self) -> Option<Input> {
        Some(self)
    }
}

/// A turn starts, after its user message was recorded.
#[derive(Clone, Debug)]
pub struct TurnStart;

/// Before each model call: the instructions (system prompt) and tools to offer. Starts as the
/// agent's own; a hook may change both, e.g. to add a tool once some state is set.
#[derive(Clone, Debug)]
pub struct Step {
    /// The system prompt for this call.
    pub instructions: String,
    /// The tools for this call. Deferred ones are not offered to the model, but run when a
    /// call names them.
    pub tools: Vec<Tool>,
}

impl Step {
    /// The step with `tool` offered too.
    pub fn tool(mut self, tool: impl AsRef<Tool>) -> Self {
        self.tools.push(tool.as_ref().clone());
        self
    }

    /// The step without the tool called `name`.
    pub fn without_tool(mut self, name: &str) -> Self {
        self.tools.retain(|t| t.name() != name);
        self
    }

    /// Whether a tool called `name` is offered in this step.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name() == name)
    }
}

/// The messages sent with this step's request. Changes apply to this request only; the
/// session keeps the originals.
#[derive(Clone, Debug)]
pub struct Context {
    /// The messages, oldest first.
    pub messages: Vec<Message>,
}

/// The request as it will be sent.
#[derive(Clone, Debug)]
pub struct Request {
    /// The request the provider gets.
    pub request: ModelRequest,
}

/// The model call failed. `attempt` counts failures in this turn, from 1.
#[derive(Clone, Debug)]
pub struct ModelError {
    /// What the provider reported.
    pub error: ProviderError,
    /// Failures so far in this turn, counting this one.
    pub attempt: usize,
}

/// What to do after a failed model call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recover {
    /// Fail the turn with the error.
    Fail,
    /// Run the step again, after any compaction a hook asked for
    /// ([`HookCx::compact`](crate::HookCx::compact)).
    Retry,
}

impl HookEvent for ModelError {
    type Out = Recover;
    fn keep(&self) -> Option<Self> {
        Some(self.clone())
    }
    fn next(out: Recover, kept: Option<Self>) -> ControlFlow<Recover, Self> {
        match (out, kept) {
            (Recover::Fail, Some(e)) => ControlFlow::Continue(e),
            (out, _) => ControlFlow::Break(out),
        }
    }
    fn finish(self) -> Recover {
        Recover::Fail
    }
}

/// The model's response, before it is recorded.
#[derive(Clone, Debug)]
pub struct Response {
    /// The assistant message: text, reasoning and tool calls.
    pub message: Message,
    /// Tokens the call used, as the provider reported them.
    pub usage: Usage,
    /// Why the model stopped.
    pub stop: StopReason,
}

/// A step finished: its usage, and the estimated size of the context against the window.
#[derive(Clone, Debug)]
pub struct StepEnd {
    /// Tokens the call used, as the provider reported them.
    pub usage: Usage,
    /// Estimated input tokens of the context now, including the response.
    pub context_tokens: u64,
    /// The model's context window, when known.
    pub context_window: Option<u32>,
}

/// A tool call the model made, before it runs. `tool` is the tool it names, when there is
/// one. A hook that changes `call.name` sets `tool` to match, as later hooks read it.
#[derive(Clone, Debug)]
pub struct ToolCall {
    /// The call: its id, the tool's name and the input.
    pub call: crate::types::ToolCall,
    /// The tool `call.name` names in this step; `None` for an unknown name.
    pub tool: Option<Tool>,
}

/// What happens to a tool call.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Decision {
    /// Run it (possibly changed: another tool, other input; the call id is kept).
    Run(ToolCall),
    /// Do not run it; the reason goes back to the model as a tool error.
    Deny(String),
    /// Do not run it; use this result instead.
    Result(crate::tool::ToolResult),
}

impl HookEvent for ToolCall {
    type Out = Decision;
    fn next(out: Decision, _: Option<Self>) -> ControlFlow<Decision, Self> {
        match out {
            Decision::Run(e) => ControlFlow::Continue(e),
            d => ControlFlow::Break(d),
        }
    }
    fn finish(self) -> Decision {
        Decision::Run(self)
    }
}

/// A tool call's result, before it is recorded. Also runs for denied and unknown calls.
#[derive(Clone, Debug)]
pub struct ToolResult {
    /// The call the result answers.
    pub call: crate::types::ToolCall,
    /// The result the model will see.
    pub result: crate::tool::ToolResult,
}

/// The model finished the turn (it stopped without tool calls, or a tool ended the turn).
/// Not run when a turn fails or is aborted.
#[derive(Clone, Debug)]
pub struct TurnEnd {
    /// The model's last message.
    pub message: Message,
    /// Tokens used over the whole turn.
    pub usage: Usage,
}

/// What happens when the model finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Next {
    /// End the turn.
    Done,
    /// Append this as a user message and keep the turn going.
    FollowUp(String),
}

impl HookEvent for TurnEnd {
    type Out = Next;
    fn keep(&self) -> Option<Self> {
        Some(self.clone())
    }
    fn next(out: Next, kept: Option<Self>) -> ControlFlow<Next, Self> {
        match (out, kept) {
            (Next::Done, Some(e)) => ControlFlow::Continue(e),
            (out, _) => ControlFlow::Break(out),
        }
    }
    fn finish(self) -> Next {
        Next::Done
    }
}

/// A compaction is about to summarise `plan.messages`.
#[derive(Clone, Debug)]
pub struct BeforeCompact {
    /// What would be summarised; a hook may change the plan.
    pub plan: CompactPlan,
}

/// What happens to a compaction.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CompactDecision {
    /// Summarise (possibly a changed plan, e.g. another focus).
    Proceed(BeforeCompact),
    /// Use this summary; the compactor is not called.
    Summary(String),
    /// Do not compact.
    Cancel,
}

impl HookEvent for BeforeCompact {
    type Out = CompactDecision;
    fn next(out: CompactDecision, _: Option<Self>) -> ControlFlow<CompactDecision, Self> {
        match out {
            CompactDecision::Proceed(e) => ControlFlow::Continue(e),
            d => ControlFlow::Break(d),
        }
    }
    fn finish(self) -> CompactDecision {
        CompactDecision::Proceed(self)
    }
}

/// A compaction was written.
#[derive(Clone, Debug)]
pub struct Compacted {
    /// The summary that now stands in for the earlier context.
    pub summary: String,
    /// The entry the kept context starts at.
    pub first_kept: String,
    /// Estimated input tokens before compacting.
    pub tokens_before: u64,
    /// Estimated input tokens after.
    pub tokens_after: u64,
}

transform!(Step, Context, Request, Response, ToolResult);
observe!(SessionStart, TurnStart, StepEnd, Compacted, Event);
