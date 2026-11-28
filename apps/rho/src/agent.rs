//! The agent: the ChatGPT sign-in, the sandboxed env, the tools, the prompt, the approval policy and the
//! session.
//!
//! Every command runs in the sandbox without asking; the model asks to leave it (`sandbox_permissions:
//! require_escalated`), and that, typing into a command outside it, and MCP tools ask through the agent's
//! `Approvals`, unless a rule allows them. Reads, patches inside the workspace, plans and skills never ask.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use manifatura::auth::openai::{Model, SignIn};
use manifatura::exec::Exec;
use manifatura::kit::context::{ContextFiles, SystemPrompt};
use manifatura::kit::{self, Approvals, Summarizer};
use manifatura::policy::{Decision, Policy};
use manifatura::providers::responses::Responses;
use manifatura::sandbox::Sandbox;
use manifatura::skills::{SkillSet, Skills, SystemPromptExt};
use manifatura::snapshots::Snapshots;
use manifatura::store::JsonlStore;
use manifatura::subagents::{Definition, Subagent, Subagents};
use manifatura::tools::Local;
use manifatura::{
    Abort, Agent, BoxStream, Effort, Env, Message, ModelInfo, ModelRequest, Provider, Runtime,
    Session, SessionId, SessionMeta, Store, StreamEvent,
};

use crate::paths;

/// Who the agent is and how it works. The tools' guidelines, the `AGENTS.md` files, the directory and the date
/// follow it.
const BASE: &str = include_str!("system.md");

/// The name rho signs in as, and keeps its sessions under.
pub(crate) const AGENT_NAME: &str = "rho";

/// The built-in tools rho registers, beside `exec_command`, `write_stdin` and `update_plan`. Edits are patches.
const TOOLS: &[&str] = &["read", "grep", "glob", "apply_patch"];

/// Tools that never ask: reading, patching inside the workspace (the sandbox refuses the rest), commands in the
/// sandbox, plans and skills.
const ALLOWED: &[&str] = &[
    "read",
    "grep",
    "glob",
    "apply_patch",
    "exec_command",
    "write_stdin",
    "update_plan",
    "skill",
    "find_tools",
    "task",
];

/// The subagents built into rho, as Markdown. Each runs rho's own agent (the same prompt and tools, so the
/// provider's prompt cache serves it) with the file's body as its brief, calling only the tools its `tools` names
/// and keeping the last of its `max_steps` for its report.
const BUILTIN_AGENTS: &[&str] = &[include_str!("agents/explore.md")];

/// What the screen and the command line ask for.
#[derive(Clone, Debug)]
pub(crate) struct ChatOptions {
    /// Open the most recent session for this directory.
    pub(crate) resume_latest: bool,
    /// Open this session.
    pub(crate) resume: Option<String>,
    /// The model to start with; the plan's first otherwise.
    pub(crate) model: Option<String>,
    /// How much the model reasons; the model's default otherwise.
    pub(crate) effort: Option<Effort>,
    /// Keep the log, behind a button on the screen.
    pub(crate) logs: bool,
    /// The context window compaction measures against, instead of the model's own.
    pub(crate) context_window: Option<u32>,
}

/// The agent and its session, set up once rho is connected.
pub(crate) struct Prepared {
    pub(crate) runtime: Runtime,
    pub(crate) agent: Agent,
    pub(crate) session: Session,
    /// The session's conversation so far, when it was resumed.
    pub(crate) history: Vec<Message>,
    pub(crate) store: Arc<JsonlStore>,
    pub(crate) summarizer: Arc<Summarizer>,
    /// The instruction files the prompt includes.
    pub(crate) files: Vec<PathBuf>,
    /// The skills the prompt lists, which `/name` also starts.
    pub(crate) skills: SkillSet,
    /// The subagents behind the `task` tool, which `/explore` also starts.
    pub(crate) subagents: Subagents,
    /// The files as each turn found them, for `/rewind`.
    pub(crate) snapshots: Snapshots,
    /// Where the tools act.
    pub(crate) env: Arc<Sandbox<Local>>,
    /// The sandbox, described.
    pub(crate) sandbox: String,
    /// The MCP servers that started, with their tools' names and whether those are offered directly.
    pub(crate) servers: Vec<(String, Vec<String>, bool)>,
    /// What went wrong along the way, to show.
    pub(crate) problems: Vec<String>,
    pub(crate) approvals: Approvals,
    pub(crate) exec: Exec,
    pub(crate) policy: Policy,
    /// Where remembered rules are saved.
    pub(crate) rules_path: PathBuf,
    /// The models the plan offers, and the one in use.
    pub(crate) models: Vec<Model>,
    pub(crate) model: String,
    pub(crate) effort: Option<Effort>,
    /// Who is signed in.
    pub(crate) account: Option<String>,
}

/// Sign in with ChatGPT, as rho.
pub(crate) fn sign_in() -> SignIn {
    SignIn::new(AGENT_NAME, SignIn::default_path(AGENT_NAME))
}

/// Builds the agent: its env, tools, instructions and hooks, the runtime, and the session to open.
pub(crate) async fn prepare(
    cwd: PathBuf,
    store: Arc<JsonlStore>,
    options: ChatOptions,
    mcp: Vec<(String, manifatura::mcp::ServerConfig)>,
) -> Result<Prepared> {
    let sign_in = sign_in();
    let Some(account) = sign_in.account() else {
        bail!("not signed in: run `rho login`");
    };
    let mut problems = Vec::new();
    let servers_starting = tokio::spawn(crate::mcp::start(mcp));
    let models = sign_in
        .models()
        .await
        .context("listing the models of your ChatGPT plan")?;
    let model = match options.model.clone() {
        Some(model) => {
            if !models.iter().any(|m| m.slug == model) {
                problems.push(format!(
                    "your plan does not list the model {model}; `rho models` lists those it does"
                ));
            }
            model
        }
        None => models
            .first()
            .map(|m| m.slug.clone())
            .context("your ChatGPT plan lists no models")?,
    };
    let listed = models.iter().find(|m| m.slug == model);
    let effort = match (options.effort, listed) {
        (Some(e), Some(m)) if !m.efforts.is_empty() && !m.efforts.iter().any(|(x, _)| *x == e) => {
            problems.push(format!(
                "{model} has no {} effort; it uses its default",
                effort_label(Some(e))
            ));
            m.default_effort
        }
        (Some(e), _) => Some(e),
        (None, m) => m.and_then(|m| m.default_effort),
    };

    let local = Local::new(&cwd)?;
    let env = Arc::new(
        Sandbox::new(local)
            .context("starting the sandbox")?
            .protect(".rho")
            .allow_unsandboxed(true),
    );
    let sandbox = env.describe();
    let exec = Exec::new().escalation(true);
    let mut tools = manifatura::tools::by_name(TOOLS).map_err(anyhow::Error::msg)?;
    tools.extend(exec.tools());
    tools.push(crate::plan::update_plan());
    let subagents = subagents()?;
    tools.push(subagents.tool());

    // MCP tools come after the built-ins. A server's tools are deferred unless it is `eager`: the model finds them
    // with find_tools, so their schemas stay out of the prompt until it needs them.
    let mut servers = Vec::new();
    for started in servers_starting.await? {
        match started {
            Ok(started) => {
                let names: Vec<String> = started
                    .tools
                    .iter()
                    .map(|tool| tool.name().to_owned())
                    .collect();
                servers.push((started.name, names, started.server.config().eager));
                tools.extend(started.tools);
            }
            Err(problem) => problems.push(problem),
        }
    }
    let deferred = tools.iter().any(|tool| tool.is_deferred());

    let home = std::env::var_os("HOME");
    let mut files = ContextFiles::new()
        .max_bytes(32 * 1024)
        .user_file(paths::config_dir().join("AGENTS.md"));
    let mut skills = Skills::new();
    if let Some(home) = &home {
        files = files.home(home);
        skills = skills.home(home);
    }
    let files = files.load(&*env).await;
    let skills = skills.load(&*env).await;
    problems.extend(skills.warnings().iter().cloned());
    tools.extend(skills.tool());
    let instructions = SystemPrompt::new(BASE.trim())
        .tools(&tools)
        .context_files(&files)
        .skills(&skills)
        .cwd(&cwd)
        .date(today())
        .build();

    // rho's own repository, outside the workspace: git writes there unsandboxed, which only rho asks for.
    let snapshots = Snapshots::new(paths::snapshots_dir(&cwd)).unsandboxed(true);
    let rules_path = paths::rules_path();
    let policy = policy(&cwd, &rules_path, &exec, &mut problems)?;
    let approvals = Approvals::new();
    let summarizer = Arc::new(Summarizer::default());
    let provider = Catalog::new(sign_in.provider(), &models);
    let window = options
        .context_window
        .or_else(|| provider.window(&model))
        .unwrap_or(DEFAULT_WINDOW);
    let mut agent = Agent::new(instructions)
        .name(AGENT_NAME)
        .model(model.clone())
        .max_steps(200)
        .tools(tools);
    if let Some(tokens) = options.context_window {
        agent = agent.context_window(tokens);
    }
    if let Some(effort) = effort {
        agent = agent.reasoning(effort);
    }
    // The catalog goes first, so the hooks after it see the tool a run_tool call stands for.
    if deferred {
        agent = agent.hooks(kit::catalog());
    }
    let agent = agent
        .hooks(policy.hooks(&approvals))
        .hooks(exec.hooks())
        .hooks(snapshots.hooks())
        .hooks(kit::compaction::trim_results(u64::from(window) / 2, 8_000))
        .hooks(kit::compaction::auto(0.85, summarizer.clone()))
        .hooks(kit::compaction::on_overflow(summarizer.clone()))
        .hooks(kit::system_delta());

    let runtime = Runtime::builder()
        .provider(provider)
        .store(store.clone())
        .env(env.clone())
        .build();
    let known = recent_sessions(store.as_ref()).await?;
    let id = match (&options.resume, options.resume_latest) {
        (Some(id), _) => {
            let id = SessionId::from(id.as_str());
            if !known.iter().any(|s| s.id == id) {
                bail!(
                    "no session {} in this directory; `rho --resume` lists them",
                    id.as_str()
                );
            }
            id
        }
        (_, true) => known
            .iter()
            .max_by_key(|s| s.updated)
            .map_or_else(SessionId::new, |latest| latest.id.clone()),
        _ => SessionId::new(),
    };
    // A new session is only written once it is used, so starting rho and quitting leaves nothing behind.
    let existed = known.iter().any(|s| s.id == id);
    let session = runtime.session(&agent, id);
    let history = if existed {
        session.history().await?
    } else {
        Vec::new()
    };
    Ok(Prepared {
        runtime,
        agent,
        session,
        history,
        store,
        summarizer,
        files: files.into_iter().map(|file| file.path).collect(),
        skills,
        subagents,
        snapshots,
        env,
        sandbox,
        servers,
        problems,
        approvals,
        exec,
        policy,
        rules_path,
        models,
        model,
        effort,
        account: account.email.or(Some(account.subject)),
    })
}

/// The built-in subagents, for the `task` tool and `/explore`.
fn subagents() -> Result<Subagents> {
    let mut list = Vec::new();
    for text in BUILTIN_AGENTS {
        let definition = Definition::parse(text)
            .map_err(anyhow::Error::msg)
            .context("a built-in subagent")?;
        list.push(Subagent::from_definition(definition));
    }
    Ok(Subagents::new(list))
}

/// What asks and what may run: your rules, the project's (which may only ask and deny: a repository must not let
/// commands out of the sandbox), then rho's defaults.
fn policy(
    cwd: &Path,
    rules_path: &Path,
    exec: &Exec,
    problems: &mut Vec<String>,
) -> Result<Policy> {
    let mut policy = Policy::new();
    if let Some(rules) = paths::read_rules(rules_path)? {
        policy = policy.rules(&rules).map_err(anyhow::Error::msg)?;
    }
    let project = cwd.join(paths::PROJECT_RULES);
    if let Some(mut rules) = paths::read_rules(&project)? {
        if !rules.allow.is_empty() {
            problems.push(format!(
                "{} allows {}: a project's rules may only ask and deny, so these were left out; add them to {} \
                 to allow them",
                paths::PROJECT_RULES,
                rules.allow.join(", "),
                rules_path.display()
            ));
            rules.allow.clear();
        }
        policy = policy.rules(&rules).map_err(anyhow::Error::msg)?;
    }
    let typing = exec.clone();
    let mut policy = policy
        .when(
            |call, _| manifatura::exec::is_escalation(call),
            Decision::Ask,
        )
        .when(
            move |call, session| typing.types_into_escalated(call, session),
            Decision::Ask,
        );
    for tool in ALLOWED {
        policy = policy.tool_default(*tool, Decision::Allow);
    }
    let save = rules_path.to_path_buf();
    Ok(policy.default(Decision::Ask).on_remember(move |rule| {
        if let Err(error) = paths::remember(&save, rule) {
            tracing::warn!("could not save the rule {rule}: {error:#}");
        }
    }))
}

/// The window to assume for a model the catalog gives none for.
const DEFAULT_WINDOW: u32 = 272_000;

/// The plan's provider, reporting each model's context window from the plan's catalog, so compaction measures
/// against the model in use after `/model` too.
struct Catalog {
    inner: Responses,
    windows: HashMap<String, u32>,
}

impl Catalog {
    fn new(inner: Responses, models: &[Model]) -> Self {
        let windows = models
            .iter()
            .filter_map(|m| Some((m.slug.clone(), m.context_window?)))
            .collect();
        Self { inner, windows }
    }

    fn window(&self, model: &str) -> Option<u32> {
        self.windows.get(model).copied()
    }
}

impl Provider for Catalog {
    fn stream(&self, req: ModelRequest, abort: Abort) -> BoxStream<'static, StreamEvent> {
        self.inner.stream(req, abort)
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        let context_window = self.window(model)?;
        // Only a compaction's summary is held to it.
        Some(ModelInfo {
            context_window,
            max_output: 32_000,
        })
    }
}

/// Reads an effort: `off`, `low`, `medium`, `high`, `xhigh`, `max` or `ultra`.
pub(crate) fn parse_effort(text: &str) -> Result<Effort, String> {
    Effort::parse(text).ok_or_else(|| {
        format!(
            "no effort {}: use off, low, medium, high, xhigh, max or ultra",
            text.trim()
        )
    })
}

/// The efforts `model` accepts, from the least, with the catalog's descriptions; off, low, medium and high for a
/// model the catalog lists none for.
pub(crate) fn efforts(models: &[Model], model: &str) -> Vec<(Effort, String)> {
    match models.iter().find(|m| m.slug == model) {
        Some(m) if !m.efforts.is_empty() => m.efforts.clone(),
        _ => [Effort::Off, Effort::Low, Effort::Medium, Effort::High]
            .into_iter()
            .map(|e| (e, String::new()))
            .collect(),
    }
}

/// An effort as `/effort` names it; the model's own default when none is set.
pub(crate) fn effort_label(effort: Option<Effort>) -> String {
    match effort {
        None => "default".into(),
        Some(Effort::Off) => "off".into(),
        Some(effort) => effort.as_str().into(),
    }
}

/// This directory's sessions, most recent first.
pub(crate) async fn recent_sessions(store: &dyn Store) -> Result<Vec<SessionMeta>> {
    let mut sessions = store.list().await?;
    sessions.retain(|s| s.agent.as_deref() == Some(AGENT_NAME));
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated));
    Ok(sessions)
}

/// How long ago a time in milliseconds since the epoch was, roughly.
pub(crate) fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let minutes = now.saturating_sub(ms) / 60_000;
    match minutes {
        0 => "just now".into(),
        1..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1440),
    }
}

/// Today's local date, for the system prompt. Day granularity keeps the prompt the same all day, so the
/// provider's prompt cache stays warm.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifatura::ToolCall;
    use serde_json::json;

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            input,
        }
    }

    #[test]
    fn the_policy_asks_only_to_leave_the_sandbox_and_for_other_tools() {
        let dir = std::env::temp_dir().join(format!("rho-policy-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".rho")).unwrap();
        std::fs::write(
            dir.join(paths::PROJECT_RULES),
            "allow = [\"exec_command(curl)\"]\ndeny = [\"exec_command(git push)\"]\n",
        )
        .unwrap();
        let mut problems = Vec::new();
        let policy = policy(&dir, &dir.join("none.toml"), &Exec::new(), &mut problems).unwrap();
        let s = SessionId::from("s");
        let decide = |c: &ToolCall| policy.decide(c, &s).decision;
        assert_eq!(
            decide(&call("exec_command", json!({"cmd": "npm install"}))),
            Decision::Allow,
            "sandboxed"
        );
        let escalated = json!({"cmd": "npm install", "sandbox_permissions": "require_escalated", "justification": "x"});
        assert_eq!(decide(&call("exec_command", escalated)), Decision::Ask);
        assert_eq!(
            decide(&call("apply_patch", json!({"input": "…"}))),
            Decision::Allow
        );
        assert_eq!(
            decide(&call("mcp__github__create_issue", json!({}))),
            Decision::Ask
        );
        assert_eq!(
            decide(&call("exec_command", json!({"cmd": "git push"}))),
            Decision::Deny,
            "the project denies"
        );
        let curl = json!({"cmd": "curl x", "sandbox_permissions": "require_escalated"});
        assert_eq!(
            decide(&call("exec_command", curl)),
            Decision::Ask,
            "the project cannot allow"
        );
        assert!(
            problems[0].contains("may only ask and deny"),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explore_reads_and_searches_within_a_budget() {
        let explore = Definition::parse(BUILTIN_AGENTS[0]).unwrap();
        assert_eq!(
            explore.tools.as_deref(),
            Some(&["read".to_owned(), "grep".into(), "glob".into()][..])
        );
        assert_eq!(explore.max_steps, Some(20));
        let tool = subagents().unwrap().tool();
        assert_eq!(tool.name(), "task");
        assert!(
            tool.spec()
                .description
                .contains("- explore: Answers one question"),
            "{}",
            tool.spec().description
        );
    }

    #[test]
    fn efforts_read_and_print() {
        assert_eq!(parse_effort("High"), Ok(Effort::High));
        assert_eq!(parse_effort("none"), Ok(Effort::Off));
        assert_eq!(parse_effort("xhigh"), Ok(Effort::XHigh));
        assert!(parse_effort("extreme").is_err());
        assert_eq!(effort_label(None), "default");
        assert_eq!(effort_label(Some(Effort::Medium)), "medium");
    }
}
