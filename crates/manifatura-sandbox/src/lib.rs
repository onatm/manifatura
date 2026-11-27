//! A sandboxed env for manifatura. [`Sandbox`] wraps another [`Env`] (usually
//! `manifatura_tools::Local`) and runs every command in a sandbox: macOS Seatbelt
//! (`sandbox-exec`), or bubblewrap (`bwrap`) on Linux:
//!
//! - commands read anything, but write only to the workspace, the env's scratch directory and
//!   `/tmp`, and never to `.git` or `.agents` at the top of the workspace;
//! - they have no network, except binding ports and talking to `localhost`, so servers and
//!   tests that use them still work;
//! - the env's own writes and removals (the file tools') follow the same rules.
//!
//! ```no_run
//! # fn demo() -> std::io::Result<()> {
//! use manifatura_sandbox::Sandbox;
//!
//! let env = Sandbox::new(manifatura_tools::Local::new(".")?)?
//!     .writable(dirs_cache())          // more places commands may write
//!     .allow_unsandboxed(true);         // commands may ask to run outside it
//! # let _ = env; Ok(()) }
//! # fn dirs_cache() -> std::path::PathBuf { "/tmp".into() }
//! ```
//!
//! A command whose [`ExecSpec::unsandboxed`] is set runs outside the sandbox, on the wrapped
//! env, only with [`Sandbox::allow_unsandboxed`]; it is refused otherwise. An app that allows
//! it should ask the user before such a command runs (`manifatura-exec` marks those calls).
//!
//! On macOS the Seatbelt policy follows those of Chromium and OpenAI's Codex. On Linux the
//! command runs in its own network and process namespaces, so "`localhost`" is the sandbox's
//! own: a server started in it is reachable from other sandboxed commands but not from the
//! host. A protected name that does not exist yet can be created by a command on Linux (the
//! env's own writes still refuse it). Elsewhere [`Sandbox::new`] fails with
//! [`io::ErrorKind::Unsupported`].

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::io;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use manifatura_agent::{DirEntry, Env, ExecSpec, Process, Stat};

#[cfg(target_os = "linux")]
#[path = "bwrap.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "seatbelt.rs"]
mod backend;

/// What a backend needs to wrap a command.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub(crate) struct Settings<'a> {
    /// Where writes are allowed; the first is the workspace.
    pub writable: &'a [PathBuf],
    /// Names at the top of the workspace that stay read-only.
    pub protected: &'a [String],
    pub network: bool,
    /// Where the command runs (bubblewrap needs it; Seatbelt keeps the wrapped env's).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub cwd: PathBuf,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
const BACKEND: &str = backend::NAME;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const BACKEND: &str = "none";

/// Names at the top of the workspace that stay read-only by default.
const PROTECTED: &[&str] = &[".git", ".agents"];

/// An [`Env`] whose commands run in a sandbox (macOS Seatbelt, or bubblewrap on Linux), writing
/// only where it allows. Reads, listings and the working directory are the wrapped env's.
pub struct Sandbox<E> {
    inner: E,
    /// Where writes are allowed, canonical. The first is the workspace.
    writable: Vec<PathBuf>,
    /// Names at the top of the workspace that stay read-only.
    protected: Vec<String>,
    network: bool,
    allow_unsandboxed: bool,
}

impl<E> std::fmt::Debug for Sandbox<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("writable", &self.writable)
            .field("protected", &self.protected)
            .field("network", &self.network)
            .field("allow_unsandboxed", &self.allow_unsandboxed)
            .finish_non_exhaustive()
    }
}

impl<E: Env> Sandbox<E> {
    /// Sandboxes `inner`: commands write only to its working directory, its scratch directory
    /// and `/tmp`, with `.git` and `.agents` at the top of the working directory read-only, and
    /// have no network beyond `localhost`. Fails with [`io::ErrorKind::Unsupported`] on a
    /// system without a backend, and with the reason when the backend cannot run (on Linux:
    /// `bwrap` missing, or user namespaces not allowed).
    pub fn new(inner: E) -> io::Result<Self> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            backend::check()?;
            let mut writable = vec![
                std::fs::canonicalize(inner.cwd())?,
                std::fs::canonicalize(inner.temp_dir())?,
            ];
            if let Ok(tmp) = std::fs::canonicalize(backend::TMP)
                && !writable.contains(&tmp)
            {
                writable.push(tmp);
            }
            Ok(Self {
                inner,
                writable,
                protected: PROTECTED.iter().map(|s| s.to_string()).collect(),
                network: false,
                allow_unsandboxed: false,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = inner;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the sandbox runs on macOS and Linux only",
            ))
        }
    }

    /// Also lets commands write under `path` (canonicalised; ignored if it does not exist).
    pub fn writable(mut self, path: impl AsRef<Path>) -> Self {
        if let Ok(p) = std::fs::canonicalize(path) {
            self.writable.push(p);
        }
        self
    }

    /// Keeps `name` at the top of the workspace read-only, beside `.git` and `.agents`
    /// (e.g. an app's own `.rho` directory).
    pub fn protect(mut self, name: impl Into<String>) -> Self {
        self.protected.push(name.into());
        self
    }

    /// Gives commands the network (default: only `localhost`).
    pub fn network(mut self, on: bool) -> Self {
        self.network = on;
        self
    }

    /// Lets a command run outside the sandbox when its [`ExecSpec::unsandboxed`] is set
    /// (default: refused). Ask the user before such a command runs.
    pub fn allow_unsandboxed(mut self, allow: bool) -> Self {
        self.allow_unsandboxed = allow;
        self
    }

    /// The wrapped env.
    pub fn inner(&self) -> &E {
        &self.inner
    }

    /// Refuses a write or removal outside the writable roots or into a protected name.
    fn check_write(&self, path: &Path) -> io::Result<()> {
        let resolved = real_path(&self.inner.resolve(path));
        let denied = |why: &str| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is outside what the sandbox lets this agent change: {why}",
                    path.display()
                ),
            ))
        };
        let Some(resolved) = resolved else {
            return denied("its path leaves its directory with `..`");
        };
        let Some(root) = self.writable.iter().find(|r| resolved.starts_with(r)) else {
            return denied("only the workspace and scratch directories are writable");
        };
        if root == &self.writable[0]
            && let Some(Component::Normal(first)) = resolved
                .strip_prefix(root)
                .ok()
                .and_then(|r| r.components().next())
            && self.protected.iter().any(|p| first == p.as_str())
        {
            return denied(&format!("`{}` is read-only", first.to_string_lossy()));
        }
        Ok(())
    }
}

/// `path` with symlinks resolved as far as it exists, and the rest appended. `None` when the
/// part that does not exist contains `..`.
fn real_path(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            let mut out = real;
            for c in rest.iter().rev() {
                out.push(c);
            }
            return Some(out);
        }
        let name = existing.file_name()?.to_os_string();
        if name == ".." {
            return None;
        }
        rest.push(name);
        if !existing.pop() {
            return None;
        }
    }
}

#[async_trait]
impl<E: Env> Env for Sandbox<E> {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }

    fn temp_dir(&self) -> &Path {
        self.inner.temp_dir()
    }

    fn describe(&self) -> String {
        format!(
            "sandbox ({}: writes to the workspace and scratch directories, {}) over {}",
            BACKEND,
            if self.network {
                "with the network"
            } else {
                "no network beyond localhost"
            },
            self.inner.describe()
        )
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    async fn exec(&self, mut spec: ExecSpec) -> io::Result<Box<dyn Process>> {
        if spec.unsandboxed {
            if !self.allow_unsandboxed {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "this env runs every command in its sandbox",
                ));
            }
            return self.inner.exec(spec).await;
        }
        if spec.argv.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let settings = Settings {
                writable: &self.writable,
                protected: &self.protected,
                network: self.network,
                cwd: spec
                    .cwd
                    .as_ref()
                    .map(|c| self.inner.resolve(c))
                    .unwrap_or_else(|| self.inner.cwd().to_path_buf()),
            };
            spec.argv = backend::wrap(&settings, &spec);
            self.inner.exec(spec).await
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no sandbox here",
        ))
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path).await
    }

    async fn read_tail(&self, path: &Path, max: u64) -> io::Result<(Vec<u8>, u64)> {
        self.inner.read_tail(path, max).await
    }

    async fn read_range(&self, path: &Path, offset: u64, max: u64) -> io::Result<(Vec<u8>, u64)> {
        self.inner.read_range(path, offset, max).await
    }

    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.check_write(path)?;
        self.inner.write(path, bytes).await
    }

    async fn stat(&self, path: &Path) -> io::Result<Option<Stat>> {
        self.inner.stat(path).await
    }

    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.inner.list(path).await
    }

    async fn remove(&self, path: &Path, recursive: bool) -> io::Result<()> {
        self.check_write(path)?;
        self.inner.remove(path, recursive).await
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        self.inner.resolve(path)
    }
}
