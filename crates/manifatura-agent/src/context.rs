//! The context: a projection of the session tree.
//!
//! The projection walks the path from the leaf to the root and folds tree records in order:
//! messages, state and compactions (a summary replacing everything before `first_kept`). Meta
//! records (model changes, labels) apply in log order wherever they sit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::record::{Entry, Record};
use crate::types::{Content, Effort, Message, Role, ToolSpec};

pub(crate) const SUMMARY_PREFIX: &str =
    "The earlier part of this conversation was compacted. Summary of it:\n\n";

pub(crate) const INTERRUPTED: &str = "No result was recorded for this call: the process stopped \
during the tool batch. It may have partially run; check its effects before retrying.";

#[derive(Default)]
pub(crate) struct Live {
    pub leaf: Option<String>,
    pub messages: Vec<Message>,
    /// The entry id of each message; empty for messages the projection synthesised.
    pub ids: Vec<String>,
    /// The estimated tokens of each message, kept as messages come and go, so measuring the
    /// context never walks it.
    tokens: Vec<u64>,
    pub state: Arc<BTreeMap<String, Value>>,
    pub model: Option<String>,
    pub reasoning: Option<Effort>,
    pub label: Option<String>,
    /// The latest summary in the context.
    pub summary: Option<String>,
    /// Provider-reported tokens for the context up to `.1` messages.
    pub measured: Option<(u64, usize)>,
}

/// The tree entries from the first to `leaf`, without meta records.
pub(crate) fn path_to<'a>(entries: &'a [Entry], leaf: Option<&str>) -> Vec<&'a Entry> {
    let by_id: HashMap<&str, &Entry> = entries
        .iter()
        .filter(|e| !e.record.is_meta())
        .map(|e| (e.id.as_str(), e))
        .collect();
    let mut path = Vec::new();
    let mut cur = leaf.and_then(|l| by_id.get(l).copied());
    while let Some(e) = cur {
        path.push(e);
        cur = e.parent_id.as_deref().and_then(|p| by_id.get(p).copied());
    }
    path.reverse();
    path
}

impl Live {
    /// The context at the last tree entry.
    pub fn project(entries: &[Entry]) -> Self {
        let leaf = entries
            .iter()
            .rev()
            .find(|e| !e.record.is_meta())
            .map(|e| e.id.as_str());
        Self::project_at(entries, leaf)
    }

    /// The context at `leaf` (`None`: an empty context whose next entry starts a new root).
    pub fn project_at(entries: &[Entry], leaf: Option<&str>) -> Self {
        let mut live = Live::default();
        for e in entries.iter().filter(|e| e.record.is_meta()) {
            live.apply(e);
        }
        for e in path_to(entries, leaf) {
            live.apply(e);
        }
        live.repair_dangling();
        live
    }

    /// Folds one entry into the context. Used both for projection and for every append.
    pub fn apply(&mut self, e: &Entry) {
        if !e.record.is_meta() {
            self.leaf = Some(e.id.clone());
        }
        match &e.record {
            Record::Message { message } => self.push(message.clone(), e.id.clone()),
            Record::State { key, value } => {
                let state = Arc::make_mut(&mut self.state);
                if value.is_null() {
                    state.remove(key);
                } else {
                    state.insert(key.clone(), value.clone());
                }
            }
            Record::Compaction {
                summary,
                first_kept,
                ..
            } => {
                if let Some(i) = self.ids.iter().position(|id| id == first_kept) {
                    self.messages.drain(..i);
                    self.ids.drain(..i);
                    self.tokens.drain(..i);
                    let head = Message::user(format!("{SUMMARY_PREFIX}{summary}"));
                    self.tokens.insert(0, estimate_tokens(&head));
                    self.messages.insert(0, head);
                    self.ids.insert(0, e.id.clone());
                    self.summary = Some(summary.clone());
                    self.measured = None;
                }
            }
            Record::ModelChange { model, reasoning } => {
                if model.is_some() {
                    self.model = model.clone();
                }
                if reasoning.is_some() {
                    self.reasoning = *reasoning;
                }
            }
            Record::Label { name } => self.label = Some(name.clone()),
            Record::Header { .. } | Record::Custom { .. } => {}
        }
    }

    fn push(&mut self, message: Message, id: String) {
        self.tokens.push(estimate_tokens(&message));
        self.messages.push(message);
        self.ids.push(id);
    }

    /// Gives every tool call a result, so a history cut off mid-batch (the process stopped) is
    /// still a valid conversation for the provider.
    fn repair_dangling(&mut self) {
        let messages = std::mem::take(&mut self.messages);
        let ids = std::mem::take(&mut self.ids);
        self.tokens.clear();
        let mut it = messages.into_iter().zip(ids).peekable();
        while let Some((m, id)) = it.next() {
            let calls = if m.role == Role::Assistant {
                m.tool_calls()
            } else {
                Vec::new()
            };
            self.push(m, id);
            if calls.is_empty() {
                continue;
            }
            let (mut results, rid) = match it.peek() {
                Some((n, _)) if n.role == Role::Tool => it.next().unwrap(),
                _ => (Message::new(Role::Tool, Vec::new()), String::new()),
            };
            let have: HashSet<String> = results
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::ToolResult { call_id, .. } => Some(call_id.clone()),
                    _ => None,
                })
                .collect();
            for c in calls.iter().filter(|c| !have.contains(&c.id)) {
                results.content.push(Content::ToolResult {
                    call_id: c.id.clone(),
                    output: INTERRUPTED.into(),
                    is_error: true,
                });
            }
            self.push(results, rid);
        }
    }

    /// Estimated input tokens for a request whose system prompt and tools come to `prompt`
    /// tokens (see [`prompt_tokens`]).
    pub fn estimate(&self, prompt: u64) -> u64 {
        debug_assert_eq!(self.tokens.len(), self.messages.len());
        if let Some((tokens, n)) = self.measured
            && n <= self.messages.len()
        {
            return tokens + self.tokens[n..].iter().sum::<u64>();
        }
        prompt + self.messages_tokens()
    }

    /// Estimated tokens of the messages alone.
    pub fn messages_tokens(&self) -> u64 {
        self.tokens.iter().sum()
    }

    /// The index where the recent window starts: the suffix from it holds at least
    /// `keep_recent` tokens (or is the whole context).
    fn recent_start(&self, keep_recent: u64) -> usize {
        let mut tokens = 0;
        for i in (0..self.tokens.len()).rev() {
            tokens += self.tokens[i];
            if tokens >= keep_recent {
                return i;
            }
        }
        0
    }

    /// Where a summary may end: the start of the recent window, moved back so it never
    /// separates tool results from the call that produced them. `None` when nothing before it
    /// is worth summarising.
    pub fn cut_point(&self, keep_recent: u64) -> Option<usize> {
        let mut cut = self.recent_start(keep_recent);
        while cut > 0 && self.messages[cut].role == Role::Tool {
            cut -= 1;
        }
        // Index 0 is the previous summary or the first message: summarising only it gains nothing.
        (cut >= 2 && !self.ids[cut].is_empty()).then_some(cut)
    }
}

/// Estimated tokens of a system prompt and the tools offered with it.
pub(crate) fn prompt_tokens(system: &str, tools: &[ToolSpec]) -> u64 {
    let tools: usize = tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
        .sum();
    chars_to_tokens(system.len() + tools)
}

fn chars_to_tokens(chars: usize) -> u64 {
    // About 3.5 characters per token for English, code and JSON alike, rounded up.
    (chars as u64 * 2).div_ceil(7)
}

/// Estimated tokens of one message: about 3.5 characters per token, plus a little framing.
pub fn estimate_tokens(m: &Message) -> u64 {
    let chars: usize = m
        .content
        .iter()
        .map(|c| match c {
            Content::Text { text, .. } => text.len(),
            Content::Thinking { text, .. } => text.len(),
            Content::ToolCall { name, input, .. } => name.len() + input.to_string().len(),
            Content::ToolResult { output, .. } => output.len(),
            Content::Image { .. } => 6_000,
        })
        .sum();
    chars_to_tokens(chars) + 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Entry;

    fn chain(records: Vec<Record>) -> Vec<Entry> {
        let mut out: Vec<Entry> = Vec::new();
        let mut parent = None;
        for r in records {
            let meta = r.is_meta();
            let e = Entry::new(if meta { None } else { parent.clone() }, r);
            if !meta {
                parent = Some(e.id.clone());
            }
            out.push(e);
        }
        out
    }

    fn msg(m: Message) -> Record {
        Record::Message { message: m }
    }

    fn call(id: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: id.into(),
                name: "read".into(),
                input: serde_json::json!({}),
            }],
        )
    }

    #[test]
    fn compactions_apply_by_id() {
        let mut entries = chain(vec![
            msg(Message::user("first")),
            msg(Message::assistant("one")),
            msg(Message::user("second")),
            msg(Message::assistant("two")),
        ]);
        let second = entries[2].id.clone();
        let leaf = entries.last().unwrap().id.clone();
        entries.push(Entry::new(
            Some(leaf),
            Record::Compaction {
                summary: "talked twice".into(),
                first_kept: second,
                tokens_before: 9,
                details: None,
            },
        ));
        let live = Live::project(&entries);
        let texts: Vec<String> = live.messages.iter().map(Message::text).collect();
        assert_eq!(
            texts,
            [
                format!("{SUMMARY_PREFIX}talked twice"),
                "second".into(),
                "two".into()
            ]
        );
        assert_eq!(live.summary.as_deref(), Some("talked twice"));
    }

    #[test]
    fn project_at_follows_a_branch() {
        let entries = chain(vec![msg(Message::user("a")), msg(Message::assistant("b"))]);
        let live = Live::project_at(&entries, Some(&entries[0].id));
        assert_eq!(live.messages.len(), 1);
        assert_eq!(live.leaf.as_deref(), Some(entries[0].id.as_str()));
        assert!(Live::project_at(&entries, None).messages.is_empty());
    }

    #[test]
    fn dangling_calls_get_interrupted_results_even_at_the_end() {
        let entries = chain(vec![msg(Message::user("go")), msg(call("a"))]);
        let live = Live::project(&entries);
        assert_eq!(live.messages.len(), 3);
        match &live.messages[2].content[0] {
            Content::ToolResult {
                call_id, is_error, ..
            } => assert!(call_id == "a" && *is_error),
            c => panic!("{c:?}"),
        }
        assert!(live.ids[2].is_empty());
    }

    #[test]
    fn cut_respects_the_recent_window_and_tool_pairs() {
        let big = "x".repeat(4_000);
        let result = |id: &str| {
            Message::new(
                Role::Tool,
                vec![Content::ToolResult {
                    call_id: id.into(),
                    output: big.clone(),
                    is_error: false,
                }],
            )
        };
        let entries = chain(vec![
            msg(Message::user("go")),
            msg(call("a")),
            msg(result("a")),
            msg(call("b")),
            msg(result("b")),
            msg(Message::assistant("done")),
        ]);
        let live = Live::project(&entries);
        // The recent window starts at result "b"; the cut moves back to its call.
        assert_eq!(live.cut_point(1_150), Some(3));
        assert_eq!(live.cut_point(1_000_000), None);
    }

    #[test]
    fn meta_records_apply_from_the_log_and_state_follows_the_path() {
        let entries = chain(vec![
            msg(Message::user("a")),
            Record::State {
                key: "k".into(),
                value: serde_json::json!(1),
            },
            Record::ModelChange {
                model: Some("x/y".into()),
                reasoning: None,
            },
            Record::ModelChange {
                model: None,
                reasoning: Some(Effort::Off),
            },
            Record::Label {
                name: "fix bug".into(),
            },
        ]);
        let live = Live::project(&entries);
        assert_eq!(live.model.as_deref(), Some("x/y"));
        assert_eq!(live.reasoning, Some(Effort::Off));
        assert_eq!(live.label.as_deref(), Some("fix bug"));
        assert_eq!(live.state.get("k"), Some(&serde_json::json!(1)));
        assert!(
            Live::project_at(&entries, Some(&entries[0].id))
                .state
                .is_empty()
        );
    }
}
