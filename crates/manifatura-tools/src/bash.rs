//! `bash`: runs a shell command; after a wait (30 s by default) a still-running command is
//! left in the background with its output in a job directory (eve's background jobs).

use std::path::Path;
use std::time::Duration;

use manifatura_agent::{Env, ExecSpec, Tool, ToolCx, ToolError, ToolResult};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{MAX_BYTES, MAX_LINES, size, tail_file};

const DEFAULT_WAIT: u64 = 30;
const MAX_WAIT: u64 = 600;

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    /// The command, run with `bash -c` (or `sh -c` where bash is missing) in the workspace.
    command: String,
    /// Seconds to wait for the command before leaving it running in the background
    /// (default 30, max 600).
    wait_seconds: Option<u64>,
}

fn shell() -> &'static str {
    if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    }
}

/// `bash`: runs a shell command in the workspace. A command still running after `wait_seconds`
/// is left in the background, with its output in a job directory.
pub fn bash() -> Tool {
    Tool::new(
        "bash",
        "Run a shell command in the workspace and return its exit code and output (stdout, then \
         stderr; long output keeps the end). A command still running after wait_seconds keeps \
         running in the background: the result gives its pid and a job directory whose stdout, \
         stderr and exit files you can inspect later (e.g. with tail or cat).",
    )
    .guideline("Use `bash` for builds, tests, git and other commands. A command still running after its wait keeps running in the background; check its job directory instead of starting it again.")
    .run(|i: BashInput, cx: ToolCx| async move {
        let env = cx.env().clone();
        let job = env
            .temp_dir()
            .join("jobs")
            .join(&manifatura_agent::new_id()[..12]);
        let (stdout, stderr, exit) = (job.join("stdout"), job.join("stderr"), job.join("exit"));
        env.write(&job.join("command"), i.command.as_bytes()).await?;
        let mut proc = env
            .exec(ExecSpec {
                argv: vec![shell().into(), "-c".into(), i.command.clone()],
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                ..Default::default()
            })
            .await?;
        let wait = Duration::from_secs(i.wait_seconds.unwrap_or(DEFAULT_WAIT).clamp(1, MAX_WAIT));
        let code = tokio::select! {
            r = proc.wait() => r?,
            _ = tokio::time::sleep(wait) => {
                let pid = proc.id();
                let bg_env = env.clone();
                let exit_file = exit.clone();
                tokio::spawn(async move {
                    let code = proc.wait().await.map(|c| c.to_string()).unwrap_or_else(|e| e.to_string());
                    let _ = bg_env.write(&exit_file, code.as_bytes()).await;
                });
                return Ok(ToolResult::ok(format!(
                    "Still running after {}s; left in the background.\npid: {}\njob_dir: {}\n\
                     The exit code will appear in job_dir/exit. Partial output so far:\n{}",
                    wait.as_secs(),
                    pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                    job.display(),
                    output(&*env, &stdout, &stderr, &job).await,
                )));
            }
            _ = cx.abort().aborted() => {
                proc.kill();
                let _ = proc.wait().await;
                return Err(ToolError::msg("aborted; the command was killed"));
            }
        };
        env.write(&exit, code.to_string().as_bytes()).await?;
        let out = output(&*env, &stdout, &stderr, &job).await;
        let text = format!("{out}\n[exit code {code}]");
        Ok(if code == 0 {
            ToolResult::ok(text)
        } else {
            ToolResult::error(text)
        })
    })
}

/// Stdout then stderr, keeping the end of each within the shared output budget. Only the
/// ends of the files are read.
async fn output(env: &dyn Env, stdout: &Path, stderr: &Path, job: &Path) -> String {
    let (err, err_len) = tail_file(env, stderr, MAX_LINES, MAX_BYTES / 4).await;
    let err_budget = if err_len == 0 { 0 } else { MAX_BYTES / 4 };
    let (out, out_len) = tail_file(env, stdout, MAX_LINES, MAX_BYTES - err_budget).await;
    let mut parts = Vec::new();
    for (name, cut, len) in [("stdout", out, out_len), ("stderr", err, err_len)] {
        if cut.text.trim().is_empty() {
            continue;
        }
        let mut s = String::new();
        if name == "stderr" {
            s.push_str("[stderr]\n");
        }
        if cut.truncated {
            s.push_str(&format!(
                "[{name} truncated: showing its last {} lines, of {}; full output in {}]\n",
                cut.lines,
                size(len),
                job.join(name).display()
            ));
        }
        s.push_str(&cut.text);
        parts.push(s);
    }
    if parts.is_empty() {
        "(no output)".into()
    } else {
        parts.join("\n")
    }
}
