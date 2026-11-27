//! Workspace snapshots for manifatura: what the files were when each turn started, kept as git trees in a
//! repository of their own (never the project's `.git`), so an app can put them back: an undo for what an agent
//! changed, whether through a file tool or a command.
//!
//! ```no_run
//! use manifatura_agent::Agent;
//! use manifatura_snapshots::Snapshots;
//!
//! let snapshots = Snapshots::new("/home/me/.local/share/my-app/snapshots/project");
//! let agent = Agent::new("You are a coding agent.").hooks(snapshots.hooks());
//! // Later, from the session's entries: the tree a turn started from, and back to it.
//! # async fn later(snapshots: Snapshots, session: manifatura_agent::Session, env: &dyn manifatura_agent::Env, message_id: &str) -> std::io::Result<()> {
//! let path = session.path().await.unwrap();
//! if let Some(tree) = manifatura_snapshots::before(&path, message_id) {
//!     snapshots.restore(env, &tree).await?;
//! }
//! # Ok(()) }
//! ```
//!
//! Snapshots run `git` through the session's [`Env`], so they work wherever its commands run; files the
//! project's `.gitignore` leaves out are left out. The repository's directory is the env's; with a sandboxed env
//! that does not let commands write there, use [`Snapshots::unsandboxed`].

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::io;
use std::path::{Path, PathBuf};

use manifatura_agent::{Entry, Env, ExecSpec, Hooks, Record, Role, hook};
use serde_json::json;

/// The kind of the custom record a snapshot is kept in: `{"tree": "<id>"}`, or `{"error": "…"}` when it failed.
pub const KIND: &str = "snapshot";

/// The state key a subagent's session has; its turns take no snapshots of their own.
const SUBAGENT_DEPTH: &str = "subagents.depth";

/// Snapshots of an env's working directory in a git repository at `dir`.
#[derive(Clone, Debug)]
pub struct Snapshots {
    dir: PathBuf,
    unsandboxed: bool,
}

/// What [`Snapshots::restore`] changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Restored {
    /// Files written back as the snapshot had them (changed or deleted since).
    pub restored: Vec<String>,
    /// Files the snapshot did not have, removed.
    pub removed: Vec<String>,
}

/// How a file differs from a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Change {
    /// It is new since.
    Added,
    /// It changed since.
    Modified,
    /// It is gone since.
    Deleted,
}

impl Snapshots {
    /// Snapshots kept in the git repository at `dir` (a path in the env), created on first use.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            unsandboxed: false,
        }
    }

    /// Runs `git` outside the env's sandbox ([`ExecSpec::unsandboxed`]), for a repository in a directory the
    /// sandbox does not let commands write to. The env must allow it.
    pub fn unsandboxed(mut self, on: bool) -> Self {
        self.unsandboxed = on;
        self
    }

    /// Where the repository is.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A `TurnStart` hook that snapshots the working directory as the turn starts and records the tree in the
    /// session (a custom [`KIND`] record, right after the turn's message), so [`before`] finds it. A subagent's
    /// turns take none. A failed snapshot is recorded with its error and does not stop the turn.
    pub fn hooks(&self) -> Hooks {
        let snapshots = self.clone();
        Hooks::new().on(move |_: hook::TurnStart, cx| {
            let snapshots = snapshots.clone();
            async move {
                if cx.state::<u64>(SUBAGENT_DEPTH).unwrap_or(0) > 0 {
                    return;
                }
                match snapshots.take(cx.env().as_ref()).await {
                    Ok(tree) => cx.custom(KIND, json!({ "tree": tree })),
                    Err(e) => cx.custom(KIND, json!({ "error": e.to_string() })),
                }
            }
        })
    }

    /// Snapshots the working directory now; the tree's id.
    pub async fn take(&self, env: &dyn Env) -> io::Result<String> {
        self.init(env).await?;
        self.git(env, &["add", "--all", "--", "."]).await?;
        Ok(self.git(env, &["write-tree"]).await?.trim().to_string())
    }

    /// How the working directory differs from the snapshot `tree`, path by path.
    pub async fn changes(&self, env: &dyn Env, tree: &str) -> io::Result<Vec<(Change, String)>> {
        let now = self.take(env).await?;
        self.diff(env, tree, &now).await
    }

    /// Puts the working directory back as the snapshot `tree` had it: files changed or deleted since are
    /// written back, files added since are removed. Files `.gitignore` leaves out are not touched.
    pub async fn restore(&self, env: &dyn Env, tree: &str) -> io::Result<Restored> {
        let changes = self.changes(env, tree).await?;
        let mut out = Restored::default();
        let back: Vec<&str> = changes
            .iter()
            .filter(|(c, _)| *c != Change::Added)
            .map(|(_, p)| p.as_str())
            .collect();
        for chunk in back.chunks(100) {
            let mut args = vec!["checkout", tree, "--"];
            args.extend(chunk);
            self.git(env, &args).await?;
        }
        out.restored = back.iter().map(|p| p.to_string()).collect();
        for (c, path) in &changes {
            if *c == Change::Added {
                env.remove(&env.cwd().join(path), false).await?;
                out.removed.push(path.clone());
            }
        }
        Ok(out)
    }

    async fn diff(&self, env: &dyn Env, from: &str, to: &str) -> io::Result<Vec<(Change, String)>> {
        let out = self
            .git(
                env,
                &[
                    "diff-tree",
                    "-r",
                    "-z",
                    "--no-renames",
                    "--name-status",
                    from,
                    to,
                ],
            )
            .await?;
        let mut fields = out.split('\0').filter(|f| !f.is_empty());
        let mut changes = Vec::new();
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            let change = match status.chars().next() {
                Some('A') => Change::Added,
                Some('D') => Change::Deleted,
                _ => Change::Modified,
            };
            changes.push((change, path.to_string()));
        }
        Ok(changes)
    }

    async fn init(&self, env: &dyn Env) -> io::Result<()> {
        if env.stat(&self.dir.join("HEAD")).await?.is_some() {
            return Ok(());
        }
        self.git(env, &["init", "--quiet"]).await.map(|_| ())
    }

    /// Runs `git` on the snapshot repository, with the env's working directory as its work tree; its stdout.
    async fn git(&self, env: &dyn Env, args: &[&str]) -> io::Result<String> {
        let id = manifatura_agent::new_id();
        let (out, err) = (
            env.temp_dir().join(format!("snapshot-{}.out", &id[..12])),
            env.temp_dir().join(format!("snapshot-{}.err", &id[..12])),
        );
        let dir = self.dir.to_string_lossy().into_owned();
        let work = env.cwd().to_string_lossy().into_owned();
        let mut argv: Vec<String> = [
            "git",
            "--git-dir",
            &dir,
            "--work-tree",
            &work,
            "-c",
            "core.quotepath=off",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.safecrlf=false",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        argv.extend(args.iter().map(|s| s.to_string()));
        let mut p = env
            .exec(ExecSpec {
                argv,
                env: vec![("GIT_CONFIG_NOSYSTEM".into(), "1".into())],
                stdout: out.clone(),
                stderr: err.clone(),
                unsandboxed: self.unsandboxed,
                ..Default::default()
            })
            .await?;
        let code = p.wait().await?;
        let stdout =
            String::from_utf8_lossy(&env.read(&out).await.unwrap_or_default()).into_owned();
        let stderr =
            String::from_utf8_lossy(&env.read(&err).await.unwrap_or_default()).into_owned();
        let _ = env.remove(&out, false).await;
        let _ = env.remove(&err, false).await;
        if code != 0 {
            return Err(io::Error::other(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                stderr.trim()
            )));
        }
        Ok(stdout)
    }
}

/// The snapshot taken when the turn of user message `message_id` started, from a session's [`path`]: the
/// files as they were before that message. `None` when the turn took none.
///
/// [`path`]: manifatura_agent::Session::path
pub fn before(path: &[Entry], message_id: &str) -> Option<String> {
    let at = path.iter().position(|e| e.id == message_id)?;
    path[at + 1..]
        .iter()
        .take_while(
            |e| !matches!(&e.record, Record::Message { message } if message.role == Role::User),
        )
        .find_map(|e| match &e.record {
            Record::Custom { kind, data } if kind == KIND => {
                data["tree"].as_str().map(str::to_string)
            }
            _ => None,
        })
}
