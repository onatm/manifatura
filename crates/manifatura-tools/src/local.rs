//! [`Local`], the environment on this machine: no isolation; commands run as the current user. Child processes get only an
//! allowlisted environment: `PATH HOME USER LANG LC_ALL TERM`, `TMPDIR` set to the env's
//! scratch directory, and variables named explicitly.
//!
//! Other environments (a sandbox, a remote machine, a Wasm host) implement
//! [`Env`] themselves.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use manifatura_agent::{DirEntry, Env, ExecSpec, FileKind, Process, Stat};
use tokio::io::AsyncWriteExt;

const ALLOWED_VARS: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];

/// An env without isolation.
pub struct Local {
    cwd: PathBuf,
    temp: PathBuf,
    pass: Vec<String>,
    set: Vec<(String, String)>,
}

/// Names the variables set for commands, not their values, which can be secrets.
impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set: Vec<&str> = self.set.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("Local")
            .field("cwd", &self.cwd)
            .field("pass", &self.pass)
            .field("set", &set)
            .finish_non_exhaustive()
    }
}

impl Local {
    /// Uses `cwd` (canonicalised) as the workspace and creates a private scratch directory
    /// under the system temp dir.
    pub fn new(cwd: impl AsRef<Path>) -> io::Result<Self> {
        let cwd = std::fs::canonicalize(cwd)?;
        let temp = std::env::temp_dir().join(format!(
            "mf-{}-{}",
            std::process::id(),
            &manifatura_agent::new_id()[..6]
        ));
        std::fs::create_dir_all(&temp)?;
        Ok(Self {
            cwd,
            temp: std::fs::canonicalize(temp)?,
            pass: Vec::new(),
            set: Vec::new(),
        })
    }

    /// Passes an extra variable from this process's environment to commands.
    pub fn pass_env(mut self, name: impl Into<String>) -> Self {
        self.pass.push(name.into());
        self
    }

    /// Sets a variable for every command.
    pub fn set_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set.push((name.into(), value.into()));
        self
    }

    fn command(
        &self,
        program: &std::ffi::OsStr,
        args: &[std::ffi::OsString],
        spec: &ExecSpec,
    ) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .current_dir(
                spec.cwd
                    .as_ref()
                    .map(|c| self.resolve(c))
                    .unwrap_or_else(|| self.cwd.clone()),
            )
            .env_clear();
        for k in ALLOWED_VARS
            .iter()
            .copied()
            .chain(self.pass.iter().map(String::as_str))
        {
            if let Some(v) = std::env::var_os(k) {
                cmd.env(k, v);
            }
        }
        cmd.env("TMPDIR", &self.temp);
        for (k, v) in self.set.iter().chain(&spec.env) {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        cmd
    }

    async fn spawn(
        &self,
        mut cmd: tokio::process::Command,
        spec: ExecSpec,
    ) -> io::Result<Box<dyn Process>> {
        cmd.stdout(std::fs::File::create(&spec.stdout)?)
            .stderr(std::fs::File::create(&spec.stderr)?)
            .stdin(if spec.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut child = cmd.spawn()?;
        if let (Some(input), Some(mut pipe)) = (spec.stdin, child.stdin.take()) {
            // A command may exit without reading its input; that is not an error here.
            tokio::spawn(async move {
                let _ = pipe.write_all(&input).await;
            });
        }
        let pgid = child.id();
        Ok(Box::new(LocalProcess { child, pgid }))
    }
}

struct LocalProcess {
    child: tokio::process::Child,
    pgid: Option<u32>,
}

#[async_trait]
impl Process for LocalProcess {
    fn id(&self) -> Option<u32> {
        self.pgid
    }

    async fn wait(&mut self) -> io::Result<i32> {
        let status = self.child.wait().await?;
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                return Ok(128 + sig);
            }
        }
        Ok(status.code().unwrap_or(-1))
    }

    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: plain syscall; a negative pid addresses the whole process group.
            unsafe {
                libc::kill(-(pgid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}

fn stat_of(m: &std::fs::Metadata) -> Stat {
    let kind = if m.is_symlink() {
        FileKind::Symlink
    } else if m.is_dir() {
        FileKind::Dir
    } else if m.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    };
    Stat {
        kind,
        len: m.len(),
        modified: m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    }
}

#[async_trait]
impl Env for Local {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn temp_dir(&self) -> &Path {
        &self.temp
    }

    fn describe(&self) -> String {
        "local (no sandbox)".into()
    }

    async fn exec(&self, spec: ExecSpec) -> io::Result<Box<dyn Process>> {
        let (program, args) = split_argv(&spec)?;
        let cmd = self.command(program.as_ref(), &args, &spec);
        self.spawn(cmd, spec).await
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        tokio::fs::read(self.resolve(path)).await
    }

    async fn read_tail(&self, path: &Path, max: u64) -> io::Result<(Vec<u8>, u64)> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = tokio::fs::File::open(self.resolve(path)).await?;
        let len = file.metadata().await?.len();
        let start = len.saturating_sub(max);
        file.seek(io::SeekFrom::Start(start)).await?;
        let mut bytes = Vec::with_capacity((len - start) as usize);
        file.take(len - start).read_to_end(&mut bytes).await?;
        Ok((bytes, len))
    }

    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let path = self.resolve(path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, bytes).await
    }

    async fn stat(&self, path: &Path) -> io::Result<Option<Stat>> {
        match tokio::fs::metadata(self.resolve(path)).await {
            Ok(m) => Ok(Some(stat_of(&m))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut rd = tokio::fs::read_dir(self.resolve(path)).await?;
        let mut out = Vec::new();
        while let Some(e) = rd.next_entry().await? {
            let kind = match e.file_type().await {
                Ok(t) if t.is_symlink() => FileKind::Symlink,
                Ok(t) if t.is_dir() => FileKind::Dir,
                Ok(t) if t.is_file() => FileKind::File,
                _ => FileKind::Other,
            };
            out.push(DirEntry {
                name: e.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn remove(&self, path: &Path, recursive: bool) -> io::Result<()> {
        let path = self.resolve(path);
        if tokio::fs::symlink_metadata(&path).await?.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(path).await
            } else {
                tokio::fs::remove_dir(path).await
            }
        } else {
            tokio::fs::remove_file(path).await
        }
    }
}

fn split_argv(spec: &ExecSpec) -> io::Result<(String, Vec<std::ffi::OsString>)> {
    let (program, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    Ok((program.clone(), args.iter().map(Into::into).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_exec_uses_allowlisted_env_and_files() {
        // SAFETY: test-only; set before any command is spawned.
        unsafe { std::env::set_var("MF_TEST_SECRET", "hunter2") };
        let env = Local::new(".").unwrap().set_env("GREETING", "hi");
        let out = env.temp_dir().join("out");
        let err = env.temp_dir().join("err");
        let mut p = env
            .exec(ExecSpec {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "echo $GREETING ${MF_TEST_SECRET:-unset} $TMPDIR; cat; exit 3".into(),
                ],
                stdin: Some(b"from stdin".to_vec()),
                stdout: out.clone(),
                stderr: err,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(p.wait().await.unwrap(), 3);
        let text = std::fs::read_to_string(out).unwrap();
        assert_eq!(
            text,
            format!("hi unset {}\nfrom stdin", env.temp_dir().display())
        );
    }

    #[tokio::test]
    async fn kill_stops_the_process_group() {
        let env = Local::new(".").unwrap();
        let mut p = env
            .exec(ExecSpec {
                argv: vec!["sh".into(), "-c".into(), "sleep 30 & sleep 30".into()],
                stdout: env.temp_dir().join("o"),
                stderr: env.temp_dir().join("e"),
                ..Default::default()
            })
            .await
            .unwrap();
        p.kill();
        let code = tokio::time::timeout(std::time::Duration::from_secs(5), p.wait())
            .await
            .expect("killed process did not exit")
            .unwrap();
        assert_eq!(code, 128 + libc::SIGKILL);
    }

    #[tokio::test]
    async fn write_creates_parents_and_stat_reports_missing() {
        let env = Local::new(std::env::temp_dir()).unwrap();
        let p = env.temp_dir().join("a/b/c.txt");
        env.write(&p, b"x").await.unwrap();
        assert_eq!(env.stat(&p).await.unwrap().unwrap().len, 1);
        assert!(
            env.stat(&env.temp_dir().join("nope"))
                .await
                .unwrap()
                .is_none()
        );
        env.remove(&env.temp_dir().join("a"), true).await.unwrap();
    }
}
