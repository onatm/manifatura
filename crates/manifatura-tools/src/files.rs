//! `read` and `edit`, sharing a tracker of what each session has seen.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use manifatura_agent::{Env, FileKind, SessionId, Stat, Tool, ToolCx, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{MAX_BYTES, MAX_LINES, clip_line, is_binary};

/// What each session last saw of each file, so `edit` can refuse to clobber
/// changes the model has not read; plus one lock per path so parallel edits serialise.
#[derive(Default)]
pub struct FileTracker {
    seen: Mutex<HashMap<(SessionId, PathBuf), (u128, u64)>>,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for FileTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seen = self.seen.lock().unwrap().len();
        f.debug_struct("FileTracker")
            .field("seen", &seen)
            .finish_non_exhaustive()
    }
}

enum Seen {
    Never,
    Current,
    Changed,
}

impl FileTracker {
    pub(crate) fn record(&self, s: &SessionId, path: &Path, stat: &Stat) {
        self.seen
            .lock()
            .unwrap()
            .insert((s.clone(), path.to_path_buf()), (stat.modified, stat.len));
    }

    fn check(&self, s: &SessionId, path: &Path, stat: &Stat) -> Seen {
        match self
            .seen
            .lock()
            .unwrap()
            .get(&(s.clone(), path.to_path_buf()))
        {
            None => Seen::Never,
            Some(&(m, l)) if m == stat.modified && l == stat.len => Seen::Current,
            Some(_) => Seen::Changed,
        }
    }

    pub(crate) fn lock(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_default()
            .clone()
    }
}

pub(crate) fn display(env: &dyn Env, path: &Path) -> String {
    path.strip_prefix(env.cwd())
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Small models often guess paths; say where paths resolve and how to look.
pub(crate) fn not_found(env: &dyn Env, shown: &str, extra: &str) -> ToolError {
    ToolError::msg(format!(
        "file not found: {shown}. Relative paths resolve against {}; use glob to find \
         files.{extra}",
        env.cwd().display()
    ))
}

pub(crate) async fn stat_file(
    env: &dyn Env,
    path: &Path,
    shown: &str,
) -> Result<Option<Stat>, ToolError> {
    match env.stat(path).await? {
        Some(s) if s.kind == FileKind::Dir => Err(ToolError::msg(format!(
            "{shown} is a directory; use glob (e.g. `{shown}/*`) to list it"
        ))),
        s => Ok(s),
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReadInput {
    /// File path, relative to the workspace or absolute.
    path: String,
    /// First line to show, 1-based.
    offset: Option<usize>,
    /// Maximum number of lines to show (default 2000).
    limit: Option<usize>,
}

/// `read`: a text file, line-numbered, a page at a time.
pub fn read(tracker: Arc<FileTracker>) -> Tool {
    Tool::new(
        "read",
        "Read a text file. Output is line-numbered and capped at 2000 lines or 50 KB; use offset \
         and limit to page through larger files. Read a file before editing it.",
    )
    .guideline("Read files with `read` (not cat); page through large files with offset and limit.")
    .run(move |i: ReadInput, cx: ToolCx| {
        let tracker = tracker.clone();
        async move {
            let env = cx.env().as_ref();
            let path = crate::resolve(env, &i.path);
            let shown = display(env, &path);
            let Some(stat) = stat_file(env, &path, &shown).await? else {
                return Err(not_found(env, &shown, ""));
            };
            let bytes = env.read(&path).await?;
            tracker.record(cx.session_id(), &path, &stat);
            if is_binary(&bytes) {
                return Ok(format!(
                    "{shown} is a binary file ({} bytes); not shown.",
                    bytes.len()
                ));
            }
            let text = String::from_utf8_lossy(&bytes);
            if text.is_empty() {
                return Ok(format!("{shown} is empty."));
            }
            let lines: Vec<&str> = text.lines().collect();
            let start = i.offset.unwrap_or(1).max(1) - 1;
            if start >= lines.len() {
                return Err(ToolError::msg(format!(
                    "offset {} is past the end of {shown} ({} lines)",
                    start + 1,
                    lines.len()
                )));
            }
            let limit = i.limit.unwrap_or(MAX_LINES).clamp(1, MAX_LINES);
            let mut out = String::new();
            let mut end = start;
            for (n, line) in lines.iter().enumerate().skip(start).take(limit) {
                let row = format!("{:>6}\t{}\n", n + 1, clip_line(line));
                if out.len() + row.len() > MAX_BYTES && end > start {
                    break;
                }
                out.push_str(&row);
                end = n + 1;
            }
            if end < lines.len() {
                out.push_str(&format!(
                    "\n[Showing lines {}-{end} of {}. Use offset={} to continue.]",
                    start + 1,
                    lines.len(),
                    end + 1
                ));
            }
            Ok(out)
        }
    })
}

#[derive(Deserialize, JsonSchema)]
struct EditInput {
    /// File path, relative to the workspace or absolute.
    path: String,
    /// Replacements, all matched against the original file. Each old_text must occur exactly
    /// once, and edits must not overlap. To create a file, give one edit with an empty
    /// old_text and the whole content as new_text.
    edits: Vec<Replacement>,
}

#[derive(Deserialize, JsonSchema)]
struct Replacement {
    /// Exact text to replace, including whitespace and indentation; empty to create a file.
    old_text: String,
    /// The replacement text.
    new_text: String,
}

/// `edit`: exact text replacements in a file, or a new file.
pub fn edit(tracker: Arc<FileTracker>) -> Tool {
    Tool::new(
        "edit",
        "Edit a file by exact text replacement. Each old_text must match exactly once in the \
         original file (include enough surrounding lines to make it unique) and edits must not \
         overlap. Several edits to one file go in one call. To create a new file (parent \
         directories included), give one edit with an empty old_text.",
    )
    .guideline("Change files with `edit`, giving exact text that occurs once; read the file first. Create new files with `edit` too, with an empty old_text.")
    .run(move |i: EditInput, cx: ToolCx| {
        let tracker = tracker.clone();
        async move {
            let env = cx.env().as_ref();
            let path = crate::resolve(env, &i.path);
            let shown = display(env, &path);
            let lock = tracker.lock(&path);
            let _guard = lock.lock().await;
            let Some(stat) = stat_file(env, &path, &shown).await? else {
                return create(env, &tracker, cx.session_id(), &path, &shown, &i.edits).await;
            };
            if let Seen::Changed = tracker.check(cx.session_id(), &path, &stat) {
                return Err(ToolError::msg(format!(
                    "{shown} changed since it was last read; read it again first"
                )));
            }
            let original = String::from_utf8(env.read(&path).await?)
                .map_err(|_| ToolError::msg(format!("{shown} is not valid UTF-8")))?;
            if i.edits.is_empty() {
                return Err(ToolError::msg("no edits given"));
            }
            let mut spans = Vec::new();
            for (k, e) in i.edits.iter().enumerate() {
                let n = k + 1;
                if e.old_text.is_empty() {
                    return Err(ToolError::msg(format!(
                        "edit {n}: old_text is empty, but {shown} already exists"
                    )));
                }
                let mut found = original.match_indices(&e.old_text);
                let Some((at, _)) = found.next() else {
                    return Err(ToolError::msg(format!(
                        "edit {n}: old_text not found in {shown}; check whitespace and \
                         indentation, or read the file again"
                    )));
                };
                let more = found.count();
                if more > 0 {
                    return Err(ToolError::msg(format!(
                        "edit {n}: old_text matches {} times in {shown}; include more \
                         surrounding text so it matches once",
                        more + 1
                    )));
                }
                spans.push((at, at + e.old_text.len(), k));
            }
            spans.sort();
            for w in spans.windows(2) {
                if w[1].0 < w[0].1 {
                    return Err(ToolError::msg(format!(
                        "edits {} and {} overlap; merge them into one edit",
                        w[0].2 + 1,
                        w[1].2 + 1
                    )));
                }
            }
            let mut updated = String::with_capacity(original.len());
            let mut last = 0;
            let mut summary = Vec::new();
            for &(start, end, k) in &spans {
                updated.push_str(&original[last..start]);
                updated.push_str(&i.edits[k].new_text);
                last = end;
                let line = original[..start].matches('\n').count() + 1;
                summary.push(diff_hunk(line, &i.edits[k].old_text, &i.edits[k].new_text));
            }
            updated.push_str(&original[last..]);
            env.write(&path, updated.as_bytes()).await?;
            if let Some(stat) = env.stat(&path).await? {
                tracker.record(cx.session_id(), &path, &stat);
            }
            Ok(format!(
                "Edited {shown} ({} edit{}).\n{}",
                spans.len(),
                if spans.len() == 1 { "" } else { "s" },
                summary.join("\n")
            ))
        }
    })
}

/// A new file, from one edit with an empty `old_text`.
async fn create(
    env: &dyn Env,
    tracker: &FileTracker,
    session: &SessionId,
    path: &Path,
    shown: &str,
    edits: &[Replacement],
) -> Result<String, ToolError> {
    let [Replacement { old_text, new_text }] = edits else {
        return Err(not_found(
            env,
            shown,
            " To create it, give one edit with an empty old_text.",
        ));
    };
    if !old_text.is_empty() {
        return Err(not_found(
            env,
            shown,
            " To create it, give one edit with an empty old_text.",
        ));
    }
    env.write(path, new_text.as_bytes()).await?;
    if let Some(stat) = env.stat(path).await? {
        tracker.record(session, path, &stat);
    }
    Ok(format!("Created {shown} ({} bytes).", new_text.len()))
}

/// A compact diff for one replacement, capped so a large edit does not flood the context.
fn diff_hunk(line: usize, old: &str, new: &str) -> String {
    const CAP: usize = 12;
    let mut out = format!("@@ line {line}");
    for (sign, text) in [('-', old), ('+', new)] {
        let lines: Vec<&str> = text.lines().collect();
        for l in lines.iter().take(CAP) {
            out.push_str(&format!("\n{sign} {}", clip_line(l)));
        }
        if lines.len() > CAP {
            out.push_str(&format!("\n{sign} … {} more lines", lines.len() - CAP));
        }
    }
    out
}
