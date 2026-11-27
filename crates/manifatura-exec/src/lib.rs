//! Commands that keep running, for manifatura: the model starts a command with
//! `exec_command`, which returns when the command exits or after a short wait. A command still
//! running then keeps running in the background under a session ID, and `write_stdin` polls
//! it, types into it or interrupts it. The tools take the shapes OpenAI's models are trained
//! on (those of OpenAI's Codex), and work with any model.
//!
//! ```no_run
//! use manifatura_agent::Agent;
//! use manifatura_exec::Exec;
//!
//! let exec = Exec::new();
//! let agent = Agent::new("You are a coding agent.")
//!     .tools(exec.tools())
//!     .hooks(exec.hooks()); // tells the model when a background command exits
//! ```
//!
//! Commands run through the session's [`Env`], so they run wherever it points; a command in a
//! terminal (`tty: true`) needs an env with terminals, such as `Local` with
//! `manifatura-tools`' `pty` feature. Output goes to a file in the env's scratch directory and
//! is read from there in bounded pieces. [`Exec::subscribe`], [`Exec::processes`] and
//! [`Exec::kill`] let an app show and stop what is running. Dropping the last handle to an
//! [`Exec`] kills every command it started.

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use manifatura_agent::{
    Env, ExecSpec, Hooks, ProcessInput, SessionId, Tool, ToolCall, ToolCx, ToolError, TtySize, hook,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, watch};

/// The tool that starts a command.
pub const EXEC_TOOL: &str = "exec_command";
/// The tool that writes to, polls or interrupts a running command.
pub const STDIN_TOOL: &str = "write_stdin";

const DEFAULT_YIELD_MS: u64 = 10_000;
const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
const DEFAULT_WRITE_YIELD_MS: u64 = 250;
/// A poll without input waits at least this long: polling in a tight loop wastes steps.
const MIN_POLL_MS: u64 = 5_000;
const MAX_POLL_MS: u64 = 300_000;
const DEFAULT_MAX_TOKENS: usize = 10_000;
/// Bytes per token, for output budgets.
const BYTES_PER_TOKEN: usize = 4;
/// The most recently used commands are never dropped to make room.
const PROTECTED: usize = 8;

/// What happens to commands, for an app that shows them.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecEvent {
    /// A command started.
    Started {
        /// The session it runs for.
        session: SessionId,
        /// Its session ID, as the model knows it.
        id: u32,
        /// The command line.
        command: String,
    },
    /// A command exited.
    Exited {
        /// The session it ran for.
        session: SessionId,
        /// Its session ID.
        id: u32,
        /// The exit code; a signal `n` is `128 + n`.
        code: i32,
    },
}

/// A command an [`Exec`] keeps, running or finished but not yet read to the end.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessInfo {
    /// Its session ID, as the model knows it.
    pub id: u32,
    /// The command line.
    pub command: String,
    /// Whether it runs in a terminal.
    pub tty: bool,
    /// Whether it runs outside the env's sandbox (see [`Exec::escalation`]).
    pub escalated: bool,
    /// When it started.
    pub started: SystemTime,
    /// Its exit code, once it has exited.
    pub exit: Option<i32>,
    /// The file its output goes to, in the env.
    pub output: PathBuf,
}

/// Commands started with `exec_command`, per session. Cheap to clone; give its
/// [`tools`](Self::tools) and [`hooks`](Self::hooks) to an agent.
#[derive(Clone)]
pub struct Exec(Arc<Inner>);

struct Inner {
    tables: Mutex<HashMap<SessionId, Table>>,
    events: broadcast::Sender<ExecEvent>,
    max_processes: usize,
    shell: String,
    env: Vec<(String, String)>,
    escalation: bool,
}

impl std::fmt::Debug for Exec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n: usize = self
            .0
            .tables
            .lock()
            .unwrap()
            .values()
            .map(|t| t.procs.len())
            .sum();
        f.debug_struct("Exec")
            .field("processes", &n)
            .field("max_processes", &self.0.max_processes)
            .field("shell", &self.0.shell)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Table {
    next: u32,
    procs: BTreeMap<u32, Arc<Proc>>,
}

/// One command. Dropping it (when it is pruned or the `Exec` goes) kills the command.
struct Proc {
    id: u32,
    session: SessionId,
    command: String,
    tty: bool,
    escalated: bool,
    started: SystemTime,
    env: Arc<dyn Env>,
    output: PathBuf,
    exit: watch::Receiver<Option<i32>>,
    control: mpsc::UnboundedSender<Control>,
    last_used: Mutex<Instant>,
    state: tokio::sync::Mutex<ProcState>,
}

struct ProcState {
    /// How much of the output the model has seen.
    offset: u64,
    input: Option<Box<dyn ProcessInput>>,
    /// The model has been told the command exited.
    exit_reported: bool,
}

impl Proc {
    fn exit_code(&self) -> Option<i32> {
        *self.exit.borrow()
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    /// Waits up to `wait` for the command to exit, or for `stop`.
    async fn wait_exit(
        &self,
        wait: Duration,
        stop: impl std::future::Future<Output = ()>,
    ) -> Option<i32> {
        let mut rx = self.exit.clone();
        tokio::select! {
            r = tokio::time::timeout(wait, rx.wait_for(Option::is_some)) => {
                r.ok().and_then(|r| r.ok().and_then(|c| *c))
            }
            _ = stop => self.exit_code(),
        }
    }
}

enum Control {
    Interrupt,
}

impl Default for Exec {
    fn default() -> Self {
        Self::new()
    }
}

impl Exec {
    /// An empty set of commands: at most 64 kept per session, run with `bash -c` (or `sh -c`
    /// where bash is missing).
    pub fn new() -> Self {
        let shell = if std::path::Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        Self(Arc::new(Inner {
            tables: Mutex::new(HashMap::new()),
            events: broadcast::channel(256).0,
            max_processes: 64,
            shell: shell.into(),
            env: vec![
                ("NO_COLOR".into(), "1".into()),
                ("PAGER".into(), "cat".into()),
                ("GIT_PAGER".into(), "cat".into()),
            ],
            escalation: false,
        }))
    }

    fn inner_mut(&mut self) -> &mut Inner {
        Arc::get_mut(&mut self.0).expect("configure an Exec before cloning it")
    }

    /// How many commands each session keeps (default 64). Past it, starting one drops the
    /// least recently used, finished ones first, and kills it if it still runs.
    pub fn max_processes(mut self, n: usize) -> Self {
        self.inner_mut().max_processes = n.max(PROTECTED + 1);
        self
    }

    /// The shell commands run with, as `<shell> -c <command>`.
    pub fn shell(mut self, path: impl Into<String>) -> Self {
        self.inner_mut().shell = path.into();
        self
    }

    /// A variable set for every command, beside `NO_COLOR=1`, `PAGER=cat` and
    /// `GIT_PAGER=cat` (and `TERM=dumb` outside a terminal).
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.inner_mut().env.push((name.into(), value.into()));
        self
    }

    /// Lets the model ask to run a command outside the env's sandbox: `exec_command` gets
    /// `sandbox_permissions` (`"use_default"` or `"require_escalated"`) and a
    /// `justification`, and such a call runs with [`ExecSpec::unsandboxed`]. Only for an env
    /// with a sandbox, and only with an approval hook that asks the user about the calls
    /// [`is_escalation`] picks out: the env runs them outside its sandbox if it allows that at
    /// all. A sandboxed command that fails as a sandbox would make it fail gets a hint saying
    /// how to ask.
    pub fn escalation(mut self, on: bool) -> Self {
        self.inner_mut().escalation = on;
        self
    }

    /// `exec_command` and `write_stdin`.
    pub fn tools(&self) -> Vec<Tool> {
        vec![self.exec_tool(), self.stdin_tool()]
    }

    /// A `Step` hook that tells the model, before its next call, about commands that exited
    /// in the background since it last looked.
    pub fn hooks(&self) -> Hooks {
        let exec = self.clone();
        Hooks::new().on(move |step: hook::Step, cx| {
            let exec = exec.clone();
            async move {
                for (id, command, code) in exec.take_unreported(cx.session_id()) {
                    cx.note(format!(
                        "Background command {id} (`{command}`) exited with code {code}. Call \
                         {STDIN_TOOL} with session_id {id} to read the rest of its output."
                    ));
                }
                step
            }
        })
    }

    /// Every command's start and exit, as they happen.
    pub fn subscribe(&self) -> broadcast::Receiver<ExecEvent> {
        self.0.events.subscribe()
    }

    /// The commands a session keeps, by session ID.
    pub fn processes(&self, session: &SessionId) -> Vec<ProcessInfo> {
        let tables = self.0.tables.lock().unwrap();
        let Some(t) = tables.get(session) else {
            return Vec::new();
        };
        t.procs
            .values()
            .map(|p| ProcessInfo {
                id: p.id,
                command: p.command.clone(),
                tty: p.tty,
                escalated: p.escalated,
                started: p.started,
                exit: p.exit_code(),
                output: p.output.clone(),
            })
            .collect()
    }

    /// Kills one of a session's commands and forgets it. `false` when there is none by that ID.
    pub fn kill(&self, session: &SessionId, id: u32) -> bool {
        let removed = self
            .0
            .tables
            .lock()
            .unwrap()
            .get_mut(session)
            .and_then(|t| t.procs.remove(&id));
        removed.is_some()
    }

    /// Kills all of a session's commands.
    pub fn kill_all(&self, session: &SessionId) {
        let removed = self.0.tables.lock().unwrap().remove(session);
        drop(removed);
    }

    fn get(&self, session: &SessionId, id: u32) -> Option<Arc<Proc>> {
        self.0
            .tables
            .lock()
            .unwrap()
            .get(session)
            .and_then(|t| t.procs.get(&id).cloned())
    }

    fn forget(&self, session: &SessionId, id: u32) {
        let removed = self
            .0
            .tables
            .lock()
            .unwrap()
            .get_mut(session)
            .and_then(|t| t.procs.remove(&id));
        drop(removed);
    }

    /// The next session ID, making room for it first.
    fn reserve(&self, session: &SessionId) -> u32 {
        let mut tables = self.0.tables.lock().unwrap();
        let t = tables.entry(session.clone()).or_default();
        while t.procs.len() >= self.0.max_processes {
            let mut by_use: Vec<(Instant, bool, u32)> = t
                .procs
                .values()
                .map(|p| (*p.last_used.lock().unwrap(), p.exit_code().is_some(), p.id))
                .collect();
            by_use.sort();
            let candidates = &by_use[..by_use.len().saturating_sub(PROTECTED)];
            let victim = candidates
                .iter()
                .find(|(_, exited, _)| *exited)
                .or(candidates.first())
                .map(|c| c.2);
            match victim {
                Some(id) => drop(t.procs.remove(&id)),
                None => break,
            }
        }
        t.next += 1;
        t.next
    }

    /// Commands that exited and whose exit the model has not seen, marked as seen.
    fn take_unreported(&self, session: &SessionId) -> Vec<(u32, String, i32)> {
        let tables = self.0.tables.lock().unwrap();
        let Some(t) = tables.get(session) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for p in t.procs.values() {
            if let Some(code) = p.exit_code()
                && let Ok(mut st) = p.state.try_lock()
                && !st.exit_reported
            {
                st.exit_reported = true;
                out.push((p.id, p.command.clone(), code));
            }
        }
        out
    }

    async fn start(&self, i: ExecInput, escalated: bool, cx: &ToolCx) -> Result<String, ToolError> {
        let began = Instant::now();
        let env = cx.env().clone();
        let session = cx.session_id().clone();
        let tty = i.tty.unwrap_or(false);
        let job = env
            .temp_dir()
            .join("exec")
            .join(&manifatura_agent::new_id()[..12]);
        let output = job.join("output");
        env.write(&job.join("command"), i.cmd.as_bytes()).await?;
        let mut vars = self.0.env.clone();
        if !tty {
            vars.push(("TERM".into(), "dumb".into()));
        }
        let mut process = env
            .exec(ExecSpec {
                argv: vec![self.0.shell.clone(), "-c".into(), i.cmd.clone()],
                cwd: i.workdir.as_ref().map(PathBuf::from),
                env: vars,
                stdin: None,
                stdout: output.clone(),
                stderr: output.clone(),
                tty: tty.then(TtySize::default),
                unsandboxed: escalated,
            })
            .await?;
        let input = process.take_input();

        let id = self.reserve(&session);
        let (exit_tx, exit_rx) = watch::channel(None);
        let (control, mut control_rx) = mpsc::unbounded_channel();
        let events = self.0.events.clone();
        let (exit_env, exit_file, exit_session) = (env.clone(), job.join("exit"), session.clone());
        tokio::spawn(async move {
            let code = loop {
                tokio::select! {
                    r = process.wait() => break r.unwrap_or(-1),
                    c = control_rx.recv() => match c {
                        Some(Control::Interrupt) => process.interrupt(),
                        // Every handle is gone: the command is no longer wanted.
                        None => {
                            process.kill();
                            break process.wait().await.unwrap_or(-1);
                        }
                    },
                }
            };
            let _ = exit_env
                .write(&exit_file, code.to_string().as_bytes())
                .await;
            let _ = exit_tx.send(Some(code));
            let _ = events.send(ExecEvent::Exited {
                session: exit_session,
                id,
                code,
            });
        });
        let proc = Arc::new(Proc {
            id,
            session: session.clone(),
            command: i.cmd.clone(),
            tty,
            escalated,
            started: SystemTime::now(),
            env,
            output,
            exit: exit_rx,
            control,
            last_used: Mutex::new(Instant::now()),
            state: tokio::sync::Mutex::new(ProcState {
                offset: 0,
                input,
                exit_reported: false,
            }),
        });
        self.0
            .tables
            .lock()
            .unwrap()
            .entry(session.clone())
            .or_default()
            .procs
            .insert(id, proc.clone());
        let _ = self.0.events.send(ExecEvent::Started {
            session,
            id,
            command: i.cmd,
        });

        let wait = clamp(
            i.yield_time_ms.unwrap_or(DEFAULT_YIELD_MS),
            MIN_YIELD_MS,
            MAX_YIELD_MS,
        );
        let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = aborted.clone();
        let abort = cx.abort().clone();
        let exit = proc
            .wait_exit(wait, async move {
                abort.aborted().await;
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .await;
        if aborted.load(std::sync::atomic::Ordering::SeqCst) && exit.is_none() {
            drop(proc);
            self.forget(cx.session_id(), id);
            return Err(ToolError::msg("aborted; the command was killed"));
        }
        let mut report = self.report(&proc, exit, began, i.max_output_tokens).await?;
        if self.0.escalation
            && !escalated
            && exit.is_some_and(|c| c != 0)
            && looks_sandboxed(&report)
        {
            report.push_str(SANDBOX_HINT);
        }
        Ok(report)
    }

    async fn write(&self, i: StdinInput, cx: &ToolCx) -> Result<String, ToolError> {
        let began = Instant::now();
        let Some(proc) = self.get(cx.session_id(), i.session_id) else {
            return Err(ToolError::msg(format!(
                "unknown session_id {}: {EXEC_TOOL} gives one for a command it leaves running, \
                 and a command's session ends once its exit has been read",
                i.session_id
            )));
        };
        proc.touch();
        let chars = i.chars.unwrap_or_default();
        if !chars.is_empty() {
            if proc.tty {
                let mut st = proc.state.lock().await;
                match st.input.as_mut() {
                    Some(input) => input.write(chars.as_bytes()).await?,
                    None => return Err(ToolError::msg("this session takes no input")),
                }
            } else if chars == "\u{3}" {
                let _ = proc.control.send(Control::Interrupt);
            } else {
                return Err(ToolError::msg(format!(
                    "session {} has no terminal, so its input is closed. Start the command \
                     with tty: true to type into it; send \"\\u0003\" (Ctrl-C) to interrupt it.",
                    proc.id
                )));
            }
        }
        let wait = if chars.is_empty() {
            clamp(
                i.yield_time_ms.unwrap_or(MIN_POLL_MS),
                MIN_POLL_MS,
                MAX_POLL_MS,
            )
        } else {
            clamp(
                i.yield_time_ms.unwrap_or(DEFAULT_WRITE_YIELD_MS),
                MIN_YIELD_MS,
                MAX_YIELD_MS,
            )
        };
        let abort = cx.abort().clone();
        let exit = proc
            .wait_exit(wait, async move { abort.aborted().await })
            .await;
        self.report(&proc, exit, began, i.max_output_tokens).await
    }

    /// The new output and the command's state, in the format the tools return. A command whose
    /// exit is reported here is forgotten.
    async fn report(
        &self,
        proc: &Arc<Proc>,
        exit: Option<i32>,
        began: Instant,
        max_tokens: Option<usize>,
    ) -> Result<String, ToolError> {
        let budget = max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).max(100) * BYTES_PER_TOKEN;
        let (text, original) = {
            let mut st = proc.state.lock().await;
            if exit.is_some() {
                st.exit_reported = true;
            }
            read_new(&*proc.env, &proc.output, &mut st.offset, budget, proc.tty).await?
        };
        let mut out = format!(
            "Chunk ID: {}\nWall time: {:.4} seconds\n",
            &manifatura_agent::new_id()[..6],
            began.elapsed().as_secs_f64()
        );
        match exit {
            Some(code) => {
                out.push_str(&format!("Process exited with code {code}\n"));
                self.forget(&proc.session, proc.id);
            }
            None => out.push_str(&format!("Process running with session ID {}\n", proc.id)),
        }
        if let Some(tokens) = original {
            out.push_str(&format!("Original token count: {tokens}\n"));
        }
        out.push_str("Output:\n");
        out.push_str(&text);
        Ok(out)
    }

    fn exec_tool(&self) -> Tool {
        let exec = self.clone();
        let mut guideline = String::from(
            "Run commands with `exec_command`. One still running after its yield time keeps \
             running with a session ID: read its output, type into it or stop it with \
             `write_stdin` instead of starting it again. Start servers and watchers this way, \
             and do not end your turn while a command you still need is running.",
        );
        if self.0.escalation {
            guideline.push_str(
                " Commands run in a sandbox: they write only in the workspace and have no \
                 network beyond localhost. When one needs more (installing dependencies, \
                 network access, writing elsewhere), run it with sandbox_permissions \
                 \"require_escalated\" and a one-sentence justification; the user is asked \
                 to approve it.",
            );
        }
        let tool = Tool::new(
            EXEC_TOOL,
            "Runs a shell command in the workspace and returns its output (stdout and stderr \
             together), or a session ID if it is still running after yield_time_ms. A \
             command left running keeps running in the background; use write_stdin with its \
             session ID to read more output, type into it or interrupt it. Use tty: true for \
             programs that need a terminal (prompts, REPLs).",
        )
        .guideline(guideline);
        if self.0.escalation {
            tool.run(move |i: EscalatedInput, cx: ToolCx| {
                let exec = exec.clone();
                async move {
                    let escalated =
                        i.sandbox_permissions == Some(SandboxPermissions::RequireEscalated);
                    if escalated
                        && i.justification
                            .as_deref()
                            .is_none_or(|j| j.trim().is_empty())
                    {
                        return Err(ToolError::msg(
                            "give a justification with require_escalated: one sentence the \
                             user reads when asked to approve the command",
                        ));
                    }
                    exec.start(i.base, escalated, &cx).await
                }
            })
        } else {
            tool.run(move |i: ExecInput, cx: ToolCx| {
                let exec = exec.clone();
                async move { exec.start(i, false, &cx).await }
            })
        }
    }

    fn stdin_tool(&self) -> Tool {
        let exec = self.clone();
        Tool::new(
            STDIN_TOOL,
            "Writes characters to a command exec_command left running and returns its new \
             output. Empty chars waits for more output (at least 5 seconds); \"\\u0003\" \
             (Ctrl-C) interrupts the command. Only a command started with tty: true takes \
             other input; end a line with \"\\n\".",
        )
        .run(move |i: StdinInput, cx: ToolCx| {
            let exec = exec.clone();
            async move { exec.write(i, &cx).await }
        })
    }
}

fn clamp(ms: u64, min: u64, max: u64) -> Duration {
    Duration::from_millis(ms.clamp(min, max))
}

/// The output written since `offset`, at most `budget` bytes of it: the beginning and the end
/// when there is more, with the original size in tokens. `offset` moves to the end.
async fn read_new(
    env: &dyn Env,
    path: &std::path::Path,
    offset: &mut u64,
    budget: usize,
    tty: bool,
) -> Result<(String, Option<usize>), ToolError> {
    let Some(stat) = env.stat(path).await? else {
        return Ok((String::new(), None));
    };
    let len = stat.len;
    let new = len.saturating_sub(*offset);
    let budget = budget as u64;
    let (bytes, original) = if new <= budget {
        (env.read_range(path, *offset, new).await?.0, None)
    } else {
        let half = budget / 2;
        let (mut head, _) = env.read_range(path, *offset, half).await?;
        let (tail, _) = env.read_range(path, len - half, half).await?;
        let cut = (new - 2 * half) as usize / BYTES_PER_TOKEN;
        head.extend_from_slice(format!("\n…{cut} tokens truncated…\n").as_bytes());
        head.extend_from_slice(&tail);
        (head, Some(new as usize / BYTES_PER_TOKEN))
    };
    *offset = len;
    let text = if tty {
        String::from_utf8_lossy(&strip_ansi_escapes::strip(&bytes)).replace("\r\n", "\n")
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok((text, original))
}

#[derive(Deserialize, JsonSchema)]
struct ExecInput {
    /// The shell command to run.
    cmd: String,
    /// The directory to run it in; defaults to the workspace.
    workdir: Option<String>,
    /// Run it in a terminal, for programs that need one or that you will type into
    /// (default false).
    tty: Option<bool>,
    /// How long to wait for the command to finish before returning, in milliseconds
    /// (default 10000, 250 to 30000). A command still running then keeps running.
    yield_time_ms: Option<u64>,
    /// The most output to return, in tokens (default 10000); longer output keeps its
    /// beginning and end.
    max_output_tokens: Option<usize>,
}

/// Whether `call` asks to run a command outside the env's sandbox (`exec_command` with
/// `sandbox_permissions: "require_escalated"`, offered with [`Exec::escalation`]): the calls an
/// approval hook asks the user about.
pub fn is_escalation(call: &ToolCall) -> bool {
    call.name == EXEC_TOOL && call.input["sandbox_permissions"] == "require_escalated"
}

/// What a sandbox's refusals print.
const DENIALS: &[&str] = &[
    "Operation not permitted",
    "Read-only file system",
    "Permission denied",
    "Could not resolve host",
    "nodename nor servname provided",
    "Network is unreachable",
    "Temporary failure in name resolution",
];

const SANDBOX_HINT: &str = "\n[The sandbox may have blocked this command: it writes only in \
the workspace and has no network beyond localhost. If it needs more, run it again with \
sandbox_permissions \"require_escalated\" and a justification.]\n";

fn looks_sandboxed(report: &str) -> bool {
    let output = report.split_once("Output:\n").map_or(report, |(_, o)| o);
    DENIALS.iter().any(|d| output.contains(d))
}

/// Where a command may run.
#[derive(Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SandboxPermissions {
    /// In the sandbox, as usual.
    UseDefault,
    /// Outside the sandbox, once the user approves.
    RequireEscalated,
}

#[derive(Deserialize, JsonSchema)]
struct EscalatedInput {
    #[serde(flatten)]
    base: ExecInput,
    /// "use_default" (the default) runs the command in the sandbox; "require_escalated" asks
    /// the user to let it run outside, for writing outside the workspace or using the network.
    sandbox_permissions: Option<SandboxPermissions>,
    /// With require_escalated: one sentence telling the user why the command needs to run
    /// outside the sandbox.
    justification: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct StdinInput {
    /// The session ID exec_command returned.
    session_id: u32,
    /// Characters to write; empty to wait for more output.
    chars: Option<String>,
    /// How long to wait for output, in milliseconds (default 250 after writing, at least 5000
    /// when only waiting).
    yield_time_ms: Option<u64>,
    /// The most output to return, in tokens (default 10000).
    max_output_tokens: Option<usize>,
}
