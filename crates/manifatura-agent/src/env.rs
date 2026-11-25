//! The environment tools act in (flue's narrow sandbox surface). `manifatura-tools` has `Local`;
//! a sandbox, a remote machine or a Wasm host implements [`Env`] itself.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

/// A command to run. Output always goes to files, so long or chatty commands never sit in
/// memory and a backgrounded command keeps a readable log.
#[derive(Clone, Debug, Default)]
pub struct ExecSpec {
    /// Program and arguments; no shell unless the program is one.
    pub argv: Vec<String>,
    /// Defaults to the env's working directory.
    pub cwd: Option<PathBuf>,
    /// Variables added to the env's allowlisted environment.
    pub env: Vec<(String, String)>,
    /// Bytes written to the command's standard input, which is then closed. `None`: no input.
    pub stdin: Option<Vec<u8>>,
    /// The file standard output goes to.
    pub stdout: PathBuf,
    /// The file standard error goes to.
    pub stderr: PathBuf,
}

/// A running command.
#[async_trait]
pub trait Process: Send {
    /// The process id, while there is one.
    fn id(&self) -> Option<u32>;
    /// The exit code; a signal `n` is reported as `128 + n`.
    async fn wait(&mut self) -> io::Result<i32>;
    /// Kills the command and everything it started.
    fn kill(&mut self);
}

/// What a path is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link, where the env reports links rather than following them.
    Symlink,
    /// Anything else: a socket, a device, a pipe.
    Other,
}

/// What [`Env::stat`] reports about a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    /// What the path is.
    pub kind: FileKind,
    /// Size in bytes.
    pub len: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub modified: u128,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's file name, without its directory.
    pub name: String,
    /// What the entry is.
    pub kind: FileKind,
}

/// Where tools act: files, commands and a scratch directory. The core and kit reach the
/// outside world only through it, so the same tools run locally, in a sandbox or on a remote
/// host.
#[async_trait]
pub trait Env: Send + Sync {
    /// The workspace root. Relative paths resolve against it.
    fn cwd(&self) -> &Path;
    /// A scratch directory owned by this env (job logs, spilled output).
    fn temp_dir(&self) -> &Path;
    /// A short description for diagnostics, e.g. `local (no sandbox)`.
    fn describe(&self) -> String;

    /// Whether paths of this env can be read through this machine's filesystem directly.
    /// Tools that walk directories in-process (grep, glob) go through [`list`](Self::list)
    /// and [`read`](Self::read) instead when it cannot (a remote env).
    fn is_local(&self) -> bool {
        true
    }

    /// Starts a command; its output goes to the files `spec` names.
    async fn exec(&self, spec: ExecSpec) -> io::Result<Box<dyn Process>>;
    /// The whole file.
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// The last `max` bytes of a file, and the file's length. The default reads the whole file;
    /// an env that can seek reads only the end, so a command's huge output costs no memory.
    async fn read_tail(&self, path: &Path, max: u64) -> io::Result<(Vec<u8>, u64)> {
        let mut bytes = self.read(path).await?;
        let len = bytes.len() as u64;
        let start = bytes
            .len()
            .saturating_sub(usize::try_from(max).unwrap_or(usize::MAX));
        bytes.drain(..start);
        Ok((bytes, len))
    }
    /// Writes the file, creating parent directories.
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// `None` when the path does not exist.
    async fn stat(&self, path: &Path) -> io::Result<Option<Stat>>;
    /// The entries of a directory, in no particular order.
    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
    /// Removes a file, or a directory with everything in it when `recursive`.
    async fn remove(&self, path: &Path, recursive: bool) -> io::Result<()>;

    /// Resolves `path` against [`cwd`](Self::cwd).
    fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd().join(path)
        }
    }
}

#[async_trait]
impl<E: Env + ?Sized> Env for Arc<E> {
    fn cwd(&self) -> &Path {
        (**self).cwd()
    }
    fn temp_dir(&self) -> &Path {
        (**self).temp_dir()
    }
    fn describe(&self) -> String {
        (**self).describe()
    }
    fn is_local(&self) -> bool {
        (**self).is_local()
    }
    async fn exec(&self, spec: ExecSpec) -> io::Result<Box<dyn Process>> {
        (**self).exec(spec).await
    }
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        (**self).read(path).await
    }
    async fn read_tail(&self, path: &Path, max: u64) -> io::Result<(Vec<u8>, u64)> {
        (**self).read_tail(path, max).await
    }
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        (**self).write(path, bytes).await
    }
    async fn stat(&self, path: &Path) -> io::Result<Option<Stat>> {
        (**self).stat(path).await
    }
    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        (**self).list(path).await
    }
    async fn remove(&self, path: &Path, recursive: bool) -> io::Result<()> {
        (**self).remove(path, recursive).await
    }
    fn resolve(&self, path: &Path) -> PathBuf {
        (**self).resolve(path)
    }
}

/// Used when no env is configured: every operation fails with a clear message.
pub(crate) struct NoEnv;

fn no_env() -> io::Error {
    io::Error::other("no environment configured (RuntimeBuilder::env)")
}

#[async_trait]
impl Env for NoEnv {
    fn cwd(&self) -> &Path {
        Path::new(".")
    }
    fn temp_dir(&self) -> &Path {
        Path::new(".")
    }
    fn describe(&self) -> String {
        "none".into()
    }
    async fn exec(&self, _: ExecSpec) -> io::Result<Box<dyn Process>> {
        Err(no_env())
    }
    async fn read(&self, _: &Path) -> io::Result<Vec<u8>> {
        Err(no_env())
    }
    async fn write(&self, _: &Path, _: &[u8]) -> io::Result<()> {
        Err(no_env())
    }
    async fn stat(&self, _: &Path) -> io::Result<Option<Stat>> {
        Err(no_env())
    }
    async fn list(&self, _: &Path) -> io::Result<Vec<DirEntry>> {
        Err(no_env())
    }
    async fn remove(&self, _: &Path, _: bool) -> io::Result<()> {
        Err(no_env())
    }
}

pub(crate) fn no_env_arc() -> Arc<dyn Env> {
    Arc::new(NoEnv)
}
