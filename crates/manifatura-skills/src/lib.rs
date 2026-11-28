//! [Agent Skills](https://agentskills.io) for manifatura: folders of instructions an agent loads
//! when a task calls for them.
//!
//! A skill is a directory with a `SKILL.md`: YAML frontmatter with a `name` and a
//! `description`, then Markdown instructions, and any files those instructions point to
//! (`scripts/`, `references/`, `assets/`). Skills load in three steps, so that only what a task
//! needs reaches the context:
//!
//! 1. the catalog, each skill's name and description, sits in the system prompt
//!    ([`SystemPromptExt::skills`]);
//! 2. the model calls the [`SKILL_TOOL`] tool ([`SkillSet::tool`]) for a skill's instructions,
//!    or the app injects them when the user asks for a skill ([`SkillSet::activation`]);
//! 3. the instructions name the skill's other files, which the model reads with its own tools.
//!
//! [`Skills`] says where to look. Everything is read through the session's [`Env`]:
//!
//! ```no_run
//! # async fn demo(env: &dyn manifatura_agent::Env, mut tools: Vec<manifatura_agent::Tool>) {
//! use manifatura_agent::Agent;
//! use manifatura_kit::compaction;
//! use manifatura_kit::context::SystemPrompt;
//! use manifatura_skills::{Skills, SystemPromptExt};
//!
//! // `.agents/skills` from the repository root down to the working directory, then `~/.agents/skills`.
//! let skills = Skills::agents("/home/me").load(env).await;
//! for warning in skills.warnings() {
//!     eprintln!("{warning}");
//! }
//! // The skill tool joins the others, so the prompt carries its guideline.
//! tools.extend(skills.tool());
//! let instructions = SystemPrompt::new("You are a coding agent.")
//!     .tools(&tools)
//!     .skills(&skills)
//!     .build();
//! let agent = Agent::new(instructions)
//!     .tools(tools)
//!     // The skill tool keeps its results, so trimming leaves the instructions whole.
//!     .hooks(compaction::trim_results(8_000, 2_000));
//! # }
//! ```

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use manifatura_agent::{Env, FileKind, Tool, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use manifatura_kit::context::{ContextFiles, SystemPrompt};

/// The name of the tool that loads a skill's instructions.
pub const SKILL_TOOL: &str = "skill";

/// The file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";

/// The skills directory of the [Agent Skills](https://agentskills.io) convention, in a project
/// and in the home directory.
pub const AGENTS_DIR: &str = ".agents/skills";

/// The default limit for one skill's instructions, in bytes.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// The most files of a skill listed when it is loaded.
const MAX_RESOURCES: usize = 50;

/// Where a skill came from. A skill hides one of the same name from an earlier source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Source {
    /// Built into the app ([`Skills::builtin`]).
    Builtin,
    /// A directory the app names ([`Skills::dir`]), such as one in the user's home.
    User,
    /// A project's skills directory ([`Skills::project`]).
    Project,
}

/// One skill: its catalog entry and its instructions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// What the model and the user call it by.
    pub name: String,
    /// What it does and when to use it, as the catalog shows it.
    pub description: String,
    /// The instructions: `SKILL.md` without its frontmatter.
    pub body: String,
    /// The skill's directory; `None` for a built-in skill.
    pub dir: Option<PathBuf>,
    /// Where it came from.
    pub source: Source,
}

impl Skill {
    /// A skill from the text of a `SKILL.md`, as [`Skills::builtin`] takes it. It needs a
    /// `name` and a `description`.
    pub fn parse(text: &str) -> Result<Skill, String> {
        let (front, body) = split(text)?;
        let name = front.name.ok_or("the frontmatter has no name")?;
        let warnings = check_name(&name);
        if let Some(w) = warnings.first() {
            return Err(w.clone());
        }
        Ok(Skill {
            name,
            description: front.description,
            body,
            dir: None,
            source: Source::Builtin,
        })
    }

    /// The `SKILL.md` it was read from; `None` for a built-in skill.
    pub fn path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(SKILL_FILE))
    }
}

/// Where to look for skills.
///
/// Later sources take precedence: built-in skills, then the [`dir`](Self::dir)s in order, then
/// the [`project`](Self::project) directories from the repository root down to the working
/// directory. A skill hides an earlier one of the same name, with a warning.
#[derive(Clone, Debug)]
pub struct Skills {
    builtin: Vec<Skill>,
    dirs: Vec<PathBuf>,
    project: Vec<PathBuf>,
    home: Option<PathBuf>,
    max_bytes: usize,
}

impl Default for Skills {
    fn default() -> Self {
        Self {
            builtin: Vec::new(),
            dirs: Vec::new(),
            project: Vec::new(),
            home: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl Skills {
    /// No sources yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The convention's locations: [`AGENTS_DIR`] in the project, then under `home`, which also
    /// bounds the walk up from the working directory.
    pub fn agents(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self::new()
            .dir(home.join(AGENTS_DIR))
            .project(AGENTS_DIR)
            .home(home)
    }

    /// A skill built into the app, with the lowest precedence.
    pub fn builtin(mut self, skill: Skill) -> Self {
        self.builtin.push(skill);
        self
    }

    /// A directory of skills, one per subdirectory (e.g. `~/.agents/skills`).
    pub fn dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.dirs.push(path.into());
        self
    }

    /// A skills directory relative to each project directory (e.g. [`AGENTS_DIR`]), searched
    /// from the repository root down to the working directory, as
    /// [`ContextFiles`] searches for `AGENTS.md`. Skills of deeper directories win.
    ///
    /// Project skills come with the repository, which may not be trusted; leave them out
    /// unless the user trusts it.
    pub fn project(mut self, relative: impl Into<PathBuf>) -> Self {
        self.project.push(relative.into());
        self
    }

    /// Where the walk up from the working directory stops when no repository is found first.
    pub fn home(mut self, path: impl Into<PathBuf>) -> Self {
        self.home = Some(path.into());
        self
    }

    /// The most text kept from one skill's instructions, in bytes (default
    /// [`DEFAULT_MAX_BYTES`]).
    pub fn max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// The skills found. A skill without a description, or whose frontmatter cannot be read,
    /// is left out with a warning; one whose name breaks the convention is loaded with one.
    pub async fn load(&self, env: &dyn Env) -> SkillSet {
        let mut set = SkillSet::default();
        for skill in &self.builtin {
            set.add(skill.clone());
        }
        for dir in &self.dirs {
            self.load_dir(env, &env.resolve(dir), Source::User, &mut set)
                .await;
        }
        if !self.project.is_empty() {
            let mut walk = ContextFiles::new();
            if let Some(home) = &self.home {
                walk = walk.home(home);
            }
            for dir in walk.dirs(env).await {
                for relative in &self.project {
                    self.load_dir(env, &dir.join(relative), Source::Project, &mut set)
                        .await;
                }
            }
        }
        set.skills.sort_by(|a, b| a.name.cmp(&b.name));
        set
    }

    async fn load_dir(&self, env: &dyn Env, dir: &Path, source: Source, set: &mut SkillSet) {
        let Ok(mut entries) = env.list(dir).await else {
            return;
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in entries {
            if entry.name.starts_with('.')
                || !matches!(entry.kind, FileKind::Dir | FileKind::Symlink)
            {
                continue;
            }
            let skill_dir = dir.join(&entry.name);
            let path = skill_dir.join(SKILL_FILE);
            if !matches!(env.stat(&path).await, Ok(Some(s)) if s.kind == FileKind::File) {
                continue;
            }
            let text = match env.read(&path).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) => {
                    set.warn(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let (front, body) = match split(&text) {
                Ok(parts) => parts,
                Err(e) => {
                    set.warn(format!("{}: {e}; skipped", path.display()));
                    continue;
                }
            };
            let name = match front.name {
                Some(name) => name,
                None => {
                    set.warn(format!(
                        "{}: the frontmatter has no name; using the directory's",
                        path.display()
                    ));
                    entry.name.clone()
                }
            };
            for w in check_name(&name) {
                set.warn(format!("{}: {w}", path.display()));
            }
            if name != entry.name {
                set.warn(format!(
                    "{}: the name {name:?} does not match the directory {:?}",
                    path.display(),
                    entry.name
                ));
            }
            set.add(Skill {
                name,
                description: front.description,
                body: truncate(body, self.max_bytes),
                dir: Some(skill_dir),
                source,
            });
        }
    }
}

/// Who sees a skill. Absent from [`SkillSet::with_visibility`], a skill is [`On`](Self::On).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Visibility {
    /// The catalog lists it with its description; the model and the user can start it.
    #[default]
    On,
    /// The catalog lists only its name, to save context; the model and the user can start it.
    NameOnly,
    /// Left out of the catalog and the tool; only the user can start it.
    UserOnly,
    /// Left out everywhere.
    Off,
}

impl Visibility {
    /// The next state, as a picker cycles them: on, name-only, user-only, off, on.
    pub fn next(self) -> Self {
        match self {
            Visibility::On => Visibility::NameOnly,
            Visibility::NameOnly => Visibility::UserOnly,
            Visibility::UserOnly => Visibility::Off,
            Visibility::Off => Visibility::On,
        }
    }

    /// Its name, as written in config: `on`, `name-only`, `user-only` or `off`.
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::On => "on",
            Visibility::NameOnly => "name-only",
            Visibility::UserOnly => "user-only",
            Visibility::Off => "off",
        }
    }

    /// Whether the model is told of it and may load it.
    pub fn for_model(self) -> bool {
        matches!(self, Visibility::On | Visibility::NameOnly)
    }
}

/// The skills [`Skills::load`] found, and what it had to say about them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillSet {
    skills: Vec<Skill>,
    warnings: Vec<String>,
    visibility: BTreeMap<String, Visibility>,
}

impl SkillSet {
    /// A set of these skills, e.g. built-in ones only. Later skills hide earlier ones of the
    /// same name.
    pub fn of(skills: impl IntoIterator<Item = Skill>) -> Self {
        let mut set = Self::default();
        for skill in skills {
            set.add(skill);
        }
        set.skills.sort_by(|a, b| a.name.cmp(&b.name));
        set
    }

    fn add(&mut self, skill: Skill) {
        if let Some(have) = self.skills.iter_mut().find(|s| s.name == skill.name) {
            let place = |s: &Skill| match s.path() {
                Some(p) => p.display().to_string(),
                None => "the built-in skill".into(),
            };
            self.warnings.push(format!(
                "skill {}: {} hides {}",
                skill.name,
                place(&skill),
                place(have)
            ));
            *have = skill;
        } else {
            self.skills.push(skill);
        }
    }

    /// The same skills, each seen as `visibility` says (by name; absent ones are on).
    pub fn with_visibility(mut self, visibility: BTreeMap<String, Visibility>) -> Self {
        self.visibility = visibility;
        self.visibility.retain(|_, v| *v != Visibility::On);
        self
    }

    /// Who sees the skill called `name`.
    pub fn visibility(&self, name: &str) -> Visibility {
        self.visibility.get(name).copied().unwrap_or_default()
    }

    /// The skills the user may start: all but those that are off.
    pub fn for_user(&self) -> impl Iterator<Item = &Skill> {
        self.skills
            .iter()
            .filter(|s| self.visibility(&s.name) != Visibility::Off)
    }

    fn for_model(&self) -> impl Iterator<Item = &Skill> {
        self.skills
            .iter()
            .filter(|s| self.visibility(&s.name).for_model())
    }

    /// The tokens its catalog entry costs the prompt, as [`section`](Self::section) writes it
    /// when the skill is on: about four characters a token.
    pub fn catalog_tokens(skill: &Skill) -> usize {
        format!("- {}: {}\n", skill.name, one_line(&skill.description))
            .len()
            .div_ceil(4)
    }

    fn warn(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    /// The skills, by name.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// The skill called `name`.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Skills that were skipped or hidden, and names that break the convention.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The catalog as a prompt section: a heading, then each skill's name and description.
    /// Empty when there are none. [`SystemPromptExt::skills`] adds it to a prompt; the
    /// [`SKILL_TOOL`] tool's guideline says when to use one.
    /// Skills that are [name-only](Visibility::NameOnly) show their name alone; those the
    /// model may not load are left out.
    pub fn section(&self) -> String {
        if self.for_model().next().is_none() {
            return String::new();
        }
        let mut out = format!("# Skills\n\nLoad these with the {SKILL_TOOL} tool:\n\n");
        for s in self.for_model() {
            match self.visibility(&s.name) {
                Visibility::NameOnly => out.push_str(&format!("- {}\n", s.name)),
                _ => out.push_str(&format!("- {}: {}\n", s.name, one_line(&s.description))),
            }
        }
        out
    }

    /// The [`SKILL_TOOL`] tool, which returns a skill's instructions by name; `None` when
    /// there are no skills. The names are an enum in its schema, so the model cannot ask for
    /// one that does not exist. It [keeps its results](manifatura_agent::ToolBuilder::keep_results),
    /// as a skill's instructions apply for the rest of the task.
    pub fn tool(&self) -> Option<Tool> {
        let names: Vec<&str> = self.for_model().map(|s| s.name.as_str()).collect();
        if names.is_empty() {
            return None;
        }
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "enum": names, "description": "The skill to load."}
            },
            "required": ["name"],
        });
        let set = self.clone();
        Some(
            Tool::new(
                SKILL_TOOL,
                "Load a skill's instructions by name. Call it when a task matches a skill in \
                 the Skills section, then follow what it returns.",
            )
            .guideline(format!(
                "When a task matches a skill in the Skills section, load it with `{SKILL_TOOL}` \
                 before you start, then follow its instructions."
            ))
            .keep_results()
            .run_json(schema, move |input: Value, cx| {
                let set = set.clone();
                async move {
                    let name = input
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let known = set.visibility(name).for_model();
                    match set
                        .activation(name, cx.env().as_ref())
                        .await
                        .filter(|_| known)
                    {
                        Some(text) => Ok(text),
                        None => Err(ToolError::msg(format!(
                            "no skill {name:?}; the skills are: {}",
                            set.for_model()
                                .map(|s| s.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))),
                    }
                }
            }),
        )
    }

    /// The skills' names.
    pub fn names(&self) -> Vec<&str> {
        self.skills.iter().map(|s| s.name.as_str()).collect()
    }

    /// What loading the skill `name` gives the model: its instructions in a `<skill_content>`
    /// block with its directory and a list of its other files, which are not read. The
    /// [`SKILL_TOOL`] tool returns it; an app that lets the user ask for a skill (`/name`) sends
    /// it as part of the user's message. `None` for an unknown name.
    pub async fn activation(&self, name: &str, env: &dyn Env) -> Option<String> {
        let skill = self.get(name)?;
        let mut out = format!(
            "<skill_content name=\"{}\">\n{}\n",
            skill.name,
            skill.body.trim()
        );
        if let Some(dir) = &skill.dir {
            out.push_str(&format!(
                "\nSkill directory: {}\nRelative paths in this skill are relative to the skill \
                 directory.\n",
                dir.display()
            ));
            let files = resources(env, dir).await;
            if !files.is_empty() {
                out.push_str("<skill_resources>\n");
                for f in &files {
                    out.push_str(&format!("<file>{f}</file>\n"));
                }
                out.push_str("</skill_resources>\n");
            }
        }
        out.push_str("</skill_content>");
        Some(out)
    }
}

/// Adds [`skills`](Self::skills) to kit's [`SystemPrompt`], so the catalog goes in like the
/// other parts of the prompt. With the `manifatura` crate, `manifatura::prelude` brings it in
/// when the `skills` feature is on.
pub trait SystemPromptExt {
    /// The catalog of these skills, after the context files, as [`SkillSet::section`] renders
    /// it. Nothing when the set is empty. Pass the [`SKILL_TOOL`] tool to
    /// [`SystemPrompt::tools`] as well, for its guideline.
    fn skills(self, skills: &SkillSet) -> Self;
}

impl SystemPromptExt for SystemPrompt {
    fn skills(self, skills: &SkillSet) -> Self {
        self.section(skills.section())
    }
}

/// The frontmatter fields this module reads; the others (`license`, `metadata`, …) are
/// allowed and ignored.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    #[serde(default)]
    description: String,
}

/// The frontmatter and the body of a `SKILL.md`.
fn split(text: &str) -> Result<(Frontmatter, String), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---")
        .filter(|r| r.starts_with('\n') || r.starts_with("\r\n"))
        .ok_or("no frontmatter")?;
    let (yaml, body) = match rest.find("\n---") {
        Some(end) => {
            let after = &rest[end + 4..];
            let body = after.split_once('\n').map_or("", |(_, b)| b);
            (&rest[..end], body)
        }
        None => return Err("the frontmatter is not closed".into()),
    };
    let front: Frontmatter = serde_norway::from_str(yaml)
        .or_else(|_| serde_norway::from_str(&quote_values(yaml)))
        .map_err(|e| format!("could not read the frontmatter: {e}"))?;
    if front.description.trim().is_empty() {
        return Err("the frontmatter has no description".into());
    }
    Ok((front, body.to_string()))
}

/// The frontmatter with each top-level value that holds `: ` single-quoted, the commonest
/// mistake in hand-written skills (`description: Use when: …`).
fn quote_values(yaml: &str) -> String {
    yaml.lines()
        .map(|line| match line.split_once(": ") {
            Some((key, value))
                if !key.starts_with([' ', '\t', '#', '-'])
                    && !value.starts_with(['\'', '"', '|', '>', '[', '{'])
                    && value.contains(": ") =>
            {
                format!("{key}: '{}'", value.replace('\'', "''"))
            }
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What is wrong with a name, by the convention: 1 to 64 lowercase letters, digits and
/// hyphens, not starting or ending with a hyphen, without two in a row.
fn check_name(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if name.is_empty() {
        out.push("the name is empty".into());
    }
    if name.chars().count() > 64 {
        out.push(format!("the name {name:?} is longer than 64 characters"));
    }
    let chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !chars || name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        out.push(format!(
            "the name {name:?} should be lowercase letters, digits and single hyphens"
        ));
    }
    out
}

/// A skill's other files, relative to its directory: two levels deep, hidden files left out,
/// at most [`MAX_RESOURCES`].
async fn resources(env: &dyn Env, dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = vec![(dir.to_path_buf(), String::new(), 0)];
    while let Some((path, prefix, depth)) = pending.pop() {
        let Ok(mut entries) = env.list(&path).await else {
            continue;
        };
        entries.sort_by(|a, b| b.name.cmp(&a.name));
        for e in entries {
            if e.name.starts_with('.') || (depth == 0 && e.name == SKILL_FILE) {
                continue;
            }
            let shown = format!("{prefix}{}", e.name);
            match e.kind {
                FileKind::Dir if depth < 1 => {
                    pending.push((path.join(&e.name), format!("{shown}/"), depth + 1))
                }
                FileKind::Dir => {}
                _ => out.push(shown),
            }
        }
    }
    out.sort();
    if out.len() > MAX_RESOURCES {
        let more = out.len() - MAX_RESOURCES;
        out.truncate(MAX_RESOURCES);
        out.push(format!("… and {more} more"));
    }
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let cut = text.floor_char_boundary(max);
        text.truncate(cut);
        text.push_str("\n[truncated]");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL: &str = "---\nname: tour\ndescription: Explain a codebase.\nlicense: MIT\n---\n# Tour\n\nRead the README.\n";

    #[test]
    fn a_skill_is_its_frontmatter_and_body() {
        let s = Skill::parse(SKILL).unwrap();
        assert_eq!(
            (s.name.as_str(), s.description.as_str()),
            ("tour", "Explain a codebase.")
        );
        assert_eq!(s.body, "# Tour\n\nRead the README.\n");
        assert_eq!(s.source, Source::Builtin);
    }

    #[test]
    fn an_unquoted_colon_is_forgiven() {
        let text = "---\nname: x\ndescription: Use when: the user asks\n---\nbody";
        assert_eq!(
            Skill::parse(text).unwrap().description,
            "Use when: the user asks"
        );
    }

    #[test]
    fn a_skill_needs_a_description_and_a_good_name() {
        assert!(Skill::parse("---\nname: x\n---\nbody").is_err());
        assert!(Skill::parse("no frontmatter").is_err());
        assert!(Skill::parse("---\nname: Bad Name\ndescription: d\n---\n").is_err());
        assert!(check_name("pdf-tools").is_empty());
        assert_eq!(check_name("-x").len(), 1);
        assert_eq!(check_name(&"a".repeat(65)).len(), 1);
    }

    #[test]
    fn the_catalog_lists_each_skill_on_one_line() {
        let set = SkillSet::of([Skill::parse(SKILL).unwrap()]);
        assert!(
            set.section()
                == "# Skills\n\nLoad these with the skill tool:\n\n- tour: Explain a codebase.\n",
            "{}",
            set.section()
        );
        assert_eq!(SkillSet::default().section(), "");
        assert!(SkillSet::default().tool().is_none());
    }

    #[test]
    fn quoting_leaves_good_yaml_alone() {
        let yaml = "name: x\ndescription: \"a: b\"\nmetadata:\n  k: v: w";
        assert_eq!(quote_values(yaml), yaml);
    }
}
