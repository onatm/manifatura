//! The Linux backend: commands under bubblewrap (`bwrap`), in a read-only view of the system
//! with the writable roots mounted writable, and their own process and network namespaces.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use manifatura_agent::ExecSpec;

use crate::Settings;

/// What the backend is called in [`describe`](manifatura_agent::Env::describe).
pub(crate) const NAME: &str = "bubblewrap";

/// Where `/tmp` is.
pub(crate) const TMP: &str = "/tmp";

/// `bwrap` from the system's directories only, never from `PATH`.
const CANDIDATES: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap"];

fn bwrap() -> Option<&'static str> {
    CANDIDATES.iter().copied().find(|p| Path::new(p).exists())
}

/// Checks that `bwrap` is there and can make a sandbox: it needs user namespaces, which some
/// systems (Ubuntu 24.04 and later, through AppArmor) restrict.
pub(crate) fn check() -> io::Result<()> {
    let Some(bwrap) = bwrap() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "bubblewrap is missing: install it (the `bubblewrap` package)",
        ));
    };
    let out = Command::new(bwrap)
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--unshare-net",
            "--unshare-pid",
        ])
        .args(["--die-with-parent", "--", "true"])
        .stdin(Stdio::null())
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "bubblewrap cannot make a sandbox here: {}. If user namespaces are restricted (Ubuntu: \
         `kernel.apparmor_restrict_unprivileged_userns`), allow them for bwrap.",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

/// `spec`'s command under `bwrap`.
pub(crate) fn wrap(s: &Settings<'_>, spec: &ExecSpec) -> Vec<String> {
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let mut a: Vec<String> = vec![bwrap().unwrap_or(CANDIDATES[0]).into()];
    let mut push = |args: &[&str]| a.extend(args.iter().map(|s| s.to_string()));
    push(&["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
    if spec.tty.is_some() {
        // The terminal the command gets is one of the host's.
        push(&["--dev-bind", "/dev/pts", "/dev/pts"]);
    }
    for root in s.writable {
        let r = path(root);
        push(&["--bind", &r, &r]);
    }
    if let Some(ws) = s.writable.first() {
        for name in s.protected {
            let p = ws.join(name);
            if p.exists() {
                let p = path(&p);
                push(&["--ro-bind", &p, &p]);
            }
        }
    }
    if !s.network {
        push(&["--unshare-net"]);
    }
    // Its own process namespace: when bwrap dies, everything in the sandbox dies with it.
    push(&["--unshare-pid", "--die-with-parent"]);
    if spec.tty.is_none() {
        // No controlling terminal to type into.
        push(&["--new-session"]);
    }
    let cwd = path(&s.cwd);
    push(&["--chdir", &cwd, "--"]);
    a.extend(spec.argv.iter().cloned());
    a
}
