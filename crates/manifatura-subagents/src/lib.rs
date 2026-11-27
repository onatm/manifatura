//! Subagents for manifatura: a [`TASK_TOOL`] tool that hands a self-contained task to another
//! agent and returns its report.
//!
//! A subagent can do anything its agent's tools allow: explore and report, or carry out one part
//! of a larger change. Each call opens a child session in the same runtime and store, sends the
//! task as its first prompt and waits for the reply; several calls in one batch run in parallel
//! unless the tool is [`sequential`](Subagents::sequential). The child starts with none of the parent's conversation, so
//! whatever it reads and searches stays out of the parent's context: only its final reply comes
//! back, as the tool's result. Aborting the parent's turn aborts the child.
//!
//! A [`Subagent`] is either an agent of its own ([`Subagent::new`]) or the calling session's own
//! agent ([`Subagent::like_parent`]): the same instructions, tools and hooks, with a
//! [`brief`](Subagent::brief) before the task saying what its role is. A child like its parent
//! sends the same system prompt and tool declarations, so a provider that caches processed
//! prompts reuses the parent's for it.
//!
//! ```no_run
//! # fn demo(mut tools: Vec<manifatura_agent::Tool>) {
//! use manifatura_agent::Agent;
//! use manifatura_kit::context::SystemPrompt;
//! use manifatura_subagents::{Subagent, Subagents};
//!
//! let explore = Subagent::like_parent(
//!     "explore",
//!     "Answers one question about the code by searching and reading it.",
//! )
//! .brief("You are exploring the code for another agent. Report what you found, with path:line.");
//! // The task tool joins the others, so the prompt carries its guideline.
//! tools.push(Subagents::new([explore]).tool());
//! let instructions = SystemPrompt::new("You are a coding agent.").tools(&tools).build();
//! let agent = Agent::new(instructions).tools(tools);
//! # }
//! ```
//!
//! A child's tool calls go through its agent's hooks, so approvals ask as they do for the parent;
//! "always" answers are shared both ways ([`Subagents::share_state`]). Each call appears in the
//! parent's events as an `action.log` line, and [`Subagents::on_event`] sees every child event.
//! Subagents nest only as deep as [`Subagents::max_depth`], one level by default.

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use manifatura_agent::{
    Abort, Agent, Entry, Error, Event, Hooks, Message, Record, Role, Runtime, Session, SessionId,
    Tool, ToolChoice, ToolCx, ToolHandler, ToolResult, ToolSpec, hook, new_id,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The name of the tool that starts a subagent.
pub const TASK_TOOL: &str = "task";

/// The kind of the custom record that opens every child session, linking it to its parent.
pub const CUSTOM_KIND: &str = "subagent";

/// The session state key holding how deep a child session is: 1 for a child of a top-level
/// session.
pub const DEPTH_KEY: &str = "subagents.depth";

/// The session state key holding the subagent's name in its child session, so hooks can tell
/// which subagent they run in (e.g. to allow it only some tools).
pub const AGENT_KEY: &str = "subagents.agent";

/// The default limit for a subagent's report, in bytes.
pub const DEFAULT_MAX_RESULT_BYTES: usize = 16 * 1024;

/// An agent offered through the [`TASK_TOOL`] tool.
#[derive(Clone, Debug)]
pub struct Subagent {
    name: String,
    description: String,
    agent: Option<Agent>,
    brief: Option<String>,
    max_steps: Option<usize>,
    tools: Option<Vec<String>>,
}

impl Subagent {
    /// A subagent that runs `agent`, whose instructions say what it does. It is recorded under
    /// `name`.
    pub fn new(name: impl Into<String>, description: impl Into<String>, agent: Agent) -> Self {
        Self {
            agent: Some(agent),
            ..Self::like_parent(name, description)
        }
    }

    /// A subagent that runs the calling session's own agent: its instructions, tools, model
    /// settings and hooks. Give it a [`brief`](Self::brief) saying what its role is.
    pub fn like_parent(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            agent: None,
            brief: None,
            max_steps: None,
            tools: None,
        }
    }

    /// Text sent before the task in the child's first prompt: its role, how to work, what to
    /// report.
    pub fn brief(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        self.brief = (!text.trim().is_empty()).then(|| text.trim().to_string());
        self
    }

    /// The most model calls the child makes (see [`Agent::max_steps`]), the last of them for its
    /// report: from halfway on, each request tells the child which step it is on, and on the
    /// last one it may not call tools ([`ToolChoice::Off`]) and is asked to write its reply
    /// from what it has found.
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = Some(n.max(1));
        self
    }

    /// The only tools the child may call, by the names the model calls them. Calls to any other
    /// tool are refused before other hooks see them, so nobody is asked to approve them. The
    /// child is still offered all of its agent's tools, so a child like its parent keeps the
    /// parent's prompt; to keep the others out of its prompt too, give an agent of its own
    /// ([`Subagent::new`]) only these.
    pub fn tools<S: Into<String>>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.tools = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// A subagent like its parent from a [`Definition`]: its body becomes the brief, and its
    /// `max_steps` and `tools` apply.
    pub fn from_definition(def: Definition) -> Self {
        let mut sub = Self::like_parent(def.name, def.description).brief(def.body);
        if let Some(n) = def.max_steps {
            sub = sub.max_steps(n);
        }
        sub.tools = def.tools;
        sub
    }

    /// The name the parent calls it by.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it is for, as the parent's tool description lists it.
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// A subagent written as Markdown: YAML frontmatter with a `name` and a `description`, and
/// optionally `max_steps` and `tools` (a list, or names separated by commas), then the brief.
///
/// ```
/// let def = manifatura_subagents::Definition::parse(
///     "---\nname: explore\ndescription: Answers one question about the code.\ntools: read, grep, glob\n---\nReport with path:line.\n",
/// )
/// .unwrap();
/// assert_eq!(def.name, "explore");
/// assert_eq!(def.tools.as_deref(), Some(&["read".to_string(), "grep".into(), "glob".into()][..]));
/// assert_eq!(def.body, "Report with path:line.\n");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Definition {
    /// Lowercase letters, digits and single hyphens.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// The most model calls the child makes.
    pub max_steps: Option<usize>,
    /// The only tools the child may call; `None` allows all of them.
    pub tools: Option<Vec<String>>,
    /// The Markdown after the frontmatter.
    pub body: String,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    #[serde(default)]
    description: String,
    max_steps: Option<usize>,
    tools: Option<Names>,
}

/// Tool names as a YAML list or as one string, separated by commas or spaces.
#[derive(Deserialize)]
#[serde(untagged)]
enum Names {
    List(Vec<String>),
    Text(String),
}

impl Names {
    fn into_vec(self) -> Vec<String> {
        let names = match self {
            Names::List(names) => names,
            Names::Text(text) => text.split([',', ' ']).map(str::to_string).collect(),
        };
        names
            .into_iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect()
    }
}

impl Definition {
    /// Reads a definition; the error says what is wrong.
    pub fn parse(text: &str) -> Result<Definition, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let rest = text
            .strip_prefix("---")
            .filter(|r| r.starts_with('\n') || r.starts_with("\r\n"))
            .ok_or("no frontmatter")?;
        let Some(end) = rest.find("\n---") else {
            return Err("the frontmatter is not closed".into());
        };
        let body = rest[end + 4..].split_once('\n').map_or("", |(_, b)| b);
        let front: Frontmatter = serde_norway::from_str(&rest[..end])
            .map_err(|e| format!("could not read the frontmatter: {e}"))?;
        let name = front.name.ok_or("the frontmatter has no name")?;
        let valid = name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if name.is_empty() || !valid || name.starts_with('-') || name.ends_with('-') {
            return Err(format!(
                "the name {name:?} should be lowercase letters, digits and single hyphens"
            ));
        }
        if front.description.trim().is_empty() {
            return Err("the frontmatter has no description".into());
        }
        Ok(Definition {
            name,
            description: front.description.trim().to_string(),
            max_steps: front.max_steps,
            tools: front.tools.map(Names::into_vec),
            body: body.to_string(),
        })
    }
}

/// What a running child did, for [`Subagents::on_event`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Progress {
    /// The session that called the task tool.
    pub parent: SessionId,
    /// The id of the task tool call.
    pub call_id: String,
    /// The subagent's name.
    pub agent: String,
    /// The child session.
    pub child: SessionId,
    /// The child's event.
    pub event: Event,
}

type Observer = Arc<dyn Fn(&Progress) + Send + Sync>;

/// The subagents a session can start, and how; [`tool`](Self::tool) makes the task tool.
#[derive(Clone)]
pub struct Subagents {
    list: Vec<Subagent>,
    max_depth: usize,
    max_result_bytes: usize,
    share: Vec<String>,
    sequential: bool,
    observer: Option<Observer>,
}

impl std::fmt::Debug for Subagents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subagents")
            .field("list", &self.list)
            .field("max_depth", &self.max_depth)
            .field("max_result_bytes", &self.max_result_bytes)
            .field("share", &self.share)
            .field("sequential", &self.sequential)
            .finish_non_exhaustive()
    }
}

impl Subagents {
    /// These subagents, nesting one level deep and sharing approvals.
    pub fn new(list: impl IntoIterator<Item = Subagent>) -> Self {
        Self {
            list: list.into_iter().collect(),
            max_depth: 1,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            share: vec![manifatura_kit::approval_key("")],
            sequential: false,
            observer: None,
        }
    }

    /// How deep subagents nest: at depth `n` a task call is refused. 1, the default, lets only
    /// top-level sessions start subagents.
    pub fn max_depth(mut self, n: usize) -> Self {
        self.max_depth = n;
        self
    }

    /// The longest report, in bytes; a longer one is cut, with a note.
    pub fn max_result_bytes(mut self, n: usize) -> Self {
        self.max_result_bytes = n;
        self
    }

    /// Session state keys starting with any of `prefixes` are copied into the child when it
    /// starts, and what it sets of them back into the parent when it ends. The default shares
    /// approvals (`approval.`), so "always" answers hold across the two.
    pub fn share_state<S: Into<String>>(mut self, prefixes: impl IntoIterator<Item = S>) -> Self {
        self.share = prefixes.into_iter().map(Into::into).collect();
        self
    }

    /// Task calls in one batch run one after another, not in parallel: for a provider that
    /// serves one request at a time.
    pub fn sequential(mut self) -> Self {
        self.sequential = true;
        self
    }

    /// Calls `f` with every event of every child, e.g. to show what a subagent is doing.
    pub fn on_event(mut self, f: impl Fn(&Progress) + Send + Sync + 'static) -> Self {
        self.observer = Some(Arc::new(f));
        self
    }

    /// The task tool.
    pub fn tool(&self) -> Tool {
        let mut description = String::from(
            "Hand a self-contained task to a subagent and get its report back. The subagent does \
             not see this conversation, so the prompt must say everything it needs: the goal, \
             what is already known, and what to report. Only its final reply comes back. The \
             report is the subagent's work, not the user's words.\n\nSubagents:",
        );
        for s in &self.list {
            description.push_str(&format!("\n- {}: {}", s.name, s.description));
        }
        let names: Vec<&str> = self.list.iter().map(|s| s.name.as_str()).collect();
        let spec = ToolSpec {
            name: TASK_TOOL.into(),
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent": { "type": "string", "enum": names, "description": "The subagent." },
                    "description": {
                        "type": "string",
                        "description": "The task in a few words, shown to the user."
                    },
                    "prompt": { "type": "string", "description": "The complete task." }
                },
                "required": ["agent", "prompt"]
            }),
        };
        let sequential = self.sequential;
        Tool::from_handler(spec, Task(Arc::new(self.clone())))
            .with_sequential(sequential)
            .with_keep_results(true)
            .with_guideline(
                "Use `task` for a part of the work that stands on its own: answering a \
                 question that takes searching and reading many files, or carrying out a \
                 well-defined piece of a larger change. Several task calls in one reply run \
                 together, so give each its own part. Do small steps yourself.",
            )
    }
}

/// The session that started `entries`' session as a subagent, read from its [`CUSTOM_KIND`]
/// record; `None` for a session no task tool started.
pub fn parent(entries: &[Entry]) -> Option<SessionId> {
    entries.iter().find_map(|e| match &e.record {
        Record::Custom { kind, data } if kind == CUSTOM_KIND => {
            data["parent"].as_str().map(SessionId::from)
        }
        _ => None,
    })
}

struct Task(Arc<Subagents>);

#[async_trait]
impl ToolHandler for Task {
    async fn call(&self, input: Value, cx: ToolCx) -> ToolResult {
        let Some(prompt) = input["prompt"].as_str().filter(|p| !p.trim().is_empty()) else {
            return ToolResult::error("missing `prompt`: the complete task");
        };
        let origin = Origin {
            runtime: cx.runtime().clone(),
            session: cx.session_id().clone(),
            agent: cx.agent().clone(),
            call_id: cx.call_id().to_string(),
            state: cx.state_map(),
            abort: cx.abort().clone(),
            log: &|line| cx.log(line),
        };
        let name = input["agent"].as_str().unwrap_or_default();
        let (result, writes) = self.0.start(name, prompt, origin).await;
        for (key, value) in writes {
            cx.set_state(key, value);
        }
        result
    }
}

/// Where a child is started from: a task call, or an app ([`Subagents::run`]).
struct Origin<'a> {
    runtime: Runtime,
    session: SessionId,
    agent: Agent,
    call_id: String,
    state: BTreeMap<String, Value>,
    abort: Abort,
    log: &'a (dyn Fn(String) + Send + Sync),
}

impl Subagents {
    /// Runs the subagent `agent` on `prompt` as a child of `parent`, as a task call would, for an
    /// app that starts one itself, such as a `/explore` command. The result is what the task
    /// call would return; the app decides how the parent learns of it (e.g. in its next
    /// prompt). `log` gets the lines a task call logs, and `abort` stops the child.
    ///
    /// `parent` must be idle: its state is read and written around the run.
    pub async fn run(
        &self,
        parent: &Session,
        agent: &str,
        prompt: &str,
        abort: &Abort,
        log: impl Fn(String) + Send + Sync,
    ) -> ToolResult {
        if parent.is_running() {
            return ToolResult::error("the parent session is running a turn");
        }
        let state = match parent.state_map().await {
            Ok(state) => state,
            Err(e) => return ToolResult::error(format!("could not read the parent session: {e}")),
        };
        let origin = Origin {
            runtime: parent.runtime(),
            session: parent.id().clone(),
            agent: parent.agent().clone(),
            call_id: format!("run-{}", &new_id()[..8]),
            state,
            abort: abort.clone(),
            log: &log,
        };
        let (result, writes) = self.start(agent, prompt, origin).await;
        for (key, value) in writes {
            if let Err(e) = parent.set_state(key, value).await {
                return ToolResult::error(format!("could not update the parent session: {e}"));
            }
        }
        result
    }

    /// Runs one child to its end: the result, and the shared state to write back to the parent.
    async fn start(
        &self,
        name: &str,
        prompt: &str,
        origin: Origin<'_>,
    ) -> (ToolResult, Vec<(String, Value)>) {
        let fail = |text: String| (ToolResult::error(text), Vec::new());
        if prompt.trim().is_empty() {
            return fail("missing `prompt`: the complete task".into());
        }
        let Some(sub) = self.list.iter().find(|s| s.name == name) else {
            let names: Vec<&str> = self.list.iter().map(|s| s.name.as_str()).collect();
            return fail(format!(
                "unknown subagent `{name}`; the subagents are: {}",
                names.join(", ")
            ));
        };
        let depth = origin
            .state
            .get(DEPTH_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        if depth >= self.max_depth {
            return fail(
                "a subagent cannot start another subagent here; do this task yourself".into(),
            );
        }

        let mut agent = sub.agent.clone().unwrap_or(origin.agent);
        agent = agent.name(sub.name.clone());
        if let Some(n) = sub.max_steps {
            agent = agent.max_steps(n).hooks(budget(n));
        }
        if let Some(tools) = &sub.tools {
            agent = agent.hooks_first(only(&sub.name, tools));
        }
        let parent = origin.session;
        let child_id = SessionId::from(format!(
            "{}.{}-{}",
            parent.as_str(),
            sub.name,
            &new_id()[..8]
        ));
        let child = origin.runtime.session(&agent, child_id.clone());

        let setup = async {
            child
                .append_custom(
                    CUSTOM_KIND,
                    json!({ "parent": parent.as_str(), "call_id": origin.call_id, "agent": sub.name }),
                )
                .await?;
            child.set_state(DEPTH_KEY, depth + 1).await?;
            child.set_state(AGENT_KEY, &sub.name).await?;
            for (key, value) in &origin.state {
                if self.shares(key) {
                    child.set_state(key.clone(), value.clone()).await?;
                }
            }
            Ok::<_, Error>(())
        };
        if let Err(e) = setup.await {
            return fail(format!("could not start subagent `{name}`: {e}"));
        }

        let text = match &sub.brief {
            Some(brief) => format!("{brief}\n\n{prompt}"),
            None => prompt.to_string(),
        };
        let mut turn = match child.prompt(text).await {
            Ok(turn) => turn,
            Err(e) => return fail(format!("could not start subagent `{name}`: {e}")),
        };
        let abort = turn.abort_handle();
        let mut aborted = false;
        loop {
            let event = tokio::select! {
                e = turn.next_event() => e,
                _ = origin.abort.aborted(), if !aborted => {
                    aborted = true;
                    abort.abort();
                    continue;
                }
            };
            let Some(event) = event else { break };
            if let Event::ActionStarted { name, input, .. } = &event {
                (origin.log)(action_line(name, input));
            }
            if let Some(observe) = &self.observer {
                observe(&Progress {
                    parent: parent.clone(),
                    call_id: origin.call_id.clone(),
                    agent: sub.name.clone(),
                    child: child_id.clone(),
                    event,
                });
            }
        }
        let result = turn.wait().await;

        // What the child set of the shared state goes back to the parent.
        let mut writes = Vec::new();
        if let Ok(state) = child.state_map().await {
            for (key, value) in state {
                if self.shares(&key) && origin.state.get(&key) != Some(&value) {
                    writes.push((key, value));
                }
            }
        }

        let result = match result {
            Ok(reply) if reply.text.trim().is_empty() => {
                ToolResult::ok(format!("subagent `{name}` finished without a reply"))
            }
            Ok(reply) => ToolResult::ok(cut(&reply.text, self.max_result_bytes)),
            Err(Error::Aborted) => ToolResult::error(format!("subagent `{name}` was aborted")),
            Err(e) => {
                let last = child
                    .history()
                    .await
                    .ok()
                    .and_then(|h| {
                        h.iter()
                            .rev()
                            .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty())
                            .map(|m| m.text())
                    })
                    .unwrap_or_default();
                let mut out = format!("subagent `{name}` stopped before finishing: {e}");
                if !last.trim().is_empty() {
                    out.push_str("\n\nIts last reply:\n");
                    out.push_str(&cut(&last, self.max_result_bytes));
                }
                ToolResult::error(out)
            }
        };
        (result, writes)
    }

    fn shares(&self, key: &str) -> bool {
        self.share.iter().any(|p| key.starts_with(p.as_str()))
    }
}

/// What the child is told on its last step.
const LAST_STEP: &str = "This is your last step, and tools are off for it. Write your final reply now \
                         from what you have found, and say what you did not get to.";

/// Tells the child which step it is on from halfway through `n`, and makes the last one a reply
/// without tool calls. Only this request carries the note; the session keeps none.
fn budget(n: usize) -> Hooks {
    Hooks::new()
        .on(move |mut e: hook::Context, cx| async move {
            match cx.step() {
                Some(step) if step >= n => e.messages.push(Message::system_note(LAST_STEP)),
                Some(step) if step * 2 > n => e.messages.push(Message::system_note(format!(
                    "Step {step} of {n}. Keep the last step for your reply."
                ))),
                _ => {}
            }
            e
        })
        .on(move |mut e: hook::Request, cx| async move {
            if cx.step().is_some_and(|step| step >= n) {
                e.request.tool_choice = ToolChoice::Off;
            }
            e
        })
}

/// Refuses calls to tools other than `allowed`.
fn only(name: &str, allowed: &[String]) -> Hooks {
    let refusal = format!(
        "{name} may use only {}; do this with them",
        allowed.join(", ")
    );
    let allowed = allowed.to_vec();
    Hooks::new().on(move |e: hook::ToolCall, _cx| {
        let decision = match allowed.contains(&e.call.name) {
            true => hook::Decision::Run(e),
            false => hook::Decision::Deny(refusal.clone()),
        };
        async move { decision }
    })
}

/// A child's tool call as one line: the tool and its input, cut to 200 characters.
fn action_line(name: &str, input: &Value) -> String {
    let input = input.to_string();
    let mut line = format!("{name} {input}");
    if line.chars().count() > 200 {
        line = line.chars().take(199).collect::<String>() + "…";
    }
    line
}

/// `text` cut to `max` bytes at a character boundary, with a note.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let end = (0..=max)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    format!(
        "{}\n[… the report goes on for {} more bytes, left out …]",
        &text[..end],
        text.len() - end
    )
}
