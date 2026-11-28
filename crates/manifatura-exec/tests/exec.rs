use std::sync::Arc;
use std::time::Duration;

use manifatura_agent::testing::ScriptedProvider;
use manifatura_agent::{Agent, Env, Message, Runtime, SessionId, Tool, ToolCx, ToolResult};
use manifatura_exec::{Exec, ExecEvent};
use manifatura_tools::Local;
use serde_json::{Value, json};

struct Ws {
    env: Arc<dyn Env>,
    exec: Exec,
    tools: Vec<Tool>,
    session: SessionId,
}

impl Ws {
    fn new() -> Self {
        let exec = Exec::new();
        Self {
            env: Arc::new(Local::new(std::env::temp_dir()).unwrap()),
            tools: exec.tools(),
            exec,
            session: SessionId::from("detached"),
        }
    }

    async fn call(&self, name: &str, input: Value) -> ToolResult {
        let tool = self.tools.iter().find(|t| t.name() == name).unwrap();
        let cx = ToolCx::detached("c").with_env(self.env.clone());
        tool.call(input, cx).await
    }

    async fn exec(&self, input: Value) -> String {
        let r = self.call("exec_command", input).await;
        assert!(!r.is_error, "{}", r.output);
        r.output
    }

    async fn stdin(&self, input: Value) -> ToolResult {
        self.call("write_stdin", input).await
    }
}

fn output(report: &str) -> &str {
    report.split_once("Output:\n").unwrap().1
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quick_command_returns_its_output_and_exit_code() {
    let ws = Ws::new();
    let r = ws
        .exec(json!({"cmd": "echo out; echo err >&2; exit 3"}))
        .await;
    assert!(r.starts_with("Chunk ID: "), "{r}");
    assert!(r.contains("Process exited with code 3\n"), "{r}");
    assert!(!r.contains("Original token count"));
    assert_eq!(output(&r), "out\nerr\n");
    assert!(
        ws.exec.processes(&ws.session).is_empty(),
        "a finished command is forgotten"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_command_keeps_running_and_is_polled_to_its_end() {
    let ws = Ws::new();
    let mut events = ws.exec.subscribe();
    let r = ws
        .exec(json!({"cmd": "echo start; sleep 1; echo end", "yield_time_ms": 250}))
        .await;
    assert!(r.contains("Process running with session ID 1\n"), "{r}");
    assert_eq!(output(&r), "start\n");
    let listed = ws.exec.processes(&ws.session);
    assert_eq!((listed.len(), listed[0].exit), (1, None));

    let r = ws.stdin(json!({"session_id": 1})).await;
    assert!(!r.is_error, "{}", r.output);
    assert!(
        r.output.contains("Process exited with code 0"),
        "{}",
        r.output
    );
    assert_eq!(output(&r.output), "end\n", "only the new output");
    assert!(
        ws.stdin(json!({"session_id": 1})).await.is_error,
        "its session ended"
    );

    assert!(matches!(
        events.recv().await.unwrap(),
        ExecEvent::Started { id: 1, .. }
    ));
    assert!(matches!(
        events.recv().await.unwrap(),
        ExecEvent::Exited { id: 1, code: 0, .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_interrupts_and_other_input_needs_a_terminal() {
    let ws = Ws::new();
    ws.exec(json!({"cmd": "sleep 30", "yield_time_ms": 250}))
        .await;
    let r = ws.stdin(json!({"session_id": 1, "chars": "y\n"})).await;
    assert!(r.is_error && r.output.contains("tty: true"), "{}", r.output);
    let r = ws.stdin(json!({"session_id": 1, "chars": "\u{3}"})).await;
    assert!(
        r.output.contains("Process exited with code 130"),
        "{}",
        r.output
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_takes_input() {
    let ws = Ws::new();
    let r = ws
        .exec(json!({"cmd": "read -r line; echo got:$line", "tty": true, "yield_time_ms": 500}))
        .await;
    assert!(r.contains("Process running with session ID 1"), "{r}");
    let r = ws
        .stdin(json!({"session_id": 1, "chars": "hello\n", "yield_time_ms": 2000}))
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert!(r.output.contains("got:hello"), "{}", r.output);
    assert!(!r.output.contains('\x1b'), "escape sequences are stripped");
}

#[tokio::test(flavor = "multi_thread")]
async fn long_output_keeps_its_beginning_and_end() {
    let ws = Ws::new();
    let r = ws
        .exec(json!({"cmd": "seq 1 100000", "max_output_tokens": 100}))
        .await;
    assert!(r.contains("Original token count: "), "{r}");
    let out = output(&r);
    assert!(out.starts_with("1\n2\n3\n"), "{out}");
    assert!(out.contains("tokens truncated"));
    assert!(out.ends_with("99999\n100000\n"), "{out}");
    assert!(out.len() < 600);
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_stops_a_command_and_dropping_exec_stops_the_rest() {
    let ws = Ws::new();
    let mut events = ws.exec.subscribe();
    ws.exec(json!({"cmd": "sleep 30", "yield_time_ms": 250}))
        .await;
    ws.exec(json!({"cmd": "sleep 30", "yield_time_ms": 250}))
        .await;
    assert!(ws.exec.kill(&ws.session, 1));
    assert!(!ws.exec.kill(&ws.session, 1));
    let mut exits = Vec::new();
    let mut next = async || loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Ok(ExecEvent::Exited { id, code, .. })) => return (id, code),
            Ok(Ok(_)) => continue,
            other => panic!("{other:?}"),
        }
    };
    exits.push(next().await);
    drop(ws); // the last handles to the Exec: its tools and itself
    exits.push(next().await);
    assert_eq!(exits, [(1, 137), (2, 137)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_hears_of_a_background_exit_before_its_next_call() {
    let exec = Exec::new();
    let p = ScriptedProvider::new()
        .call(
            "exec_command",
            json!({"cmd": "sleep 0.3", "yield_time_ms": 250}),
        )
        .call(
            "exec_command",
            json!({"cmd": "sleep 1", "yield_time_ms": 5000}),
        )
        .text("done");
    let env = Local::new(std::env::temp_dir()).unwrap();
    let rt = Runtime::builder()
        .provider(p.clone())
        .env(env)
        .model("m")
        .build();
    let agent = Agent::new("test").tools(exec.tools()).hooks(exec.hooks());
    let session = rt.session(&agent, SessionId::new());
    session.prompt("go").await.unwrap().wait().await.unwrap();

    let reqs = p.requests();
    assert_eq!(reqs.len(), 3);
    let last = reqs[2]
        .messages
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        last.contains("Background command 1 (`sleep 0.3`) exited with code 0"),
        "{last}"
    );
    let second = reqs[1]
        .messages
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !second.contains("Background command"),
        "not before it exits"
    );
}

#[test]
fn escalation_adds_its_fields_only_when_on() {
    let props = |exec: &Exec| exec.tools()[0].spec().input_schema["properties"].clone();
    assert!(props(&Exec::new()).get("sandbox_permissions").is_none());
    let p = props(&Exec::new().escalation(true));
    assert!(p.get("sandbox_permissions").is_some() && p.get("justification").is_some());
    assert!(p.get("cmd").is_some(), "the usual fields stay");

    let call = |input: Value| manifatura_agent::ToolCall {
        id: "c".into(),
        name: "exec_command".into(),
        input,
    };
    assert!(manifatura_exec::is_escalation(&call(
        json!({"cmd": "npm i", "sandbox_permissions": "require_escalated"})
    )));
    assert!(!manifatura_exec::is_escalation(&call(json!({"cmd": "ls"}))));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn an_escalated_command_runs_outside_the_sandbox() {
    // Not under /tmp, which the sandbox lets commands write to.
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("mf-exec-esc-{}", std::process::id()));
    let (dir, outside) = (base.join("ws"), base.join("outside"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let outside = std::fs::canonicalize(outside).unwrap();
    let sandbox = manifatura_sandbox::Sandbox::new(Local::new(&dir).unwrap())
        .unwrap()
        .allow_unsandboxed(true);
    let exec = Exec::new().escalation(true);
    let ws = Ws {
        env: Arc::new(sandbox),
        tools: exec.tools(),
        exec,
        session: SessionId::from("detached"),
    };
    let cmd = format!("echo yes > {}/x.txt", outside.display());

    let r = ws.exec(json!({"cmd": cmd})).await;
    assert!(r.contains("Process exited with code 1"), "{r}");
    assert!(r.contains("require_escalated"), "a hint: {r}");
    let r = ws
        .call(
            "exec_command",
            json!({"cmd": cmd, "sandbox_permissions": "require_escalated"}),
        )
        .await;
    assert!(
        r.is_error && r.output.contains("justification"),
        "{}",
        r.output
    );
    let r = ws
        .exec(json!({
            "cmd": format!("{cmd}; sleep 1"),
            "sandbox_permissions": "require_escalated",
            "justification": "Writes the test file outside the workspace.",
            "yield_time_ms": 250
        }))
        .await;
    assert!(r.contains("session ID 2"), "the failed attempt was 1: {r}");
    assert!(ws.exec.processes(&ws.session)[0].escalated);
    let typing = |chars: &str| manifatura_agent::ToolCall {
        id: "c".into(),
        name: "write_stdin".into(),
        input: json!({"session_id": 2, "chars": chars}),
    };
    assert!(
        ws.exec
            .types_into_escalated(&typing("rm -rf ~\n"), &ws.session)
    );
    assert!(!ws.exec.types_into_escalated(&typing("\u{3}"), &ws.session));
    assert!(!ws.exec.types_into_escalated(&typing(""), &ws.session));
    let r = ws.stdin(json!({"session_id": 2})).await;
    assert!(
        r.output.contains("Process exited with code 0"),
        "{}",
        r.output
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("x.txt")).unwrap(),
        "yes\n"
    );
    let _ = std::fs::remove_dir_all(base);
}
