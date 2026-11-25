//! Script tools: any executable plus a schema. The tool input arrives as JSON on stdin;
//! stdout is the result. A non-zero exit becomes a tool error that includes stderr.

use std::time::Duration;

use async_trait::async_trait;
use manifatura_agent::{ExecSpec, Tool, ToolCx, ToolHandler, ToolResult, ToolSpec};
use serde_json::Value;

use crate::truncate::{MAX_BYTES, MAX_LINES, tail_file};

#[derive(Debug)]
/// A tool run by a program: the input goes to its stdin as JSON, and its stdout is the result.
/// A non-zero exit is an error, with stderr. Finish it with [`build`](Self::build).
pub struct ScriptTool {
    spec: ToolSpec,
    argv: Vec<String>,
    timeout: Duration,
}

impl ScriptTool {
    /// `argv` is the program and its fixed arguments, e.g. `["python3", "tools/weather.py"]`.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        argv: Vec<String>,
    ) -> Self {
        Self {
            spec: ToolSpec {
                name: name.into(),
                description: description.into(),
                input_schema,
            },
            argv,
            timeout: Duration::from_secs(120),
        }
    }

    /// How long a run may take before it is killed (default two minutes).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The tool.
    pub fn build(self) -> Tool {
        Tool::from_handler(self.spec.clone(), self)
    }
}

#[async_trait]
impl ToolHandler for ScriptTool {
    async fn call(&self, input: Value, cx: ToolCx) -> ToolResult {
        let env = cx.env();
        let dir = env
            .temp_dir()
            .join("scripts")
            .join(&manifatura_agent::new_id()[..12]);
        let (out, err) = (dir.join("stdout"), dir.join("stderr"));
        if let Err(e) = env
            .write(&dir.join("input.json"), input.to_string().as_bytes())
            .await
        {
            return ToolResult::error(e.to_string());
        }
        let spawned = env
            .exec(ExecSpec {
                argv: self.argv.clone(),
                stdin: Some(input.to_string().into_bytes()),
                stdout: out.clone(),
                stderr: err.clone(),
                ..Default::default()
            })
            .await;
        let mut proc = match spawned {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("cannot run {:?}: {e}", self.argv)),
        };
        let code = tokio::select! {
            r = proc.wait() => r,
            _ = tokio::time::sleep(self.timeout) => {
                proc.kill();
                let _ = proc.wait().await;
                return ToolResult::error(format!("timed out after {}s", self.timeout.as_secs()));
            }
            _ = cx.abort().aborted() => {
                proc.kill();
                let _ = proc.wait().await;
                return ToolResult::error("aborted");
            }
        };
        let stdout = tail_file(&**env, &out, MAX_LINES, MAX_BYTES).await.0.text;
        let stderr = tail_file(&**env, &err, 200, 8 * 1024).await.0.text;
        let _ = env.remove(&dir, true).await;
        match code {
            Ok(0) => ToolResult::ok(stdout),
            Ok(code) => ToolResult::error(
                format!("exit code {code}\n{stderr}\n{stdout}")
                    .trim()
                    .to_string(),
            ),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}
