//! The `AGENTS.md` convention: instruction files found from the repository root down to the
//! working directory, and a system prompt built around them.
//!
//! Everything is read through the session's [`Env`], so discovery works wherever the env
//! points. What only the app knows (the user's own file, the home directory, today's date) it
//! passes in.
//!
//! The agent's tools are chosen first, built-ins by name or tools of its own, so that the prompt
//! can carry their [guidelines](manifatura_agent::Tool::guideline):
//!
//! ```no_run
//! # async fn demo(env: &dyn manifatura_agent::Env, tools: Vec<manifatura_agent::Tool>) {
//! use manifatura_agent::Agent;
//! use manifatura_kit::context::{ContextFiles, SystemPrompt};
//!
//! // e.g. manifatura_tools::by_name(&["read", "edit", "bash"]), plus tools of the agent's own
//! let files = ContextFiles::new()
//!     .user_file("/home/me/.config/myagent/AGENTS.md")
//!     .home("/home/me")
//!     .max_bytes(16 * 1024)
//!     .load(env)
//!     .await;
//! let instructions = SystemPrompt::new("You are a coding agent in a terminal.")
//!     .tools(&tools)
//!     .guideline("When the task is done, say briefly what you changed.")
//!     .context_files(&files)
//!     .cwd(env.cwd())
//!     .date("2026-11-23")
//!     .build();
//! let agent = Agent::new(instructions).tools(tools);
//! # }
//! ```

use std::path::{Path, PathBuf};

use manifatura_agent::{Env, Tool};

/// The file names looked for in each directory, in order; the first that exists is used.
pub const DEFAULT_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// The default limit for one file's text, in bytes.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// One instruction file and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFile {
    /// Where the file was found.
    pub path: PathBuf,
    /// Its text, cut at the limit if it was longer.
    pub text: String,
}

/// Which instruction files to load.
///
/// The order is the user's file, then one file per directory from the root down to the working
/// directory, so more specific files come later. The root is the nearest ancestor of the working
/// directory that holds `.git`, or [`home`](Self::home) if that comes first; with neither above
/// it, only the working directory itself is searched.
#[derive(Clone, Debug)]
pub struct ContextFiles {
    names: Vec<String>,
    user_file: Option<PathBuf>,
    home: Option<PathBuf>,
    max_bytes: usize,
}

impl Default for ContextFiles {
    fn default() -> Self {
        Self {
            names: DEFAULT_NAMES.iter().map(|n| n.to_string()).collect(),
            user_file: None,
            home: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl ContextFiles {
    /// `AGENTS.md`, else `CLAUDE.md`, in each directory, up to 64 KiB each; no user file.
    pub fn new() -> Self {
        Self::default()
    }

    /// The file names to look for in each directory, in order of preference
    /// (default [`DEFAULT_NAMES`]).
    pub fn names<S: Into<String>>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.names = names.into_iter().map(Into::into).collect();
        self
    }

    /// A file of the user's own, loaded before the project's (e.g. `~/.config/app/AGENTS.md`).
    pub fn user_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.user_file = Some(path.into());
        self
    }

    /// Where the walk up from the working directory stops when no repository is found first.
    pub fn home(mut self, path: impl Into<PathBuf>) -> Self {
        self.home = Some(path.into());
        self
    }

    /// The most text kept from one file, in bytes (default [`DEFAULT_MAX_BYTES`]). Longer files
    /// are cut at a character boundary and marked `[truncated]`.
    pub fn max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// The directories searched, root first. Each is checked through `env`.
    pub async fn dirs(&self, env: &dyn Env) -> Vec<PathBuf> {
        let cwd = env.cwd();
        let mut dirs = Vec::new();
        let mut rooted = false;
        for dir in cwd.ancestors() {
            dirs.push(dir.to_path_buf());
            if self.home.as_deref() == Some(dir) || exists(env, &dir.join(".git")).await {
                rooted = true;
                break;
            }
        }
        if !rooted {
            dirs.truncate(1);
        }
        dirs.reverse();
        dirs
    }

    /// The files found, in prompt order. Missing or unreadable files are skipped, as is a file
    /// whose text was already loaded (a symlink to another, say).
    pub async fn load(&self, env: &dyn Env) -> Vec<ContextFile> {
        let mut candidates: Vec<PathBuf> = self.user_file.iter().cloned().collect();
        for dir in self.dirs(env).await {
            for name in &self.names {
                let path = dir.join(name);
                if is_file(env, &path).await {
                    candidates.push(path);
                    break;
                }
            }
        }
        let mut out: Vec<ContextFile> = Vec::new();
        for path in candidates {
            let Ok(bytes) = env.read(&path).await else {
                continue;
            };
            let text = truncate(String::from_utf8_lossy(&bytes).into_owned(), self.max_bytes);
            if text.trim().is_empty() || out.iter().any(|f| f.path == path || f.text == text) {
                continue;
            }
            out.push(ContextFile { path, text });
        }
        out
    }
}

async fn exists(env: &dyn Env, path: &Path) -> bool {
    matches!(env.stat(path).await, Ok(Some(_)))
}

async fn is_file(env: &dyn Env, path: &Path) -> bool {
    matches!(env.stat(path).await, Ok(Some(s)) if s.kind == manifatura_agent::FileKind::File)
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let cut = text.floor_char_boundary(max);
        text.truncate(cut);
        text.push_str("\n[truncated]");
    }
    text
}

/// The context files as a prompt section: a heading, a line on precedence, then each file
/// under its path. Empty when there are none.
pub fn section(files: &[ContextFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# Project context\n\nInstructions from context files. Follow them; more specific \
         (deeper) files take precedence.\n",
    );
    for f in files {
        out.push_str(&format!("\n## {}\n\n{}\n", f.path.display(), f.text.trim()));
    }
    out
}

/// A system prompt in pi's order: base text, guidelines, context files, extra sections, then
/// the working environment.
///
/// Nothing in it changes during a session unless the app rebuilds it, which keeps a provider's
/// prompt cache valid; pass the date at day granularity for the same reason.
#[derive(Clone, Debug, Default)]
pub struct SystemPrompt {
    base: String,
    guidelines: Vec<String>,
    context: String,
    sections: Vec<String>,
    environment: Vec<(String, String)>,
}

impl SystemPrompt {
    /// A prompt that starts with `base`, the agent's own instructions.
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            ..Self::default()
        }
    }

    /// The [guidelines](Tool::guideline) of these tools, in order, skipping deferred tools
    /// (the model does not see them) and repeats. Pass the tools the agent registers.
    pub fn tools(mut self, tools: &[Tool]) -> Self {
        for g in tools
            .iter()
            .filter(|t| !t.is_deferred())
            .filter_map(Tool::guideline)
        {
            if !self.guidelines.iter().any(|have| have == g) {
                self.guidelines.push(g.to_string());
            }
        }
        self
    }

    /// A bullet under "Guidelines", for advice that belongs to no one tool.
    pub fn guideline(mut self, text: impl Into<String>) -> Self {
        self.guidelines.push(text.into());
        self
    }

    /// Several [`guideline`](Self::guideline)s.
    pub fn guidelines<S: Into<String>>(mut self, texts: impl IntoIterator<Item = S>) -> Self {
        self.guidelines.extend(texts.into_iter().map(Into::into));
        self
    }

    /// The files [`ContextFiles::load`] found, as [`section`] renders them.
    pub fn context_files(mut self, files: &[ContextFile]) -> Self {
        self.context = section(files);
        self
    }

    /// Any other text, after the context files (e.g. a prompt the user appends, or the catalog
    /// of `manifatura-skills`). Empty text adds nothing.
    pub fn section(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.trim().is_empty() {
            self.sections.push(text);
        }
        self
    }

    /// `Working directory: …` at the end.
    pub fn cwd(self, cwd: &Path) -> Self {
        self.fact("Working directory", cwd.display().to_string())
    }

    /// `Current date: …` at the end.
    pub fn date(self, date: impl Into<String>) -> Self {
        self.fact("Current date", date)
    }

    /// Another `label: value` line at the end (e.g. `Environment`, `Platform`).
    pub fn fact(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.push((label.into(), value.into()));
        self
    }

    /// The prompt: the base, the guidelines, the context files, the sections, then the facts.
    pub fn build(&self) -> String {
        let mut out = self.base.trim_end().to_string();
        out.push('\n');
        if !self.guidelines.is_empty() {
            out.push_str("\nGuidelines:\n");
            for g in &self.guidelines {
                out.push_str(&format!("- {}\n", g.trim()));
            }
        }
        if !self.context.is_empty() {
            out.push('\n');
            out.push_str(&self.context);
        }
        for s in &self.sections {
            out.push('\n');
            out.push_str(s.trim());
            out.push('\n');
        }
        if !self.environment.is_empty() {
            out.push('\n');
            for (label, value) in &self.environment {
                out.push_str(&format!("{label}: {value}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_whole_characters() {
        assert_eq!(truncate("héllo".into(), 2), "h\n[truncated]");
        assert_eq!(truncate("short".into(), 64), "short");
    }

    #[test]
    fn the_prompt_comes_in_order_and_skips_empty_parts() {
        let files = [ContextFile {
            path: "/r/AGENTS.md".into(),
            text: "Use tabs.\n".into(),
        }];
        let prompt = SystemPrompt::new("Base.")
            .guideline("Read first.")
            .context_files(&files)
            .section("Extra.")
            .cwd(Path::new("/r"))
            .date("2026-11-23")
            .build();
        assert_eq!(
            prompt,
            "Base.\n\nGuidelines:\n- Read first.\n\n# Project context\n\nInstructions from \
             context files. Follow them; more specific (deeper) files take precedence.\n\n\
             ## /r/AGENTS.md\n\nUse tabs.\n\nExtra.\n\nWorking directory: /r\nCurrent date: \
             2026-11-23\n"
        );
        assert_eq!(SystemPrompt::new("Base.").build(), "Base.\n");
    }

    #[test]
    fn guidelines_come_from_the_offered_tools_once_each() {
        let tool = |name: &str, guideline: &str| {
            Tool::new(name, "x")
                .guideline(guideline)
                .run_json(serde_json::json!({"type": "object"}), |_, _| async {
                    Ok("")
                })
        };
        let tools = [
            tool("a", "Use a."),
            tool("b", "Use a."),
            tool("hidden", "Never shown.").with_deferred(true),
            Tool::new("plain", "x").run_json(serde_json::json!({}), |_, _| async { Ok("") }),
            tool("c", "Use c."),
        ];
        let prompt = SystemPrompt::new("Base.").tools(&tools).build();
        assert_eq!(prompt, "Base.\n\nGuidelines:\n- Use a.\n- Use c.\n");
    }
}
