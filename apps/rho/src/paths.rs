//! Where rho keeps things: its settings, rules, MCP servers and own `AGENTS.md` in `$XDG_CONFIG_HOME/rho` (or
//! `~/.config/rho`), and each directory's sessions in `$XDG_DATA_HOME/rho/sessions` (or `~/.local/share/rho`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use manifatura::policy::{Rule, Rules};
use serde::Deserialize;

/// A project's own rules, which may only ask and deny.
pub(crate) const PROJECT_RULES: &str = ".rho/rules.toml";

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

/// Your rules: what may run without asking, what asks, what never runs.
pub(crate) fn rules_path() -> PathBuf {
    config_dir().join("rules.toml")
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

/// `config.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// The model to start with; `rho models` lists the plan's.
    pub(crate) model: Option<String>,
    /// How much the model reasons: off, low, medium or high.
    pub(crate) effort: Option<String>,
    /// The model's context window in tokens, which compaction measures against.
    pub(crate) context_window: Option<u32>,
}

fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// `config.toml`, or the defaults when there is none.
pub(crate) fn config() -> Result<Config> {
    let path = config_dir().join("config.toml");
    match read(&path)? {
        Some(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
        None => Ok(Config::default()),
    }
}

/// The rules in `path`, if it exists.
pub(crate) fn read_rules(path: &Path) -> Result<Option<Rules>> {
    match read(path)? {
        Some(text) => Ok(Some(
            toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?,
        )),
        None => Ok(None),
    }
}

/// Adds `rule` to the allow list in `path`, keeping the rest.
pub(crate) fn remember(path: &Path, rule: &Rule) -> Result<()> {
    let mut rules = read_rules(path)?.unwrap_or_default();
    let text = rule.to_string();
    if rules.allow.contains(&text) {
        return Ok(());
    }
    rules.allow.push(text);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, toml::to_string(&rules)?)
        .with_context(|| format!("writing {}", path.display()))
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

    #[test]
    fn remembered_rules_join_the_allow_list_once() {
        let dir = std::env::temp_dir().join(format!("rho-rules-{}", std::process::id()));
        let path = dir.join("rules.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "deny = [\"exec_command(git push --force)\"]\n").unwrap();
        let rule = Rule::command("exec_command", ["git", "commit"]);
        remember(&path, &rule).unwrap();
        remember(&path, &rule).unwrap();
        let rules = read_rules(&path).unwrap().unwrap();
        assert_eq!(rules.allow, ["exec_command(git commit)"]);
        assert_eq!(rules.deny, ["exec_command(git push --force)"]);
        assert!(read_rules(&dir.join("missing.toml")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
