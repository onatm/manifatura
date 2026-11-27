//! The chat on the terminal's main screen. The transcript goes into native scrollback; thinking and replies
//! stream into it above the pinned controls. Approvals come from the agent's `Approvals` and are answered by id;
//! background commands come from its `Exec`.

mod approval;
mod files;
mod layout;
pub(crate) mod logs;
mod rewind;
mod screen;
mod text;
mod view;

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self as terminal_events, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
    EnableMouseCapture, Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::{execute, terminal};
use manifatura::exec::ExecEvent;
use manifatura::kit::{Answer, ApprovalEvent};
use manifatura::skills::Skill;
use manifatura::{Abort, Effort, Event, Session, SessionId, SessionMeta};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui_textarea::{TextArea, WrapMode};
use tokio::sync::{broadcast, mpsc};

use self::approval::Ask;
use self::logs::{LogLine, Logs};
use self::rewind::{Outcome, Point, Restore, Rewind};
use self::screen::ChatScreen;
use self::view::{Activity, Item, Level, View, dim, explore_message, tokens, typed};
use crate::agent::{
    ChatOptions, Prepared, ago, effort_label, efforts, parse_effort, recent_sessions,
};

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const KEPT_LOG_LINES: usize = 200;
/// The commands, their argument and what they do, for completion and `/help`.
const COMMANDS: &[(&str, &str, &str)] = &[
    ("/new", "", "start a new session"),
    ("/resume", "[ID]", "pick a session or open it by ID"),
    ("/sessions", "", "list this directory's sessions"),
    (
        "/rewind",
        "",
        "go back to before a message: the code, the conversation or both",
    ),
    (
        "/undo",
        "",
        "undo the last turn: its changes and the conversation",
    ),
    (
        "/explore",
        "<question>",
        "hand a question about the code to the explore subagent",
    ),
    (
        "/compact",
        "[focus]",
        "summarise older messages to make room",
    ),
    ("/model", "[model]", "the plan's models, or switch to one"),
    (
        "/effort",
        "[level]",
        "how much the model reasons, from the levels it has",
    ),
    ("/ps", "", "the commands running in the background"),
    ("/kill", "<id>", "stop a background command"),
    ("/rules", "", "the approval rules and where they are kept"),
    ("/status", "", "model, session, sandbox and plan"),
    (
        "/tools",
        "",
        "show tool output from the current or last turn",
    ),
    ("/mcp", "", "the MCP servers and their tools"),
    ("/skills", "", "the skills; start one with /name [request]"),
    ("/help", "", "what the keys and commands do"),
    ("/exit", "", "quit"),
];
const MAX_COMPLETIONS: usize = 8;
/// The prompt that wakes the agent when a command it left in the background exits; the exec hook's note before
/// its first step says which command and how it ended.
const WAKE: &str = "A command you left running in the background has exited; the note says which. If the task \
                    depends on it, carry on; otherwise say in a line what happened.";

/// One way to complete what is typed.
#[derive(Clone, Debug)]
struct Completion {
    /// The input it becomes.
    text: String,
    label: String,
    about: String,
    /// Whether Enter on it runs it straight away (it needs no argument).
    runs: bool,
}

/// How many log lines the open panel shows.
const LOG_PANEL: usize = 10;
const LOG_BUTTON: &str = " logs ";

/// Everything the screen needs to start; the agent is set up once it is up.
pub(crate) struct Start {
    pub(crate) cwd: PathBuf,
    pub(crate) store: Arc<manifatura::store::JsonlStore>,
    pub(crate) options: ChatOptions,
    /// The MCP servers to start beside it.
    pub(crate) mcp: Vec<(String, manifatura::mcp::ServerConfig)>,
}

struct App {
    view: View,
    input: TextArea<'static>,
    files: files::FilePicker,
    history: Vec<String>,
    /// The recalled entry and the draft to restore when Down reaches the end.
    browsing: Option<(usize, TextArea<'static>)>,
    log: VecDeque<LogLine>,
    /// Whether the log is kept at all (`--logs`), and whether its panel is open.
    logs: bool,
    logs_open: bool,
    /// Where the `logs` button was drawn, in terminal cells, for clicks.
    log_button: Option<Rect>,
    /// The highlighted completion, while the input is a command being typed.
    picked: usize,
    completions_hidden: bool,
    /// This directory's sessions, for completing `/resume`.
    sessions: Vec<SessionMeta>,
    /// Selected row while the session picker is open.
    resume_pick: Option<usize>,
    /// The rewind picker, while it is open.
    rewind: Option<Rewind>,
    /// The approvals waiting, oldest first.
    asks: VecDeque<Ask>,
    /// Set by a first Ctrl-C with nothing to stop; a second one quits.
    armed: bool,
    tick: usize,
    quit: bool,
    abort: Option<Abort>,
    /// The agent, once it is set up.
    ready: Option<Prepared>,
    events: Option<broadcast::Receiver<Event>>,
    approvals: Option<broadcast::Receiver<ApprovalEvent>>,
    commands: Option<broadcast::Receiver<ExecEvent>>,
    /// A running `/explore`: its question, and what it reports.
    exploring: Option<(String, mpsc::UnboundedReceiver<Explored>)>,
    /// The session IDs of commands left running in the background.
    background: BTreeSet<u32>,
    /// The context the last model call started with, and the window.
    context: Option<(u64, Option<u32>)>,
    model: String,
    effort: Option<Effort>,
}

/// What a one-line input could complete to: the commands and skills it begins, or after `/resume `, the sessions.
fn completions(
    line: &str,
    sessions: &[SessionMeta],
    skills: &[Skill],
    levels: &[(Effort, String)],
) -> Vec<Completion> {
    if !line.starts_with('/') {
        return Vec::new();
    }
    if let Some(partial) = line.strip_prefix("/effort ") {
        let partial = partial.trim().to_lowercase();
        return levels
            .iter()
            .map(|(effort, about)| (effort_label(Some(*effort)), about))
            .filter(|(level, _)| level.starts_with(&partial) && *level != partial)
            .map(|(level, about)| Completion {
                text: format!("/effort {level}"),
                label: level,
                about: about.clone(),
                runs: true,
            })
            .collect();
    }
    if let Some(partial) = line.strip_prefix("/resume ") {
        let partial = partial.trim();
        return sessions
            .iter()
            .filter(|s| s.id.as_str().starts_with(partial) && s.id.as_str() != partial)
            .take(MAX_COMPLETIONS)
            .map(|s| Completion {
                text: format!("/resume {}", s.id.as_str()),
                label: s.id.as_str().into(),
                about: ago(s.updated),
                runs: true,
            })
            .collect();
    }
    if line.contains(' ') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|(name, ..)| name.starts_with(line))
        .map(|(name, argument, about)| Completion {
            text: match argument.is_empty() {
                true => (*name).to_owned(),
                false => format!("{name} "),
            },
            label: format!("{name} {argument}").trim_end().to_owned(),
            about: (*about).to_owned(),
            runs: argument.is_empty() || *name == "/resume",
        })
        .chain(
            skills
                .iter()
                .map(|skill| (format!("/{}", skill.name), skill))
                .filter(|(name, _)| name.starts_with(line) && !is_command(name))
                .map(|(name, skill)| Completion {
                    text: format!("{name} "),
                    label: format!("{name} [request]"),
                    about: skill.description.clone(),
                    runs: false,
                }),
        )
        .collect()
}

/// Whether `name` is one of the chat's own commands, which come before a skill of the same name.
fn is_command(name: &str) -> bool {
    matches!(name, "/quit") || COMMANDS.iter().any(|(command, ..)| *command == name)
}

/// Runs the screen until the person quits. Returns the session to mention on the way out.
pub(crate) async fn run(start: Start, logs: &mut Logs) -> (Option<SessionId>, Result<()>) {
    let mut term = match ChatScreen::open() {
        Ok(term) => term,
        Err(error) => {
            ratatui::restore();
            return (None, Err(error.into()));
        }
    };
    let enhanced = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
    let mut out = std::io::stdout();
    let _ = execute!(out, EnableBracketedPaste);
    // Clicks reach the screen only with the mouse captured, which also takes the scroll wheel from the terminal:
    // only when there is a log to open.
    let mouse = start.options.logs;
    if mouse {
        let _ = execute!(out, EnableMouseCapture);
    }
    if enhanced {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    // Terminal input is read on its own thread, polling, so that the terminal is free between polls to answer
    // the viewport's questions about the cursor.
    let (keys, mut input) = mpsc::unbounded_channel();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reading = stop.clone();
    std::thread::spawn(move || {
        while !reading.load(std::sync::atomic::Ordering::Relaxed) {
            if terminal_events::poll(Duration::from_millis(100)).unwrap_or(false) {
                let Ok(event) = terminal_events::read() else {
                    break;
                };
                if keys.send(event).is_err() {
                    break;
                }
            }
        }
    });

    let (session, result) = screen(&mut term, start, logs, &mut input).await;

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = term.finish();
    ratatui::restore();
    (session, result)
}

async fn screen(
    term: &mut ChatScreen,
    start: Start,
    logs: &mut Logs,
    keys: &mut mpsc::UnboundedReceiver<Input>,
) -> (Option<SessionId>, Result<()>) {
    let Start {
        cwd,
        store,
        options,
        mcp,
    } = start;
    let mut app = App::new(cwd.clone(), &options);
    let directory = std::env::var_os("HOME")
        .and_then(|home| {
            cwd.strip_prefix(home)
                .ok()
                .map(|relative| Path::new("~").join(relative))
        })
        .unwrap_or_else(|| cwd.clone());
    app.view.push(Item::Header {
        directory: directory.display().to_string(),
        model: "ChatGPT plan".into(),
    });
    let mut loading = Some(tokio::spawn(crate::agent::prepare(
        cwd, store, options, mcp,
    )));
    let mut ticker = tokio::time::interval(Duration::from_millis(100));

    let result: Result<()> = async {
        loop {
            term.draw(&mut app)?;
            if app.quit {
                return Ok(());
            }
            let connecting = loading.is_some();
            tokio::select! {
                input = keys.recv() => match input {
                    Some(input) => app.input(input).await,
                    None => return Ok(()),
                },
                event = next(&mut app.events) => match event {
                    Ok(event) => app.event(event).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => app.events = None,
                },
                event = next(&mut app.approvals) => match event {
                    Ok(event) => app.approval(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => app.resync_approvals(),
                    Err(broadcast::error::RecvError::Closed) => app.approvals = None,
                },
                event = next(&mut app.commands) => match event {
                    Ok(event) => app.command_event(event).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => app.commands = None,
                },
                Some(explored) = recv_explored(&mut app.exploring) => app.explored(explored).await,
                () = app.files.receive() => {}
                Some(line) = logs.0.recv() => app.log_line(line),
                loaded = async { loading.as_mut().expect("checked").await }, if connecting => {
                    loading = None;
                    app.ready(loaded??).await?;
                }
                _ = ticker.tick(), if app.view.is_busy() || connecting => app.tick += 1,
            }
        }
    }
    .await;
    let id = app.ready.as_ref().map(|ready| ready.session.id().clone());
    // Commands left running in the background end with the chat.
    if let (Some(ready), Some(id)) = (&app.ready, &id) {
        ready.exec.kill_all(id);
    }
    drop(app);
    (id, result)
}

/// Your messages on the session's current branch, newest first, each with the snapshot its turn took.
async fn rewind_points(session: &Session) -> Result<Vec<Point>> {
    let path = session.path().await?;
    let mut points: Vec<Point> = path
        .iter()
        .filter_map(|e| match &e.record {
            manifatura::Record::Message { message } if message.role == manifatura::Role::User => {
                Some(Point {
                    id: e.id.clone(),
                    parent: e.parent_id.clone(),
                    text: typed(&message.text()),
                    tree: manifatura::snapshots::before(&path, &e.id),
                })
            }
            _ => None,
        })
        .collect();
    points.reverse();
    Ok(points)
}

/// What a running `/explore` sends the screen: the lines it logs, then its report.
enum Explored {
    Line(String),
    Done(manifatura::ToolResult),
}

/// The turn and call ids `/explore` shows its work under, and its subagent.
const EXPLORE: &str = "explore";

/// The running `/explore`'s next message, or never when none runs.
async fn recv_explored(
    exploring: &mut Option<(String, mpsc::UnboundedReceiver<Explored>)>,
) -> Option<Explored> {
    match exploring {
        Some((_, rx)) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The next message of `rx`, or never when there is none.
async fn next<T: Clone>(
    rx: &mut Option<broadcast::Receiver<T>>,
) -> Result<T, broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn composer() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_wrap_mode(WrapMode::Glyph);
    input.set_cursor_line_style(Style::new());
    input.set_placeholder_style(dim());
    input
}

impl App {
    fn new(cwd: PathBuf, options: &ChatOptions) -> Self {
        Self {
            view: View::new(),
            input: composer(),
            files: files::FilePicker::new(cwd),
            history: Vec::new(),
            browsing: None,
            log: VecDeque::new(),
            logs: options.logs,
            logs_open: false,
            log_button: None,
            picked: 0,
            completions_hidden: false,
            sessions: Vec::new(),
            resume_pick: None,
            rewind: None,
            asks: VecDeque::new(),
            armed: false,
            tick: 0,
            quit: false,
            abort: None,
            ready: None,
            events: None,
            approvals: None,
            commands: None,
            exploring: None,
            background: BTreeSet::new(),
            context: None,
            model: options.model.clone().unwrap_or_default(),
            effort: options.effort,
        }
    }

    async fn ready(&mut self, prepared: Prepared) -> Result<()> {
        let session = &prepared.session;
        self.model = prepared.model.clone();
        self.effort = prepared.effort;
        if !prepared.history.is_empty() {
            self.view.notice(
                Level::Info,
                format!("Resumed session {}", session.id().as_str()),
            );
            self.view.replay(&prepared.history);
        }
        self.view.notice(
            Level::Info,
            format!(
                "{} · {} effort · {}",
                self.model,
                effort_label(self.effort),
                prepared.sandbox
            ),
        );
        for problem in &prepared.problems {
            self.view.notice(Level::Warn, problem.clone());
        }
        self.events = Some(session.subscribe());
        self.approvals = Some(prepared.approvals.subscribe());
        self.commands = Some(prepared.exec.subscribe());
        self.sessions = recent_sessions(prepared.store.as_ref()).await?;
        self.ready = Some(prepared);
        Ok(())
    }

    fn log_line(&mut self, line: LogLine) {
        self.log.push_back(line);
        if self.log.len() > KEPT_LOG_LINES {
            self.log.pop_front();
        }
    }

    /// A session event: to the transcript, and the context meter.
    async fn event(&mut self, event: Event) {
        self.view.apply(&event);
        match &event {
            Event::StepStarted {
                context_tokens,
                context_window,
                ..
            } => {
                self.context = Some((*context_tokens, *context_window));
            }
            // A command a call left running is followed until it exits.
            Event::ActionResult { name, output, .. } if name == manifatura::exec::EXEC_TOOL => {
                if let Some(id) = output
                    .lines()
                    .find_map(|l| l.strip_prefix("Process running with session ID "))
                    .and_then(|id| id.trim().parse().ok())
                {
                    self.background.insert(id);
                }
            }
            Event::ActionResult { name, output, .. } if name == manifatura::exec::STDIN_TOOL => {
                // A command whose exit the model read is done with.
                if output
                    .lines()
                    .any(|l| l.starts_with("Process exited with code "))
                {
                    self.forget_finished();
                }
            }
            Event::TurnCompleted { .. } => {
                self.abort = None;
                // A first turn is what puts a new session in the store.
                if let Some(ready) = &self.ready {
                    match recent_sessions(ready.store.as_ref()).await {
                        Ok(sessions) => self.sessions = sessions,
                        Err(error) => self
                            .view
                            .notice(Level::Warn, format!("Could not list sessions: {error}")),
                    }
                }
            }
            _ => {}
        }
    }

    /// Drops the background commands the agent no longer keeps.
    fn forget_finished(&mut self) {
        let Some(ready) = &self.ready else { return };
        let kept: BTreeSet<u32> = ready
            .exec
            .processes(ready.session.id())
            .iter()
            .map(|p| p.id)
            .collect();
        self.background.retain(|id| kept.contains(id));
    }

    fn approval(&mut self, event: ApprovalEvent) {
        match event {
            ApprovalEvent::Requested(request) => self.asks.push_back(Ask::new(request)),
            ApprovalEvent::Answered { id, .. } | ApprovalEvent::Cancelled { id, .. } => {
                self.asks.retain(|ask| ask.id() != id);
            }
            _ => {}
        }
    }

    /// After missing events, the requests still waiting are what to show.
    fn resync_approvals(&mut self) {
        if let Some(ready) = &self.ready {
            self.asks = ready
                .approvals
                .pending()
                .into_iter()
                .map(Ask::new)
                .collect();
        }
    }

    /// A command left in the background exited. While the agent works, it hears of it before its next step
    /// (the exec hook's note); while it is idle, it is woken to carry on.
    async fn command_event(&mut self, event: ExecEvent) {
        let Some(ready) = &self.ready else { return };
        let ExecEvent::Exited {
            session, id, code, ..
        } = event
        else {
            return;
        };
        if &session != ready.session.id() || !self.background.remove(&id) {
            return;
        }
        let level = if code == 0 { Level::Info } else { Level::Warn };
        self.view.notice(
            level,
            format!("background command {id} exited with code {code}"),
        );
        if self.wakes() {
            self.prompt(WAKE.into()).await;
        }
    }

    /// Whether an exit in the background starts a turn: only when nothing runs and nobody is being asked.
    fn wakes(&self) -> bool {
        self.ready.is_some()
            && !self.view.is_busy()
            && self.asks.is_empty()
            && self.abort.is_none()
            && self.exploring.is_none()
    }

    async fn input(&mut self, input: Input) {
        match input {
            Input::Key(key) if key.kind != KeyEventKind::Release => self.key(key).await,
            Input::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) => {
                let on_button = self
                    .log_button
                    .is_some_and(|button| button.contains((column, row).into()));
                if on_button {
                    self.logs_open = !self.logs_open;
                }
            }
            Input::Paste(text)
                if self.asks.is_empty() && self.resume_pick.is_none() && self.rewind.is_none() =>
            {
                self.picked = 0;
                self.completions_hidden = false;
                self.input
                    .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
            }
            _ => {}
        }
        self.files.sync(&self.input, self.completions_hidden);
    }

    async fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !(ctrl && key.code == KeyCode::Char('c')) {
            self.armed = false;
        }
        if let Some(ask) = self.asks.front_mut() {
            let answer = if ctrl && key.code == KeyCode::Char('c') {
                self.interrupt();
                Some(Answer::Deny(Some("the user interrupted".into())))
            } else {
                ask.key(key)
            };
            if let Some(answer) = answer {
                let ask = self.asks.pop_front().expect("checked");
                if let Some(ready) = &self.ready {
                    ready.approvals.answer(ask.id(), answer);
                }
            }
            return;
        }
        if let Some(rewind) = &mut self.rewind {
            match rewind.key(key) {
                Some(Outcome::Cancel) => self.rewind = None,
                Some(Outcome::Go(point, restore)) => {
                    self.rewind = None;
                    self.rewind_to(point, restore).await;
                }
                None => {}
            }
            return;
        }
        if let Some(selected) = self.resume_pick {
            let last = self.sessions.len().saturating_sub(1);
            match (key.code, ctrl) {
                (KeyCode::Up, _) => self.resume_pick = Some(selected.saturating_sub(1)),
                (KeyCode::Down, _) => self.resume_pick = Some((selected + 1).min(last)),
                (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => self.resume_pick = None,
                (KeyCode::Enter, _) => {
                    self.resume_pick = None;
                    if let Some(session) = self.sessions.get(selected) {
                        self.resume(session.id.clone()).await;
                    }
                }
                _ => {}
            }
            return;
        }
        if self.completion_key(key) {
            return;
        }
        if !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Tab) {
            self.picked = 0;
        }
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => {
                if self.view.is_busy() {
                    self.interrupt();
                } else if !self.text().is_empty() {
                    self.input = composer();
                    self.browsing = None;
                } else if self.armed {
                    self.quit = true;
                } else {
                    self.armed = true;
                }
            }
            (KeyCode::Char('d'), true) if self.text().is_empty() && !self.view.is_busy() => {
                self.quit = true
            }
            (KeyCode::Char('l'), true) if self.logs => self.logs_open = !self.logs_open,
            (KeyCode::Esc, _) if self.view.is_busy() => self.interrupt(),
            (KeyCode::Enter, _) => {
                let newline = key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                // A trailing backslash is a new line, for terminals that cannot tell Shift+Enter from Enter.
                let continued = self
                    .input
                    .lines()
                    .last()
                    .is_some_and(|line| line.ends_with('\\'));
                if newline || continued {
                    if continued {
                        self.input.delete_char();
                    }
                    self.input.insert_newline();
                } else {
                    let text = self.text();
                    if self.ready.is_none() && !text.trim_start().starts_with('/') {
                        return; // Keep the draft and cursor until it can actually be sent.
                    }
                    if !text.trim().is_empty() {
                        self.input = composer();
                        self.browsing = None;
                        self.submit(text.trim().to_owned()).await;
                    }
                }
            }
            (KeyCode::Up, false) if self.input.cursor().0 == 0 => self.browse(-1),
            (KeyCode::Down, false) if self.input.cursor().0 + 1 >= self.input.lines().len() => {
                self.browse(1)
            }
            _ => {
                self.input.input(key);
            }
        }
    }

    fn text(&self) -> String {
        self.input.lines().join("\n")
    }

    /// A completion consumes Enter when it inserts a file or a command argument.
    /// Fully specified slash commands continue to the normal submission handler.
    fn completion_key(&mut self, key: KeyEvent) -> bool {
        let files = self.files.active();
        let options = self.completions();
        if key.code == KeyCode::Esc && (files || !options.is_empty() && !self.view.is_busy()) {
            self.completions_hidden = true;
            return true;
        }
        if !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Esc) {
            self.completions_hidden = false;
        }
        let code = match (key.code, key.modifiers) {
            (KeyCode::Char('p'), KeyModifiers::CONTROL) if files => KeyCode::Up,
            (KeyCode::Char('n'), KeyModifiers::CONTROL) if files => KeyCode::Down,
            (_, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => return false,
            (code, _) => code,
        };
        let picked = self.picked.min(options.len().saturating_sub(1));
        let Some(option) = options.get(picked) else {
            return files
                && (matches!(code, KeyCode::Up | KeyCode::Down | KeyCode::Tab)
                    || code == KeyCode::Enter
                        && key.modifiers.is_empty()
                        && self.files.searching());
        };
        match code {
            KeyCode::Up => self.picked = picked.checked_sub(1).unwrap_or(options.len() - 1),
            KeyCode::Down => self.picked = (picked + 1) % options.len(),
            KeyCode::Tab => self.complete(option),
            KeyCode::Enter
                if key.modifiers.is_empty()
                    && (files || self.text().trim() != option.text.trim()) =>
            {
                self.complete(option);
                return !option.runs;
            }
            _ => return false,
        }
        true
    }

    /// Matches for the active file mention or slash command.
    fn completions(&self) -> Vec<Completion> {
        if self.files.active() && self.asks.is_empty() && !self.completions_hidden {
            return self
                .files
                .matches
                .iter()
                .map(|path| Completion {
                    text: path.clone(),
                    label: path.clone(),
                    about: String::new(),
                    runs: false,
                })
                .collect();
        }
        match (
            self.input.lines(),
            self.asks.is_empty() && !self.completions_hidden,
        ) {
            ([line], true) => {
                let skills = self
                    .ready
                    .as_ref()
                    .map_or(&[][..], |ready| ready.skills.skills());
                let levels = self.efforts();
                completions(line, &self.sessions, skills, &levels)
            }
            _ => Vec::new(),
        }
    }

    fn complete(&mut self, completion: &Completion) {
        if self.files.active() {
            files::insert_path(&mut self.input, &completion.text);
        } else {
            self.input = composer();
            self.input.insert_str(&completion.text);
        }
        self.picked = 0;
    }

    /// Up and Down through what was sent before.
    fn browse(&mut self, step: isize) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() as isize;
        let at = self.browsing.as_ref().map_or(last, |(i, _)| *i as isize) + step;
        if at >= last {
            if let Some((_, draft)) = self.browsing.take() {
                self.input = draft;
            }
            return;
        }
        let at = at.max(0) as usize;
        match &mut self.browsing {
            Some((i, _)) => *i = at,
            None => self.browsing = Some((at, std::mem::replace(&mut self.input, composer()))),
        }
        let mut input = composer();
        input.insert_str(&self.history[at]);
        self.input = input;
    }

    fn interrupt(&mut self) {
        if let Some(abort) = &self.abort {
            abort.abort();
        }
    }

    async fn submit(&mut self, text: String) {
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        if text.starts_with('/') {
            return self.command(&text).await;
        }
        self.send(text.clone(), text).await;
    }

    /// Sends `text` to the agent, shown as `shown`: as a new turn, or while it works, before its next step.
    async fn send(&mut self, shown: String, text: String) {
        let Some(ready) = &self.ready else {
            self.view.notice(Level::Info, "still connecting");
            return;
        };
        if self.exploring.is_some() {
            self.view.notice(
                Level::Info,
                "explore is still working; send this once it reports, or press Esc to stop it",
            );
            return;
        }
        if self.view.is_busy() {
            // Sent while the agent works: it reads it before its next step.
            match ready.session.steer(text) {
                true => self
                    .view
                    .push(Item::User(format!("{shown}  (sent while working)"))),
                false => self
                    .view
                    .notice(Level::Warn, "could not send that while the agent works"),
            }
            return;
        }
        self.view.push(Item::User(shown));
        self.prompt(text).await;
    }

    /// Starts a turn for `text`, which the transcript already shows.
    async fn prompt(&mut self, text: String) {
        let Some(ready) = &self.ready else { return };
        match ready.session.prompt(text).await {
            Ok(turn) => {
                self.abort = Some(turn.abort_handle());
                // The screen follows the session's events; the turn's own result adds nothing to them.
                tokio::spawn(async move {
                    let _ = turn.wait().await;
                });
            }
            Err(error) => self.view.notice(Level::Error, error.to_string()),
        }
    }

    async fn command(&mut self, line: &str) {
        let (command, argument) = line
            .split_once(' ')
            .map_or((line, ""), |(c, a)| (c, a.trim()));
        if matches!(command, "/exit" | "/quit") {
            self.quit = true;
            return;
        }
        if command == "/help" {
            for (name, argument, about) in COMMANDS {
                let name = format!("{name} {argument}");
                self.view
                    .notice(Level::Info, format!("{:<18}{about}", name.trim_end()));
            }
            self.view.notice(
                Level::Info,
                "Enter sends (while the agent works, before its next step) · Shift+Enter inserts a line · @ picks a file · \
                 Tab completes · Esc or Ctrl-C stops the agent · Ctrl-D or Ctrl-C twice quits · Up and Down recall what \
                 you sent · with --logs, click logs (or Ctrl-L) for the log",
            );
            return;
        }
        if command == "/tools" {
            self.view.show_tools();
            return;
        }
        if command == "/status" {
            let details = self.status();
            self.view.notice(Level::Info, details);
            return;
        }
        let Some(ready) = &mut self.ready else {
            self.view.notice(Level::Info, "still connecting");
            return;
        };
        if command == "/ps" {
            let processes = ready.exec.processes(ready.session.id());
            if processes.is_empty() {
                self.view.notice(Level::Info, "no background commands");
            }
            for p in processes {
                let state = match p.exit {
                    Some(code) => format!("exited {code}"),
                    None => "running".to_owned(),
                };
                let outside = if p.escalated {
                    " · outside the sandbox"
                } else {
                    ""
                };
                self.view.notice(
                    Level::Info,
                    format!("{:>3}  {state}{outside}  {}", p.id, p.command),
                );
            }
            return;
        }
        if command == "/kill" {
            match argument.parse::<u32>() {
                Ok(id) if ready.exec.kill(ready.session.id(), id) => {
                    self.background.remove(&id);
                    self.view
                        .notice(Level::Info, format!("stopped command {id}"));
                }
                Ok(id) => self
                    .view
                    .notice(Level::Warn, format!("no command {id}; /ps lists them")),
                Err(_) => self
                    .view
                    .notice(Level::Info, "/kill <id>: /ps lists the commands"),
            }
            return;
        }
        if command == "/rules" {
            let rules = ready.policy.list();
            if rules.is_empty() {
                self.view.notice(Level::Info, "no rules yet");
            }
            for (rule, decision) in rules {
                self.view.notice(
                    Level::Info,
                    format!("{:<6} {rule}", format!("{decision:?}").to_lowercase()),
                );
            }
            self.view.notice(
                Level::Info,
                format!(
                    "allow, ask and deny rules: {} (yours) and .rho/rules.toml (the project's: ask and deny only)",
                    ready.rules_path.display()
                ),
            );
            return;
        }
        if self.view.is_busy() {
            self.view.notice(
                Level::Info,
                format!("{command} waits until the agent is done"),
            );
            return;
        }
        let skill = command
            .strip_prefix('/')
            .filter(|name| !is_command(command) && ready.skills.get(name).is_some());
        if let Some(name) = skill {
            return self.start_skill(name, argument, line).await;
        }
        match command {
            "/new" => {
                let session = ready.runtime.session(&ready.agent, SessionId::new());
                self.view.replay(&[]);
                self.view.notice(
                    Level::Info,
                    format!("new session {}", session.id().as_str()),
                );
                self.switch(session);
            }
            "/resume" if !argument.is_empty() => self.resume(SessionId::from(argument)).await,
            "/resume" | "/sessions" => match recent_sessions(ready.store.as_ref()).await {
                Err(error) => self
                    .view
                    .notice(Level::Error, format!("Could not list sessions: {error}")),
                Ok(sessions) => {
                    self.sessions = sessions;
                    if self.sessions.is_empty() {
                        self.view
                            .notice(Level::Info, "No sessions in this directory yet");
                    } else if command == "/resume" {
                        self.resume_pick = Some(0);
                    } else {
                        let current = ready.session.id();
                        for row in &self.sessions {
                            let mark = if &row.id == current { "● " } else { "  " };
                            self.view.notice(
                                Level::Info,
                                format!("{mark}{}  {}", row.id.as_str(), ago(row.updated)),
                            );
                        }
                    }
                }
            },
            "/model" if argument.is_empty() => {
                for m in &ready.models {
                    let mark = if m.slug == self.model { "● " } else { "  " };
                    self.view.notice(
                        Level::Info,
                        format!("{mark}{:<24}{}", m.slug, m.display_name),
                    );
                }
            }
            "/model" => {
                if !ready.models.iter().any(|m| m.slug == argument) {
                    self.view.notice(
                        Level::Warn,
                        format!("the plan has no model {argument}; /model lists them"),
                    );
                } else {
                    match ready.session.set_model(argument).await {
                        Ok(()) => {
                            self.model = argument.to_owned();
                            self.view.notice(Level::Info, format!("model {argument}"));
                            // An effort the new model does not have becomes its default.
                            let levels = efforts(&ready.models, argument);
                            let kept = self
                                .effort
                                .is_none_or(|e| levels.iter().any(|(l, _)| *l == e));
                            let default = ready
                                .models
                                .iter()
                                .find(|m| m.slug == argument)
                                .and_then(|m| m.default_effort);
                            if !kept
                                && let Some(default) = default
                                && ready.session.set_reasoning(default).await.is_ok()
                            {
                                self.effort = Some(default);
                                self.view.notice(
                                    Level::Info,
                                    format!(
                                        "effort {}, {argument}'s default",
                                        effort_label(self.effort)
                                    ),
                                );
                            }
                        }
                        Err(error) => self
                            .view
                            .notice(Level::Error, format!("could not switch: {error}")),
                    }
                }
            }
            "/effort" if argument.is_empty() => {
                self.view.notice(
                    Level::Info,
                    format!("effort is {}", effort_label(self.effort)),
                );
                for (effort, about) in efforts(&ready.models, &self.model) {
                    let mark = if Some(effort) == self.effort {
                        "● "
                    } else {
                        "  "
                    };
                    self.view.notice(
                        Level::Info,
                        format!("{mark}{:<8}{about}", effort_label(Some(effort))),
                    );
                }
            }
            "/effort" => match parse_effort(argument) {
                Ok(effort)
                    if !efforts(&ready.models, &self.model)
                        .iter()
                        .any(|(e, _)| *e == effort) =>
                {
                    self.view.notice(
                        Level::Warn,
                        format!(
                            "{} has no {} effort; /effort lists its levels",
                            self.model,
                            effort_label(Some(effort))
                        ),
                    );
                }
                Ok(effort) => match ready.session.set_reasoning(effort).await {
                    Ok(()) => {
                        self.effort = Some(effort);
                        self.view
                            .notice(Level::Info, format!("effort {}", effort_label(self.effort)));
                    }
                    Err(error) => self
                        .view
                        .notice(Level::Error, format!("could not set the effort: {error}")),
                },
                Err(error) => self.view.notice(Level::Warn, error),
            },
            "/rewind" | "/undo" => match rewind_points(&ready.session).await {
                Err(error) => self
                    .view
                    .notice(Level::Error, format!("could not read the session: {error}")),
                Ok(points) if points.is_empty() => {
                    self.view.notice(Level::Info, "nothing to go back to yet")
                }
                Ok(points) if command == "/undo" => {
                    let point = points.into_iter().next().expect("checked");
                    let restore = if point.tree.is_some() {
                        Restore::Both
                    } else {
                        Restore::Conversation
                    };
                    self.rewind_to(point, restore).await;
                }
                Ok(points) => self.rewind = Some(Rewind::new(points)),
            },
            "/explore" if argument.is_empty() => self.view.notice(
                Level::Info,
                "/explore <question>: what to find out about the code",
            ),
            "/explore" => self.explore(argument, line),
            "/compact" => {
                let focus = (!argument.is_empty()).then(|| argument.to_owned());
                self.view.activity = Activity::Compacting;
                let result = ready.session.compact(ready.summarizer.clone(), focus).await;
                self.view.activity = Activity::Idle;
                if let Err(error) = result {
                    self.view
                        .notice(Level::Error, format!("could not compact: {error}"));
                }
            }
            "/mcp" => {
                if ready.servers.is_empty() {
                    let path = crate::paths::config_dir().join("mcp.toml");
                    self.view.notice(
                        Level::Info,
                        format!("no MCP servers; add them to {}", path.display()),
                    );
                }
                for (name, tools, eager) in &ready.servers {
                    let offered = if *eager {
                        "offered"
                    } else {
                        "found with find_tools"
                    };
                    self.view.notice(
                        Level::Info,
                        format!("{name} ({offered}): {}", tools.join(", ")),
                    );
                }
            }
            "/skills" => {
                if ready.skills.is_empty() {
                    self.view.notice(
                        Level::Info,
                        "no skills; add them to .agents/skills/<name>/SKILL.md",
                    );
                }
                for skill in ready.skills.skills() {
                    let from = skill
                        .path()
                        .map_or_else(|| "built in".to_owned(), |path| path.display().to_string());
                    self.view.notice(
                        Level::Info,
                        format!("/{}  {}  ({from})", skill.name, skill.description),
                    );
                }
            }
            _ => self.view.notice(
                Level::Warn,
                format!("unknown command {command}; /help lists them"),
            ),
        }
    }

    /// Starts the skill `name` from `/name request`: its instructions go to the agent ahead of the request, so it
    /// needs no step to load them. The transcript shows what was typed.
    async fn start_skill(&mut self, name: &str, request: &str, line: &str) {
        let Some(ready) = &self.ready else { return };
        let Some(instructions) = ready.skills.activation(name, &*ready.env).await else {
            return;
        };
        let request = match request {
            "" => format!("Use the {name} skill."),
            request => request.to_owned(),
        };
        self.send(line.to_owned(), format!("{instructions}\n\n{request}"))
            .await;
    }

    /// Starts the explore subagent on `question` from `/explore question`, without waiting for the model to
    /// choose it. The transcript shows it as a task; its report then goes to the agent with the question.
    fn explore(&mut self, question: &str, line: &str) {
        let Some(ready) = &self.ready else { return };
        self.view.push(Item::User(line.to_owned()));
        self.view.apply(&Event::TurnStarted {
            turn: EXPLORE.into(),
        });
        self.view.apply(&Event::ActionStarted {
            turn: EXPLORE.into(),
            call_id: EXPLORE.into(),
            name: manifatura::subagents::TASK_TOOL.into(),
            input: serde_json::json!({ "agent": EXPLORE, "description": question }),
        });
        let abort = Abort::new();
        self.abort = Some(abort.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        let (subagents, session, prompt) = (
            ready.subagents.clone(),
            ready.session.clone(),
            question.to_owned(),
        );
        tokio::spawn(async move {
            let lines = tx.clone();
            let log = move |line| {
                let _ = lines.send(Explored::Line(line));
            };
            let report = subagents.run(&session, EXPLORE, &prompt, &abort, log).await;
            let _ = tx.send(Explored::Done(report));
        });
        self.exploring = Some((question.to_owned(), rx));
    }

    async fn explored(&mut self, explored: Explored) {
        let report = match explored {
            Explored::Line(text) => {
                return self.view.apply(&Event::ActionLog {
                    turn: EXPLORE.into(),
                    call_id: EXPLORE.into(),
                    text,
                });
            }
            Explored::Done(report) => report,
        };
        let Some((question, _)) = self.exploring.take() else {
            return;
        };
        let aborted = self.abort.take().is_some_and(|abort| abort.is_aborted());
        self.view.apply(&Event::ActionResult {
            turn: EXPLORE.into(),
            call_id: EXPLORE.into(),
            name: manifatura::subagents::TASK_TOOL.into(),
            output: report.output.clone(),
            is_error: report.is_error,
        });
        let outcome = if aborted {
            manifatura::Outcome::Aborted
        } else {
            manifatura::Outcome::Completed
        };
        self.view.apply(&Event::TurnCompleted {
            turn: EXPLORE.into(),
            outcome,
            error: None,
            usage: Default::default(),
        });
        if !aborted {
            self.prompt(explore_message(&question, &report.output))
                .await;
        }
    }

    /// Goes back to before `point`: the files as its turn found them, the conversation as it was before it, or
    /// both. A conversation that goes back puts the message in the composer, to edit and send again.
    async fn rewind_to(&mut self, point: Point, restore: Restore) {
        let Some(ready) = &self.ready else { return };
        if restore != Restore::Conversation {
            let Some(tree) = &point.tree else {
                self.view.notice(
                    Level::Warn,
                    "there is no snapshot of the files for that message",
                );
                return;
            };
            match ready.snapshots.restore(&*ready.env, tree).await {
                Ok(r) => self.view.notice(
                    Level::Info,
                    format!(
                        "files restored: {} written back, {} removed",
                        r.restored.len(),
                        r.removed.len()
                    ),
                ),
                Err(error) => {
                    self.view.notice(
                        Level::Error,
                        format!("could not restore the files: {error}"),
                    );
                    return;
                }
            }
        }
        if restore != Restore::Code {
            match ready.session.branch(point.parent.clone()).await {
                Ok(()) => {
                    let first = point.text.lines().next().unwrap_or_default();
                    self.view.notice(
                        Level::Info,
                        format!("conversation rewound to before: {first}"),
                    );
                    self.context = None;
                    self.input = composer();
                    self.input.insert_str(&point.text);
                }
                Err(error) => self
                    .view
                    .notice(Level::Error, format!("could not rewind: {error}")),
            }
        }
    }

    async fn resume(&mut self, id: SessionId) {
        let Some(ready) = &self.ready else { return };
        // Opening an unknown ID creates a session header. Recheck the store even for a picker selection.
        match recent_sessions(ready.store.as_ref()).await {
            Ok(sessions) if sessions.iter().any(|s| s.id == id) => {}
            Ok(_) => {
                self.view.notice(
                    Level::Warn,
                    format!("No session {} here; /resume lists them", id.as_str()),
                );
                return;
            }
            Err(error) => {
                self.view
                    .notice(Level::Error, format!("Could not list sessions: {error}"));
                return;
            }
        }
        let session = ready.runtime.session(&ready.agent, id);
        match session.history().await {
            Ok(history) => {
                self.view.notice(
                    Level::Info,
                    format!(
                        "Resumed session {} ({} messages)",
                        session.id().as_str(),
                        history.len()
                    ),
                );
                self.view.replay(&history);
                self.switch(session);
            }
            Err(error) => self
                .view
                .notice(Level::Error, format!("Could not resume session: {error}")),
        }
    }

    fn switch(&mut self, session: Session) {
        self.events = Some(session.subscribe());
        self.abort = None;
        self.context = None;
        self.background.clear();
        if let Some(ready) = &mut self.ready {
            ready.session = session;
        }
    }

    /// The current model's effort levels.
    fn efforts(&self) -> Vec<(Effort, String)> {
        match &self.ready {
            Some(ready) => efforts(&ready.models, &self.model),
            None => efforts(&[], &self.model),
        }
    }

    fn effort_label(&self) -> String {
        effort_label(self.effort)
    }

    fn status(&self) -> String {
        let mut parts = vec![format!("{} · {} effort", self.model, self.effort_label())];
        match self.context {
            Some((used, Some(window))) => parts.push(format!(
                "context {}% {}/{}",
                used * 100 / u64::from(window).max(1),
                tokens(used),
                tokens(window.into())
            )),
            Some((used, None)) => parts.push(format!("context {}", tokens(used))),
            None => {}
        }
        if let Some(ready) = &self.ready {
            parts.push(format!("session {}", ready.session.id().as_str()));
            parts.push(ready.sandbox.clone());
            for path in &ready.files {
                parts.push(format!("instructions from {}", path.display()));
            }
            if !ready.skills.is_empty() {
                parts.push(format!("skills: {}", ready.skills.names().join(", ")));
            }
            if let Some(email) = &ready.account {
                parts.push(format!("signed in as {email}"));
            }
        }
        parts.push(format!(
            "Using ChatGPT plan · Manage usage: {}",
            manifatura::auth::openai::USAGE_URL
        ));
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn loading_app() -> App {
        let options = ChatOptions {
            resume_latest: false,
            resume: None,
            model: Some("gpt-test".into()),
            effort: Some(Effort::High),
            logs: false,
            context_window: None,
        };
        App::new(PathBuf::from("."), &options)
    }

    fn request(name: &str, input: serde_json::Value) -> manifatura::kit::Request {
        let call = manifatura::ToolCall {
            id: "r".into(),
            name: name.into(),
            input,
        };
        manifatura::kit::Request::new(SessionId::from("s"), call)
    }

    #[tokio::test]
    async fn loading_and_approval_preserve_the_composer() {
        use ratatui::backend::TestBackend;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let mut app = loading_app();
        app.input(Input::Paste("draft\r\nsecond line".into())).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.text(),
            "draft\nsecond line",
            "nothing is sent before the agent is ready"
        );
        assert!(app.history.is_empty());

        let input =
            serde_json::json!({"cmd": "npm install", "sandbox_permissions": "require_escalated"});
        app.approval(ApprovalEvent::Requested(
            request("exec_command", input).remember("exec_command(npm install)"),
        ));
        for (width, height) in [(80, 24), (60, 16)] {
            let backend = TestBackend::new(width, height);
            let mut term = Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Inline(height / 2),
                },
            )
            .unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            let rendered = term.backend().to_string();
            for label in [
                "outside the sandbox?",
                "npm install",
                "› 1. Allow once (y)",
                "Deny (n)",
            ] {
                assert!(rendered.contains(label), "{rendered}");
            }
            assert!(!rendered.contains("second line"));
        }
        // Answered elsewhere (another client): the question goes away.
        let id = app.asks[0].id().to_owned();
        app.approval(ApprovalEvent::Answered {
            id,
            session: SessionId::from("s"),
            answer: Answer::Allow,
        });
        assert!(app.asks.is_empty());
        assert_eq!(app.text(), "draft\nsecond line");
    }

    #[tokio::test]
    async fn prompt_and_footer_stay_at_the_bottom_as_input_grows() {
        use ratatui::backend::TestBackend;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let mut app = loading_app();
        for (width, height) in [(80, 24), (60, 16), (100, 32)] {
            for draft in ["/", "first line\nsecond line", "wrap ".repeat(22).as_str()] {
                app.input = composer();
                app.input.insert_str(draft);
                let rows = app.pane_height(width, height);
                let area = Rect::new(0, height - rows, width, rows);
                let mut term = Terminal::with_options(
                    TestBackend::new(width, height),
                    TerminalOptions {
                        viewport: Viewport::Fixed(area),
                    },
                )
                .unwrap();
                term.draw(|f| app.draw(f)).unwrap();
                let buf = term.backend().buffer();
                assert_eq!(buf[(0, height - 2)].symbol(), " ");
                assert_eq!(
                    buf[(0, height - 1)].symbol(),
                    "g",
                    "the model in the footer"
                );
                if draft == "/" {
                    assert_eq!(buf[(0, height - 3)].symbol(), "›");
                    assert!(term.backend().to_string().contains("/new"));
                }
            }
        }
        app.input = composer();
        app.input.insert_str("/st");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(app.completions().is_empty());
        assert_eq!(app.text(), "/st");
        app.history.push("earlier question".into());
        app.browse(-1);
        app.browse(1);
        assert_eq!(app.text(), "/st");
    }

    #[test]
    fn draft_and_sent_message_share_padding_and_text_alignment() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::{Color, Modifier};
        use ratatui::widgets::Paragraph;

        let mut app = loading_app();
        let draft = "short\n12345678901";
        app.input.insert_str(draft);
        let (width, height) = (13, 20);
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| {
            let sent = Item::User(draft.into()).lines(width.into());
            f.render_widget(Paragraph::new(sent), f.area());
            app.draw(f);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let surface = buf[(0, 1)].bg;
        assert_ne!(surface, Color::Reset);
        for top in [1, height - 6] {
            for y in top..top + 5 {
                assert!(
                    (0..width).all(|x| buf[(x, y)].bg == surface),
                    "missing shading on row {y}"
                );
                assert_eq!(buf[(width - 1, y)].symbol(), " ");
            }
            for y in [top, top + 4] {
                assert!((0..width).all(|x| buf[(x, y)].symbol() == " "));
            }
            assert_eq!(buf[(0, top + 1)].symbol(), "›");
            assert_eq!(buf[(2, top + 1)].symbol(), "s");
            assert!(!buf[(2, top + 1)].modifier.contains(Modifier::BOLD));
            assert_eq!(buf[(2, top + 2)].symbol(), "1");
            assert_eq!(buf[(2, top + 3)].symbol(), "1");
        }
        assert!(buf[(0, 2)].modifier.contains(Modifier::DIM));
        assert!(buf[(0, height - 5)].modifier.contains(Modifier::BOLD));
    }

    #[tokio::test]
    async fn at_opens_a_file_picker_and_escape_keeps_the_draft() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = loading_app();
        app.input(Input::Paste("Compare\nthe @".into())).await;
        let mut term = Terminal::new(TestBackend::new(70, 12)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(term.backend().to_string().contains("Type a file name"));
        app.input(Input::Key(KeyEvent::from(KeyCode::Esc))).await;
        term.draw(|f| app.draw(f)).unwrap();
        assert!(!term.backend().to_string().contains("Type a file name"));
        assert_eq!(app.text(), "Compare\nthe @");
    }

    #[tokio::test]
    async fn file_selection_replaces_only_the_active_mention() {
        use ratatui_textarea::CursorMove;
        use std::fs;

        let root = std::env::temp_dir().join(format!("rho-file-picker-{}", std::process::id()));
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        for name in [
            "docs/report one.md",
            "docs/report two.md",
            "report.tmp",
            ".git/report",
        ] {
            fs::write(root.join(name), "").unwrap();
        }
        fs::write(root.join(".gitignore"), "*.tmp\n").unwrap();
        let mut app = loading_app();
        app.files = files::FilePicker::new(root.clone());
        app.input(Input::Paste("@previous\nRésumé @rpt with me".into()))
            .await;
        app.input.move_cursor(CursorMove::Jump(1, 9));
        app.input(Input::Key(KeyEvent::from(KeyCode::Left))).await;
        assert!(app.files.searching());
        app.files.receive().await;
        let mut matches = app.files.matches.clone();
        matches.sort();
        assert_eq!(matches, ["docs/report one.md", "docs/report two.md"]);
        let chosen = app.files.matches[1].clone();
        app.input(Input::Key(KeyEvent::from(KeyCode::Down))).await;
        app.input(Input::Key(KeyEvent::from(KeyCode::Enter))).await;
        assert_eq!(
            app.text(),
            format!("@previous\nRésumé \"{chosen}\" with me")
        );
        assert!(!app.files.active());
        assert!(app.history.is_empty());
        app.input(Input::Paste("and ".into())).await;
        assert!(app.text().ends_with("and with me"));
        app.input(Input::Key(KeyEvent::from(KeyCode::End))).await;
        app.input(Input::Paste(" dev@example.org".into())).await;
        assert!(!app.files.active());
        app.input(Input::Paste(" @zzz".into())).await;
        app.input(Input::Paste("q".into())).await;
        app.files.receive().await;
        assert_eq!(app.files.hint(), Some("No matching files"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn picker_scrolls_to_the_selection_and_escape_restores_input() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = loading_app();
        app.sessions = (0..12)
            .map(|i| SessionMeta {
                id: SessionId::from(format!("session-{i:02}").as_str()),
                agent: None,
                updated: 0,
            })
            .collect();
        app.resume_pick = Some(0);
        for _ in 0..20 {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .await;
        }
        let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let rendered = term.backend().to_string();
        assert!(
            rendered.contains("› session-11") && !rendered.contains("session-00"),
            "{rendered}"
        );
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(app.resume_pick, Some(10));
        app.input(Input::Paste("ignored in picker".into())).await;
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert_eq!(app.resume_pick, None);
        assert!(app.text().is_empty());
    }

    #[test]
    fn commands_and_sessions_complete() {
        let sessions = ["01a2-aaaa", "01b3-bbbb"].map(|id| SessionMeta {
            id: SessionId::from(id),
            updated: 0,
            agent: None,
        });
        let names = |line: &str| -> Vec<String> {
            completions(line, &sessions, &[], &efforts(&[], "m"))
                .into_iter()
                .map(|c| c.text)
                .collect()
        };
        assert_eq!(names("/re"), ["/resume ", "/rewind"]);
        assert_eq!(names("/s"), ["/sessions", "/status", "/skills"]);
        assert_eq!(
            names("/"),
            COMMANDS
                .iter()
                .map(|(n, a, _)| if a.is_empty() {
                    n.to_string()
                } else {
                    format!("{n} ")
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(names("/resume 01b"), ["/resume 01b3-bbbb"]);
        assert_eq!(
            names("/resume 01a2-aaaa"),
            Vec::<String>::new(),
            "a complete id needs no completing"
        );
        assert!(names("hello").is_empty() && names("/compact focus").is_empty());
        assert!(
            completions("/new", &sessions, &[], &[])[0].runs
                && !completions("/compact", &sessions, &[], &[])[0].runs
        );
        assert_eq!(names("/effort o"), ["/effort off"]);
        assert_eq!(names("/effort m"), ["/effort medium"]);
    }

    #[test]
    fn skills_complete_after_the_commands_they_do_not_shadow() {
        let skill = |name: &str| {
            Skill::parse(&format!(
                "---\nname: {name}\ndescription: About {name}.\n---\nBody."
            ))
            .unwrap()
        };
        let skills = [skill("codebase-tour"), skill("status")];
        let found = completions("/", &[], &skills, &[]);
        let tour = found
            .iter()
            .find(|c| c.text == "/codebase-tour ")
            .expect("the skill is offered");
        assert!(!tour.runs && tour.about == "About codebase-tour.");
        assert_eq!(
            found
                .iter()
                .filter(|c| c.text.starts_with("/status"))
                .count(),
            1,
            "the command wins"
        );
        assert_eq!(
            completions("/co", &[], &skills, &[])
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>(),
            ["/compact ", "/codebase-tour "]
        );
    }
}
