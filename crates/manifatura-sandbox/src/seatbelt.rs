//! The macOS backend: commands under Seatbelt (`sandbox-exec`) with a deny-by-default policy.

use std::io;
use std::path::{Path, PathBuf};

use manifatura_agent::ExecSpec;

use crate::Settings;

/// What the backend is called in [`describe`](manifatura_agent::Env::describe).
pub(crate) const NAME: &str = "macOS Seatbelt";

/// Where `/tmp` really is.
pub(crate) const TMP: &str = "/private/tmp";

/// `sandbox-exec` from `/usr/bin` only, never from `PATH`.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

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

/// Checks that `sandbox-exec` is there.
pub(crate) fn check() -> io::Result<()> {
    if Path::new(SANDBOX_EXEC).exists() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{SANDBOX_EXEC} is missing"),
        ))
    }
}

/// The Seatbelt policy and its parameters (`-D` definitions).
fn policy(s: &Settings<'_>) -> (String, Vec<(String, PathBuf)>) {
    let mut policy = String::from(BASE_POLICY);
    let mut params = Vec::new();
    let mut allow = Vec::new();
    let mut anchors = Vec::new();
    for (i, root) in s.writable.iter().enumerate() {
        let key = format!("WRITABLE_ROOT_{i}");
        params.push((key.clone(), root.clone()));
        anchors.push(format!(
            "(deny file-write-unlink (require-all (literal (param \"{key}\")) (vnode-type DIRECTORY)))"
        ));
        if i > 0 || s.protected.is_empty() {
            allow.push(format!("(subpath (param \"{key}\"))"));
            continue;
        }
        // In the workspace, everything but the protected names, which also may not be
        // created.
        let mut parts = vec![format!("(subpath (param \"{key}\"))")];
        for (j, name) in s.protected.iter().enumerate() {
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
    policy.push_str(if s.network {
        NETWORK_POLICY
    } else {
        LOOPBACK_POLICY
    });
    policy.push_str(CLOSING_POLICY);
    (policy, params)
}

/// `spec`'s command under `sandbox-exec`.
pub(crate) fn wrap(s: &Settings<'_>, spec: &ExecSpec) -> Vec<String> {
    let (policy, params) = policy(s);
    let mut out = vec![SANDBOX_EXEC.to_string(), "-p".into(), policy];
    out.extend(
        params
            .into_iter()
            .map(|(k, v)| format!("-D{k}={}", v.to_string_lossy())),
    );
    out.push("--".into());
    out.extend(spec.argv.iter().cloned());
    out
}
