//! The background tasks: the commands the agent left running and the subagents it started. A panel lists them
//! (`/tasks`, ↓ on an empty composer, or a click on the footer); a command opens to its output as it runs, a
//! subagent to its own transcript, followed live. ← → move between the session and its subagents.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use manifatura::exec::ProcessInfo;
use manifatura::{Outcome, SessionId};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::view::{clip, copper, dim};
use super::{App, Level, View};

/// How much of a command's output the panel reads, from its end.
const TAIL_BYTES: u64 = 64 * 1024;

/// A subagent this session started, through the task tool or `/name task`.
pub(crate) struct Child {
    pub(crate) id: SessionId,
    pub(crate) agent: String,
    /// What it was asked, in short.
    pub(crate) label: String,
    pub(crate) started: Instant,
    /// How long it ran and how it ended, once it has.
    pub(crate) finished: Option<(u64, Outcome)>,
}

impl Child {
    pub(crate) fn state(&self) -> String {
        match self.finished {
            None => format!("running {}", duration(self.started.elapsed().as_secs())),
            Some((secs, Outcome::Completed)) => format!("done in {}", duration(secs)),
            Some((_, Outcome::Aborted)) => "stopped".into(),
            Some(_) => "failed".into(),
        }
    }
}

/// One row of the panel.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Task {
    Command(ProcessInfo),
    /// An index into the session's subagents.
    Subagent(usize),
}

/// The panel, while it is open: the row selected, and the command opened, if one is.
#[derive(Debug, Default)]
pub(crate) struct Tasks {
    pub(crate) selected: usize,
    pub(crate) open: Option<u32>,
}

/// What a key or a click in the panel does.
#[derive(Debug, PartialEq)]
pub(crate) enum Action {
    Nothing,
    Close,
    Kill(u32),
    Focus(usize),
}

/// `secs` as the panel says it: 12s, 4m, 2h.
pub(crate) fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// The last `count` lines of the output file at `path`, without terminal escapes, each as the terminal last left
/// it (what follows its last carriage return).
pub(crate) fn tail(path: &Path, count: usize) -> Vec<String> {
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
        file.read_to_end(&mut bytes)
    });
    if read.is_err() {
        return Vec::new();
    }
    // Carriage returns first: the escapes' stripping takes them too.
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| strip_ansi_escapes::strip_str(line.rsplit('\r').next().unwrap_or_default()))
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.split_off(lines.len().saturating_sub(count))
}

fn command_state(p: &ProcessInfo) -> String {
    match p.exit {
        Some(code) => format!("exited {code}"),
        None => {
            let secs = SystemTime::now()
                .duration_since(p.started)
                .map_or(0, |d| d.as_secs());
            format!("running {}", duration(secs))
        }
    }
}

impl App {
    /// The commands, then the subagents, newest last.
    pub(super) fn task_list(&self) -> Vec<Task> {
        let commands = self
            .ready
            .as_ref()
            .map(|ready| ready.exec.processes(ready.session.id()))
            .unwrap_or_default();
        commands
            .into_iter()
            .map(Task::Command)
            .chain((0..self.children.len()).map(Task::Subagent))
            .collect()
    }

    /// The tasks still running, for the footer.
    pub(super) fn running_tasks(&self) -> usize {
        self.background.len()
            + self
                .children
                .iter()
                .filter(|c| c.finished.is_none())
                .count()
    }

    /// Opens the panel, or says there is nothing in it.
    pub(super) fn open_tasks(&mut self) {
        if self.task_list().is_empty() {
            self.view
                .notice(Level::Info, "no background commands or subagents");
        } else {
            self.tasks = Some(Tasks::default());
        }
    }

    pub(super) fn tasks_key(&mut self, key: KeyEvent) -> Action {
        let list = self.task_list();
        let Some(tasks) = &mut self.tasks else {
            return Action::Nothing;
        };
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if let Some(id) = tasks.open {
            return match key.code {
                _ if ctrl_c => Action::Close,
                KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                    tasks.open = None;
                    Action::Nothing
                }
                KeyCode::Char('k') | KeyCode::Char('x') => Action::Kill(id),
                _ => Action::Nothing,
            };
        }
        if list.is_empty() {
            return Action::Close;
        }
        tasks.selected = tasks.selected.min(list.len() - 1);
        match key.code {
            _ if ctrl_c => Action::Close,
            KeyCode::Esc => Action::Close,
            KeyCode::Up => {
                tasks.selected = tasks.selected.saturating_sub(1);
                Action::Nothing
            }
            KeyCode::Down => {
                tasks.selected = (tasks.selected + 1).min(list.len() - 1);
                Action::Nothing
            }
            KeyCode::Enter | KeyCode::Right => Self::choose(tasks, &list[tasks.selected]),
            KeyCode::Char('k') | KeyCode::Char('x') => match &list[tasks.selected] {
                Task::Command(p) if p.exit.is_none() => Action::Kill(p.id),
                _ => Action::Nothing,
            },
            _ => Action::Nothing,
        }
    }

    fn choose(tasks: &mut Tasks, task: &Task) -> Action {
        match task {
            Task::Command(p) => {
                tasks.open = Some(p.id);
                Action::Nothing
            }
            Task::Subagent(i) => Action::Focus(*i),
        }
    }

    /// A click on row `row` of the panel: it opens, as Enter would.
    pub(super) fn tasks_click(&mut self, row: usize) -> Action {
        let list = self.task_list();
        let Some(tasks) = &mut self.tasks else {
            return Action::Nothing;
        };
        match list.get(row) {
            Some(task) => {
                tasks.selected = row;
                Self::choose(tasks, task)
            }
            None => Action::Nothing,
        }
    }

    /// The panel's height: the list, or an opened command's output, which may take most of the screen.
    pub(super) fn tasks_height(&self, tasks: &Tasks, height: u16, limit: u16) -> u16 {
        if tasks.open.is_some() {
            (height * 2 / 3)
                .max(limit)
                .min(height.saturating_sub(2).max(1))
        } else {
            (self.task_list().len() + 2).min(usize::from(limit)) as u16
        }
    }

    pub(super) fn draw_tasks(&mut self, f: &mut Frame, area: Rect) {
        self.task_rows.clear();
        let Some(tasks) = &self.tasks else { return };
        let width = usize::from(area.width);
        let height = usize::from(area.height);
        let list = self.task_list();
        let mut rows: Vec<Line<'static>> = Vec::new();
        let open = tasks.open.and_then(|id| {
            list.iter().find_map(|task| match task {
                Task::Command(p) if p.id == id => Some(p.clone()),
                _ => None,
            })
        });
        match open {
            Some(p) => {
                let outside = if p.escalated {
                    " · outside the sandbox"
                } else {
                    ""
                };
                rows.push(Line::styled(
                    clip(
                        &format!(
                            "Command {} · {}{outside} · {}",
                            p.id,
                            command_state(&p),
                            p.command
                        ),
                        width,
                    ),
                    copper(),
                ));
                let room = height.saturating_sub(2);
                let output = tail(&p.output, room);
                if output.is_empty() {
                    rows.push(Line::styled("  no output yet", dim()));
                }
                for line in output {
                    rows.push(Line::raw(clip(&format!("  {line}"), width)));
                }
                let stop = if p.exit.is_none() { "k stop · " } else { "" };
                rows.truncate(height.saturating_sub(1));
                rows.push(Line::styled(format!("{stop}Esc back"), dim()));
            }
            None if tasks.open.is_some() => {
                rows.push(Line::styled("That command is gone.", dim()));
                rows.push(Line::styled("Esc back", dim()));
            }
            None => {
                rows.push(Line::styled("Background tasks", copper()));
                let count = height.saturating_sub(2).max(1);
                let selected = tasks.selected.min(list.len().saturating_sub(1));
                let first = (selected + 1).saturating_sub(count);
                for (i, task) in list.iter().enumerate().skip(first).take(count) {
                    let (mark, style) = if i == selected {
                        ("› ", copper())
                    } else {
                        ("  ", Style::new())
                    };
                    let (name, state, what, outside) = match task {
                        Task::Command(p) => (
                            format!("command {}", p.id),
                            command_state(p),
                            p.command.clone(),
                            p.escalated,
                        ),
                        Task::Subagent(c) => {
                            let child = &self.children[*c];
                            (
                                format!("{} subagent", child.agent),
                                child.state(),
                                child.label.clone(),
                                false,
                            )
                        }
                    };
                    let state_style = if state.starts_with("running") {
                        Style::new().fg(Color::Green)
                    } else {
                        dim()
                    };
                    let head = format!("{name:<16} ");
                    let state = format!("{state:<12} ");
                    let note = if outside { "  outside the sandbox" } else { "" };
                    let room = width.saturating_sub(2 + head.len() + state.len() + note.len());
                    rows.push(Line::from(vec![
                        Span::styled(mark, style),
                        Span::styled(head, style.add_modifier(Modifier::BOLD)),
                        Span::styled(state, state_style),
                        Span::styled(clip(what.lines().next().unwrap_or_default(), room), style),
                        Span::styled(note, dim()),
                    ]));
                    self.task_rows.push((area.y + (rows.len() - 1) as u16, i));
                }
                rows.push(Line::styled(
                    "↑↓ select · Enter open · k stop · Esc close",
                    dim(),
                ));
            }
        }
        rows.truncate(height);
        // The rows sit at the bottom of the pane.
        let top = area.bottom() - rows.len() as u16;
        let shift = top - area.y;
        for (y, _) in &mut self.task_rows {
            *y += shift;
        }
        for (i, line) in rows.into_iter().enumerate() {
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

    /// Shows session `target` (`None` for this one, else one of its subagents): the screen is wiped and its
    /// conversation drawn again, then followed live.
    pub(super) async fn focus(&mut self, target: Option<usize>) {
        if target == self.viewing {
            return;
        }
        let Some(ready) = &self.ready else { return };
        let session = match target {
            None => ready.session.clone(),
            Some(i) => ready
                .runtime
                .session(&ready.agent, self.children[i].id.clone()),
        };
        // Not `history`, which would wait for the turn that is running.
        let history = match session.history_so_far().await {
            Ok(history) => history,
            Err(error) => {
                self.view.notice(
                    Level::Error,
                    format!("could not read that conversation: {error}"),
                );
                return;
            }
        };
        match target {
            None => {
                let mut main = self.stashed.take().unwrap_or_else(View::new);
                main.clear();
                main.replay(&history);
                self.view = main;
            }
            Some(i) => {
                let child = &self.children[i];
                let mut view = View::new();
                view.clear();
                view.notice(
                    Level::Info,
                    format!("{} subagent · {}", child.agent, child.label),
                );
                view.replay(&history);
                if child.finished.is_none() {
                    view.follow(&history);
                }
                let main = std::mem::replace(&mut self.view, view);
                if self.stashed.is_none() {
                    self.stashed = Some(main);
                }
            }
        }
        self.viewing = target;
    }

    /// The session after (`step` 1) or before (-1) the one shown, in the order: this session, then its subagents.
    pub(super) async fn cycle(&mut self, step: isize) {
        if self.children.is_empty() {
            return;
        }
        let count = self.children.len() as isize + 1;
        let at = self.viewing.map_or(0, |i| i as isize + 1);
        let next = (at + step).rem_euclid(count);
        self.focus((next > 0).then(|| next as usize - 1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_short() {
        assert_eq!(duration(12), "12s");
        assert_eq!(duration(125), "2m");
        assert_eq!(duration(7300), "2h");
    }

    #[test]
    fn the_tail_is_the_last_lines_as_the_terminal_left_them() {
        let dir = std::env::temp_dir().join(format!("rho-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out");
        std::fs::write(
            &path,
            "one\ntwo\n\x1b[32mgreen\x1b[0m\nloading 10%\rloading 100%\n\n",
        )
        .unwrap();
        assert_eq!(tail(&path, 2), ["green", "loading 100%"]);
        assert_eq!(tail(&path, 10).len(), 4);
        assert!(tail(&dir.join("missing"), 3).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
