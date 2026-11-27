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
        let req = ModelRequest {
            model: self.model.clone().unwrap_or_else(|| cx.model().to_string()),
            system: SUMMARY_PROMPT.into(),
            messages: vec![Message::user(ask)],
            tools: Vec::new(),
            max_tokens: Some(
                info.map(|i| i.max_output.min(self.max_tokens))
                    .unwrap_or(self.max_tokens),
            ),
            reasoning: Some(Effort::Off),
        };
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
                    (Role::System, Content::Text { text }) => {
                        out.push_str(&format!("[system note]\n{text}\n\n"))
                    }
                    (Role::User, Content::Text { text }) => {
                        out.push_str(&format!("[user]\n{text}\n\n"))
                    }
                    (_, Content::Text { text }) => {
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
