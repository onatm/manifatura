//! Where rho keeps things: `config.toml` (settings, rules, MCP servers, subagent settings), its own `AGENTS.md`,
//! skills and agents in `$XDG_CONFIG_HOME/rho` (or `~/.config/rho`), and each directory's sessions in `$XDG_DATA_HOME/rho/sessions` (or `~/.local/share/rho`).

use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use manifatura::mcp::ServerConfig;
use manifatura::policy::{Rule, Rules};
use manifatura::skills::Visibility;
use serde::Deserialize;
use toml_edit::{Array, DocumentMut, Item, Table};

/// A project's own config: its rules, which may only ask and deny, and settings for subagents.
pub(crate) const PROJECT_CONFIG: &str = ".rho/config.toml";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `$name` when it is an absolute path, else `fallback` under the home directory.
fn base(name: &str, fallback: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

/// rho's config directory.
pub(crate) fn config_dir() -> PathBuf {
    base("XDG_CONFIG_HOME", ".config").join("rho")
}

/// Where this directory's sessions are kept.
pub(crate) fn sessions_dir(cwd: &Path) -> PathBuf {
    base("XDG_DATA_HOME", ".local/share")
        .join("rho/sessions")
        .join(slug(cwd))
}

/// Where this directory's snapshots are kept: a git repository of rho's own, beside the sessions.
pub(crate) fn snapshots_dir(cwd: &Path) -> PathBuf {
    base("XDG_DATA_HOME", ".local/share")
        .join("rho/snapshots")
        .join(format!("{}.git", slug(cwd)))
}

/// A directory name for a path: `/Users/me/src/app` is `Users-me-src-app`.
pub(crate) fn slug(path: &Path) -> String {
    let mut slug = String::new();
    for c in path.to_string_lossy().chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_owned()
}

/// `config.toml`: everything rho is configured with, in one file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// The model new sessions start on; `rho models` lists the plan's.
    pub(crate) model: Option<String>,
    /// The effort new sessions start on, when their model has it.
    pub(crate) effort: Option<String>,
    /// The model's context window in tokens, which compaction measures against.
    pub(crate) context_window: Option<u32>,
    /// Take the mouse, so the footer and the task list answer clicks. The terminal's scroll wheel then reaches
    /// rho instead of the scrollback.
    pub(crate) mouse: Option<bool>,
    /// Other agents' folders to read too: `claude` reads `.claude/skills` and `~/.claude/skills`.
    #[serde(default)]
    pub(crate) compat: Vec<Compat>,
    /// What may run without asking, what asks, what never runs.
    #[serde(default)]
    pub(crate) rules: Rules,
    /// MCP servers, by name.
    #[serde(default)]
    pub(crate) mcp: BTreeMap<String, ServerConfig>,
    /// Settings for subagents, by name, over what their files say.
    #[serde(default)]
    pub(crate) agents: BTreeMap<String, AgentConfig>,
}

/// Another agent whose folders rho also reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Compat {
    Claude,
}

/// A subagent's settings in `config.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentConfig {
    /// The only tools it may call (`mcp__server__*` patterns for MCP tools).
    pub(crate) tools: Option<Vec<String>>,
    /// The MCP servers whose tools it gets.
    pub(crate) mcp: Option<Vec<String>>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
}

/// A project's `.rho/config.toml`: its rules, which may only ask and deny, and settings for subagents.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectConfig {
    #[serde(default)]
    pub(crate) rules: Rules,
    #[serde(default)]
    pub(crate) agents: BTreeMap<String, AgentConfig>,
    /// Who sees each skill in this project (`/skills` writes it): `name-only`, `user-only` or `off`.
    #[serde(default)]
    pub(crate) skills: BTreeMap<String, Visibility>,
}

fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Reads a config file; an empty one when it does not exist.
fn parse<T: Default + serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    match read(path)? {
        Some(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
        None => Ok(T::default()),
    }
}

/// Your `config.toml`.
pub(crate) fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub(crate) fn config() -> Result<Config> {
    parse(&config_path())
}

/// The project's `.rho/config.toml`, under `cwd`.
pub(crate) fn project_config(cwd: &Path) -> Result<ProjectConfig> {
    parse(&cwd.join(PROJECT_CONFIG))
}

/// Writes who sees each skill under `[skills]` in `path`, leaving out those that are on and keeping the rest of
/// the file as it is.
pub(crate) fn save_skills(path: &Path, visibility: &BTreeMap<String, Visibility>) -> Result<()> {
    let mut doc: DocumentMut = read(path)?
        .unwrap_or_default()
        .parse()
        .with_context(|| format!("reading {}", path.display()))?;
    // A new table goes after the others, so the file keeps its order.
    let mut table = Table::new();
    table.set_position(Some(isize::MAX));
    for (name, seen) in visibility {
        if *seen != Visibility::On {
            table.insert(name, toml_edit::value(seen.as_str()));
        }
    }
    if table.is_empty() {
        doc.remove("skills");
    } else {
        doc.insert("skills", Item::Table(table));
    }
    write(path, &doc.to_string())
}

/// Adds `rule` to the allow list under `[rules]` in `path`, keeping the rest of the file as it is.
pub(crate) fn remember(path: &Path, rule: &Rule) -> Result<()> {
    let mut doc: DocumentMut = read(path)?
        .unwrap_or_default()
        .parse()
        .with_context(|| format!("reading {}", path.display()))?;
    let rules = doc
        .entry("rules")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .context("`rules` is not a table")?;
    let allow = rules
        .entry("allow")
        .or_insert_with(|| Item::Value(Array::new().into()))
        .as_array_mut()
        .context("`rules.allow` is not a list")?;
    let text = rule.to_string();
    if allow.iter().any(|v| v.as_str() == Some(text.as_str())) {
        return Ok(());
    }
    allow.push(text);
    write(path, &doc.to_string())
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Folds the files rho used to keep apart into `config.toml`: `rules.toml` and `mcp.toml` beside yours, and a
/// project's `.rho/rules.toml`. Each old file is kept as `.bak`. What happened, for the screen.
pub(crate) fn migrate(cwd: &Path) -> Vec<String> {
    let user = config_path();
    let moves = [
        (config_dir().join("rules.toml"), user.clone(), "rules", None),
        (config_dir().join("mcp.toml"), user, "mcp", Some("servers")),
        (
            cwd.join(".rho/rules.toml"),
            cwd.join(PROJECT_CONFIG),
            "rules",
            None,
        ),
    ];
    let mut notices = Vec::new();
    for (old, config, key, inner) in moves {
        match fold(&old, &config, key, inner) {
            Ok(Some(notice)) => notices.push(notice),
            Ok(None) => {}
            Err(error) => notices.push(format!("{error:#}")),
        }
    }
    notices
}

/// Moves `old` (or its `inner` table) under `[key]` in `config`.
fn fold(old: &Path, config: &Path, key: &str, inner: Option<&str>) -> Result<Option<String>> {
    let Some(text) = read(old)? else {
        return Ok(None);
    };
    let from: DocumentMut = text
        .parse()
        .with_context(|| format!("reading {}", old.display()))?;
    let table = match inner {
        Some(inner) => from
            .get(inner)
            .and_then(Item::as_table)
            .cloned()
            .unwrap_or_default(),
        None => from.as_table().clone(),
    };
    let mut doc: DocumentMut = read(config)?
        .unwrap_or_default()
        .parse()
        .with_context(|| format!("reading {}", config.display()))?;
    if doc.contains_key(key) {
        return Ok(Some(format!(
            "{} is no longer read: move it under [{key}] in {}, which already has one",
            old.display(),
            config.display()
        )));
    }
    let mut table = table;
    table.set_implicit(true);
    doc.insert(key, Item::Table(table));
    write(config, &doc.to_string())?;
    let backup = old.with_extension("toml.bak");
    std::fs::rename(old, &backup).with_context(|| format!("renaming {}", old.display()))?;
    Ok(Some(format!(
        "moved {} under [{key}] in {}; the old file is {}",
        old.display(),
        config.display(),
        backup.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_becomes_a_slug() {
        assert_eq!(
            slug(Path::new("/Users/me/src/my app")),
            "Users-me-src-my-app"
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rho-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn remembered_rules_join_the_allow_list_once_and_keep_the_file() {
        let dir = scratch("remember");
        let path = dir.join("config.toml");
        let text = "# mine\nmodel = \"m\"\n\n[rules]\ndeny = [\"exec_command(git push --force)\"] # never\n";
        std::fs::write(&path, text).unwrap();
        let rule = Rule::command("exec_command", ["git", "commit"]);
        remember(&path, &rule).unwrap();
        remember(&path, &rule).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.starts_with("# mine\n") && written.contains("# never"),
            "{written}"
        );
        let config: Config = toml::from_str(&written).unwrap();
        assert_eq!(config.rules.allow, ["exec_command(git commit)"]);
        assert_eq!(config.rules.deny, ["exec_command(git push --force)"]);
        // A file that does not exist yet gets the table.
        remember(&dir.join("new.toml"), &rule).unwrap();
        let config: Config = parse(&dir.join("new.toml")).unwrap();
        assert_eq!(config.rules.allow, ["exec_command(git commit)"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn one_file_holds_settings_rules_servers_and_agents() {
        let config: Config = toml::from_str(
            r#"
model = "m"
effort = "high"
compat = ["claude"]

[rules]
allow = ["mcp__exa__*"]

[mcp.exa]
url = "https://mcp.exa.ai/mcp"
env_headers = { x-api-key = "EXA_API_KEY" }
eager = true

[mcp.fs]
command = "npx"
args = ["-y", "server-filesystem"]

[agents.explore]
effort = "low"
mcp = ["exa"]
"#,
        )
        .unwrap();
        assert_eq!(config.compat, [Compat::Claude]);
        assert_eq!(config.rules.allow, ["mcp__exa__*"]);
        let exa = &config.mcp["exa"];
        assert_eq!(
            (exa.url.as_deref(), exa.eager),
            (Some("https://mcp.exa.ai/mcp"), true)
        );
        assert_eq!(exa.env_headers["x-api-key"], "EXA_API_KEY");
        assert_eq!(config.mcp["fs"].command.as_deref(), Some("npx"));
        assert_eq!(
            config.agents["explore"].mcp.as_deref(),
            Some(&["exa".to_string()][..])
        );
        // A project may set rules and agents, nothing else.
        assert!(toml::from_str::<ProjectConfig>("model = \"m\"\n").is_err());
    }

    #[test]
    fn skill_choices_are_saved_for_the_project() {
        let dir = scratch("skill-choices");
        let path = dir.join("config.toml");
        std::fs::write(&path, "# ours\n[rules]\ndeny = []\n").unwrap();
        let choices = BTreeMap::from([
            ("deploy".to_string(), Visibility::Off),
            ("tour".to_string(), Visibility::On),
            ("legacy".to_string(), Visibility::NameOnly),
        ]);
        save_skills(&path, &choices).unwrap();
        let project: ProjectConfig = parse(&path).unwrap();
        assert_eq!(
            project.skills,
            BTreeMap::from([
                ("deploy".to_string(), Visibility::Off),
                ("legacy".to_string(), Visibility::NameOnly),
            ])
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .starts_with("# ours")
        );
        save_skills(&path, &BTreeMap::new()).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("[skills]"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_files_fold_into_the_config() {
        let dir = scratch("fold");
        let config = dir.join("config.toml");
        std::fs::write(&config, "model = \"m\"\n").unwrap();
        let rules = dir.join("rules.toml");
        std::fs::write(&rules, "allow = [\"mcp__exa__*\"]\n").unwrap();
        let mcp = dir.join("mcp.toml");
        std::fs::write(
            &mcp,
            "[servers.exa]\nurl = \"https://mcp.exa.ai/mcp\"\neager = true\n",
        )
        .unwrap();
        assert!(fold(&rules, &config, "rules", None).unwrap().is_some());
        assert!(
            fold(&mcp, &config, "mcp", Some("servers"))
                .unwrap()
                .is_some()
        );
        let parsed: Config = parse(&config).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("m"));
        assert_eq!(parsed.rules.allow, ["mcp__exa__*"]);
        assert!(
            parsed.mcp.contains_key("exa"),
            "{}",
            std::fs::read_to_string(&config).unwrap()
        );
        assert!(!rules.exists() && dir.join("rules.toml.bak").exists());
        // Nothing left to move; and a table already there is not overwritten.
        assert!(fold(&rules, &config, "rules", None).unwrap().is_none());
        std::fs::write(&rules, "deny = []\n").unwrap();
        let kept = fold(&rules, &config, "rules", None).unwrap().unwrap();
        assert!(kept.contains("no longer read") && rules.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
