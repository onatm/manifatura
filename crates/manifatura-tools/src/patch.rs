//! `apply_patch`: adds, deletes, updates and moves files with one patch, in the format of
//! OpenAI's Codex (`*** Begin Patch` … `*** End Patch`), which OpenAI's models are trained to
//! write. It is a text tool: on the Responses API the model writes the patch raw, following
//! [`GRAMMAR`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use manifatura_agent::{Env, Grammar, Tool, ToolCx, ToolError};

use crate::files::{FileTracker, display, not_found, stat_file};

/// The patch format as a Lark grammar, for providers that constrain the model's output.
pub const GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

const DESCRIPTION: &str = "\
Edit files with a patch. This is a FREEFORM tool, so do not wrap the patch in JSON. The patch \
is one envelope holding one or more file operations:

*** Begin Patch
*** Add File: <path>          (every following line starts with +: the new file's contents)
*** Delete File: <path>
*** Update File: <path>
*** Move to: <new path>       (optional, right after Update File: renames the file)
@@ <a line naming the class or function, to find the right place>
 <context line, unchanged; a leading space>
-<line to remove>
+<line to add>
*** End Patch

In an Update, each change starts with @@ (optionally followed by a line from the file that \
comes before it) and shows about 3 lines of unchanged context above and below the lines it \
changes. Paths are relative to the workspace. Add File replaces a file that exists. The whole \
patch is checked before any file changes: if one part fails, nothing is written.";

/// `apply_patch`: edits files with a patch in Codex's format (see [`GRAMMAR`]). It shares a
/// [`FileTracker`] with `read` and `edit`, so a file it changes counts as seen.
pub fn apply_patch(tracker: Arc<FileTracker>) -> Tool {
    Tool::new("apply_patch", DESCRIPTION)
        .guideline(
            "Change files with `apply_patch`: one patch can add, update, move and delete \
             several files. Give each change a few lines of context so it applies to the \
             right place.",
        )
        .sequential()
        .grammar(Grammar::lark(GRAMMAR))
        .run_text(move |text: String, cx: ToolCx| {
            let tracker = tracker.clone();
            async move { run(&text, &tracker, &cx).await }
        })
}

async fn run(text: &str, tracker: &FileTracker, cx: &ToolCx) -> Result<String, ToolError> {
    let hunks = parse(text).map_err(ToolError::msg)?;
    let env = cx.env().as_ref();

    // Every path the patch touches, locked in a fixed order.
    let mut paths = BTreeSet::new();
    for h in &hunks {
        paths.insert(crate::resolve(env, h.path()));
        if let Hunk::Update {
            move_to: Some(to), ..
        } = h
        {
            paths.insert(crate::resolve(env, to));
        }
    }
    let locks: Vec<_> = paths.iter().map(|p| tracker.lock(p)).collect();
    let mut guards = Vec::with_capacity(locks.len());
    for l in &locks {
        guards.push(l.lock().await);
    }

    // Work out every file's new contents first; nothing is written unless all of it applies.
    // `files` holds the contents as the patch leaves them (None: deleted), so a later hunk
    // sees what an earlier one did.
    let mut files: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    let mut report = Vec::new();
    for h in &hunks {
        let path = crate::resolve(env, h.path());
        let shown = display(env, &path);
        match h {
            Hunk::Add { contents, .. } => {
                stat_file(env, &path, &shown).await?; // refuses a directory
                files.insert(path, Some(contents.clone()));
                report.push(format!("A {shown}"));
            }
            Hunk::Delete { .. } => {
                current(env, &files, &path, &shown).await?;
                files.insert(path, None);
                report.push(format!("D {shown}"));
            }
            Hunk::Update {
                move_to, chunks, ..
            } => {
                let original = current(env, &files, &path, &shown).await?;
                let updated = apply_chunks(&original, chunks, &shown).map_err(ToolError::msg)?;
                match move_to {
                    Some(to) => {
                        let to = crate::resolve(env, to);
                        let to_shown = display(env, &to);
                        files.insert(path, None);
                        files.insert(to, Some(updated));
                        report.push(format!("M {to_shown}"));
                    }
                    None => {
                        files.insert(path, Some(updated));
                        report.push(format!("M {shown}"));
                    }
                }
            }
        }
    }

    for (path, contents) in &files {
        match contents {
            Some(text) => {
                env.write(path, text.as_bytes()).await?;
                if let Some(stat) = env.stat(path).await? {
                    tracker.record(cx.session_id(), path, &stat);
                }
            }
            None => {
                if env.stat(path).await?.is_some() {
                    env.remove(path, false).await?;
                }
            }
        }
    }
    drop(guards);
    Ok(format!(
        "Success. Updated the following files:\n{}",
        report.join("\n")
    ))
}

/// A file's text as the patch has left it so far, or as it is on disk.
async fn current(
    env: &dyn Env,
    files: &BTreeMap<PathBuf, Option<String>>,
    path: &Path,
    shown: &str,
) -> Result<String, ToolError> {
    match files.get(path) {
        Some(Some(text)) => return Ok(text.clone()),
        Some(None) => {
            return Err(ToolError::msg(format!(
                "{shown} was deleted earlier in this patch"
            )));
        }
        None => {}
    }
    if stat_file(env, path, shown).await?.is_none() {
        return Err(not_found(env, shown, " To create it, use *** Add File."));
    }
    String::from_utf8(env.read(path).await?)
        .map_err(|_| ToolError::msg(format!("{shown} is not valid UTF-8")))
}

#[derive(Debug, PartialEq)]
enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

impl Hunk {
    fn path(&self) -> &str {
        match self {
            Hunk::Add { path, .. } | Hunk::Delete { path } | Hunk::Update { path, .. } => path,
        }
    }
}

/// One change in an Update: the lines it replaces and their replacement.
#[derive(Debug, Default, PartialEq)]
struct Chunk {
    /// The `@@` line: a line before the change, to find where it is.
    context: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    /// The change is at the end of the file (`*** End of File`).
    eof: bool,
}

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF: &str = "*** End of File";

fn parse(text: &str) -> Result<Vec<Hunk>, String> {
    let mut lines: Vec<&str> = text
        .trim()
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    strip_heredoc(&mut lines);
    if lines.first().map(|l| l.trim()) != Some(BEGIN) {
        return Err(format!("the patch must start with `{BEGIN}`"));
    }
    if lines.last().map(|l| l.trim()) != Some(END) {
        return Err(format!("the patch must end with `{END}`"));
    }
    let body = &lines[1..lines.len() - 1];
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        let n = i + 2; // the line's number in the patch
        if let Some(path) = line.strip_prefix(ADD) {
            let mut contents = String::new();
            i += 1;
            while let Some(l) = body.get(i).and_then(|l| l.strip_prefix('+')) {
                contents.push_str(l);
                contents.push('\n');
                i += 1;
            }
            hunks.push(Hunk::Add {
                path: path.trim().to_string(),
                contents,
            });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            hunks.push(Hunk::Delete {
                path: path.trim().to_string(),
            });
            i += 1;
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let path = path.trim().to_string();
            i += 1;
            let move_to = match body.get(i).and_then(|l| l.strip_prefix(MOVE)) {
                Some(to) => {
                    i += 1;
                    Some(to.trim().to_string())
                }
                None => None,
            };
            let mut chunks: Vec<Chunk> = Vec::new();
            while let Some(&l) = body.get(i) {
                if l.starts_with("*** ") && l.trim() != EOF {
                    break;
                }
                if l.trim() == EOF {
                    match chunks.last_mut() {
                        Some(c) => c.eof = true,
                        None => return Err(format!("line {}: `{EOF}` before any change", i + 2)),
                    }
                } else if l == "@@" || l.starts_with("@@ ") {
                    let context = l.strip_prefix("@@ ").map(str::to_string);
                    chunks.push(Chunk {
                        context,
                        ..Chunk::default()
                    });
                } else {
                    // A change without an @@ line starts one, if none is open.
                    if chunks.last().is_none_or(|c| c.eof) {
                        chunks.push(Chunk::default());
                    }
                    let c = chunks.last_mut().expect("pushed above");
                    match l.chars().next() {
                        // An empty line is an unchanged empty line.
                        None => {
                            c.old.push(String::new());
                            c.new.push(String::new());
                        }
                        Some(' ') => {
                            c.old.push(l[1..].to_string());
                            c.new.push(l[1..].to_string());
                        }
                        Some('-') => c.old.push(l[1..].to_string()),
                        Some('+') => c.new.push(l[1..].to_string()),
                        Some(_) => {
                            return Err(format!(
                                "line {}: `{l}` in the update of {path} starts with none of \
                                 ' ', '-', '+' or '@@'",
                                i + 2
                            ));
                        }
                    }
                }
                i += 1;
            }
            if chunks.iter().all(|c| c.old.is_empty() && c.new.is_empty()) && move_to.is_none() {
                return Err(format!("line {n}: the update of {path} changes nothing"));
            }
            hunks.push(Hunk::Update {
                path,
                move_to,
                chunks,
            });
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            return Err(format!(
                "line {n}: `{line}` is not a valid hunk header. Valid hunk headers: \
                 `{ADD}<path>`, `{DELETE}<path>`, `{UPDATE}<path>`"
            ));
        }
    }
    if hunks.is_empty() {
        return Err("the patch changes no files".into());
    }
    Ok(hunks)
}

/// A patch sent as a shell heredoc (`apply_patch <<'EOF'` … `EOF`) loses the wrapper.
fn strip_heredoc(lines: &mut Vec<&str>) {
    let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
        return;
    };
    let Some((_, tag)) = first.split_once("<<") else {
        return;
    };
    let tag = tag.trim().trim_matches(|c| c == '\'' || c == '"');
    if !tag.is_empty() && last.trim() == tag && lines.len() >= 2 {
        lines.remove(0);
        lines.pop();
    }
}

/// The file's text with the chunks applied. Line endings follow the file (`\r\n` or `\n`),
/// and the result ends with one.
fn apply_chunks(original: &str, chunks: &[Chunk], shown: &str) -> Result<String, String> {
    let crlf = original.contains("\r\n");
    let lines: Vec<String> = original.lines().map(str::to_string).collect();
    let mut replacements: Vec<(usize, usize, &[String])> = Vec::new();
    let mut cursor = 0;
    for c in chunks {
        if let Some(ctx) = &c.context {
            let Some(at) = seek(&lines, std::slice::from_ref(ctx), cursor, false) else {
                return Err(format!(
                    "failed to find the context line `{ctx}` in {shown}"
                ));
            };
            cursor = at + 1;
        }
        if c.old.is_empty() {
            // Only additions: after the context line, or at the end of the file.
            let at = if c.context.is_some() && !c.eof {
                cursor
            } else {
                lines.len()
            };
            replacements.push((at, 0, &c.new));
            continue;
        }
        let mut old: &[String] = &c.old;
        let mut new: &[String] = &c.new;
        let mut found = seek(&lines, old, cursor, c.eof);
        // A trailing empty context line often stands for the end of the file.
        if found.is_none() && old.last().is_some_and(String::is_empty) {
            old = &old[..old.len() - 1];
            if new.last().is_some_and(String::is_empty) {
                new = &new[..new.len() - 1];
            }
            found = seek(&lines, old, cursor, c.eof);
        }
        let Some(at) = found else {
            return Err(format!(
                "failed to find the expected lines in {shown}:\n{}\nRead the file again and \
                 give lines exactly as they are.",
                c.old.join("\n")
            ));
        };
        replacements.push((at, old.len(), new));
        cursor = at + old.len();
    }
    let mut out = lines;
    replacements.sort_by_key(|r| r.0);
    for (at, len, new) in replacements.into_iter().rev() {
        out.splice(at..at + len, new.iter().cloned());
    }
    let nl = if crlf { "\r\n" } else { "\n" };
    let mut text = out.join(nl);
    if !text.is_empty() {
        text.push_str(nl);
    }
    Ok(text)
}

/// Where `pattern` starts in `lines`, at or after `start`: matched exactly, then ignoring
/// trailing whitespace, then ignoring surrounding whitespace, then with typographic dashes,
/// quotes and spaces made plain. With `eof`, the end of the file is tried first.
fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    let starts: Vec<usize> = if eof && last >= start {
        std::iter::once(last).chain(start..last).collect()
    } else {
        (start..=last).collect()
    };
    let passes: [fn(&str) -> std::borrow::Cow<'_, str>; 4] = [
        |s| s.into(),
        |s| s.trim_end().into(),
        |s| s.trim().into(),
        |s| plain(s.trim()).into(),
    ];
    passes.iter().find_map(|f| {
        starts
            .iter()
            .copied()
            .find(|&i| pattern.iter().zip(&lines[i..]).all(|(p, l)| f(p) == f(l)))
    })
}

/// Typographic punctuation and spaces as their ASCII forms.
fn plain(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn parses_every_kind_of_hunk() {
        let p = "*** Begin Patch\n*** Add File: new.txt\n+one\n+two\n*** Delete File: old.txt\n\
                 *** Update File: a.rs\n*** Move to: b.rs\n@@ fn main() {\n-    old();\n+    new();\n \
                 }\n*** End of File\n*** End Patch\n";
        let h = parse(p).unwrap();
        assert_eq!(
            h,
            vec![
                Hunk::Add {
                    path: "new.txt".into(),
                    contents: "one\ntwo\n".into()
                },
                Hunk::Delete {
                    path: "old.txt".into()
                },
                Hunk::Update {
                    path: "a.rs".into(),
                    move_to: Some("b.rs".into()),
                    chunks: vec![Chunk {
                        context: Some("fn main() {".into()),
                        old: s(&["    old();", "}"]),
                        new: s(&["    new();", "}"]),
                        eof: true,
                    }],
                },
            ]
        );
    }

    #[test]
    fn rejects_bad_patches_with_a_reason() {
        assert!(
            parse("*** Add File: a\n+x\n*** End Patch")
                .unwrap_err()
                .contains("start with")
        );
        assert!(
            parse("*** Begin Patch\n*** Add File: a\n+x")
                .unwrap_err()
                .contains("end with")
        );
        let e = parse("*** Begin Patch\n*** Frobnicate: a\n*** End Patch").unwrap_err();
        assert!(
            e.contains("line 2") && e.contains("not a valid hunk header"),
            "{e}"
        );
        let e = parse("*** Begin Patch\n*** Update File: a\n@@\nx\n*** End Patch").unwrap_err();
        assert!(e.contains("starts with none of"), "{e}");
        assert!(parse("*** Begin Patch\n*** End Patch").is_err());
    }

    #[test]
    fn a_heredoc_wrapper_is_dropped() {
        let p = "apply_patch <<'EOF'\n*** Begin Patch\n*** Delete File: a\n*** End Patch\nEOF";
        assert_eq!(parse(p).unwrap(), vec![Hunk::Delete { path: "a".into() }]);
    }

    fn update(p: &str) -> Vec<Chunk> {
        match parse(p).unwrap().remove(0) {
            Hunk::Update { chunks, .. } => chunks,
            h => panic!("{h:?}"),
        }
    }

    #[test]
    fn applies_chunks_in_order_with_context() {
        let original = "fn a() {\n    x();\n}\n\nfn b() {\n    x();\n}\n";
        let chunks = update(
            "*** Begin Patch\n*** Update File: f\n@@ fn b() {\n-    x();\n+    y();\n*** End Patch",
        );
        assert_eq!(
            apply_chunks(original, &chunks, "f").unwrap(),
            "fn a() {\n    x();\n}\n\nfn b() {\n    y();\n}\n",
            "the @@ line picks the second x()"
        );
    }

    #[test]
    fn additions_go_after_their_context_or_at_the_end() {
        let chunks = update("*** Begin Patch\n*** Update File: f\n@@ a\n+b\n@@\n+z\n*** End Patch");
        assert_eq!(
            apply_chunks("a\nc\n", &chunks, "f").unwrap(),
            "a\nb\nc\nz\n"
        );
    }

    #[test]
    fn matching_tolerates_whitespace_and_typography_and_keeps_crlf() {
        let chunks = update(
            "*** Begin Patch\n*** Update File: f\n@@\n-let s = \"a-b\";  \n+let s = \"c\";\n*** End Patch",
        );
        assert_eq!(
            apply_chunks(
                "x\r\n  let s = \u{201C}a\u{2013}b\u{201D};\r\n",
                &chunks,
                "f"
            )
            .unwrap(),
            "x\r\nlet s = \"c\";\r\n"
        );
    }

    #[test]
    fn eof_prefers_the_last_match_and_misses_are_explained() {
        let chunks = update(
            "*** Begin Patch\n*** Update File: f\n@@\n-x\n+y\n*** End of File\n*** End Patch",
        );
        assert_eq!(apply_chunks("x\nx\n", &chunks, "f").unwrap(), "x\ny\n");
        let chunks = update("*** Begin Patch\n*** Update File: f\n@@\n-nope\n+y\n*** End Patch");
        let e = apply_chunks("x\n", &chunks, "f").unwrap_err();
        assert!(
            e.contains("failed to find the expected lines in f:\nnope"),
            "{e}"
        );
    }
}
