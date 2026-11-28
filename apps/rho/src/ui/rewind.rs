//! The rewind picker: one of your earlier messages, then what to put back as it was before it: the files, the
//! conversation, or both.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use super::view::{accent, clip, dim};

/// A message to go back to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Point {
    /// Its entry in the session.
    pub(crate) id: String,
    /// The entry before it, which the conversation goes back to.
    pub(crate) parent: Option<String>,
    /// What was typed.
    pub(crate) text: String,
    /// The files as its turn found them, if a snapshot was taken.
    pub(crate) tree: Option<String>,
}

/// What to put back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Restore {
    Both,
    Conversation,
    Code,
}

impl Restore {
    fn label(self) -> &'static str {
        match self {
            Restore::Both => "Restore the code and the conversation",
            Restore::Conversation => "Restore the conversation only",
            Restore::Code => "Restore the code only",
        }
    }
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Cancel,
    Go(Point, Restore),
}

pub(crate) struct Rewind {
    /// Newest first.
    points: Vec<Point>,
    selected: usize,
    /// The point chosen, and the option highlighted.
    choosing: Option<usize>,
}

impl Rewind {
    /// A picker over `points`, newest first.
    pub(crate) fn new(points: Vec<Point>) -> Self {
        Self {
            points,
            selected: 0,
            choosing: None,
        }
    }

    fn options(&self) -> Vec<Restore> {
        match self.points[self.selected].tree {
            Some(_) => vec![Restore::Both, Restore::Conversation, Restore::Code],
            None => vec![Restore::Conversation],
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        match (self.choosing, key.code) {
            (_, _) if ctrl_c => Some(Outcome::Cancel),
            (None, KeyCode::Esc) => Some(Outcome::Cancel),
            (Some(_), KeyCode::Esc) => {
                self.choosing = None;
                None
            }
            (None, KeyCode::Up) => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            (None, KeyCode::Down) => {
                self.selected = (self.selected + 1).min(self.points.len().saturating_sub(1));
                None
            }
            (None, KeyCode::Enter) => {
                self.choosing = Some(0);
                None
            }
            (Some(i), KeyCode::Up) => {
                self.choosing = Some(i.saturating_sub(1));
                None
            }
            (Some(i), KeyCode::Down) => {
                self.choosing = Some((i + 1).min(self.options().len() - 1));
                None
            }
            (Some(i), KeyCode::Enter) => {
                let restore = self.options()[i];
                Some(Outcome::Go(self.points[self.selected].clone(), restore))
            }
            _ => None,
        }
    }

    fn lines(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        let (width, height) = (usize::from(width), usize::from(height));
        let mut rows = Vec::new();
        match self.choosing {
            None => {
                rows.push(Line::styled("Rewind to before a message", accent()));
                let count = height.saturating_sub(2).max(1);
                let first = (self.selected + 1).saturating_sub(count);
                for (i, p) in self.points.iter().enumerate().skip(first).take(count) {
                    let (mark, style) = if i == self.selected {
                        ("› ", accent())
                    } else {
                        ("  ", Style::new())
                    };
                    let files = if p.tree.is_some() {
                        ""
                    } else {
                        "  (no snapshot of the files)"
                    };
                    let text = p.text.lines().next().unwrap_or_default();
                    let room = width.saturating_sub(2 + files.len());
                    rows.push(Line::from(vec![
                        ratatui::text::Span::styled(mark, style),
                        ratatui::text::Span::styled(clip(text, room), style),
                        ratatui::text::Span::styled(files, dim()),
                    ]));
                }
                rows.push(Line::styled("↑↓ select · Enter choose · Esc cancel", dim()));
            }
            Some(chosen) => {
                let text = self.points[self.selected]
                    .text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                rows.push(Line::styled(
                    clip(&format!("Before: {text}"), width),
                    Style::new().add_modifier(Modifier::BOLD),
                ));
                for (i, option) in self.options().into_iter().enumerate() {
                    let (mark, style) = if i == chosen {
                        ("› ", accent())
                    } else {
                        ("  ", Style::new())
                    };
                    rows.push(Line::styled(format!("{mark}{}", option.label()), style));
                }
                rows.push(Line::styled("↑↓ select · Enter restore · Esc back", dim()));
            }
        }
        rows.truncate(height);
        rows
    }

    pub(super) fn height(&self, width: u16, limit: u16) -> u16 {
        self.lines(width, limit).len() as u16
    }

    pub(super) fn draw(&self, f: &mut Frame, area: Rect) {
        let lines = self.lines(area.width, area.height);
        let top = area.bottom() - lines.len() as u16;
        for (i, line) in lines.into_iter().enumerate() {
            f.render_widget(
                line,
                Rect {
                    y: top + i as u16,
                    height: 1,
                    ..area
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(id: &str, tree: Option<&str>) -> Point {
        Point {
            id: id.into(),
            parent: Some(format!("{id}-parent")),
            text: format!("message {id}"),
            tree: tree.map(str::to_string),
        }
    }

    #[test]
    fn pick_a_message_then_what_to_restore() {
        let mut rw = Rewind::new(vec![point("b", Some("t2")), point("a", Some("t1"))]);
        let key = |code| KeyEvent::from(code);
        assert_eq!(rw.key(key(KeyCode::Down)), None);
        let rows: Vec<String> = rw.lines(60, 8).iter().map(Line::to_string).collect();
        assert_eq!(rows[2], "› message a");
        assert_eq!(rw.key(key(KeyCode::Enter)), None);
        assert_eq!(rw.key(key(KeyCode::Down)), None);
        assert_eq!(rw.key(key(KeyCode::Down)), None);
        let rows: Vec<String> = rw.lines(60, 8).iter().map(Line::to_string).collect();
        assert_eq!(rows[3], "› Restore the code only");
        assert_eq!(
            rw.key(key(KeyCode::Enter)),
            Some(Outcome::Go(point("a", Some("t1")), Restore::Code))
        );
        // Esc goes back a step, then cancels.
        assert_eq!(rw.key(key(KeyCode::Esc)), None);
        assert_eq!(rw.key(key(KeyCode::Esc)), Some(Outcome::Cancel));
    }

    #[test]
    fn without_a_snapshot_only_the_conversation_goes_back() {
        let mut rw = Rewind::new(vec![point("a", None)]);
        let rows: Vec<String> = rw.lines(70, 8).iter().map(Line::to_string).collect();
        assert!(rows[1].ends_with("(no snapshot of the files)"), "{rows:?}");
        rw.key(KeyEvent::from(KeyCode::Enter));
        rw.key(KeyEvent::from(KeyCode::Down));
        assert_eq!(
            rw.key(KeyEvent::from(KeyCode::Enter)),
            Some(Outcome::Go(point("a", None), Restore::Conversation))
        );
    }
}
