//! The sidebar, beside the composer when it is open (Ctrl-B, `/sidebar`, or `sidebar = true` in the config) and
//! the terminal is wide enough: the session's model, effort and context, the plan, the files changed, the
//! background commands and subagents, the MCP servers and the skills. The scrollback above keeps the full width.

use manifatura::skills::Visibility;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::App;
use super::tasks::Task;
use super::theme::{accent, agent, dim, error, ok, theme, warn};
use super::view::{clip, tokens};

/// The sidebar's width, and the narrowest terminal it opens in.
pub(super) const WIDTH: u16 = 36;
pub(super) const MIN_TERMINAL: u16 = 100;

/// How many rows each list shows before it says how many more.
const LIST: usize = 4;

/// The context used, as a meter of `cells` cells and the share, coloured as it fills: from 80% and from 95%.
pub(super) fn meter(used: u64, window: u32, cells: usize) -> (String, ratatui::style::Style) {
    let share = used * 100 / u64::from(window).max(1);
    let full = ((share as usize * cells).div_ceil(100)).min(cells);
    let style = match share {
        95.. => error(),
        80.. => warn(),
        _ => ok(),
    };
    (
        format!("{}{} {share}%", "▰".repeat(full), "▱".repeat(cells - full)),
        style,
    )
}

impl App {
    /// Whether the sidebar shows in a terminal `width` wide.
    pub(super) fn sidebar_shown(&self, width: u16) -> bool {
        self.sidebar && width >= MIN_TERMINAL
    }

    pub(super) fn sidebar_lines(&self) -> Vec<Line<'static>> {
        let width = usize::from(WIDTH) - 2;
        let heading = |name: String| Line::styled(name, dim().add_modifier(Modifier::BOLD));
        let more = |hidden: usize| Line::styled(format!("⋯ {hidden} more"), dim());
        let mut out = vec![heading("SESSION".into())];
        out.push(Line::from(vec![
            Span::raw(clip(&self.model, width.saturating_sub(10))),
            Span::styled(format!(" · {}", self.effort_label()), dim()),
        ]));
        match self.context {
            Some((used, Some(window))) => {
                let (bar, style) = meter(used, window, 16);
                out.push(Line::styled(bar, style));
                out.push(Line::styled(
                    format!("{} of {}", tokens(used), tokens(window.into())),
                    dim(),
                ));
            }
            Some((used, None)) => out.push(Line::styled(format!("{} used", tokens(used)), dim())),
            None => {}
        }
        let view = self.main_view_ref();
        let plan = view.plan();
        if !plan.is_empty() {
            let done = plan.iter().filter(|(_, s)| s == "completed").count();
            out.push(Line::default());
            out.push(heading(format!("PLAN {done}/{}", plan.len())));
            for (step, status) in plan {
                let (mark, style) = match status.as_str() {
                    "completed" => ("✓ ", dim()),
                    "in_progress" => ("● ", accent()),
                    _ => ("○ ", ratatui::style::Style::new()),
                };
                out.push(Line::styled(clip(&format!("{mark}{step}"), width), style));
            }
        }
        let changes = view.changes();
        if !changes.is_empty() {
            out.push(Line::default());
            out.push(heading("CHANGES".into()));
            for (file, (added, removed)) in changes.iter().take(LIST) {
                let counts = format!(" +{added} −{removed}");
                let name = file.rsplit('/').next().unwrap_or(file);
                out.push(Line::from(vec![
                    Span::raw(clip(name, width.saturating_sub(counts.len()))),
                    Span::styled(format!(" +{added}"), ok()),
                    Span::styled(format!(" −{removed}"), error()),
                ]));
            }
            if changes.len() > LIST {
                out.push(more(changes.len() - LIST));
            }
        }
        let tasks = self.task_list();
        if !tasks.is_empty() {
            out.push(Line::default());
            out.push(heading("TASKS".into()));
            for task in tasks.iter().rev().take(LIST) {
                let line = match task {
                    Task::Command(p) => {
                        let (mark, style) = match p.exit {
                            None => ("● ", ok()),
                            Some(0) => ("○ ", dim()),
                            Some(_) => ("✗ ", error()),
                        };
                        Line::from(vec![
                            Span::styled(mark, style),
                            Span::raw(clip(&p.command, width.saturating_sub(8))),
                            Span::styled(format!(" · {}", p.id), dim()),
                        ])
                    }
                    Task::Subagent(i) => {
                        let child = &self.children[*i];
                        let mark = if child.finished.is_none() {
                            "↳ "
                        } else {
                            "✓ "
                        };
                        Line::from(vec![
                            Span::styled(mark, agent()),
                            Span::raw(clip(&child.agent, width / 2)),
                            Span::styled(format!(" · {}", clip(&child.state(), width / 2)), dim()),
                        ])
                    }
                };
                out.push(line);
            }
            if tasks.len() > LIST {
                out.push(more(tasks.len() - LIST));
            }
        }
        if let Some(ready) = &self.ready {
            if !ready.servers.is_empty() {
                out.push(Line::default());
                out.push(heading("MCP".into()));
                for (name, tools, _) in ready.servers.iter().take(LIST) {
                    out.push(Line::from(vec![
                        Span::styled("● ", ok()),
                        Span::raw(clip(name, width / 2)),
                        Span::styled(format!(" · {} tools", tools.len()), dim()),
                    ]));
                }
                if ready.servers.len() > LIST {
                    out.push(more(ready.servers.len() - LIST));
                }
            }
            if !ready.skills.is_empty() {
                let mut counts = [0; 4];
                for skill in ready.skills.skills() {
                    counts[match ready.skills.visibility(&skill.name) {
                        Visibility::NameOnly => 1,
                        Visibility::UserOnly => 2,
                        Visibility::Off => 3,
                        _ => 0,
                    }] += 1;
                }
                let names = ["on", "name-only", "user-only", "off"];
                let text: Vec<String> = counts
                    .iter()
                    .zip(names)
                    .filter(|(n, _)| **n > 0)
                    .map(|(n, name)| format!("{n} {name}"))
                    .collect();
                out.push(Line::default());
                out.push(heading("SKILLS".into()));
                out.push(Line::styled(clip(&text.join(" · "), width), dim()));
            }
        }
        out
    }

    /// Draws the sidebar in `area`, its rule on the left; what does not fit is left out from the bottom.
    pub(super) fn draw_sidebar(&self, f: &mut Frame, area: Rect) {
        let rule = theme().rule;
        for y in area.top()..area.bottom() {
            f.render_widget(
                Span::styled("│", ratatui::style::Style::new().fg(rule)),
                Rect {
                    y,
                    height: 1,
                    width: 1,
                    ..area
                },
            );
        }
        for (i, line) in self
            .sidebar_lines()
            .into_iter()
            .take(area.height.into())
            .enumerate()
        {
            f.render_widget(
                line,
                Rect {
                    x: area.x + 2,
                    y: area.y + i as u16,
                    width: area.width.saturating_sub(2),
                    height: 1,
                },
            );
        }
    }
}
