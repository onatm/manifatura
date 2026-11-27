#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use manifatura_agent::{Env, ExecSpec, TtySize};
use manifatura_sandbox::Sandbox;
use manifatura_tools::Local;

struct Ws {
    dir: PathBuf,
    outside: PathBuf,
}

impl Ws {
    fn new(name: &str) -> Self {
        // Not under /tmp, which the sandbox lets commands write to.
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("mf-sbx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        Self {
            dir: std::fs::canonicalize(dir).unwrap(),
            outside: std::fs::canonicalize(outside).unwrap(),
        }
    }

    fn sandbox(&self) -> Sandbox<Local> {
        Sandbox::new(Local::new(&self.dir).unwrap()).unwrap()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.dir.parent().unwrap());
    }
}

/// Runs `script` with bash; its exit code and output.
async fn run(env: &dyn Env, script: &str, unsandboxed: bool) -> (i32, String) {
    let out = env
        .temp_dir()
        .join(format!("out-{}", manifatura_agent::new_id()));
    let mut p = env
        .exec(ExecSpec {
            argv: vec!["/bin/bash".into(), "-c".into(), script.into()],
            stdout: out.clone(),
            stderr: out.clone(),
            unsandboxed,
            ..Default::default()
        })
        .await
        .unwrap();
    let code = tokio::time::timeout(Duration::from_secs(20), p.wait())
        .await
        .unwrap()
        .unwrap();
    (
        code,
        String::from_utf8_lossy(&env.read(&out).await.unwrap()).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_write_to_the_workspace_only() {
    let ws = Ws::new("write");
    let env = ws.sandbox();
    let (code, out) = run(
        &env,
        "echo hi > a.txt && mkdir -p src && touch src/b.rs && cat a.txt",
        false,
    )
    .await;
    assert_eq!((code, out.as_str()), (0, "hi\n"));
    assert!(ws.dir.join("src/b.rs").exists());

    let (code, out) = run(
        &env,
        &format!("echo no > {}/x.txt", ws.outside.display()),
        false,
    )
    .await;
    assert_ne!(code, 0);
    assert!(
        out.contains("Operation not permitted") || out.contains("Read-only file system"),
        "{out}"
    );
    assert!(!ws.outside.join("x.txt").exists());

    // On Linux a protected name that does not exist yet can be created.
    #[cfg(target_os = "macos")]
    {
        let (code, _) = run(&env, "mkdir .git", false).await;
        assert_ne!(code, 0, ".git may not be created");
    }
    std::fs::create_dir(ws.dir.join(".git")).unwrap();
    let (code, _) = run(&env, "touch .git/HEAD", false).await;
    assert_ne!(code, 0, ".git is read-only");
    let (code, out) = run(&env, "cat /etc/hosts > /dev/null && echo read", false).await;
    assert_eq!(
        (code, out.as_str()),
        (0, "read\n"),
        "reads are not restricted"
    );
    let (code, _) = run(
        &env,
        "echo t > /tmp/mf-sbx-tmp-$$ && rm /tmp/mf-sbx-tmp-$$",
        false,
    )
    .await;
    assert_eq!(code, 0, "/tmp is writable");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_envs_own_writes_follow_the_same_rules() {
    let ws = Ws::new("files");
    let env = ws.sandbox();
    env.write(Path::new("notes/a.md"), b"x").await.unwrap();
    assert!(ws.dir.join("notes/a.md").exists());
    let e = env.write(&ws.outside.join("y"), b"x").await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    let e = env.write(Path::new(".git/config"), b"x").await.unwrap_err();
    assert!(e.to_string().contains("`.git` is read-only"), "{e}");
    let e = env
        .write(Path::new("../outside/z"), b"x")
        .await
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(env.remove(&ws.outside, true).await.is_err());
    assert!(ws.outside.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn unsandboxed_commands_need_permission() {
    let ws = Ws::new("escalate");
    let script = format!("echo yes > {}/x.txt", ws.outside.display());
    let env = ws.sandbox();
    let out = env.temp_dir().join("o");
    let refused = env
        .exec(ExecSpec {
            argv: vec!["/bin/bash".into(), "-c".into(), script.clone()],
            stdout: out.clone(),
            stderr: out,
            unsandboxed: true,
            ..Default::default()
        })
        .await;
    assert_eq!(
        refused.err().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );

    let env = ws.sandbox().allow_unsandboxed(true);
    let (code, _) = run(&env, &script, true).await;
    assert_eq!(code, 0);
    assert!(ws.outside.join("x.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn localhost_works_without_the_network() {
    let ws = Ws::new("net");
    let env = ws.sandbox();
    // A server on a loopback port and a client to it, both in the sandbox.
    let script = r#"python3 - <<'PY'
import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen(1)
c = socket.create_connection(s.getsockname(), timeout=5)
a, _ = s.accept(); c.sendall(b"ok"); print(a.recv(2).decode())
PY"#;
    let (code, out) = run(&env, script, false).await;
    assert_eq!((code, out.trim()), (0, "ok"), "{out}");
    let (code, out) = run(
        &env,
        "python3 -c 'import socket; socket.create_connection((\"1.1.1.1\", 53), timeout=3)'",
        false,
    )
    .await;
    assert_ne!(code, 0, "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn terminals_work_in_the_sandbox() {
    let ws = Ws::new("tty");
    let env = ws.sandbox();
    let out = env.temp_dir().join("tty-out");
    let mut p = env
        .exec(ExecSpec {
            argv: vec![
                "/bin/bash".into(),
                "-c".into(),
                "tty; test -t 0 && echo interactive".into(),
            ],
            stdout: out.clone(),
            stderr: out.clone(),
            tty: Some(TtySize::default()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(p.wait().await.unwrap(), 0);
    let text = String::from_utf8_lossy(&env.read(&out).await.unwrap()).into_owned();
    assert!(
        text.contains("/dev/") && text.contains("interactive"),
        "{text}"
    );
}
