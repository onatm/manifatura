//! The agent: the ChatGPT sign-in, the sandboxed env, the tools, the prompt, the approval policy and the
//! session.
//!
//! Every command runs in the sandbox without asking; the model asks to leave it (`sandbox_permissions:
//! require_escalated`), and that, typing into a command outside it, and MCP tools ask through the agent's
//! `Approvals`, unless a rule allows them. Reads, patches inside the workspace, plans and skills never ask.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use manifatura::auth::openai::{Model, SignIn};
use manifatura::exec::Exec;
use manifatura::kit::context::{ContextFile, ContextFiles, SystemPrompt};
use manifatura::kit::{self, Approvals, Summarizer};
use manifatura::policy::{Decision, Policy, Rules};
use manifatura::providers::responses::Responses;
use manifatura::sandbox::Sandbox;
use manifatura::skills::{AGENTS_DIR, SkillSet, Skills, SystemPromptExt};
use manifatura::snapshots::Snapshots;
use manifatura::store::JsonlStore;
use manifatura::subagents::{Definition, Progress, Subagent, Subagents};
use manifatura::tools::Local;
use manifatura::{
    Abort, Agent, BoxStream, Effort, Env, Message, ModelInfo, ModelRequest, Provider, Runtime,
    Session, SessionId, SessionMeta, Store, StreamEvent, Tool,
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
/// provider's prompt cache serves it) on the session's model, with the file's body as its brief, calling only the
/// tools its `tools` names and keeping the last of its steps for its report.
const BUILTIN_AGENTS: &[&str] = &[include_str!("agents/explore.md")];

/// Where a project keeps subagents of its own, beside yours in rho's config directory.
pub(crate) const PROJECT_AGENTS: &str = ".rho/agents";

/// What every subagent you define is told before its own instructions.
const AGENT_PREAMBLE: &str = include_str!("agents/preamble.md");

/// What the screen and the command line ask for.
#[derive(Clone, Debug)]
pub(crate) struct ChatOptions {
    /// Open the most recent session for this directory.
    pub(crate) resume_latest: bool,
    /// Open this session.
    pub(crate) resume: Option<String>,
    /// The model chosen for this run (`--model`), which the session then keeps.
    pub(crate) model: Option<String>,
    /// The effort chosen for this run (`--effort`), which the session then keeps.
    pub(crate) effort: Option<Effort>,
    /// For a session that has not chosen: the config's model (else the plan's first) and effort (else the
    /// model's own default).
    pub(crate) default_model: Option<String>,
    pub(crate) default_effort: Option<Effort>,
    /// Keep the log, behind a button on the screen.
    pub(crate) logs: bool,
    /// Take the mouse for clicks, which also takes the scroll wheel from the terminal.
    pub(crate) mouse: bool,
    /// Your themes, to switch between with `/theme`, and whether the sidebar starts open.
    pub(crate) themes: crate::ui::theme::Themes,
    pub(crate) sidebar: bool,
    /// The context window compaction measures against, instead of the model's own.
    pub(crate) context_window: Option<u32>,
    /// Your rules, other agents' folders to read, and settings for subagents, from `config.toml`.
    pub(crate) rules: Rules,
    pub(crate) compat: Vec<paths::Compat>,
    pub(crate) agents: std::collections::BTreeMap<String, paths::AgentConfig>,
    /// What starting up changed, such as old config files folded into `config.toml`, for the screen.
    pub(crate) notices: Vec<String>,
}

/// The agent and its session, set up once rho is connected.
pub(crate) struct Prepared {
    pub(crate) runtime: Runtime,
    pub(crate) agent: Agent,
    pub(crate) session: Session,
    /// The session's conversation so far, when it was resumed.
    pub(crate) history: Vec<Message>,
    pub(crate) store: Arc<JsonlStore>,
    /// The instruction files the prompt includes.
    pub(crate) files: Vec<PathBuf>,
    /// The context files as loaded, and the directory, for building the prompt again.
    pub(crate) context: Vec<ContextFile>,
    pub(crate) cwd: PathBuf,
    /// The skills the prompt lists, which `/skill:name` also starts.
    pub(crate) skills: SkillSet,
    /// The session whose turn last failed for want of room, even after a summary.
    pub(crate) overflowed: Overflowed,
    /// The subagents behind the `task` tool, which `/name task` also starts.
    pub(crate) subagents: Subagents,
    /// What the running subagents do, event by event, for the screen to follow them.
    pub(crate) progress: Option<tokio::sync::mpsc::UnboundedReceiver<Progress>>,
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
    /// Your `config.toml`, where "always allow" writes.
    pub(crate) config_path: PathBuf,
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
    let mut problems = options.notices.clone();
    let servers_starting = tokio::spawn(crate::mcp::start(mcp));
    let models = sign_in
        .models()
        .await
        .context("listing the models of your ChatGPT plan")?;
    // The session to open, and what it last chose: a session keeps its model and effort, whatever the defaults
    // are now, until one is chosen for it (a flag, /model, /effort).
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
    let (recorded_model, recorded_effort) = if existed {
        recorded_choice(&store.load(&id).await?)
    } else {
        (None, None)
    };
    let default_model = match &options.default_model {
        Some(model) if models.iter().any(|m| &m.slug == model) => model.clone(),
        other => {
            if let Some(model) = other {
                problems.push(format!(
                    "your plan does not list the model {model} in your config; `rho models` lists those it does"
                ));
            }
            models
                .first()
                .map(|m| m.slug.clone())
                .context("your ChatGPT plan lists no models")?
        }
    };
    let model = match (&options.model, recorded_model) {
        (Some(model), _) => {
            if !models.iter().any(|m| &m.slug == model) {
                problems.push(format!(
                    "your plan does not list the model {model}; `rho models` lists those it does"
                ));
            }
            model.clone()
        }
        (None, Some(model)) if models.iter().any(|m| m.slug == model) => model,
        (None, Some(model)) => {
            problems.push(format!(
                "this session's model, {model}, is no longer on your plan; it continues on {default_model}"
            ));
            default_model
        }
        (None, None) => default_model,
    };
    let effort = fit_effort(
        &models,
        &model,
        &[options.effort, recorded_effort, options.default_effort],
    );
    if let Some(wanted) = options.effort.filter(|e| Some(*e) != effort) {
        problems.push(format!(
            "{model} has no {} effort; it uses {}",
            effort_label(Some(wanted)),
            effort_label(effort)
        ));
    }

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
    let (progress, watched) = tokio::sync::mpsc::unbounded_channel();
    let project = paths::project_config(&cwd)?;
    let subagents = subagents(
        &cwd,
        &options.agents,
        &project.agents,
        &models,
        &mut problems,
    )?
    .on_event(move |p: &Progress| {
        let _ = progress.send(p.clone());
    });
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
    let skills = skill_sources(home.as_deref().map(Path::new), &options.compat);
    if let Some(home) = &home {
        files = files.home(home);
    }
    let files = files.load(&*env).await;
    let skills = skills
        .load(&*env)
        .await
        .with_visibility(project.skills.clone());
    problems.extend(skills.warnings().iter().cloned());
    tools.extend(skills.tool());
    let instructions = instructions(&tools, &files, &skills, &cwd);

    // rho's own repository, outside the workspace: git writes there unsandboxed, which only rho asks for.
    let snapshots = Snapshots::new(paths::snapshots_dir(&cwd)).unsandboxed(true);
    let config_path = paths::config_path();
    let policy = policy(
        &options.rules,
        project.rules.clone(),
        &config_path,
        &exec,
        &mut problems,
    )?;
    let approvals = Approvals::new();
    let summarizer = Arc::new(Summarizer::default());
    let overflowed = Overflowed::default();
    let provider = Catalog::new(sign_in.provider(), &models);
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
        // Older turns' tool results stay in the session but out of the requests; a summary is asked for only past
        // 85% of the window, or when the provider still finds the request too long.
        .hooks(kit::compaction::drop_old_results())
        .hooks(kit::compaction::auto(0.85, summarizer.clone()))
        .hooks(kit::compaction::on_overflow(summarizer))
        .hooks(overflow_seen(&overflowed))
        .hooks(kit::system_delta());

    let runtime = Runtime::builder()
        .provider(provider)
        .store(store.clone())
        .env(env.clone())
        .build();
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
        files: files.iter().map(|file| file.path.clone()).collect(),
        context: files,
        cwd: cwd.clone(),
        skills,
        overflowed,
        subagents,
        progress: Some(watched),
        snapshots,
        env,
        sandbox,
        servers,
        problems,
        approvals,
        exec,
        policy,
        config_path,
        models,
        model,
        effort,
        account: account.email.or(Some(account.subject)),
    })
}

/// The session whose turn last failed because its context no longer fit the window.
pub(crate) type Overflowed = Arc<std::sync::Mutex<Option<SessionId>>>;

/// After `on_overflow` has had its go: a request still too long marks its session, so the screen can say what to do.
fn overflow_seen(overflowed: &Overflowed) -> manifatura::Hooks {
    let overflowed = overflowed.clone();
    manifatura::Hooks::new().on(move |e: manifatura::hook::ModelError, cx| {
        let overflowed = overflowed.clone();
        async move {
            if e.error.kind == manifatura::ErrorKind::ContextOverflow {
                *overflowed.lock().unwrap() = Some(cx.session_id().clone());
            }
            manifatura::hook::Recover::Fail
        }
    })
}

/// The system prompt: rho's instructions, the tools' guidelines, the context files, the skills the model may
/// load, the directory and the date.
fn instructions(tools: &[Tool], files: &[ContextFile], skills: &SkillSet, cwd: &Path) -> String {
    SystemPrompt::new(BASE.trim())
        .tools(tools)
        .context_files(files)
        .skills(skills)
        .cwd(cwd)
        .date(today())
        .build()
}

/// The agent again once the skills' visibility changed: its prompt and its skill tool. Sessions open already keep
/// theirs; those opened from now get this one.
pub(crate) fn with_skills(
    agent: Agent,
    files: &[ContextFile],
    skills: &SkillSet,
    cwd: &Path,
) -> Agent {
    let agent = match skills.tool() {
        Some(tool) => agent.with_tool(tool),
        None => agent.without_tool(manifatura::skills::SKILL_TOOL),
    };
    let text = instructions(agent.tool_list(), files, skills, cwd);
    agent.with_instructions(text)
}

/// Where skills come from, a later source hiding an earlier one of the same name: other agents' folders when
/// `compat` asks for them, `~/.agents/skills`, rho's own, then the project's `.agents/skills` from the repository
/// root down to the working directory.
fn skill_sources(home: Option<&Path>, compat: &[paths::Compat]) -> Skills {
    let claude = compat.contains(&paths::Compat::Claude);
    let mut skills = Skills::new();
    if let Some(home) = home {
        if claude {
            skills = skills.dir(home.join(".claude/skills"));
        }
        skills = skills.dir(home.join(AGENTS_DIR)).home(home);
    }
    skills = skills.dir(paths::config_dir().join("skills"));
    if claude {
        skills = skills.project(".claude/skills");
    }
    skills.project(AGENTS_DIR)
}

/// The subagents: rho's own, yours and the project's, for the `task` tool and `/name task`.
fn subagents(
    cwd: &Path,
    yours: &BTreeMap<String, paths::AgentConfig>,
    project: &BTreeMap<String, paths::AgentConfig>,
    models: &[Model],
    problems: &mut Vec<String>,
) -> Result<Subagents> {
    let mut list: Vec<Subagent> = Vec::new();
    let mut add = |sub: Subagent| match list.iter_mut().find(|s| s.name() == sub.name()) {
        Some(have) => *have = sub,
        None => list.push(sub),
    };
    for text in BUILTIN_AGENTS {
        let definition = Definition::parse(text)
            .map_err(anyhow::Error::msg)
            .context("a built-in subagent")?;
        add(Subagent::from_definition(definition));
    }
    // Yours, then the project's: a later one replaces an earlier one of the same name.
    for dir in [paths::config_dir().join("agents"), cwd.join(PROJECT_AGENTS)] {
        for (path, definition) in read_agents(&dir, problems) {
            let mut definition = definition;
            definition.body = format!("{AGENT_PREAMBLE}\n\n{}", definition.body.trim());
            tracing::debug!("subagent {} from {}", definition.name, path.display());
            add(Subagent::from_definition(definition));
        }
    }
    // Then the settings in config.toml over what the files say.
    for (name, config) in yours.iter().chain(project) {
        let Some(at) = list.iter().position(|s| s.name() == name) else {
            problems.push(format!("[agents.{name}]: there is no subagent {name}"));
            continue;
        };
        let effort = match config.effort.as_deref().map(parse_effort).transpose() {
            Ok(effort) => effort,
            Err(error) => {
                problems.push(format!("[agents.{name}]: {error}"));
                None
            }
        };
        if let Some(model) = &config.model
            && !models.iter().any(|m| &m.slug == model)
        {
            problems.push(format!(
                "[agents.{name}]: your plan does not list the model {model}"
            ));
        }
        // A server's tools join the tools it may call; with no list, it may call them all already.
        let base = config
            .tools
            .clone()
            .or_else(|| list[at].allowed_tools().map(<[String]>::to_vec));
        let tools = match (&config.mcp, base) {
            (Some(servers), Some(mut tools)) => {
                tools.extend(servers.iter().map(|server| format!("mcp__{server}__*")));
                Some(tools)
            }
            (_, tools) => tools,
        };
        let sub = list[at].clone();
        list[at] = sub.with(tools, config.model.clone(), effort);
    }
    Ok(Subagents::new(list))
}

/// The agents in `dir`, one Markdown file each; a file that does not read is left out with a problem.
fn read_agents(dir: &Path, problems: &mut Vec<String>) -> Vec<(PathBuf, Definition)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "md"))
        .collect();
    paths.sort();
    let mut found = Vec::new();
    for path in paths {
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| Definition::parse(&text))
        {
            Ok(definition) => found.push((path, definition)),
            Err(error) => problems.push(format!("{}: {error}; left out", path.display())),
        }
    }
    found
}

/// What asks and what may run: your rules, the project's (which may only ask and deny: a repository must not let
/// commands out of the sandbox), then rho's defaults.
fn policy(
    user: &Rules,
    mut project: Rules,
    config_path: &Path,
    exec: &Exec,
    problems: &mut Vec<String>,
) -> Result<Policy> {
    let mut policy = Policy::new()
        .rules(user)
        .map_err(|e| anyhow::anyhow!("[rules] in {}: {e}", config_path.display()))?;
    if !project.allow.is_empty() {
        problems.push(format!(
            "{} allows {}: a project's rules may only ask and deny, so these were left out; add them under [rules] \
             in {} to allow them",
            paths::PROJECT_CONFIG,
            project.allow.join(", "),
            config_path.display()
        ));
        project.allow.clear();
    }
    policy = policy
        .rules(&project)
        .map_err(|e| anyhow::anyhow!("[rules] in {}: {e}", paths::PROJECT_CONFIG))?;
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
    let save = config_path.to_path_buf();
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
        let context_window = self.window(model).unwrap_or(DEFAULT_WINDOW);
        // Only a summary is held to it.
        Some(ModelInfo {
            context_window,
            max_output: 32_000,
        })
    }
}

/// The model and effort a session last recorded, from its entries.
fn recorded_choice(entries: &[manifatura::Entry]) -> (Option<String>, Option<Effort>) {
    let (mut model, mut effort) = (None, None);
    for entry in entries {
        if let manifatura::Record::ModelChange {
            model: m,
            reasoning: r,
        } = &entry.record
        {
            model = m.clone().or(model);
            effort = r.or(effort);
        }
    }
    (model, effort)
}

/// The first of `wanted` that `model` offers, else the model's own default.
pub(crate) fn fit_effort(
    models: &[Model],
    model: &str,
    wanted: &[Option<Effort>],
) -> Option<Effort> {
    let listed = models.iter().find(|m| m.slug == model);
    let offered = |e: &Effort| {
        listed.is_none_or(|m| m.efforts.is_empty() || m.efforts.iter().any(|(x, _)| x == e))
    };
    wanted
        .iter()
        .flatten()
        .find(|e| offered(e))
        .copied()
        .or_else(|| default_effort(models, model))
}

/// The effort `model` uses when none is chosen, from the plan's catalog.
pub(crate) fn default_effort(models: &[Model], model: &str) -> Option<Effort> {
    models
        .iter()
        .find(|m| m.slug == model)
        .and_then(|m| m.default_effort)
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
        let project = Rules {
            allow: vec!["exec_command(curl)".into()],
            deny: vec!["exec_command(git push)".into()],
            ..Rules::default()
        };
        let mut problems = Vec::new();
        let policy = policy(
            &Rules::default(),
            project,
            &dir.join("config.toml"),
            &Exec::new(),
            &mut problems,
        )
        .unwrap();
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
    fn agents_come_from_files_and_settings_change_them() {
        let explore = Definition::parse(BUILTIN_AGENTS[0]).unwrap();
        assert_eq!(
            explore.tools.as_deref(),
            Some(&["read".to_owned(), "grep".into(), "glob".into()][..])
        );
        assert_eq!(explore.max_steps, None, "the default limit applies");

        let dir = std::env::temp_dir().join(format!("rho-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(PROJECT_AGENTS)).unwrap();
        std::fs::write(
            dir.join(PROJECT_AGENTS).join("reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews a diff.\ntools: read, exec_command\n---\nFind bugs.\n",
        )
        .unwrap();
        std::fs::write(dir.join(PROJECT_AGENTS).join("broken.md"), "no frontmatter").unwrap();
        let setting = |mcp: Option<&[&str]>, effort: Option<&str>| paths::AgentConfig {
            mcp: mcp.map(|m| m.iter().map(|s| s.to_string()).collect()),
            effort: effort.map(str::to_string),
            ..Default::default()
        };
        let yours = BTreeMap::from([
            ("explore".to_string(), setting(Some(&["exa"]), Some("low"))),
            ("ghost".to_string(), setting(None, None)),
        ]);
        let mut problems = Vec::new();
        let subagents = subagents(&dir, &yours, &BTreeMap::new(), &[], &mut problems).unwrap();
        let tool = subagents.tool();
        assert_eq!(tool.name(), "task");
        let about = &tool.spec().description;
        assert!(
            about.contains("- explore: Answers one question")
                && about.contains("- reviewer: Reviews a diff."),
            "{about}"
        );
        let explore = subagents
            .list()
            .iter()
            .find(|s| s.name() == "explore")
            .unwrap();
        assert_eq!(
            explore.allowed_tools().unwrap(),
            ["read", "grep", "glob", "mcp__exa__*"]
        );
        assert!(
            problems.iter().any(|p| p.contains("broken.md")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("no subagent ghost")),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn model(slug: &str, default: Option<Effort>, efforts: &[Effort]) -> Model {
        let mut m = Model::new(slug);
        m.default_effort = default;
        m.efforts = efforts.iter().map(|e| (*e, String::new())).collect();
        m
    }

    #[test]
    fn an_effort_the_model_lacks_falls_through_to_the_next_then_its_default() {
        let models = [
            model("a", Some(Effort::Medium), &[Effort::Low, Effort::Medium]),
            model("b", Some(Effort::High), &[Effort::High, Effort::XHigh]),
        ];
        let fit = |model, wanted: &[Option<Effort>]| fit_effort(&models, model, wanted);
        // A flag, then what the session recorded, then the config's default.
        assert_eq!(
            fit("a", &[Some(Effort::Low), Some(Effort::Medium)]),
            Some(Effort::Low)
        );
        assert_eq!(
            fit("a", &[None, Some(Effort::XHigh), Some(Effort::Low)]),
            Some(Effort::Low)
        );
        assert_eq!(fit("b", &[Some(Effort::Low), None]), Some(Effort::High));
        assert_eq!(fit("b", &[]), Some(Effort::High));
        // A model the catalog does not know takes what is asked.
        assert_eq!(fit("c", &[Some(Effort::Max)]), Some(Effort::Max));
        assert_eq!(default_effort(&models, "c"), None);
    }

    #[test]
    fn the_last_choice_recorded_is_the_sessions() {
        let change = |model: Option<&str>, reasoning| {
            manifatura::Entry::new(
                None,
                manifatura::Record::ModelChange {
                    model: model.map(str::to_string),
                    reasoning,
                },
            )
        };
        let entries = [
            change(Some("a"), Some(Effort::Low)),
            change(Some("b"), None),
            change(None, Some(Effort::High)),
        ];
        assert_eq!(
            recorded_choice(&entries),
            (Some("b".to_string()), Some(Effort::High))
        );
        assert_eq!(recorded_choice(&[]), (None, None));
    }

    #[tokio::test]
    async fn project_skills_hide_yours_and_claudes_are_read_when_asked() {
        let home = std::env::temp_dir().join(format!("rho-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let skill = |dir: PathBuf, name: &str, about: &str| {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(
                dir.join(name).join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {about}\n---\nDo it.\n"),
            )
            .unwrap();
        };
        skill(home.join(".agents/skills"), "a", "Yours.");
        skill(home.join(".claude/skills"), "b", "Claude's.");
        let project = home.join("proj");
        skill(project.join(".agents/skills"), "a", "The project's.");
        let env = Local::new(&project).unwrap();

        let found = skill_sources(Some(&home), &[]).load(&env).await;
        let names: Vec<(&str, &str)> = found
            .skills()
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str()))
            .collect();
        assert_eq!(names, [("a", "The project's.")]);
        assert!(found.warnings().iter().any(|w| w.contains("hides")));

        let found = skill_sources(Some(&home), &[paths::Compat::Claude])
            .load(&env)
            .await;
        assert_eq!(found.skills().len(), 2);
        let _ = std::fs::remove_dir_all(&home);
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
