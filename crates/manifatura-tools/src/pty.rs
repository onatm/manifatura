//! Commands in a terminal for [`Local`](crate::Local) (`pty` feature), on `portable-pty`.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use manifatura_agent::{ExecSpec, Process, ProcessInput, TtySize};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::local::signal_group;

/// How long a finished command's last output may take to reach its file.
const DRAIN: Duration = Duration::from_secs(1);

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Starts `spec` in a new terminal. The terminal's output is copied to `spec.stdout` by a
/// thread of its own.
pub(crate) fn spawn(
    spec: &ExecSpec,
    size: TtySize,
    cwd: PathBuf,
    vars: Vec<(OsString, OsString)>,
) -> io::Result<Box<dyn Process>> {
    if spec.argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    }
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(other)?;
    let mut cmd = CommandBuilder::from_argv(spec.argv.iter().map(OsString::from).collect());
    cmd.cwd(cwd);
    cmd.env_clear();
    let has_term = vars.iter().any(|(k, _)| k == "TERM");
    for (k, v) in vars {
        cmd.env(k, v);
    }
    if !has_term {
        cmd.env("TERM", "xterm-256color");
    }
    let child = pair.slave.spawn_command(cmd).map_err(other)?;
    // Only the child holds the terminal's other end now, so reading ends when it is done.
    drop(pair.slave);
    let pgid = child.process_id();

    let mut reader = pair.master.try_clone_reader().map_err(other)?;
    let mut out = std::fs::File::create(&spec.stdout)?;
    let copier = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut writer = pair.master.take_writer().map_err(other)?;
    if let Some(input) = &spec.stdin {
        writer.write_all(input)?;
        writer.flush()?;
    }
    Ok(Box::new(PtyProcess {
        child,
        _master: pair.master,
        pgid,
        copier: Some(copier),
        writer: Some(Arc::new(Mutex::new(writer))),
    }))
}

struct PtyProcess {
    child: Box<dyn Child + Send + Sync>,
    /// Kept open while the command runs: dropping it hangs up the terminal.
    _master: Box<dyn MasterPty + Send>,
    pgid: Option<u32>,
    copier: Option<std::thread::JoinHandle<()>>,
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
}

#[async_trait]
impl Process for PtyProcess {
    fn id(&self) -> Option<u32> {
        self.pgid
    }

    async fn wait(&mut self) -> io::Result<i32> {
        // Polled, so the wait can be dropped and started again.
        let status = loop {
            if let Some(s) = self.child.try_wait()? {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        if let Some(copier) = self.copier.take() {
            // A process the command left behind can keep the terminal open: give the copier
            // a moment to write the last output, not forever.
            let joined = tokio::task::spawn_blocking(move || copier.join());
            let _ = tokio::time::timeout(DRAIN, joined).await;
        }
        Ok(match status.signal() {
            Some(name) => signal_number(name).map_or(-1, |n| 128 + n),
            None => status.exit_code() as i32,
        })
    }

    fn kill(&mut self) {
        signal_group(self.pgid, libc::SIGKILL);
        let _ = self.child.kill();
    }

    fn interrupt(&mut self) {
        signal_group(self.pgid, libc::SIGINT);
    }

    fn take_input(&mut self) -> Option<Box<dyn ProcessInput>> {
        self.writer
            .take()
            .map(|w| Box::new(PtyInput(w)) as Box<dyn ProcessInput>)
    }
}

struct PtyInput(Arc<Mutex<Box<dyn Write + Send>>>);

#[async_trait]
impl ProcessInput for PtyInput {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let w = self.0.clone();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut w = w.lock().unwrap();
            w.write_all(&bytes)?;
            w.flush()
        })
        .await
        .map_err(other)?
    }
}

/// The number of the signal `portable-pty` names (it reports `strsignal`'s text).
fn signal_number(name: &str) -> Option<i32> {
    (1..32).find(|&n| {
        // SAFETY: strsignal returns a pointer to a static or thread-local string.
        let s = unsafe { libc::strsignal(n) };
        !s.is_null() && unsafe { std::ffi::CStr::from_ptr(s) }.to_string_lossy() == name
    })
}
