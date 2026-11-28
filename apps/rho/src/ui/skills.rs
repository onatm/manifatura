//! The `/skills` list: every skill found, with where it comes from, what its catalog entry costs the prompt and who
//! sees it. Typing filters; `t` (with nothing typed) sorts by cost; Space or Enter cycles who sees the selected
//! skill; Esc saves the choices for the project and closes.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use manifatura::skills::{SkillSet, Source, Visibility};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::view::{accent, clip, dim};

struct Row {
    name: String,
    description: String,
    /// Where it was found: project, user, claude or built in.
    source: &'static str,
    tokens: usize,
    seen: Visibility,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Closed without saving (Ctrl-C).
    Cancel,
    /// Closed with Esc: who sees each skill now.
    Save(BTreeMap<String, Visibility>),
}

pub(crate) struct SkillsPicker {
    rows: Vec<Row>,
    filter: String,
    selected: usize,
    by_tokens: bool,
}

/// Where a skill came from, as the list says it.
fn source(skill: &manifatura::skills::Skill) -> &'static str {
    let from_claude = skill
        .dir
        .as_ref()
        .is_some_and(|dir| dir.components().any(|c| c.as_os_str() == ".claude"));
    match skill.source {
        Source::Builtin => "built in",
        _ if from_claude => "claude",
        Source::Project => "project",
        _ => "user",
    }
}

impl SkillsPicker {
    pub(crate) fn new(skills: &SkillSet) -> Self {
        let rows = skills
            .skills()
            .iter()
            .map(|skill| Row {
                name: skill.name.clone(),
                description: skill.description.clone(),
                source: source(skill),
                tokens: SkillSet::catalog_tokens(skill),
                seen: skills.visibility(&skill.name),
            })
            .collect();
        Self {
            rows,
            filter: String::new(),
            selected: 0,
            by_tokens: false,
        }
    }

    /// The rows shown, in order: those the filter matches, by name or by cost.
    fn shown(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        let mut shown: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let row = &self.rows[i];
                filter.is_empty()
                    || row.name.to_lowercase().contains(&filter)
                    || row.description.to_lowercase().contains(&filter)
                    || row.source.contains(&filter)
            })
            .collect();
        if self.by_tokens {
            shown.sort_by_key(|&i| std::cmp::Reverse(self.rows[i].tokens));
        }
        shown
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shown = self.shown();
        match key.code {
            KeyCode::Char('c') if ctrl => return Some(Outcome::Cancel),
            KeyCode::Esc => {
                return Some(Outcome::Save(
                    self.rows
                        .iter()
                        .map(|row| (row.name.clone(), row.seen))
                        .collect(),
                ));
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(shown.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(&i) = shown.get(self.selected) {
                    self.rows[i].seen = self.rows[i].seen.next();
                }
            }
            KeyCode::Char('t') if self.filter.is_empty() => self.by_tokens = !self.by_tokens,
            KeyCode::Char(c) if !ctrl => {
                self.filter.push(c);
                self.selected = 0;
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            _ => {}
        }
        None
    }

    fn lines(&self, width: u16, height: u16) -> Vec<Line<'static>> {
        let (width, height) = (usize::from(width), usize::from(height));
        let shown = self.shown();
        let mut rows = Vec::new();
        let sort = if self.by_tokens { "by cost" } else { "by name" };
        let filter = if self.filter.is_empty() {
            "type to filter".to_owned()
        } else {
            format!("filter: {}", self.filter)
        };
        rows.push(Line::from(vec![
            Span::styled("Skills", accent()),
            Span::styled(format!(" · {} · {filter} · {sort}", shown.len()), dim()),
        ]));
        if shown.is_empty() {
            rows.push(Line::styled("  no skill matches", dim()));
        }
        let count = height.saturating_sub(2).max(1);
        let selected = self.selected.min(shown.len().saturating_sub(1));
        let first = (selected + 1).saturating_sub(count);
        let name_width = self
            .rows
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0)
            .min(28);
        for (at, &i) in shown.iter().enumerate().skip(first).take(count) {
            let row = &self.rows[i];
            let (mark, style) = if at == selected {
                ("› ", accent())
            } else {
                ("  ", Style::new())
            };
            let state_style = match row.seen {
                Visibility::On => super::theme::ok(),
                Visibility::Off => dim(),
                _ => super::theme::warn(),
            };
            let head = format!("{:<name_width$} ", clip(&row.name, name_width));
            let state = format!("{:<10}", row.seen.as_str());
            let meta = format!("{:<9} ~{:>4} tok  ", row.source, row.tokens);
            let room = width.saturating_sub(2 + head.chars().count() + 10 + meta.chars().count());
            let name_style = if row.seen == Visibility::Off {
                dim()
            } else {
                style.add_modifier(Modifier::BOLD)
            };
            rows.push(Line::from(vec![
                Span::styled(mark, style),
                Span::styled(head, name_style),
                Span::styled(state, state_style),
                Span::styled(meta, dim()),
                Span::styled(
                    clip(row.description.lines().next().unwrap_or_default(), room),
                    dim(),
                ),
            ]));
        }
        rows.push(Line::styled(
            "↑↓ select · Space cycle on, name-only, user-only, off · t sort · Esc save",
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
    use manifatura::skills::Skill;

    use super::*;

    fn set() -> SkillSet {
        let skill = |name: &str, about: &str| {
            Skill::parse(&format!(
                "---\nname: {name}\ndescription: {about}\n---\nDo it.\n"
            ))
            .unwrap()
        };
        SkillSet::of([
            skill("deploy", "Ship it."),
            skill(
                "tour",
                "Explain the codebase, from its entry points down to the details that matter.",
            ),
        ])
        .with_visibility(BTreeMap::from([("deploy".to_string(), Visibility::Off)]))
    }

    #[test]
    fn typing_filters_space_cycles_and_esc_saves() {
        let mut picker = SkillsPicker::new(&set());
        let key = |code| KeyEvent::from(code);
        let rows: Vec<String> = picker.lines(100, 10).iter().map(Line::to_string).collect();
        assert!(
            rows[1].contains("deploy") && rows[1].contains("off"),
            "{rows:?}"
        );
        assert!(
            rows[2].contains("tour") && rows[2].contains("on"),
            "{rows:?}"
        );
        // `t` sorts by cost while nothing is typed: the longer description first.
        picker.key(key(KeyCode::Char('t')));
        let rows: Vec<String> = picker.lines(100, 10).iter().map(Line::to_string).collect();
        assert!(rows[1].contains("tour"), "{rows:?}");
        for c in "dep".chars() {
            picker.key(key(KeyCode::Char(c)));
        }
        assert_eq!(picker.shown().len(), 1);
        picker.key(key(KeyCode::Char(' ')));
        assert_eq!(
            picker.key(key(KeyCode::Esc)),
            Some(Outcome::Save(BTreeMap::from([
                ("deploy".to_string(), Visibility::On),
                ("tour".to_string(), Visibility::On),
            ])))
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(picker.key(ctrl_c), Some(Outcome::Cancel));
    }
}
