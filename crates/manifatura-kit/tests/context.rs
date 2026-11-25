//! `kit::context` against an in-memory env: which files are found, in what order.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use manifatura_agent::{DirEntry, Env, ExecSpec, FileKind, Process, Stat};
use manifatura_kit::context::ContextFiles;

/// Files and directories by absolute path; a path ending in `/` is a directory.
struct MemEnv {
    cwd: PathBuf,
    files: BTreeMap<PathBuf, Option<String>>,
}

impl MemEnv {
    fn new(cwd: &str, entries: &[(&str, &str)]) -> Self {
        let files = entries
            .iter()
            .map(|(p, text)| match p.strip_suffix('/') {
                Some(dir) => (PathBuf::from(dir), None),
                None => (PathBuf::from(p), Some(text.to_string())),
            })
            .collect();
        Self {
            cwd: cwd.into(),
            files,
        }
    }
}

#[async_trait]
impl Env for MemEnv {
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    fn temp_dir(&self) -> &Path {
        Path::new("/tmp")
    }
    fn describe(&self) -> String {
        "memory".into()
    }
    async fn exec(&self, _: ExecSpec) -> io::Result<Box<dyn Process>> {
        Err(io::Error::other("no processes"))
    }
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self.files.get(path) {
            Some(Some(text)) => Ok(text.clone().into_bytes()),
            Some(None) => Err(io::Error::other("is a directory")),
            None => Err(io::ErrorKind::NotFound.into()),
        }
    }
    async fn write(&self, _: &Path, _: &[u8]) -> io::Result<()> {
        Err(io::Error::other("read-only"))
    }
    async fn stat(&self, path: &Path) -> io::Result<Option<Stat>> {
        Ok(self.files.get(path).map(|f| Stat {
            kind: if f.is_some() {
                FileKind::File
            } else {
                FileKind::Dir
            },
            len: f.as_ref().map_or(0, |t| t.len() as u64),
            modified: 0,
        }))
    }
    async fn list(&self, _: &Path) -> io::Result<Vec<DirEntry>> {
        Ok(Vec::new())
    }
    async fn remove(&self, _: &Path, _: bool) -> io::Result<()> {
        Err(io::Error::other("read-only"))
    }
}

fn texts(files: &[manifatura_kit::context::ContextFile]) -> Vec<&str> {
    files.iter().map(|f| f.text.as_str()).collect()
}

#[tokio::test]
async fn files_come_from_the_repository_root_down_after_the_users_own() {
    let env = MemEnv::new(
        "/r/a/b",
        &[
            ("/r/.git/", ""),
            ("/r/AGENTS.md", "root"),
            ("/r/CLAUDE.md", "root, claude"),
            ("/r/a/CLAUDE.md", "a"),
            ("/r/a/b/AGENTS.md", "b"),
            ("/AGENTS.md", "above the repository"),
            ("/u/AGENTS.md", "user"),
        ],
    );
    let files = ContextFiles::new()
        .user_file("/u/AGENTS.md")
        .load(&env)
        .await;
    assert_eq!(texts(&files), ["user", "root", "a", "b"]);
    assert_eq!(files[1].path, Path::new("/r/AGENTS.md"));
}

#[tokio::test]
async fn a_git_file_marks_the_root_too() {
    // A worktree's `.git` is a file.
    let env = MemEnv::new(
        "/r/a",
        &[("/r/.git", "gitdir: …"), ("/r/AGENTS.md", "root")],
    );
    assert_eq!(texts(&ContextFiles::new().load(&env).await), ["root"]);
}

#[tokio::test]
async fn without_a_repository_or_home_only_the_cwd_is_searched() {
    let env = MemEnv::new("/x/y", &[("/x/AGENTS.md", "x"), ("/x/y/AGENTS.md", "y")]);
    assert_eq!(texts(&ContextFiles::new().load(&env).await), ["y"]);
    let found = ContextFiles::new().home("/x").load(&env).await;
    assert_eq!(texts(&found), ["x", "y"]);
}

#[tokio::test]
async fn names_are_configurable_and_directories_are_not_files() {
    let env = MemEnv::new(
        "/r",
        &[
            ("/r/.git/", ""),
            ("/r/AGENTS.md/", ""),
            ("/r/CLAUDE.md", "claude"),
            ("/r/RULES.md", "rules"),
        ],
    );
    assert_eq!(texts(&ContextFiles::new().load(&env).await), ["claude"]);
    let custom = ContextFiles::new().names(["RULES.md"]).load(&env).await;
    assert_eq!(texts(&custom), ["rules"]);
}

#[tokio::test]
async fn repeats_and_empty_files_are_skipped_and_long_ones_cut() {
    let env = MemEnv::new(
        "/r/a/b",
        &[
            ("/r/.git/", ""),
            ("/r/AGENTS.md", "same"),
            ("/r/a/AGENTS.md", "same"),
            ("/r/a/b/AGENTS.md", "  \n"),
            ("/u/AGENTS.md", "0123456789"),
        ],
    );
    let files = ContextFiles::new()
        .user_file("/u/AGENTS.md")
        .max_bytes(4)
        .load(&env)
        .await;
    assert_eq!(texts(&files), ["0123\n[truncated]", "same"]);
}
