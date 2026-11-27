//! A sandboxed env for manifatura. [`Sandbox`] wraps another [`Env`] (usually
//! `manifatura_tools::Local`) and runs every command under macOS Seatbelt (`sandbox-exec`):
//!
//! - commands read anything, but write only to the workspace, the env's scratch directory and
//!   `/private/tmp`, and never to `.git` or `.agents` at the top of the workspace;
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
//! macOS only for now: elsewhere [`Sandbox::new`] fails with [`io::ErrorKind::Unsupported`].
//! The Seatbelt policy follows those of Chromium and OpenAI's Codex.

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::io;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use manifatura_agent::{DirEntry, Env, ExecSpec, Process, Stat};

/// `sandbox-exec` from `/usr/bin` only, never from `PATH`.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Names at the top of the workspace that stay read-only by default.
const PROTECTED: &[&str] = &[".git", ".agents"];

/// What every sandboxed command may do: run, read, use terminals, ask the system for the
/// basics (user lookup, CPU information).
const BASE_POLICY: &str = r#"(version 1)
(deny default)

; Children inherit the policy.
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))

; Reads are not restricted.
(allow file-read*)

(allow file-write-data (require-all (path "/dev/null") (vnode-type CHARACTER-DEVICE)))

(allow sysctl-read)
(allow sysctl-write (sysctl-name "kern.grade_cputype"))
(allow iokit-open (iokit-registry-entry-class "RootDomainUserClient"))
(allow mach-lookup
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.PowerManagement.control"))
(allow ipc-posix-sem)
(allow ipc-posix-shm-read-data ipc-posix-shm-write-create ipc-posix-shm-write-unlink
  (ipc-posix-name-regex #"^/__KMP_REGISTERED_LIB_[0-9]+$"))

; Preferences, which tools read when reads are not restricted.
(allow ipc-posix-shm-read* (ipc-posix-name-prefix "apple.cfprefs."))
(allow mach-lookup
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (local-name "com.apple.cfprefsd.agent"))
(allow user-preference-read)

; Terminals.
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-read* file-write*
  (require-all (regex #"^/dev/ttys[0-9]+") (extension "com.apple.sandbox.pty")))
(allow file-ioctl (regex #"^/dev/ttys[0-9]+"))
"#;

/// Ports and `localhost`, without the network beyond.
const LOOPBACK_POLICY: &str = r#"
(allow network-bind (local ip "*:*"))
(allow network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
"#;

/// The network, with what name resolution and TLS need.
const NETWORK_POLICY: &str = r#"
(allow network-outbound)
(allow network-inbound)
(allow network-bind)
(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))
(allow mach-lookup
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.system.opendirectoryd.membership")
  (global-name "com.apple.SecurityServer")
  (global-name "com.apple.networkd")
  (global-name "com.apple.ocspd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.SystemConfiguration.configd"))
"#;

/// Policy that must come last: fcntls that write through read-only descriptors
/// (`F_MAKECOMPRESSED`, `F_TRANSFEREXTENTS`).
const CLOSING_POLICY: &str = "(deny system-fcntl (fcntl-command 80 110))\n";

/// An [`Env`] whose commands run under macOS Seatbelt, writing only where it allows. Reads,
/// listings and the working directory are the wrapped env's.
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
    /// and `/private/tmp`, with `.git` and `.agents` at the top of the working directory
    /// read-only, and have no network beyond `localhost`.
    pub fn new(inner: E) -> io::Result<Self> {
        if !cfg!(target_os = "macos") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the sandbox runs on macOS only for now",
            ));
        }
        if !Path::new(SANDBOX_EXEC).exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{SANDBOX_EXEC} is missing"),
            ));
        }
        let mut writable = vec![
            std::fs::canonicalize(inner.cwd())?,
            std::fs::canonicalize(inner.temp_dir())?,
        ];
        if let Ok(tmp) = std::fs::canonicalize("/private/tmp") {
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

    /// The Seatbelt policy and its parameters (`-D` definitions).
    fn policy(&self) -> (String, Vec<(String, PathBuf)>) {
        let mut policy = String::from(BASE_POLICY);
        let mut params = Vec::new();
        let mut allow = Vec::new();
        let mut anchors = Vec::new();
        for (i, root) in self.writable.iter().enumerate() {
            let key = format!("WRITABLE_ROOT_{i}");
            params.push((key.clone(), root.clone()));
            anchors.push(format!(
                "(deny file-write-unlink (require-all (literal (param \"{key}\")) (vnode-type DIRECTORY)))"
            ));
            if i > 0 || self.protected.is_empty() {
                allow.push(format!("(subpath (param \"{key}\"))"));
                continue;
            }
            // In the workspace, everything but the protected names, which also may not be
            // created.
            let mut parts = vec![format!("(subpath (param \"{key}\"))")];
            for (j, name) in self.protected.iter().enumerate() {
                let pkey = format!("{key}_PROTECTED_{j}");
                params.push((pkey.clone(), root.join(name)));
                parts.push(format!("(require-not (literal (param \"{pkey}\")))"));
                parts.push(format!("(require-not (subpath (param \"{pkey}\")))"));
            }
            allow.push(format!("(require-all {})", parts.join(" ")));
        }
        policy.push_str(&format!(
            "\n(allow file-write*\n  {}\n)\n",
            allow.join("\n  ")
        ));
        policy.push_str(&anchors.join("\n"));
        policy.push('\n');
        policy.push_str(if self.network {
            NETWORK_POLICY
        } else {
            LOOPBACK_POLICY
        });
        policy.push_str(CLOSING_POLICY);
        (policy, params)
    }

    /// `argv` under `sandbox-exec`.
    fn wrap(&self, argv: Vec<String>) -> Vec<String> {
        let (policy, params) = self.policy();
        let mut out = vec![SANDBOX_EXEC.to_string(), "-p".into(), policy];
        out.extend(
            params
                .into_iter()
                .map(|(k, v)| format!("-D{k}={}", v.to_string_lossy())),
        );
        out.push("--".into());
        out.extend(argv);
        out
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
            "sandbox (macOS Seatbelt: writes to the workspace and scratch directories, {}) over {}",
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
        spec.argv = self.wrap(std::mem::take(&mut spec.argv));
        self.inner.exec(spec).await
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
