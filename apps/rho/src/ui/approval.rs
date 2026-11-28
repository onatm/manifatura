//! The focused approval that temporarily occupies the composer: one pending request from the agent's
//! [`Approvals`](manifatura::kit::Approvals), answered by id.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use manifatura::kit::{Answer, Request};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::text::plain;
use super::view::{accent, clip, dim, user_message_style};

/// One choice: its label, a shorter one for narrow terminals, its key and its answer.
struct Choice {
    label: String,
    short: &'static str,
    key: char,
    answer: Answer,
}

pub(crate) struct Ask {
    pub(crate) request: Request,
    choices: Vec<Choice>,
    selected: usize,
}

impl Ask {
    pub(crate) fn new(request: Request) -> Self {
        let session = match request.call.name.as_str() {
            "exec_command" | "write_stdin" | "bash" => "Allow this command for the session",
            _ => "Allow this tool for the session",
        };
        let mut choices = vec![
            Choice {
                label: "Allow once".into(),
                short: "Once",
                key: 'y',
                answer: Answer::Allow,
            },
            Choice {
                label: session.into(),
                short: "Session",
                key: 'a',
                answer: Answer::AllowAlways,
            },
        ];
        if let Some(rules) = &request.remember {
            choices.push(Choice {
                label: format!("Always allow {rules}"),
                short: "Always",
                key: 'r',
                answer: Answer::AllowAndRemember,
            });
        }
        choices.push(Choice {
            label: "Deny".into(),
            short: "Deny",
            key: 'n',
            answer: Answer::Deny(None),
        });
        Self {
            request,
            choices,
            selected: 0,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.request.id
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Option<Answer> {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return None;
        }
        let last = self.choices.len() - 1;
        let choice = match key.code {
            KeyCode::Up => {
                self.selected = self.selected.checked_sub(1).unwrap_or(last);
                return None;
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1) % self.choices.len();
                return None;
            }
            _ if key.kind != KeyEventKind::Press => return None,
            KeyCode::Enter => self.selected,
            KeyCode::Esc => last,
            KeyCode::Char(c) => {
                let c = c.to_ascii_lowercase();
                match c.to_digit(10) {
                    Some(n) if (1..=self.choices.len() as u32).contains(&n) => n as usize - 1,
                    _ => self.choices.iter().position(|choice| choice.key == c)?,
                }
            }
            _ => return None,
        };
        Some(self.choices[choice].answer.clone())
    }

    fn title(&self) -> String {
        let call = &self.request.call;
        let escalated = call.input["sandbox_permissions"] == "require_escalated";
        match call.name.as_str() {
            "exec_command" if escalated => "Run this command outside the sandbox?".into(),
            "exec_command" | "bash" => "Run this command?".into(),
            "write_stdin" => format!(
                "Type into command {}, which runs outside the sandbox?",
                call.input["session_id"]
            ),
            "apply_patch" => "Apply this patch?".into(),
            "read" => "Read this file?".into(),
            name => format!("Run {name}?"),
        }
    }

    fn target(&self) -> String {
        let input = &self.request.call.input;
        let field = match self.request.call.name.as_str() {
            "exec_command" => "cmd",
            "bash" => "command",
            "write_stdin" => "chars",
            "apply_patch" => "input",
            "read" => "path",
            "glob" | "grep" => "pattern",
            _ => return input.to_string(),
        };
        match input[field].as_str() {
            Some(value) => value.to_owned(),
            None => input.to_string(),
        }
    }

    fn lines(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        let height = usize::from(height);
        let width = usize::from(width);
        if height == 0 {
            return Vec::new();
        }
        let target = plain(&self.target(), width.saturating_sub(3), Style::new());
        let reason = self
            .request
            .reason
            .as_deref()
            .map(|r| plain(r, width.saturating_sub(3), dim()))
            .unwrap_or_default();
        // Short terminals keep the selected choice visible; larger ones spend the rest on the target.
        let title_rows = usize::from(height >= 2);
        let target_min = usize::from(height >= 3);
        let hint_rows = usize::from(height >= 4);
        let choices = self
            .choices
            .len()
            .min(height - title_rows - target_min - hint_rows);
        let mut room = height - title_rows - hint_rows - choices;
        let reason_rows = reason.len().min(room.saturating_sub(target_min + 1));
        room -= reason_rows;
        let target_rows = target.len().min(room);
        let spare = room - target_rows;
        let mut rows = Vec::new();
        if spare >= 2 {
            rows.push(Line::default());
        }
        if title_rows > 0 {
            rows.push(Line::styled(
                format!("  {}", clip(&self.title(), width.saturating_sub(2))),
                Style::new().add_modifier(Modifier::BOLD),
            ));
        }
        let truncated = target.len() > target_rows;
        for (i, mut line) in target.into_iter().take(target_rows).enumerate() {
            line.spans.insert(0, Span::raw("  "));
            if truncated && i + 1 == target_rows {
                line.push_span("…");
            }
            rows.push(line);
        }
        for mut line in reason.into_iter().take(reason_rows) {
            line.spans.insert(0, Span::raw("  "));
            rows.push(line);
        }
        if spare >= 1 {
            rows.push(Line::default());
        }
        let first = (self.selected + 1).saturating_sub(choices);
        for (i, choice) in self.choices.iter().enumerate().skip(first).take(choices) {
            let (mark, style) = if i == self.selected {
                ("›", accent())
            } else {
                (" ", Style::new())
            };
            let full = format!("{} ({})", choice.label, choice.key);
            let short = format!("{} ({})", choice.short, choice.key);
            let label = if full.len() + 5 <= width { full } else { short };
            rows.push(Line::styled(
                format!(
                    "{mark} {}. {}",
                    i + 1,
                    clip(&label, width.saturating_sub(5))
                ),
                style,
            ));
        }
        if spare >= 3 {
            rows.push(Line::default());
        }
        if hint_rows > 0 {
            let hint = if width >= 38 {
                "  ↑↓ select · Enter confirm · Esc deny"
            } else {
                "  ↑↓ select · Enter · Esc"
            };
            rows.push(Line::styled(hint, dim()));
        }
        rows
    }

    pub(super) fn height(&self, width: u16, limit: u16) -> u16 {
        self.lines(width, limit).len() as u16
    }

    pub(super) fn draw(&self, f: &mut Frame, area: Rect) {
        let lines = self.lines(area.width, area.height);
        let height = lines.len() as u16;
        let area = Rect {
            y: area.bottom() - height,
            height,
            ..area
        };
        let body = Rect {
            height: height.saturating_sub(u16::from(height >= 4)),
            ..area
        };
        f.render_widget(Block::new().style(user_message_style()), body);
        f.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use manifatura::{SessionId, ToolCall};

    use super::*;

    fn request(input: serde_json::Value, remember: Option<&str>) -> Request {
        let call = ToolCall {
            id: "1".into(),
            name: "exec_command".into(),
            input,
        };
        let mut r = Request::new(SessionId::from("s"), call).reason("Installs the dependencies.");
        if let Some(rules) = remember {
            r = r.remember(rules);
        }
        r
    }

    #[test]
    fn an_escalation_shows_its_command_reason_and_the_rule_to_remember() {
        let input =
            serde_json::json!({"cmd": "npm install", "sandbox_permissions": "require_escalated"});
        let mut ask = Ask::new(request(input, Some("exec_command(npm install)")));
        let rows: Vec<String> = ask.lines(80, 12).iter().map(Line::to_string).collect();
        let text = rows.join("\n");
        assert!(
            text.contains("Run this command outside the sandbox?"),
            "{text}"
        );
        assert!(
            text.contains("npm install") && text.contains("Installs the dependencies."),
            "{text}"
        );
        assert!(
            text.contains("Always allow exec_command(npm install) (r)"),
            "{text}"
        );
        assert_eq!(
            ask.key(KeyEvent::from(KeyCode::Char('r'))),
            Some(Answer::AllowAndRemember)
        );
        assert_eq!(
            ask.key(KeyEvent::from(KeyCode::Char('a'))),
            Some(Answer::AllowAlways)
        );
        assert_eq!(
            ask.key(KeyEvent::from(KeyCode::Char('4'))),
            Some(Answer::Deny(None))
        );
        assert_eq!(
            ask.key(KeyEvent::from(KeyCode::Esc)),
            Some(Answer::Deny(None))
        );
    }

    #[test]
    fn without_a_rule_to_offer_there_are_three_choices() {
        let mut ask = Ask::new(request(serde_json::json!({"cmd": "rm -rf build"}), None));
        assert_eq!(ask.choices.len(), 3);
        assert_eq!(ask.key(KeyEvent::from(KeyCode::Char('r'))), None);
        assert_eq!(
            ask.key(KeyEvent::from(KeyCode::Char('3'))),
            Some(Answer::Deny(None))
        );
        // Short terminals keep the selection visible.
        assert_eq!(ask.key(KeyEvent::from(KeyCode::Up)), None);
        let rows: Vec<String> = ask.lines(30, 4).iter().map(Line::to_string).collect();
        assert!(rows.iter().any(|r| r.contains("› 3.")), "{rows:?}");
    }
}
