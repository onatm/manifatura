//! What the screen shows of a session: finished items, written once into the terminal's scrollback, and the
//! live part (the reply as it streams, the tools running) redrawn below them. It is fed manifatura's session
//! events only.
//!
//! Each item is marked in the left column by what it is: `›` you, `•` an update while rho works, `◇` reads and
//! searches (grouped while they run back to back), `✎` an edit, `$` a command, `↳` a subagent, `☰` the plan, `◆` a
//! skill, `⌁` any other tool. The answer is unmarked and brightest.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use manifatura::{Content, Event, Message, Outcome, Phase, Role};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::text::{markdown, plain, wrap};
pub(crate) use super::theme::{accent, dim};
use super::theme::{agent, bright, error, info, ok, theme, warn};

/// A finished piece of the transcript.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Item {
    Header {
        directory: String,
        model: String,
    },
    User(String),
    /// A paragraph of a reply: an update while rho works, or its answer. `opens` marks a message's first.
    Assistant {
        text: String,
        update: bool,
        opens: bool,
    },
    /// Reasoning, whole, with how long it went on when that is known.
    Reasoning {
        text: String,
        seconds: Option<u64>,
    },
    Tool(Arc<ToolOutput>),
    /// Reads and searches made back to back.
    Explored(Vec<Arc<ToolOutput>>),
    ToolDetails(Arc<ToolOutput>),
    Notice(Level, String),
}

impl Item {
    /// The answer, or a paragraph of it.
    #[cfg(test)]
    pub(crate) fn answer(text: &str) -> Self {
        Item::Assistant {
            text: text.into(),
            update: false,
            opens: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Warn,
    Error,
}

pub(super) fn user_message_style() -> Style {
    let theme = theme();
    Style::new().fg(theme.bright).bg(theme.band)
}

/// Leave two cells for the chevron and one at the right edge.
pub(super) fn prompt_text_width(width: usize) -> usize {
    width.saturating_sub(3).max(1)
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct ToolOutput {
    /// The tool, or for a task the subagent.
    name: String,
    summary: String,
    output: String,
    is_error: bool,
    /// For a subagent: whether it is one, its tool calls as the transcript names them, and how long it took.
    subagent: bool,
    children: Vec<String>,
    seconds: Option<u64>,
    /// Lines shown under the call: a patch's changes, a plan's steps.
    details: Vec<(String, Style)>,
}

/// Whether a call only looks: reads and searches, which the transcript groups.
fn looks(name: &str) -> bool {
    matches!(name, "read" | "grep" | "glob")
}

/// How many of a subagent's calls show under it.
const CHILD_LINES: usize = 4;

/// How many changed lines of a patch show under it.
const PATCH_LINES: usize = 12;

impl ToolOutput {
    fn heading(&self) -> String {
        let mark = if self.is_error { "✗" } else { "✓" };
        format!("{mark} {} {}", title(&self.name), self.summary)
    }

    /// The mark a call shows in the left column, coloured by what it is, or red when it failed.
    fn mark(&self) -> Span<'static> {
        let (glyph, style) = glyph(&self.name, self.subagent);
        Span::styled(
            format!("{glyph} "),
            if self.is_error { error() } else { style },
        )
    }

    /// A finished call: its mark and what it did, then what came of it.
    fn tree(&self, width: usize) -> Vec<Line<'static>> {
        match self.name.as_str() {
            "apply_patch" => self.patch(width),
            "update_plan" => {
                let done = self
                    .details
                    .iter()
                    .filter(|(step, _)| step.starts_with('✓'))
                    .count();
                let mut out = head(
                    self.mark(),
                    vec![
                        Span::raw("Plan "),
                        Span::styled(format!("{done}/{}", self.details.len()), dim()),
                    ],
                    width,
                );
                for (step, style) in &self.details {
                    out.extend(wrap(
                        vec![Span::raw("  "), Span::styled(step.clone(), *style)],
                        width,
                        "    ",
                    ));
                }
                if self.is_error {
                    out.extend(branches(&[self.result()], width));
                }
                out
            }
            _ => {
                let mut out = head(self.mark(), self.label(), width);
                let mut under: Vec<(String, Style)> = Vec::new();
                if self.children.len() > CHILD_LINES {
                    under.push((
                        format!("⋯ {} earlier", self.children.len() - CHILD_LINES),
                        dim(),
                    ));
                }
                let shown = self.children.len().saturating_sub(CHILD_LINES);
                under.extend(self.children[shown..].iter().map(|c| (c.clone(), dim())));
                under.extend(self.details.iter().cloned());
                under.push(self.result());
                out.extend(branches(&under, width));
                out
            }
        }
    }

    /// What the call's line says after the mark: a command as it ran, otherwise the tool and what it worked on.
    fn label(&self) -> Vec<Span<'static>> {
        match (self.name.as_str(), self.subagent) {
            ("exec_command" | "bash", false) => vec![Span::raw(self.summary.clone())],
            (_, true) => vec![
                Span::styled(self.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {}", self.summary), dim()),
            ],
            (name, false) => vec![
                Span::styled(
                    title(name).to_owned(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" {}", self.summary)),
            ],
        }
    }

    /// An edit: the files with the lines added and removed, then the changes, the first of them.
    fn patch(&self, width: usize) -> Vec<Line<'static>> {
        let (added, removed) = self
            .details
            .iter()
            .filter(|(_, style)| !style.add_modifier.contains(Modifier::BOLD))
            .fold((0, 0), |(a, r), (line, _)| match line.chars().next() {
                Some('+') => (a + 1, r),
                Some('-') => (a, r + 1),
                _ => (a, r),
            });
        let mut label = vec![Span::raw(format!("Edited {}", self.summary))];
        if added > 0 {
            label.push(Span::styled(format!(" +{added}"), ok()));
        }
        if removed > 0 {
            label.push(Span::styled(format!(" −{removed}"), error()));
        }
        let mut out = head(self.mark(), label, width);
        let theme = theme();
        let mut changed = 0;
        let mut hidden = 0;
        for (line, style) in &self.details {
            let file = style.add_modifier.contains(Modifier::BOLD);
            if !file {
                changed += 1;
                if changed > PATCH_LINES {
                    hidden += 1;
                    continue;
                }
            }
            let background = match line.chars().next() {
                _ if file => None,
                Some('+') => Some(theme.added),
                Some('-') => Some(theme.removed),
                _ => None,
            };
            let mut row = Line::from(vec![
                Span::raw("    "),
                Span::styled(clip(line, width.saturating_sub(4)), *style),
            ]);
            if let Some(background) = background {
                row = row.style(Style::new().bg(background));
            }
            out.push(row);
        }
        if hidden > 0 {
            out.push(Line::styled(
                format!("    ⋯ {hidden} more changed lines · ctrl-o shows them"),
                dim(),
            ));
        }
        if self.is_error {
            out.extend(branches(&[self.result()], width));
        }
        out
    }

    /// What came of the call, in a line: an error's cause, a subagent's count of calls, or the output's size.
    fn result(&self) -> (String, Style) {
        let lines: Vec<&str> = self
            .output
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .collect();
        if self.is_error {
            return (
                lines.first().copied().unwrap_or("failed").to_owned(),
                error(),
            );
        }
        if self.subagent {
            let calls = match self.children.len() {
                1 => "1 tool call".to_owned(),
                n => format!("{n} tool calls"),
            };
            let time = self
                .seconds
                .map_or_else(String::new, |s| format!(" · {}", duration(s)));
            return (format!("Reported · {calls}{time}"), dim());
        }
        match self.name.as_str() {
            "exec_command" | "write_stdin" => return exec_result(&self.output),
            "apply_patch" => {
                let files = lines
                    .iter()
                    .filter(|l| l.len() > 2 && matches!(&l[..2], "A " | "M " | "D "))
                    .count();
                let text = if files == 1 {
                    "Updated 1 file".to_owned()
                } else {
                    format!("Updated {files} files")
                };
                return (text, dim());
            }
            "update_plan" => return ("Plan updated".to_owned(), dim()),
            _ => {}
        }
        let text = match (lines.len(), self.name.as_str()) {
            (0, _) => "No output".to_owned(),
            (1, _) => lines[0].trim().to_owned(),
            (n, "read") => format!("{n} lines"),
            (n, "grep") => format!("{n} matching lines"),
            (n, "glob") => format!("{n} paths"),
            (n, _) => format!("{n} lines of output"),
        };
        (text, dim())
    }
}

/// The mark of a call by what it is.
fn glyph(name: &str, subagent: bool) -> (&'static str, Style) {
    if subagent {
        return ("↳", agent());
    }
    match name {
        "read" | "grep" | "glob" => ("◇", info()),
        "exec_command" | "bash" | "write_stdin" => ("$", ok()),
        "apply_patch" => ("✎", accent()),
        "update_plan" => ("☰", agent()),
        "skill" => ("◆", info()),
        _ => ("⌁", info()),
    }
}

/// Reads and searches as one item: how many of each, then each on a line with what it found.
fn explored(calls: &[Arc<ToolOutput>], width: usize, live: bool) -> Vec<Line<'static>> {
    let count =
        |name: &str, one: &str, many: &str| match calls.iter().filter(|c| c.name == name).count() {
            0 => None,
            1 => Some(format!("1 {one}")),
            n => Some(format!("{n} {many}")),
        };
    let counts: Vec<String> = [
        count("grep", "search", "searches"),
        count("glob", "find", "finds"),
        count("read", "read", "reads"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut out = head(
        Span::styled("◇ ", info()),
        vec![
            Span::raw(if live { "Exploring" } else { "Explored" }),
            Span::styled(format!(" · {}", counts.join(", ")), dim()),
        ],
        width,
    );
    let rows: Vec<(String, Style)> = calls
        .iter()
        .map(|call| {
            let (result, style) = call.result();
            let verb = title(&call.name).to_lowercase();
            (
                format!("{verb} {} · {result}", call.summary),
                style.patch(dim()),
            )
        })
        .collect();
    out.extend(branches(&rows, width));
    out
}

/// What a command's report (`exec_command`, `write_stdin`) says: its exit code and output size, or
/// that it keeps running.
fn exec_result(report: &str) -> (String, Style) {
    let output = report.split_once("Output:\n").map_or("", |(_, o)| o);
    let count = output.lines().filter(|l| !l.trim().is_empty()).count();
    let size = match count {
        0 => "no output".to_owned(),
        1 => output
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .to_owned(),
        n => format!("{n} lines of output"),
    };
    for line in report.lines() {
        if let Some(code) = line.strip_prefix("Process exited with code ") {
            if code.trim() == "0" {
                return (format!("✓ {size}"), dim());
            }
            return (format!("Exit {} · {size}", code.trim()), error());
        }
        if let Some(id) = line.strip_prefix("Process running with session ID ") {
            return (
                format!("Running in the background as {} · {size}", id.trim()),
                dim(),
            );
        }
    }
    (size, dim())
}

/// Lines to show under a call, from its input: a patch's changed lines, a plan's steps.
pub(crate) fn tool_details(name: &str, input: &Value) -> Vec<(String, Style)> {
    match name {
        "apply_patch" => patch_lines(input["input"].as_str().unwrap_or_default()),
        "update_plan" => plan_steps(input)
            .into_iter()
            .map(|(step, status)| match status.as_str() {
                "completed" => (format!("✓ {step}"), dim()),
                "in_progress" => (format!("● {step}"), accent()),
                _ => (format!("○ {step}"), Style::new()),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A plan's steps and their status (`pending`, `in_progress`, `completed`).
fn plan_steps(input: &Value) -> Vec<(String, String)> {
    input["plan"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .map(|s| {
                    (
                        s["step"].as_str().unwrap_or_default().to_owned(),
                        s["status"].as_str().unwrap_or("pending").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The files a patch changes, each with the lines it adds and removes.
fn patch_counts(patch: &str) -> Vec<(String, usize, usize)> {
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    for line in patch.lines() {
        let file = ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
            .iter()
            .find_map(|p| line.strip_prefix(p));
        if let Some(path) = file {
            out.push((path.to_owned(), 0, 0));
        } else if line.starts_with("***") {
            continue;
        } else if let Some(last) = out.last_mut() {
            if line.starts_with('+') {
                last.1 += 1;
            } else if line.starts_with('-') {
                last.2 += 1;
            }
        }
    }
    out
}

/// A patch as the transcript shows it: each file, then its added and removed lines, coloured.
fn patch_lines(patch: &str) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    for line in patch.lines() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let shown = if let Some(path) = line.strip_prefix("*** Add File: ") {
            Some((format!("new {path}"), bold))
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            Some((path.to_owned(), bold))
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            Some((format!("deleted {path}"), bold))
        } else if let Some(path) = line.strip_prefix("*** Move to: ") {
            Some((format!("→ {path}"), bold))
        } else if line.starts_with('+') {
            Some((line.to_owned(), ok()))
        } else if line.starts_with('-') {
            Some((line.to_owned(), error()))
        } else {
            None
        };
        out.extend(shown);
    }
    out
}

/// A line led by a mark, wrapped under itself.
fn head(mark: Span<'static>, mut spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    spans.insert(0, mark);
    wrap(spans, width, "  ")
}

/// Lines under a call: the first after `└`, the rest lined up with it, each wrapped rather than cut.
fn branches(lines: &[(String, Style)], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, (text, style)) in lines.iter().enumerate() {
        let lead = if i == 0 { "  └ " } else { "    " };
        out.extend(wrap(
            vec![
                Span::styled(lead, dim()),
                Span::styled(text.clone(), *style),
            ],
            width,
            "    ",
        ));
    }
    out
}

/// Seconds as `42s` or `6m 20s`.
fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        _ => format!("{}m {}s", seconds / 60, seconds % 60),
    }
}

/// A reply's paragraph: an update dim, after a `•` when it opens its message; the answer bright.
fn reply(text: &str, update: bool, opens: bool, width: usize) -> Vec<Line<'static>> {
    if !update {
        return markdown(text, width)
            .into_iter()
            .map(|line| {
                let style = bright().patch(line.style);
                line.style(style)
            })
            .collect();
    }
    markdown(text, width.saturating_sub(2).max(8))
        .into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            if line.width() == 0 {
                return line;
            }
            for span in &mut line.spans {
                span.style = span.style.patch(dim());
            }
            let lead = if i == 0 && opens { "• " } else { "  " };
            line.spans.insert(0, Span::styled(lead, dim()));
            line
        })
        .collect()
}

impl Item {
    /// The item as wrapped lines, after a blank line that separates it from the one before.
    pub(crate) fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut out = vec![Line::default()];
        match self {
            Item::Header { directory, model } => {
                let mut header = Line::from(vec![
                    Span::styled("rho  ", accent()),
                    Span::styled(directory.clone(), dim()),
                ]);
                if header.width() + model.width() + 2 <= width {
                    header.push_span(" ".repeat(width - header.width() - model.width()));
                    header.push_span(Span::styled(model.clone(), dim()));
                }
                out.push(header);
            }
            Item::User(text) => {
                let style = user_message_style();
                let padding = Line::styled(" ".repeat(width), style);
                out.push(padding.clone());
                let body = text.trim_end_matches(['\r', '\n']);
                for (i, mut row) in plain(body, prompt_text_width(width), Style::new())
                    .into_iter()
                    .enumerate()
                {
                    let prefix = if i == 0 {
                        Span::styled("› ", accent())
                    } else {
                        Span::raw("  ")
                    };
                    row.spans.insert(0, prefix);
                    row.push_span(" ".repeat(width.saturating_sub(row.width())));
                    out.push(row.style(style));
                }
                out.push(padding);
            }
            Item::Assistant {
                text,
                update,
                opens,
            } => out.extend(reply(text, *update, *opens, width)),
            Item::Reasoning { text, seconds } => {
                let style = dim().add_modifier(Modifier::ITALIC);
                let heading = match seconds {
                    Some(seconds) => format!("∴ Thought for {}", duration(*seconds)),
                    None => "∴ Thought".to_owned(),
                };
                out.push(Line::styled(heading, style));
                for mut line in plain(text.trim(), width.saturating_sub(2).max(8), style) {
                    line.spans.insert(0, Span::raw("  "));
                    out.push(line);
                }
            }
            Item::Tool(tool) => out.extend(tool.tree(width)),
            Item::Explored(calls) => out.extend(explored(calls, width, false)),
            Item::ToolDetails(tool) => {
                out.extend(plain(&tool.heading(), width, dim()));
                out.extend(plain(&tool.output.replace('\t', "  "), width, Style::new()));
            }
            Item::Notice(level, text) => {
                let style = match level {
                    Level::Info => dim(),
                    Level::Warn => warn(),
                    Level::Error => error(),
                };
                out.extend(plain(text, width, style));
            }
        }
        out
    }
}

/// Clip to terminal cells, leaving room for an ellipsis.
pub(crate) fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 0;
    let mut clipped: String = text
        .chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used < width
        })
        .collect();
    clipped.push('…');
    clipped
}

/// A call in a line: its path or pattern when it has one, else its input.
/// How a tool, or a subagent, is named in the transcript.
fn title(name: &str) -> &str {
    match name {
        "read" => "Read",
        "grep" => "Search",
        "glob" => "Find",
        "exec_command" | "bash" => "Run",
        "write_stdin" => "Input",
        "apply_patch" => "Patch",
        "update_plan" => "Plan",
        "explore" => "Explore",
        "skill" => "Skill",
        "find_tools" => "Find tools",
        "run_tool" => "Tool",
        other => other,
    }
}

/// The name a call is shown under, and whether it is a subagent's: a task is shown as its subagent.
fn shown_name(name: &str, input: &Value) -> (String, bool) {
    match (name, input["agent"].as_str()) {
        ("task", Some(agent)) => (agent.to_owned(), true),
        _ => (name.to_owned(), false),
    }
}

/// A subagent's tool call, from the line the task tool logs for it (`grep {"pattern":…}`), as the transcript
/// names it.
fn subagent_call(line: &str) -> String {
    let (name, input) = line.split_once(' ').unwrap_or((line, ""));
    match serde_json::from_str::<Value>(input) {
        Ok(input) => format!("{} {}", title(name), tool_summary(name, &input)),
        Err(_) => clip(line, 160),
    }
}

pub(crate) fn tool_summary(name: &str, input: &Value) -> String {
    let text = match name {
        "task" => input["description"]
            .as_str()
            .or(input["prompt"].as_str())
            .map(str::to_owned),
        "read" => input["path"].as_str().map(str::to_owned),
        "grep" | "glob" => input["pattern"].as_str().map(str::to_owned),
        "exec_command" => input["cmd"].as_str().map(|cmd| {
            match input["sandbox_permissions"] == "require_escalated" {
                true => format!("{cmd}  (outside the sandbox)"),
                false => cmd.to_owned(),
            }
        }),
        "bash" => input["command"].as_str().map(str::to_owned),
        "write_stdin" => {
            let id = &input["session_id"];
            Some(match input["chars"].as_str().unwrap_or_default() {
                "" => format!("wait for {id}"),
                "\u{3}" => format!("interrupt {id}"),
                chars => format!("{id}: {}", chars.escape_debug()),
            })
        }
        "apply_patch" => {
            let files: Vec<&str> = input["input"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                        .iter()
                        .find_map(|p| l.strip_prefix(p))
                })
                .collect();
            Some(files.join(", "))
        }
        "update_plan" => input["explanation"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| Some(String::new())),
        "run_tool" => input["name"]
            .as_str()
            .map(|name| format!("{name} {}", input["input"])),
        // An MCP tool: what it searches for or opens, when it says.
        _ => ["query", "url", "urls", "path", "name", "text"]
            .iter()
            .find_map(|key| match &input[*key] {
                serde_json::Value::String(text) => Some(text.clone()),
                serde_json::Value::Null => None,
                other => Some(other.to_string()),
            }),
    }
    .unwrap_or_else(|| input.to_string());
    clip(&text.replace('\n', " ⏎ "), 160)
}

/// What the agent is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Activity {
    Idle,
    Thinking,
    Generating,
    Running,
}

struct Running {
    call_id: String,
    /// The tool, or for a task the subagent.
    name: String,
    summary: String,
    /// For a subagent: whether it is one, and its tool calls so far.
    subagent: bool,
    children: Vec<String>,
    details: Vec<(String, Style)>,
    /// For a patch: the files it changes, with lines added and removed.
    changes: Vec<(String, usize, usize)>,
    started: Instant,
}

/// The display state of one session.
///
/// A reply goes to the scrollback as it streams, not when it ends: the answer paragraph by paragraph (at blank
/// lines outside code blocks, so its markdown renders as a whole). Reasoning stays live while it goes on, its
/// latest lines showing, and goes to the scrollback as one line once the answer (or a tool call) starts. Only
/// the part still being written is live, so text never moves once it is on the screen.
pub(crate) struct View {
    committed: Vec<Item>,
    /// The reply's text so far, and how much of it is already in the scrollback.
    streaming: String,
    streamed: usize,
    /// Its reasoning, and when the reasoning started.
    reasoning: String,
    thinking_since: Option<Instant>,
    running: Vec<Running>,
    tools: Vec<Arc<ToolOutput>>,
    /// Reads and searches finished back to back, shown live until something else comes.
    exploring: Vec<Arc<ToolOutput>>,
    /// Whether the message streaming now is an update rather than the answer, whether its first paragraph is
    /// in the scrollback, and whether this model call streamed any text.
    update: bool,
    opened: bool,
    streamed_text: bool,
    /// The latest plan's steps and their status, and the files changed in the session, with lines added and
    /// removed.
    plan: Vec<(String, String)>,
    changes: BTreeMap<String, (usize, usize)>,
    /// Set when the scrollback is to be wiped before the next items are written.
    cleared: bool,
    pub(crate) activity: Activity,
    /// When the turn started.
    pub(crate) since: Option<Instant>,
}

impl View {
    pub(crate) fn new() -> Self {
        Self {
            committed: Vec::new(),
            streaming: String::new(),
            streamed: 0,
            reasoning: String::new(),
            thinking_since: None,
            running: Vec::new(),
            tools: Vec::new(),
            exploring: Vec::new(),
            update: false,
            opened: false,
            streamed_text: false,
            plan: Vec::new(),
            changes: BTreeMap::new(),
            cleared: false,
            activity: Activity::Idle,
            since: None,
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.activity != Activity::Idle
    }

    /// Adds a finished item, after the reads and searches before it.
    pub(crate) fn push(&mut self, item: Item) {
        self.flush_explored();
        self.committed.push(item);
    }

    /// The reads and searches made back to back go to the scrollback as one item.
    fn flush_explored(&mut self) {
        if !self.exploring.is_empty() {
            let calls = std::mem::take(&mut self.exploring);
            self.committed.push(Item::Explored(calls));
        }
    }

    /// The latest plan's steps and their status.
    pub(crate) fn plan(&self) -> &[(String, String)] {
        &self.plan
    }

    /// The files changed in the session, with lines added and removed.
    pub(crate) fn changes(&self) -> &BTreeMap<String, (usize, usize)> {
        &self.changes
    }

    /// Takes what a call did into the plan and the changed files.
    fn track(&mut self, name: &str, input: &Value) {
        match name {
            "update_plan" => self.plan = plan_steps(input),
            "apply_patch" => {
                self.count_changes(patch_counts(input["input"].as_str().unwrap_or_default()))
            }
            _ => {}
        }
    }

    fn count_changes(&mut self, changes: Vec<(String, usize, usize)>) {
        for (file, added, removed) in changes {
            let entry = self.changes.entry(file).or_default();
            entry.0 += added;
            entry.1 += removed;
        }
    }

    pub(crate) fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.push(Item::Notice(level, text.into()));
    }

    /// Wipes the transcript: the scrollback is cleared before the next items are written.
    pub(crate) fn clear(&mut self) {
        self.committed.clear();
        self.tools.clear();
        self.exploring.clear();
        self.plan.clear();
        self.changes.clear();
        self.cleared = true;
    }

    /// Shows a turn under way, for a conversation joined while it runs (`messages`, as replayed): the calls
    /// without results yet run on, and its events take it from here.
    pub(crate) fn follow(&mut self, messages: &[Message]) {
        let answered: std::collections::HashSet<&str> = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::ToolResult { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        for content in messages.iter().flat_map(|m| &m.content) {
            if let Content::ToolCall { id, name, input } = content
                && !answered.contains(id.as_str())
            {
                let (name_shown, subagent) = shown_name(name, input);
                self.running.push(Running {
                    call_id: id.clone(),
                    name: name_shown,
                    summary: tool_summary(name, input),
                    subagent,
                    children: Vec::new(),
                    details: tool_details(name, input),
                    changes: Vec::new(),
                    started: Instant::now(),
                });
            }
        }
        self.activity = if self.running.is_empty() {
            Activity::Thinking
        } else {
            Activity::Running
        };
        self.since = Some(Instant::now());
    }

    /// Whether the scrollback is to be wiped, once.
    pub(crate) fn take_cleared(&mut self) -> bool {
        std::mem::take(&mut self.cleared)
    }

    /// The items finished since the last call, oldest first, for the scrollback.
    pub(crate) fn take_committed(&mut self) -> Vec<Item> {
        std::mem::take(&mut self.committed)
    }

    fn finish_tool(&mut self, tool: ToolOutput) {
        let looked = looks(&tool.name) && !tool.is_error && !tool.subagent;
        let tool = Arc::new(tool);
        self.tools.push(tool.clone());
        if looked {
            self.exploring.push(tool);
        } else {
            self.push(Item::Tool(tool));
        }
    }

    pub(crate) fn show_tools(&mut self) {
        if self.tools.is_empty() {
            self.notice(Level::Info, "No tool results in this turn");
        } else {
            self.committed
                .extend(self.tools.iter().cloned().map(Item::ToolDetails));
        }
    }

    pub(crate) fn running_label(&self) -> String {
        let Some(call) = self.running.first() else {
            return "Running tools".into();
        };
        let verb = match call.name.as_str() {
            "read" => "Reading",
            "grep" => "Searching",
            "glob" => "Finding",
            "exec_command" | "bash" => "Running",
            "write_stdin" => "Waiting on",
            "apply_patch" => "Patching",
            "explore" => "Exploring",
            other => other,
        };
        let mut label = format!("{verb} {}", call.summary);
        if let Some(latest) = call.children.last() {
            label.push_str(&format!(" · {latest}"));
        }
        if self.running.len() > 1 {
            label.push_str(&format!(" (+{} more)", self.running.len() - 1));
        }
        label
    }

    /// The calls running now, as trees: a subagent with every call it has made so far.
    pub(crate) fn running_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if !self.exploring.is_empty() {
            out.extend(explored(&self.exploring, width, true));
        }
        for call in &self.running {
            let (glyph, _) = glyph(&call.name, call.subagent);
            let label = match (call.name.as_str(), call.subagent) {
                ("exec_command" | "bash", false) => call.summary.clone(),
                _ => format!("{} {}", title(&call.name), call.summary),
            };
            out.extend(head(
                Span::styled(format!("{glyph} "), dim()),
                vec![Span::styled(label, dim())],
                width,
            ));
            let shown = call.children.len().saturating_sub(CHILD_LINES);
            let mut children: Vec<(String, Style)> = Vec::new();
            if shown > 0 {
                children.push((format!("⋯ {shown} earlier"), dim()));
            }
            children.extend(
                call.children[shown..]
                    .iter()
                    .map(|child| (child.clone(), dim())),
            );
            out.extend(branches(&children, width));
        }
        out
    }

    /// Ends the reasoning: it goes to the scrollback whole.
    fn collapse_reasoning(&mut self) {
        let reasoning = std::mem::take(&mut self.reasoning);
        let seconds = self
            .thinking_since
            .take()
            .map(|since| since.elapsed().as_secs());
        if !reasoning.trim().is_empty() {
            self.push(Item::Reasoning {
                text: reasoning.trim().to_owned(),
                seconds,
            });
        }
    }

    /// Moves the reply's finished paragraphs to the scrollback; with `all`, the rest too.
    fn commit_text(&mut self, all: bool) {
        let rest = &self.streaming[self.streamed..];
        let end = match all {
            true => rest.len(),
            false => paragraph_end(rest),
        };
        if end == 0 {
            return;
        }
        let text = rest[..end].trim_end();
        if !text.trim().is_empty() {
            // Paragraphs keep the blank line between them, as the whole reply rendered would have.
            let opens = !self.opened;
            self.opened = true;
            self.push(Item::Assistant {
                text: text.to_owned(),
                update: self.update,
                opens,
            });
        }
        self.streamed += end;
    }

    /// Ends the reply being streamed, whatever is left of it going to the scrollback.
    fn finish_reply(&mut self) {
        self.collapse_reasoning();
        self.commit_text(true);
        self.streaming.clear();
        self.streamed = 0;
        self.opened = false;
    }

    /// A stored conversation as items, when a session is resumed.
    pub(crate) fn replay(&mut self, messages: &[Message]) {
        self.tools.clear();
        self.exploring.clear();
        let mut calls = std::collections::HashMap::new();
        let mut calls_input = std::collections::HashMap::new();
        for message in messages {
            if message.role == Role::User {
                self.tools.clear();
            }
            for content in &message.content {
                match (message.role, content) {
                    (Role::User, Content::Text { text, .. }) => self.push(Item::User(typed(text))),
                    (Role::Assistant, Content::Thinking { text, .. })
                        if !text.trim().is_empty() =>
                    {
                        self.push(Item::Reasoning {
                            text: text.trim().to_owned(),
                            seconds: None,
                        })
                    }
                    (Role::Assistant, Content::Text { text, phase }) if !text.trim().is_empty() => {
                        self.push(Item::Assistant {
                            text: text.clone(),
                            update: *phase == Some(Phase::Commentary),
                            opens: true,
                        })
                    }
                    (Role::Assistant, Content::ToolCall { id, name, input }) => {
                        if !looks(name) {
                            // A call that failed changed nothing; one in an old session is taken as done.
                            calls_input.insert(id.clone(), (name.clone(), input.clone()));
                        }
                        calls.insert(
                            id.clone(),
                            (
                                shown_name(name, input),
                                tool_summary(name, input),
                                tool_details(name, input),
                            ),
                        );
                    }
                    (
                        Role::Tool,
                        Content::ToolResult {
                            call_id,
                            output,
                            is_error,
                        },
                    ) => {
                        if !*is_error && let Some((name, input)) = calls_input.remove(call_id) {
                            self.track(&name, &input);
                        }
                        let ((name, subagent), summary, details) = calls
                            .remove(call_id)
                            .unwrap_or_else(|| (("tool".into(), false), String::new(), Vec::new()));
                        // A subagent's own calls are in its session, not this one.
                        let tool = ToolOutput {
                            name,
                            summary,
                            output: output.clone(),
                            is_error: *is_error,
                            subagent,
                            details,
                            ..Default::default()
                        };
                        self.finish_tool(tool);
                    }
                    _ => {}
                }
            }
        }
        self.flush_explored();
    }

    pub(crate) fn apply(&mut self, event: &Event) {
        match event {
            Event::TurnStarted { .. } => {
                self.activity = Activity::Thinking;
                self.since = Some(Instant::now());
                self.tools.clear();
            }
            Event::StepStarted { .. } => {
                self.activity = Activity::Thinking;
                self.streamed_text = false;
                self.update = false;
            }
            Event::MessageStarted { phase, .. } => {
                // The message before it ends here; this one is an update or the answer.
                self.finish_reply();
                self.update = *phase == Some(Phase::Commentary);
            }
            Event::ReasoningDelta { text, .. } => {
                self.thinking_since.get_or_insert_with(Instant::now);
                self.reasoning.push_str(text);
                self.activity = Activity::Thinking;
            }
            Event::TextDelta { text, .. } => {
                // The reasoning is over once the answer starts.
                self.collapse_reasoning();
                self.streamed_text = true;
                self.streaming.push_str(text);
                self.commit_text(false);
                self.activity = Activity::Generating;
            }
            Event::MessageAppended { message, .. } if message.role == Role::Assistant => {
                // A reply that did not stream (none do here, but a provider may) is shown whole.
                if !self.streamed_text {
                    self.streaming = message.text();
                }
                self.finish_reply();
            }
            Event::ActionStarted {
                call_id,
                name,
                input,
                ..
            } => {
                let summary = tool_summary(name, input);
                let details = tool_details(name, input);
                let changes = match name.as_str() {
                    "apply_patch" => patch_counts(input["input"].as_str().unwrap_or_default()),
                    _ => Vec::new(),
                };
                if name == "update_plan" {
                    self.plan = plan_steps(input);
                }
                let (name, subagent) = shown_name(name, input);
                let started = Instant::now();
                self.running.push(Running {
                    call_id: call_id.clone(),
                    name,
                    summary,
                    subagent,
                    children: Vec::new(),
                    details,
                    changes,
                    started,
                });
                self.activity = Activity::Running;
            }
            Event::ActionLog { call_id, text, .. } => {
                if let Some(running) = self.running.iter_mut().find(|r| &r.call_id == call_id) {
                    running.children.push(subagent_call(text));
                }
            }
            Event::ActionResult {
                call_id,
                name,
                output,
                is_error,
                ..
            } => {
                let mut tool = ToolOutput {
                    name: name.clone(),
                    output: output.clone(),
                    is_error: *is_error,
                    ..Default::default()
                };
                if let Some(i) = self.running.iter().position(|r| &r.call_id == call_id) {
                    let running = self.running.remove(i);
                    tool.name = running.name;
                    tool.summary = running.summary;
                    tool.subagent = running.subagent;
                    tool.seconds = running
                        .subagent
                        .then(|| running.started.elapsed().as_secs());
                    tool.children = running.children;
                    tool.details = running.details;
                    if !*is_error {
                        self.count_changes(running.changes);
                    }
                }
                self.finish_tool(tool);
                self.activity = match self.running.is_empty() {
                    true => Activity::Thinking,
                    false => Activity::Running,
                };
            }
            Event::Compacted {
                tokens_before,
                tokens_after,
                ..
            } => {
                self.notice(
                    Level::Info,
                    format!(
                        "context compacted: {} → {} tokens",
                        tokens(*tokens_before),
                        tokens(*tokens_after)
                    ),
                );
            }
            Event::TurnCompleted { outcome, error, .. } => {
                // What an interrupted reply got to stays on the screen.
                self.finish_reply();
                self.flush_explored();
                self.running.clear();
                self.activity = Activity::Idle;
                self.since = None;
                match outcome {
                    Outcome::Aborted => self.notice(Level::Warn, "interrupted"),
                    Outcome::Failed => self.notice(
                        Level::Error,
                        format!("error: {}", error.as_deref().unwrap_or("the turn failed")),
                    ),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// The live part: what is being written and not yet in the scrollback (the reasoning's latest lines, the
    /// answer's unfinished paragraph), limited to its last `max` lines.
    pub(crate) fn live_lines(&self, width: usize, max: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if !self.reasoning.trim().is_empty() {
            let style = dim().add_modifier(Modifier::ITALIC);
            let tail: Vec<_> = self
                .reasoning
                .lines()
                .rev()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .take(max)
                .collect();
            for line in tail.into_iter().rev() {
                out.extend(wrap(
                    vec![Span::styled(format!("∴ {line}"), style)],
                    width,
                    "  ",
                ));
            }
        }
        let text = &self.streaming[self.streamed..];
        if !text.trim().is_empty() {
            out.extend(reply(text, self.update, !self.opened, width));
        }
        if out.len() > max {
            out.drain(..out.len() - max);
        }
        out
    }
}

/// Only commit at a complete paragraph or closing fence. Every previous commit ended outside a block.
fn paragraph_end(rest: &str) -> usize {
    let mut fence: Option<(u8, usize)> = None;
    let mut end = 0;
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        if !line.ends_with('\n') {
            break;
        }
        at += line.len();
        let trimmed = line.trim_start_matches(' ');
        let marker = trimmed.as_bytes().first().copied();
        if line.len() - trimmed.len() <= 3
            && let Some(marker @ (b'`' | b'~')) = marker
        {
            let length = trimmed.bytes().take_while(|&b| b == marker).count();
            let tail = &trimmed[length..];
            match fence {
                Some((open, min)) if marker == open && length >= min && tail.trim().is_empty() => {
                    fence = None;
                    end = at;
                }
                None if length >= 3 && (marker == b'~' || !tail.contains('`')) => {
                    fence = Some((marker, length))
                }
                _ => {}
            }
        } else if fence.is_none() && line.trim().is_empty() {
            end = at;
        }
    }
    end
}

/// `12.3k`, `850`.
pub(crate) fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        _ => format!("{:.1}k", n as f64 / 1_000.0),
    }
}

/// What `/name task` sends the agent once the subagent has reported: the report, then the task.
pub(crate) fn agent_message(agent: &str, task: &str, report: &str) -> String {
    format!(
        "<agent_report name=\"{agent}\">\n{}\n</agent_report>\n\n{}\n\n{task}",
        report.trim(),
        agent_note(agent)
    )
}

/// What the agent is told of a report a subagent wrote for the user.
fn agent_note(agent: &str) -> String {
    format!(
        "The {agent} subagent worked on my request; its report is above. Answer from it, and check something \
         yourself only where it left it unclear."
    )
}

/// The note of an earlier version, which only `/explore` used, so that its sessions still show as typed.
const EXPLORE_NOTE: &str = "The explore subagent looked into my question; its report is above. Answer from it, and read a \
                            file yourself only to check something it left unclear.";

/// What the person typed for a user message: a skill started with `/skill:name request` was sent as the skill's
/// instructions and then the request, and shows as the command again; a task handed to a subagent with
/// `/name task`, sent with its report, shows as the command.
pub(crate) fn typed(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("<agent_report name=\"")
        && let Some((agent, rest)) = rest.split_once("\">\n")
        && let Some((_, after)) = rest.rsplit_once("\n</agent_report>\n\n")
        && let Some(task) = after.strip_prefix(&agent_note(agent))
    {
        return format!("/{agent} {}", task.trim_start());
    }
    if let Some(rest) = text.strip_prefix("<explore_report>\n")
        && let Some((_, after)) = rest.rsplit_once("\n</explore_report>\n\n")
        && let Some(question) = after.strip_prefix(EXPLORE_NOTE)
    {
        return format!("/explore {}", question.trim_start());
    }
    if text == super::WAKE {
        return "(a background command exited)".to_owned();
    }
    let Some(rest) = text.strip_prefix("<skill_content name=\"") else {
        return text.to_owned();
    };
    let (Some((name, _)), Some((_, request))) = (
        rest.split_once('"'),
        rest.split_once("</skill_content>\n\n"),
    ) else {
        return text.to_owned();
    };
    match request == format!("Use the {name} skill.") {
        true => format!("/skill:{name}"),
        false => format!("/skill:{name} {request}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_skill_message_replays_as_its_command() {
        let sent = "<skill_content name=\"codebase-tour\">\nBody.\n</skill_content>\n\n";
        assert_eq!(
            super::typed(&format!("{sent}how does chat work?")),
            "/skill:codebase-tour how does chat work?"
        );
        assert_eq!(
            super::typed(&format!("{sent}Use the codebase-tour skill.")),
            "/skill:codebase-tour"
        );
        assert_eq!(super::typed("hello"), "hello");
        assert_eq!(
            super::typed(super::super::WAKE),
            "(a background command exited)"
        );
        let sent = super::agent_message("explore", "where is main?", "main is in src/main.rs:1\n");
        assert_eq!(super::typed(&sent), "/explore where is main?");
        let sent = super::agent_message("reviewer", "the last commit", "Looks fine.");
        assert_eq!(super::typed(&sent), "/reviewer the last commit");
    }

    use super::*;
    use manifatura::Usage;
    use serde_json::json;

    #[test]
    fn a_task_shows_its_subagent_and_what_it_does() {
        let mut view = View::new();
        let turn = "t".to_owned();
        view.apply(&Event::TurnStarted { turn: turn.clone() });
        view.apply(&Event::ActionStarted {
            turn: turn.clone(),
            call_id: "1".into(),
            name: "task".into(),
            input: json!({ "agent": "explore", "description": "where main is", "prompt": "Find main." }),
        });
        assert_eq!(view.running_label(), "Exploring where main is");
        for text in [
            r#"glob {"pattern":"**/*.rs"}"#,
            r#"grep {"pattern":"fn main"}"#,
        ] {
            view.apply(&Event::ActionLog {
                turn: turn.clone(),
                call_id: "1".into(),
                text: text.into(),
            });
        }
        assert_eq!(
            view.running_label(),
            "Exploring where main is · Search fn main"
        );
        view.apply(&Event::ActionResult {
            turn,
            call_id: "1".into(),
            name: "task".into(),
            output: "main is in src/main.rs:1".into(),
            is_error: false,
        });
        let items = view.take_committed();
        let lines: Vec<String> = items[0].lines(60).iter().map(Line::to_string).collect();
        assert_eq!(
            lines[1..4],
            [
                "↳ explore where main is",
                "  └ Find **/*.rs",
                "    Search fn main"
            ]
        );
        assert!(
            lines[4].starts_with("    Reported · 2 tool calls · "),
            "{lines:?}"
        );
    }

    /// The lines a finished call shows, after starting and ending it.
    fn finished(name: &str, input: serde_json::Value, output: &str) -> Vec<String> {
        let mut view = View::new();
        let turn = "t".to_owned();
        view.apply(&Event::TurnStarted { turn: turn.clone() });
        view.apply(&Event::ActionStarted {
            turn: turn.clone(),
            call_id: "1".into(),
            name: name.into(),
            input,
        });
        view.apply(&Event::ActionResult {
            turn,
            call_id: "1".into(),
            name: name.into(),
            output: output.into(),
            is_error: false,
        });
        view.take_committed()[0]
            .lines(80)
            .iter()
            .map(Line::to_string)
            .collect()
    }

    #[test]
    fn commands_show_their_exit_code_or_session() {
        let report =
            "Chunk ID: a\nWall time: 0.1 seconds\nProcess exited with code 2\nOutput:\nerror: no\n";
        let lines = finished("exec_command", json!({"cmd": "cargo test"}), report);
        assert_eq!(lines, ["", "$ cargo test", "  └ Exit 2 · error: no"]);
        let report = "Chunk ID: a\nWall time: 10 seconds\nProcess running with session ID 3\nOutput:\nready\n";
        let input = json!({"cmd": "npm run dev", "sandbox_permissions": "require_escalated"});
        let lines = finished("exec_command", input, report);
        assert_eq!(lines[1], "$ npm run dev  (outside the sandbox)");
        assert_eq!(lines[2], "  └ Running in the background as 3 · ready");
        let lines = finished(
            "write_stdin",
            json!({"session_id": 3, "chars": "\u{3}"}),
            "Process exited with code 130\nOutput:\n",
        );
        assert_eq!(lines[1], "$ Input interrupt 3");
    }

    #[test]
    fn a_patch_shows_its_files_and_changed_lines() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@ fn a\n-    old();\n+    new();\n*** Add File: b.md\n+# B\n*** End Patch";
        let lines = finished(
            "apply_patch",
            json!({"input": patch}),
            "Success. Updated the following files:\nM src/a.rs\nA b.md",
        );
        assert_eq!(
            lines,
            [
                "",
                "✎ Edited src/a.rs, b.md +2 −1",
                "    src/a.rs",
                "    -    old();",
                "    +    new();",
                "    new b.md",
                "    +# B",
            ]
        );
    }

    #[test]
    fn a_plan_shows_its_steps() {
        let plan = json!({"plan": [
            {"step": "Read the code", "status": "completed"},
            {"step": "Fix the bug", "status": "in_progress"},
            {"step": "Run the tests", "status": "pending"}
        ]});
        let lines = finished("update_plan", plan, "Plan updated");
        assert_eq!(
            lines[1..],
            [
                "☰ Plan 1/3",
                "  ✓ Read the code",
                "  ● Fix the bug",
                "  ○ Run the tests",
            ]
        );
    }

    #[test]
    fn events_become_items() {
        let mut view = View::new();
        let turn = "t".to_owned();
        view.apply(&Event::TurnStarted { turn: turn.clone() });
        view.apply(&Event::TextDelta {
            turn: turn.clone(),
            text: "Let me look.".into(),
        });
        assert_eq!(view.activity, Activity::Generating);
        assert_eq!(view.live_lines(40, 10).len(), 1);
        view.apply(&Event::MessageAppended {
            turn: turn.clone(),
            message: Message::assistant("Let me look."),
            usage: None,
        });
        view.apply(&Event::ActionStarted {
            turn: turn.clone(),
            call_id: "1".into(),
            name: "grep".into(),
            input: json!({ "pattern": "fn main" }),
        });
        assert_eq!(view.activity, Activity::Running);
        view.apply(&Event::ActionResult {
            turn: turn.clone(),
            call_id: "1".into(),
            name: "grep".into(),
            output: "src/main.rs:1: fn main() {\nsrc/bin.rs:3: fn main() {".into(),
            is_error: false,
        });
        view.apply(&Event::TurnCompleted {
            turn,
            outcome: Outcome::Failed,
            error: Some("boom".into()),
            usage: Usage::default(),
        });
        let items = view.take_committed();
        assert_eq!(items[0], Item::answer("Let me look."));
        // A search, with the reads and searches after it, is one item once something else comes.
        assert!(matches!(&items[1], Item::Explored(calls) if calls[0].summary == "fn main"));
        assert_eq!(items[2], Item::Notice(Level::Error, "error: boom".into()));
        assert!(!view.is_busy());
        let lines: Vec<String> = items[1].lines(60).iter().map(Line::to_string).collect();
        assert_eq!(
            lines,
            [
                "",
                "◇ Explored · 1 search",
                "  └ search fn main · 2 matching lines"
            ]
        );
        view.show_tools();
        let details = view.take_committed();
        assert!(
            details[0]
                .lines(40)
                .iter()
                .any(|line| line.to_string().contains("src/bin.rs"))
        );
        view.apply(&Event::TurnStarted {
            turn: "next".into(),
        });
        view.show_tools();
        assert!(matches!(&view.take_committed()[..], [Item::Notice(..)]));
    }

    #[test]
    fn replies_reach_the_scrollback_as_they_stream() {
        let mut view = View::new();
        let turn = "t".to_owned();
        let delta = |view: &mut View, text: &str, reasoning: bool| {
            let text = text.to_owned();
            let turn = turn.clone();
            view.apply(&match reasoning {
                true => Event::ReasoningDelta { turn, text },
                false => Event::TextDelta { turn, text },
            });
        };
        // Reasoning stays live, all of it, while it goes on.
        delta(&mut view, "First thought.\nSecond", true);
        delta(&mut view, " thought.\nThird", true);
        assert!(view.take_committed().is_empty());
        assert_eq!(view.live_lines(40, 10).len(), 3);
        // Once the answer starts it goes to the scrollback whole; the answer goes in paragraph by paragraph, not
        // inside a code block.
        delta(&mut view, "One.\n\n```\na\n\nb\n", false);
        let items = view.take_committed();
        let thought = "First thought.\nSecond thought.\nThird";
        assert_eq!(
            items,
            [
                Item::Reasoning {
                    text: thought.into(),
                    seconds: Some(0)
                },
                Item::answer("One.")
            ]
        );
        let lines: Vec<String> = items[0].lines(80).iter().map(Line::to_string).collect();
        assert_eq!(
            lines,
            [
                "",
                "∴ Thought for 0s",
                "  First thought.",
                "  Second thought.",
                "  Third"
            ]
        );
        delta(&mut view, "```\nAfter.", false);
        assert_eq!(
            view.take_committed(),
            [Item::Assistant {
                text: "```\na\n\nb\n```".into(),
                update: false,
                opens: false
            }]
        );
        view.apply(&Event::MessageAppended {
            turn: turn.clone(),
            message: Message::assistant("ignored"),
            usage: None,
        });
        assert_eq!(
            view.take_committed(),
            [Item::Assistant {
                text: "After.".into(),
                update: false,
                opens: false
            }]
        );
        assert!(view.live_lines(40, 10).is_empty());
        // Reasoning that ends in a tool call, with no answer, collapses as well.
        delta(&mut view, "Look at the file.", true);
        view.apply(&Event::MessageAppended {
            turn,
            message: Message::new(Role::Assistant, Vec::new()),
            usage: None,
        });
        assert!(
            matches!(&view.take_committed()[..], [Item::Reasoning { text, .. }] if text == "Look at the file.")
        );
    }

    #[test]
    fn updates_are_marked_and_dim_and_the_answer_is_not() {
        let mut view = View::new();
        let turn = "t".to_owned();
        let start = |view: &mut View, phase| {
            view.apply(&Event::MessageStarted {
                turn: turn.clone(),
                phase,
            })
        };
        let text = |view: &mut View, text: &str| {
            view.apply(&Event::TextDelta {
                turn: turn.clone(),
                text: text.into(),
            })
        };
        start(&mut view, Some(Phase::Commentary));
        text(&mut view, "Looking.\n\nStill looking.\n\n");
        start(&mut view, Some(Phase::FinalAnswer));
        text(&mut view, "Done.");
        view.apply(&Event::MessageAppended {
            turn: turn.clone(),
            message: Message::assistant("Looking. Done."),
            usage: None,
        });
        let items = view.take_committed();
        let lines =
            |item: &Item| -> Vec<String> { item.lines(40).iter().map(Line::to_string).collect() };
        // The update's first line is marked, the rest lined up under it.
        assert_eq!(
            lines(&items[0])[1..],
            ["• Looking.", "", "  Still looking."]
        );
        assert_eq!(items[1], Item::answer("Done."));
        assert_eq!(lines(&items[1])[1], "Done.");
        assert_eq!(items[0].lines(40)[1].spans[1].style.fg, theme().dim);
    }

    #[test]
    fn reads_and_searches_group_until_something_else_comes() {
        let mut view = View::new();
        let turn = "t".to_owned();
        for (id, name, input) in [
            ("1", "grep", json!({"pattern": "fn main"})),
            ("2", "read", json!({"path": "src/main.rs"})),
        ] {
            view.apply(&Event::ActionStarted {
                turn: turn.clone(),
                call_id: id.into(),
                name: name.into(),
                input,
            });
            view.apply(&Event::ActionResult {
                turn: turn.clone(),
                call_id: id.into(),
                name: name.into(),
                output: "a\nb".into(),
                is_error: false,
            });
        }
        // Live while it goes on, then one item.
        assert!(view.take_committed().is_empty());
        assert_eq!(
            view.running_lines(60)[0].to_string(),
            "◇ Exploring · 1 search, 1 read"
        );
        view.apply(&Event::ActionStarted {
            turn: turn.clone(),
            call_id: "3".into(),
            name: "apply_patch".into(),
            input: json!({"input": "*** Begin Patch\n*** Update File: src/main.rs\n-a\n+b\n+c\n*** End Patch"}),
        });
        view.apply(&Event::ActionResult {
            turn,
            call_id: "3".into(),
            name: "apply_patch".into(),
            output: "M src/main.rs".into(),
            is_error: false,
        });
        let items = view.take_committed();
        assert!(matches!(&items[..], [Item::Explored(calls), Item::Tool(_)] if calls.len() == 2));
        assert_eq!(view.changes().get("src/main.rs"), Some(&(2, 1)));
    }

    #[test]
    fn reasoning_uses_all_the_available_transcript_rows() {
        let mut view = View::new();
        let text = (1..=12)
            .map(|i| format!("thought {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.apply(&Event::ReasoningDelta {
            turn: "t".into(),
            text,
        });
        assert_eq!(view.live_lines(80, 20).len(), 12);
        assert_eq!(view.live_lines(80, 3)[0].to_string(), "∴ thought 10");
    }

    #[test]
    fn a_streamed_block_waits_for_its_matching_fence() {
        for (open, body, close) in [("~~~~rs", "~~~", "~~~~"), ("````md", "```", "````")] {
            let partial = format!("{open}\n{body}\n\ncode\n");
            assert_eq!(paragraph_end(&partial), 0);
            let finished = format!("{partial}{close}\n");
            assert_eq!(paragraph_end(&format!("{finished}next")), finished.len());
        }
    }

    #[test]
    fn token_counts_are_short() {
        assert_eq!(tokens(850), "850");
        assert_eq!(tokens(12_345), "12.3k");
    }

    #[test]
    fn a_clear_drops_what_was_not_yet_written_and_asks_once_for_a_wipe() {
        let mut view = View::new();
        view.notice(Level::Info, "later message");
        view.clear();
        view.replay(&[Message::user("kept question")]);
        assert!(view.take_cleared());
        assert!(!view.take_cleared());
        let items = view.take_committed();
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], Item::User(text) if text == "kept question"));
    }
}
