//! The `/model` list: the plan's models, each with the effort it would run at. ↑↓ move between models, and a
//! model moved to starts at its default effort (the current one keeps the session's); ←→ change the effort; Enter
//! uses both, for this session and the ones started after; Esc closes without changing anything.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use manifatura::Effort;
use manifatura::auth::openai::Model;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::view::{clip, copper, dim};
use crate::agent::{effort_label, efforts, fit_effort};

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Cancel,
    /// Enter: this model at this effort.
    Choose(String, Option<Effort>),
}

pub(crate) struct ModelPicker {
    models: Vec<Model>,
    /// The session's model and effort when the list opened.
    current: (String, Option<Effort>),
    selected: usize,
    /// The effort the selected model would run at.
    effort: Option<Effort>,
}

impl ModelPicker {
    pub(crate) fn new(models: &[Model], model: &str, effort: Option<Effort>) -> Self {
        let selected = models.iter().position(|m| m.slug == model).unwrap_or(0);
        let mut picker = Self {
            models: models.to_vec(),
            current: (model.to_owned(), effort),
            selected,
            effort: None,
        };
        picker.select(selected);
        picker
    }

    /// Moves to model `at`: the current model at the session's effort, another at its default.
    fn select(&mut self, at: usize) {
        self.selected = at;
        let Some(model) = self.models.get(at) else {
            return;
        };
        let wanted = if model.slug == self.current.0 {
            self.current.1
        } else {
            None
        };
        self.effort = fit_effort(&self.models, &model.slug, &[wanted]);
    }

    fn levels(&self) -> Vec<(Effort, String)> {
        self.models
            .get(self.selected)
            .map(|m| efforts(&self.models, &m.slug))
            .unwrap_or_default()
    }

    /// Moves the effort one level down (`-1`) or up (`1`), stopping at the ends.
    fn step(&mut self, by: isize) {
        let levels = self.levels();
        if levels.is_empty() {
            return;
        }
        let at = self
            .effort
            .and_then(|e| levels.iter().position(|(l, _)| *l == e));
        let next = match at {
            Some(at) => at.saturating_add_signed(by).min(levels.len() - 1),
            None if by < 0 => 0,
            None => levels.len() - 1,
        };
        self.effort = Some(levels[next].0);
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.kind == KeyEventKind::Release || self.models.is_empty() {
            return (key.code == KeyCode::Esc).then_some(Outcome::Cancel);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Some(Outcome::Cancel),
            KeyCode::Char('c') if ctrl => return Some(Outcome::Cancel),
            KeyCode::Enter => {
                return Some(Outcome::Choose(
                    self.models[self.selected].slug.clone(),
                    self.effort,
                ));
            }
            KeyCode::Up => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down => self.select((self.selected + 1).min(self.models.len() - 1)),
            KeyCode::Left => self.step(-1),
            KeyCode::Right => self.step(1),
            _ => {}
        }
        None
    }

    fn lines(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        let (width, height) = (usize::from(width), usize::from(height));
        let mut rows = vec![Line::from(vec![
            Span::styled("Model", copper()),
            Span::styled(
                format!(
                    " · now {} at {} effort",
                    self.current.0,
                    effort_label(self.current.1)
                ),
                dim(),
            ),
        ])];
        // The effort row's description, under the list.
        let about = self
            .levels()
            .into_iter()
            .find(|(e, _)| Some(*e) == self.effort)
            .map(|(_, about)| about)
            .filter(|about| !about.is_empty());
        let count = height
            .saturating_sub(2 + usize::from(about.is_some()))
            .max(1);
        let first = (self.selected + 1).saturating_sub(count);
        let slug_width = self
            .models
            .iter()
            .map(|m| m.slug.chars().count())
            .max()
            .unwrap_or(0)
            .min(32);
        for (at, model) in self.models.iter().enumerate().skip(first).take(count) {
            let chosen = at == self.selected;
            let (mark, style) = if chosen {
                ("› ", copper().add_modifier(Modifier::BOLD))
            } else {
                ("  ", Style::new())
            };
            let now = if model.slug == self.current.0 {
                "● "
            } else {
                "  "
            };
            let mut spans = vec![
                Span::styled(mark, style),
                Span::styled(now, copper()),
                Span::styled(
                    format!("{:<slug_width$}  ", clip(&model.slug, slug_width)),
                    style,
                ),
            ];
            if chosen {
                // Every level, the chosen one marked.
                for (effort, _) in self.levels() {
                    let label = effort_label(Some(effort));
                    if Some(effort) == self.effort {
                        spans.push(Span::styled(format!("[{label}] "), copper()));
                    } else {
                        spans.push(Span::styled(format!(" {label}  "), dim()));
                    }
                }
            } else {
                let default = effort_label(model.default_effort);
                let room = width.saturating_sub(4 + slug_width + 2);
                let name = if model.display_name == model.slug {
                    format!("default {default}")
                } else {
                    format!("{} · default {default}", model.display_name)
                };
                spans.push(Span::styled(clip(&name, room), dim()));
            }
            rows.push(Line::from(spans));
        }
        if let Some(about) = about {
            rows.push(Line::styled(
                format!("  {}", clip(&about, width.saturating_sub(2))),
                dim(),
            ));
        }
        rows.push(Line::styled(
            "↑↓ model · ←→ effort · Enter use for this and new sessions · Esc cancel",
            dim(),
        ));
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

    fn models() -> Vec<Model> {
        let model = |slug: &str, default: Effort| {
            let mut m = Model::new(slug);
            m.default_effort = Some(default);
            m.efforts = [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh]
                .into_iter()
                .map(|e| (e, String::new()))
                .collect();
            m
        };
        vec![model("a", Effort::Medium), model("b", Effort::High)]
    }

    #[test]
    fn another_model_starts_at_its_default_and_arrows_change_the_effort() {
        let key = |code| KeyEvent::from(code);
        let mut picker = ModelPicker::new(&models(), "a", Some(Effort::Low));
        // The current model keeps the session's effort.
        assert_eq!(picker.effort, Some(Effort::Low));
        picker.key(key(KeyCode::Down));
        assert_eq!(picker.effort, Some(Effort::High));
        picker.key(key(KeyCode::Right));
        picker.key(key(KeyCode::Right));
        assert_eq!(picker.effort, Some(Effort::XHigh));
        picker.key(key(KeyCode::Up));
        assert_eq!(picker.effort, Some(Effort::Low));
        picker.key(key(KeyCode::Down));
        picker.key(key(KeyCode::Left));
        assert_eq!(
            picker.key(key(KeyCode::Enter)),
            Some(Outcome::Choose("b".into(), Some(Effort::Medium)))
        );
        let rows: Vec<String> = picker.lines(100, 10).iter().map(Line::to_string).collect();
        assert!(rows[2].contains("[medium]"), "{rows:?}");
        assert!(rows[1].contains("default medium"), "{rows:?}");
        assert_eq!(picker.key(key(KeyCode::Esc)), Some(Outcome::Cancel));
    }
}
