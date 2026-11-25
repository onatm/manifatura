//! `grep` and `glob`. Both walk the local filesystem in-process and respect
//! `.gitignore`, `.ignore` and hidden-file rules the way ripgrep does (but do search dotfiles,
//! skipping `.git`). In an env that is not local (a remote one) they walk through the env
//! instead, skipping `.git`, `node_modules`, `target`, `__pycache__` and `.venv`, with lower
//! limits: each directory and file is a round trip.

use std::path::{Path, PathBuf};

use manifatura_agent::{Env, FileKind, Tool, ToolCx, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::MAX_BYTES;

const MAX_FILE: u64 = 2 * 1024 * 1024;

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

#[derive(Deserialize, JsonSchema)]
struct GrepInput {
    /// A regular expression (Rust regex syntax), or plain text with `literal`.
    pattern: String,
    /// Directory or file to search (default: the workspace).
    path: Option<String>,
    /// Only search files whose path matches this glob, e.g. `*.rs` or `src/**/*.ts`.
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    /// Treat the pattern as plain text.
    #[serde(default)]
    literal: bool,
    /// Maximum matching lines to return (default 100).
    limit: Option<usize>,
}

/// `grep`: matching lines across files, respecting `.gitignore`.
pub fn grep() -> Tool {
    Tool::new(
        "grep",
        "Search file contents for a pattern. Returns `path:line: text` for matching lines, \
         respecting .gitignore. Narrow with path and glob.",
    )
    .guideline("Search file contents with `grep`; it respects .gitignore.")
    .run(|i: GrepInput, cx: ToolCx| async move {
        let env = cx.env().clone();
        let root = env.cwd().to_path_buf();
        let start = crate::resolve(&*env, i.path.as_deref().unwrap_or("."));
        let mut m = Matches::new(&i)?;
        if !env.is_local() {
            let mut files = 0;
            for (path, kind) in walk_env(&*env, &start).await? {
                if kind != FileKind::File || !m.wants(&rel(&root, &path), &path) {
                    continue;
                }
                files += 1;
                if files > REMOTE_FILES {
                    m.more = true;
                    break;
                }
                if env
                    .stat(&path)
                    .await
                    .ok()
                    .flatten()
                    .is_none_or(|s| s.len > MAX_FILE)
                {
                    continue;
                }
                let Ok(bytes) = env.read(&path).await else {
                    continue;
                };
                if !m.scan(&rel(&root, &path), &bytes) {
                    break;
                }
            }
            return Ok(m.finish());
        }
        tokio::task::spawn_blocking(move || {
            for entry in walker(&start).flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let path = entry.path();
                let shown = rel(&root, path);
                if !m.wants(&shown, path) {
                    continue;
                }
                if entry.metadata().map(|m| m.len() > MAX_FILE).unwrap_or(true) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                if !m.scan(&shown, &bytes) {
                    break;
                }
            }
            Ok::<_, ToolError>(m.finish())
        })
        .await
        .map_err(ToolError::msg)?
    })
}

/// Files read at most by a grep through a non-local env.
const REMOTE_FILES: usize = 2_000;
/// Entries listed at most by a walk through a non-local env.
const REMOTE_ENTRIES: usize = 20_000;
const REMOTE_SKIP: &[&str] = &[".git", "node_modules", "target", "__pycache__", ".venv"];

/// Every entry under `start` (not `start` itself), through the env.
async fn walk_env(env: &dyn Env, start: &Path) -> Result<Vec<(PathBuf, FileKind)>, ToolError> {
    let mut out = Vec::new();
    let mut dirs = vec![start.to_path_buf()];
    if env
        .stat(start)
        .await?
        .is_some_and(|s| s.kind == FileKind::File)
    {
        return Ok(vec![(start.to_path_buf(), FileKind::File)]);
    }
    while let Some(dir) = dirs.pop() {
        let mut entries = env.list(&dir).await?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in entries {
            if out.len() >= REMOTE_ENTRIES {
                return Ok(out);
            }
            let p = dir.join(&e.name);
            if e.kind == FileKind::Dir {
                if REMOTE_SKIP.contains(&e.name.as_str()) {
                    continue;
                }
                dirs.push(p.clone());
            }
            out.push((p, e.kind));
        }
    }
    Ok(out)
}

/// Matching lines, collected across files.
struct Matches {
    re: regex::Regex,
    glob: Option<globset::GlobMatcher>,
    limit: usize,
    out: String,
    count: usize,
    more: bool,
}

impl Matches {
    fn new(i: &GrepInput) -> Result<Self, ToolError> {
        let pattern = if i.literal {
            regex::escape(&i.pattern)
        } else {
            i.pattern.clone()
        };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(i.ignore_case)
            .build()
            .map_err(|e| ToolError::msg(format!("invalid pattern: {e}")))?;
        let glob = match &i.glob {
            Some(g) => Some(compile_glob(g)?),
            None => None,
        };
        Ok(Self {
            re,
            glob,
            limit: i.limit.unwrap_or(100).clamp(1, 1000),
            out: String::new(),
            count: 0,
            more: false,
        })
    }

    fn wants(&self, shown: &str, path: &Path) -> bool {
        match &self.glob {
            Some(g) => g.is_match(shown) || path.file_name().is_some_and(|n| g.is_match(n)),
            None => true,
        }
    }

    /// Adds a file's matching lines; `false` once the limit is reached.
    fn scan(&mut self, shown: &str, bytes: &[u8]) -> bool {
        if crate::truncate::is_binary(bytes) {
            return true;
        }
        let text = String::from_utf8_lossy(bytes);
        for (n, line) in text.lines().enumerate() {
            if !self.re.is_match(line) {
                continue;
            }
            if self.count == self.limit || self.out.len() > MAX_BYTES {
                self.more = true;
                return false;
            }
            let line: String = line.trim_end().chars().take(300).collect();
            self.out.push_str(&format!("{shown}:{}: {line}\n", n + 1));
            self.count += 1;
        }
        true
    }

    fn finish(mut self) -> String {
        if self.count == 0 {
            return if self.more {
                "No matches in the files searched; narrow the search with path or glob.".into()
            } else {
                "No matches.".into()
            };
        }
        if self.more {
            self.out.push_str(&format!(
                "[stopped after {} matches; narrow the search with path or glob]",
                self.count
            ));
        }
        self.out
    }
}

fn compile_glob(g: &str) -> Result<globset::GlobMatcher, ToolError> {
    globset::GlobBuilder::new(g)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| ToolError::msg(format!("invalid glob: {e}")))
}

#[derive(Deserialize, JsonSchema)]
struct GlobInput {
    /// Glob relative to the search directory, e.g. `**/*.rs` or `docs/*.md`.
    pattern: String,
    /// Directory to search (default: the workspace).
    path: Option<String>,
}

/// `glob`: paths matching a pattern, most recently modified first, respecting `.gitignore`.
pub fn glob() -> Tool {
    Tool::new(
        "glob",
        "Find files by path pattern, respecting .gitignore. Returns matching paths, most \
         recently modified first.",
    )
    .guideline("Find files by pattern with `glob`; it respects .gitignore.")
    .run(|i: GlobInput, cx: ToolCx| async move {
        let env = cx.env().clone();
        let root = env.cwd().to_path_buf();
        let start = crate::resolve(&*env, i.path.as_deref().unwrap_or("."));
        let m = compile_glob(&i.pattern)?;
        if !env.is_local() {
            let mut hits: Vec<(u128, PathBuf)> = Vec::new();
            for (p, _) in walk_env(&*env, &start).await? {
                if !m.is_match(p.strip_prefix(&start).unwrap_or(&p)) {
                    continue;
                }
                hits.push((0, p));
            }
            // Modification times cost a round trip each, so a long list stays in path order,
            // and says so.
            if hits.len() > LISTED {
                let total = hits.len();
                let shown = hits.into_iter().take(LISTED).map(|(_, p)| p).collect();
                return Ok(listing(&root, shown, total, "in path order; "));
            }
            for h in &mut hits {
                h.0 = env
                    .stat(&h.1)
                    .await
                    .ok()
                    .flatten()
                    .map(|s| s.modified)
                    .unwrap_or(0);
            }
            hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let total = hits.len();
            return Ok(listing(
                &root,
                hits.into_iter().map(|(_, p)| p).collect(),
                total,
                "",
            ));
        }
        tokio::task::spawn_blocking(move || {
            let mut hits: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
            for entry in walker(&start).flatten() {
                let p = entry.path();
                if p == start || !m.is_match(p.strip_prefix(&start).unwrap_or(p)) {
                    continue;
                }
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::UNIX_EPOCH);
                hits.push((mtime, p.to_path_buf()));
            }
            hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let total = hits.len();
            let shown = hits.into_iter().take(LISTED).map(|(_, p)| p).collect();
            Ok::<_, ToolError>(listing(&root, shown, total, ""))
        })
        .await
        .map_err(ToolError::msg)?
    })
}

/// Paths `glob` lists at most.
const LISTED: usize = 500;

/// The first [`LISTED`] of `total` matches, and how many more there are.
fn listing(root: &Path, shown: Vec<PathBuf>, total: usize, order: &str) -> String {
    if shown.is_empty() {
        return "No files matched.".to_string();
    }
    let mut out: Vec<String> = shown.iter().map(|p| rel(root, p)).collect();
    if total > shown.len() {
        out.push(format!(
            "[{} more not shown; {order}use a narrower pattern]",
            total - shown.len()
        ));
    }
    out.join("\n")
}
