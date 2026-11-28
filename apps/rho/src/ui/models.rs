//! The `/model` grid: the plan's models down, their efforts across, each model's default marked `◎` and the
//! session's `●`. ↑↓ move between models, and a model moved to starts at its default effort (the current one keeps
//! the session's); ←→ change the effort; Enter uses both, for this session and the ones started after; Esc closes
//! without changing anything.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use manifatura::Effort;
use manifatura::auth::openai::Model;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::theme::selected;
use super::view::{accent, clip, dim};
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

    /// The efforts any model has, from the least: the grid's columns.
    fn columns(&self) -> Vec<Effort> {
        Effort::ALL
            .into_iter()
            .filter(|e| {
                self.models
                    .iter()
                    .any(|m| efforts(&self.models, &m.slug).iter().any(|(x, _)| x == e))
            })
            .collect()
    }

    fn lines(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        let (width, height) = (usize::from(width), usize::from(height));
        let columns = self.columns();
        let slug_width = self
            .models
            .iter()
            .map(|m| m.slug.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(8, 28);
        // Each effort a column wide enough for its name.
        let cell = columns
            .iter()
            .map(|e| effort_label(Some(*e)).len() + 2)
            .max()
            .unwrap_or(4)
            .max(5);
        let centred = |text: &str| format!("{text:^cell$}");
        let mut rows = vec![Line::from(vec![
            Span::styled("Model and effort", accent()),
            Span::styled(
                format!(
                    " · now {} at {}",
                    self.current.0,
                    effort_label(self.current.1)
                ),
                dim(),
            ),
        ])];
        let mut heading = vec![Span::raw(" ".repeat(4 + slug_width))];
        for effort in &columns {
            heading.push(Span::styled(centred(&effort_label(Some(*effort))), dim()));
        }
        rows.push(Line::from(heading));
        // The selected effort's description, under the grid.
        let about = self
            .levels()
            .into_iter()
            .find(|(e, _)| Some(*e) == self.effort)
            .map(|(e, about)| (effort_label(Some(e)), about))
            .filter(|(_, about)| !about.is_empty());
        let count = height
            .saturating_sub(4 + usize::from(about.is_some()))
            .max(1);
        let first = (self.selected + 1).saturating_sub(count);
        for (at, model) in self.models.iter().enumerate().skip(first).take(count) {
            let chosen = at == self.selected;
            let offered = efforts(&self.models, &model.slug);
            let now = model.slug == self.current.0;
            let mut spans = vec![
                Span::styled(if chosen { "› " } else { "  " }, accent()),
                Span::styled(if now { "● " } else { "  " }, accent()),
                Span::styled(
                    format!("{:<slug_width$}", clip(&model.slug, slug_width)),
                    if chosen { accent() } else { Style::new() },
                ),
            ];
            for effort in &columns {
                let mark = if !offered.iter().any(|(e, _)| e == effort) {
                    " "
                } else if now && self.current.1 == Some(*effort) {
                    "●"
                } else if model.default_effort == Some(*effort) {
                    "◎"
                } else {
                    "○"
                };
                if chosen && self.effort == Some(*effort) {
                    spans.push(Span::styled(centred(&format!("[{mark}]")), accent()));
                } else {
                    spans.push(Span::styled(centred(mark), dim()));
                }
            }
            let mut line = Line::from(spans);
            if chosen {
                line = line.style(selected());
            }
            rows.push(line);
        }
        if let Some((name, about)) = about {
            rows.push(Line::from(vec![
                Span::raw(format!("  {name}: ")),
                Span::styled(clip(&about, width.saturating_sub(name.len() + 4)), dim()),
            ]));
        }
        rows.push(Line::styled(
            clip(
                "◎ its default · ● now · ↑↓ model, at its default · ←→ effort · Enter here and for new sessions · Esc",
                width,
            ),
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
        // A column per effort; the session's cell, each default and the cursor marked.
        assert!(
            rows[1].contains("low") && rows[1].contains("xhigh"),
            "{rows:?}"
        );
        assert!(
            rows[2].starts_with("  ● a") && rows[2].contains("●") && rows[2].contains("◎"),
            "{rows:?}"
        );
        assert!(
            rows[3].starts_with("›") && rows[3].contains("[○]"),
            "{rows:?}"
        );
        assert_eq!(picker.key(key(KeyCode::Esc)), Some(Outcome::Cancel));
    }
}
