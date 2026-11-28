//! Compaction policy: a summarising [`Compactor`], and the hooks that decide when to use it.
//! The core only compacts when asked ([`Session::compact`](manifatura_agent::Session::compact),
//! [`HookCx::compact`](manifatura_agent::HookCx::compact)); these bundles are how an app asks.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use manifatura_agent::{
    CompactCx, CompactPlan, Compactor, Content, Effort, ErrorKind, Hooks, Message, ModelRequest,
    Role, StopReason, StreamEvent, estimate_tokens, hook,
};

/// Window assumed when neither the agent nor the provider knows one.
pub const DEFAULT_WINDOW: u32 = 128_000;

/// The instructions [`Summarizer`] gives the model.
pub const SUMMARY_PROMPT: &str = "You compact conversations between a user and an AI \
agent so the agent can continue the work with less context. Write a concise but complete \
summary for the agent: the user's goals and constraints, decisions made, files read or \
changed (with paths), commands run and their important results, errors met and how they were \
handled, and what remains to be done. Preserve exact names, paths, identifiers and numbers. \
If the transcript starts with an earlier summary, fold it in. Reply with the summary only.";

/// Asks a model to summarise the plan's messages as a plain-text transcript (long tool output
/// shortened first), folding in the previous summary.
#[derive(Clone, Debug)]
pub struct Summarizer {
    keep_recent: u64,
    model: Option<String>,
    max_tokens: u32,
}

impl Default for Summarizer {
    fn default() -> Self {
        Self {
            keep_recent: 20_000,
            model: None,
            max_tokens: 4_096,
        }
    }
}

impl Summarizer {
    /// For small context windows: keeps 6k recent tokens.
    pub fn small() -> Self {
        Self::default().keep_recent(6_000)
    }

    /// Recent context kept verbatim (default 20k tokens).
    pub fn keep_recent(mut self, tokens: u64) -> Self {
        self.keep_recent = tokens;
        self
    }

    /// The model that writes summaries (default: the session's).
    pub fn model(mut self, spec: impl Into<String>) -> Self {
        self.model = Some(spec.into());
        self
    }

    /// The longest summary, in tokens (default 4,096; also capped by the model's output limit).
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
}

#[async_trait]
impl Compactor for Summarizer {
    fn keep_recent_tokens(&self) -> u64 {
        self.keep_recent
    }

    async fn summarize(&self, plan: &CompactPlan, cx: &CompactCx) -> Result<String, String> {
        let info = cx.model_info();
        let window = info.map(|i| i.context_window).unwrap_or(DEFAULT_WINDOW);
        // Leave half the window for the summary prompt's own overhead and the reply.
        let text = transcript(&plan.messages, window as usize * 7 / 4);
        let mut ask =
            format!("Summarise this conversation transcript.\n\n<transcript>\n{text}</transcript>");
        if let Some(f) = plan.focus.as_deref().filter(|f| !f.trim().is_empty()) {
            ask.push_str(&format!("\n\nPay particular attention to: {f}"));
        }
        let model = self.model.clone().unwrap_or_else(|| cx.model().to_string());
        let max_tokens = info
            .map(|i| i.max_output.min(self.max_tokens))
            .unwrap_or(self.max_tokens);
        let req = ModelRequest::new(model, vec![Message::user(ask)])
            .system(SUMMARY_PROMPT)
            .max_tokens(max_tokens)
            .reasoning(Effort::Off);
        let mut stream = cx.provider().stream(req, cx.abort().clone());
        while let Some(ev) = stream.next().await {
            if let StreamEvent::Done(r) = ev {
                return match r.stop {
                    StopReason::Error(e) => Err(e.message),
                    StopReason::Aborted => Err("aborted".into()),
                    _ if r.message.text().trim().is_empty() => {
                        Err("the model returned an empty summary".into())
                    }
                    _ => Ok(r.message.text().trim().to_string()),
                };
            }
        }
        Err("the provider stream ended without a response".into())
    }
}

/// Compacts once per turn when the provider reports a context overflow, then retries the step.
pub fn on_overflow(compactor: Arc<dyn Compactor>) -> Hooks {
    Hooks::new().on(move |e: hook::ModelError, cx| {
        let compactor = compactor.clone();
        async move {
            if e.error.kind != ErrorKind::ContextOverflow {
                return hook::Recover::Fail;
            }
            // The turn this session last compacted in, so a second overflow fails instead of
            // looping. It is written before the retried step, which reads it.
            let turn = cx.turn_id().unwrap_or_default().to_string();
            if cx.state::<String>(OVERFLOW_TURN).as_deref() == Some(turn.as_str()) {
                return hook::Recover::Fail;
            }
            cx.set_state(OVERFLOW_TURN, turn);
            cx.compact(compactor, None);
            hook::Recover::Retry
        }
    })
}

/// The session state key [`on_overflow`] keeps the last turn it compacted in under.
const OVERFLOW_TURN: &str = "compaction.overflow_turn";

/// Compacts before the next model call once the context passes `threshold` of the window, for
/// apps that want compaction to happen by itself.
pub fn auto(threshold: f32, compactor: Arc<dyn Compactor>) -> Hooks {
    Hooks::new().on(move |e: hook::StepEnd, cx| {
        let compactor = compactor.clone();
        async move {
            let window = e.context_window.unwrap_or(DEFAULT_WINDOW);
            if e.context_tokens as f64 > f64::from(window) * f64::from(threshold) {
                cx.compact(compactor, None);
            }
        }
    })
}

/// In what is sent (the session keeps the originals): tool results longer than `max_chars`
/// before the most recent `keep_recent` tokens shrink to their first 200 characters and a note.
/// Results of tools that [keep their results](manifatura_agent::ToolBuilder::keep_results)
/// stay whole.
pub fn trim_results(keep_recent: u64, max_chars: usize) -> Hooks {
    Hooks::new().on(move |mut e: hook::Context, cx| async move {
        let end = recent_start(&e.messages, keep_recent);
        let kept: Vec<String> = e.messages[..end]
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::ToolCall { id, name, .. }
                    if cx.tool(name).is_some_and(|t| t.keeps_results()) =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for m in e.messages[..end]
            .iter_mut()
            .filter(|m| m.role == Role::Tool)
        {
            for c in &mut m.content {
                if let Content::ToolResult {
                    call_id, output, ..
                } = c
                    && output.len() > max_chars
                    && !output.contains(TRIM_MARK)
                    && !kept.contains(call_id)
                {
                    *output = trimmed(output);
                }
            }
        }
        e
    })
}

/// Which tool results [`drop_old_results`] leaves out of what is sent. The session keeps them all.
///
/// Results of the current turn and the `keep_turns` before it go whole; older ones become a
/// one-line stub, so the model knows the call ran and can run it again. A turn starts at each
/// user message. Short results, errors, and the results of tools that
/// [keep them](manifatura_agent::ToolBuilder::keep_results) always go whole.
///
/// A turn that grows past `ceiling` of the window drops its own oldest results too, down to
/// `floor`, in one batch: the point it dropped up to is kept in the session state, so later
/// steps send the same start and the provider's prompt cache keeps working until the next batch.
#[derive(Clone, Debug)]
pub struct DropResults {
    keep_turns: usize,
    keep_under: usize,
    ceiling: f32,
    floor: f32,
}

impl Default for DropResults {
    /// The current turn and the one before it; results under 1,000 characters; a batch past 70%
    /// of the window, down to 50%.
    fn default() -> Self {
        Self {
            keep_turns: 1,
            keep_under: 1_000,
            ceiling: 0.7,
            floor: 0.5,
        }
    }
}

impl DropResults {
    /// The turns before the current one whose results go whole (default 1).
    pub fn keep_turns(mut self, n: usize) -> Self {
        self.keep_turns = n;
        self
    }

    /// Results shorter than this many characters always go whole (default 1,000).
    pub fn keep_under(mut self, chars: usize) -> Self {
        self.keep_under = chars;
        self
    }

    /// The shares of the window at which a turn drops its own results, and down to which
    /// (default 0.7 and 0.5). Keep `ceiling` under the threshold of [`auto`], so results go
    /// before a summary is asked for.
    pub fn window(mut self, ceiling: f32, floor: f32) -> Self {
        self.ceiling = ceiling;
        self.floor = floor.min(ceiling);
        self
    }

    /// The `Context` hook.
    pub fn hooks(self) -> Hooks {
        Hooks::new().on(move |mut e: hook::Context, cx| {
            let policy = self.clone();
            async move {
                let messages = &mut e.messages;
                let names: HashMap<String, String> = messages
                    .iter()
                    .flat_map(|m| &m.content)
                    .filter_map(|c| match c {
                        Content::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                        _ => None,
                    })
                    .collect();
                let droppable = |call_id: &str, output: &str, is_error: bool| {
                    !is_error
                        && output.len() >= policy.keep_under
                        && !output.starts_with(DROP_MARK)
                        && !names
                            .get(call_id)
                            .and_then(|name| cx.tool(name))
                            .is_some_and(|t| t.keeps_results())
                };

                // Results before the turns kept.
                let starts: Vec<usize> = (0..messages.len())
                    .filter(|&i| messages[i].role == Role::User)
                    .collect();
                let boundary = starts
                    .len()
                    .checked_sub(policy.keep_turns + 1)
                    .map_or(0, |at| starts[at]);
                // The results the model has not seen yet are never dropped.
                let newest = messages
                    .iter()
                    .rposition(|m| m.role == Role::Tool)
                    .filter(|&i| i >= boundary)
                    .unwrap_or(messages.len());
                // The point a long turn dropped up to, if it is still in the context.
                let through: Option<String> = cx.state(DROPPED_THROUGH);
                let through = through
                    .and_then(|id| result_index(messages, &id))
                    .filter(|&i| i >= boundary && i < newest);

                let drop_at = |i: usize, messages: &mut Vec<Message>| -> u64 {
                    let before = estimate_tokens(&messages[i]);
                    for c in &mut messages[i].content {
                        if let Content::ToolResult {
                            call_id,
                            output,
                            is_error,
                        } = c
                            && droppable(call_id, output, *is_error)
                        {
                            let name = names.get(call_id).map_or("the tool", String::as_str);
                            *output = dropped(name, output.len());
                        }
                    }
                    before.saturating_sub(estimate_tokens(&messages[i]))
                };
                // Everything before `end` is dropped as it was in the last request.
                let end = through.map_or(boundary, |i| i + 1);
                for i in 0..end {
                    if messages[i].role == Role::Tool {
                        drop_at(i, messages);
                    }
                }

                // A long turn: the next batch, oldest first, down to the floor.
                let window = f64::from(cx.status().context_window.unwrap_or(DEFAULT_WINDOW));
                let overhead: u64 = cx
                    .tools()
                    .iter()
                    .map(|t| serde_json::to_string(t.spec()).map_or(0, |s| s.len() as u64 / 4))
                    .sum();
                let mut tokens = overhead + messages.iter().map(estimate_tokens).sum::<u64>();
                if tokens as f64 > window * f64::from(policy.ceiling) {
                    let goal = (window * f64::from(policy.floor)) as u64;
                    let mut point = None;
                    for i in end..newest {
                        if tokens <= goal {
                            break;
                        }
                        if messages[i].role == Role::Tool {
                            tokens = tokens.saturating_sub(drop_at(i, messages));
                            point = Some(i);
                        }
                    }
                    if let Some(id) = point.and_then(|i| last_call_id(&messages[i])) {
                        cx.set_state(DROPPED_THROUGH, id);
                    }
                }
                e
            }
        })
    }
}

/// The default [`DropResults`]: results of the current turn and the one before go whole.
pub fn drop_old_results() -> Hooks {
    DropResults::default().hooks()
}

/// The session state key [`DropResults`] keeps the call it last dropped a long turn's results
/// up to under.
const DROPPED_THROUGH: &str = "compaction.dropped_through";
const DROP_MARK: &str = "[output of ";

fn dropped(name: &str, chars: usize) -> String {
    format!(
        "{DROP_MARK}{name} dropped from the context: {chars} characters; call the tool again if you need it]"
    )
}

/// The index of the tool message holding the result of `call_id`.
fn result_index(messages: &[Message], call_id: &str) -> Option<usize> {
    messages.iter().position(|m| {
        m.role == Role::Tool
            && m.content
                .iter()
                .any(|c| matches!(c, Content::ToolResult { call_id: id, .. } if id == call_id))
    })
}

fn last_call_id(message: &Message) -> Option<String> {
    message.content.iter().rev().find_map(|c| match c {
        Content::ToolResult { call_id, .. } => Some(call_id.clone()),
        _ => None,
    })
}

const TRIM_KEEP_CHARS: usize = 200;
const TRIM_MARK: &str = "[tool output trimmed";

/// The index where the most recent `keep_recent` tokens start.
fn recent_start(messages: &[Message], keep_recent: u64) -> usize {
    let mut tokens = 0;
    for i in (0..messages.len()).rev() {
        tokens += estimate_tokens(&messages[i]);
        if tokens >= keep_recent {
            return i;
        }
    }
    0
}

fn trimmed(output: &str) -> String {
    let cut = output.floor_char_boundary(TRIM_KEEP_CHARS);
    format!(
        "{}…\n{TRIM_MARK} to save context: {} characters in total; run the tool again if you need it]",
        &output[..cut],
        output.len()
    )
}

/// A plain-text transcript for a summariser, capped at `max_chars` by shortening the longest
/// tool results first.
pub fn transcript(messages: &[Message], max_chars: usize) -> String {
    let mut out = String::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut limit = 2_000;
    loop {
        out.clear();
        for m in messages {
            for c in &m.content {
                match (m.role, c) {
                    (Role::System, Content::Text { text, .. }) => {
                        out.push_str(&format!("[system note]\n{text}\n\n"))
                    }
                    (Role::User, Content::Text { text, .. }) => {
                        out.push_str(&format!("[user]\n{text}\n\n"))
                    }
                    (_, Content::Text { text, .. }) => {
                        out.push_str(&format!("[assistant]\n{text}\n\n"))
                    }
                    (_, Content::ToolCall { id, name, input }) => {
                        names.insert(id.clone(), name.clone());
                        out.push_str(&format!(
                            "[assistant called {name}] {}\n\n",
                            clip(&input.to_string(), limit)
                        ));
                    }
                    (
                        _,
                        Content::ToolResult {
                            call_id,
                            output,
                            is_error,
                        },
                    ) => {
                        let name = names.get(call_id).map(String::as_str).unwrap_or("tool");
                        let kind = if *is_error { "error" } else { "result" };
                        out.push_str(&format!("[{name} {kind}]\n{}\n\n", clip(output, limit)));
                    }
                    _ => {}
                }
            }
        }
        if out.len() <= max_chars || limit <= 100 {
            if out.len() > max_chars {
                let keep = out.floor_char_boundary(max_chars);
                out.truncate(keep);
            }
            return out;
        }
        limit /= 2;
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let cut = s.floor_char_boundary(max);
    format!("{}… [{} more characters]", &s[..cut], s.len() - cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transcript_shrinks_tool_output_to_fit() {
        let msgs = vec![
            Message::user("hi"),
            Message::new(
                Role::Assistant,
                vec![Content::ToolCall {
                    id: "1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                }],
            ),
            Message::new(
                Role::Tool,
                vec![Content::ToolResult {
                    call_id: "1".into(),
                    output: "y".repeat(10_000),
                    is_error: false,
                }],
            ),
        ];
        let t = transcript(&msgs, 1_500);
        assert!(t.len() <= 1_500);
        assert!(t.starts_with("[user]\nhi"));
        assert!(t.contains("[bash result]"));
    }
}
