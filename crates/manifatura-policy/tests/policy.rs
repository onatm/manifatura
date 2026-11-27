use std::sync::{Arc, Mutex};

use manifatura_agent::testing::ScriptedProvider;
use manifatura_agent::{Agent, Content, Role, Runtime, SessionId, Tool, ToolCall};
use manifatura_kit::{Answer, ApprovalEvent, Approvals};
use manifatura_policy::{Decision, Policy, Rule, Rules};
use serde_json::{Value, json};

fn call(name: &str, input: Value) -> ToolCall {
    ToolCall {
        id: "c".into(),
        name: name.into(),
        input,
    }
}

fn cmd(line: &str) -> ToolCall {
    call("exec_command", json!({ "cmd": line }))
}

fn decide(p: &Policy, c: &ToolCall) -> Decision {
    p.decide(c, &SessionId::from("s")).decision
}

#[test]
fn rules_read_and_print() {
    let r = Rule::parse("exec_command(git commit:*)").unwrap();
    assert_eq!(r, Rule::command("exec_command", ["git", "commit"]));
    assert_eq!(r.to_string(), "exec_command(git commit)");
    assert_eq!(
        Rule::parse("exec_command(npm run *)").unwrap().to_string(),
        "exec_command(npm run)"
    );
    assert_eq!(
        Rule::parse(" mcp__github__* ").unwrap(),
        Rule::tool("mcp__github__*")
    );
    assert!(Rule::parse("exec_command(").is_err());
    assert!(Rule::parse("two words").is_err());
    assert!(Rule::parse("exec_command()").is_err());
}

#[test]
fn deny_beats_ask_beats_allow_and_every_command_must_be_allowed() {
    let rules: Rules = serde_json::from_value(json!({
        "allow": ["exec_command(git)", "mcp__exa__*"],
        "ask": ["exec_command(git push)"],
        "deny": ["exec_command(git push --force)", "mcp__exa__crawl"]
    }))
    .unwrap();
    let p = Policy::new().rules(&rules).unwrap();
    assert_eq!(decide(&p, &cmd("git commit -m x")), Decision::Allow);
    assert_eq!(decide(&p, &cmd("git push origin main")), Decision::Ask);
    assert_eq!(
        decide(&p, &cmd("git push --force origin main")),
        Decision::Deny
    );
    assert_eq!(
        decide(&p, &cmd("ls && git push --force")),
        Decision::Deny,
        "any part denies"
    );
    assert_eq!(
        decide(&p, &cmd("git add . && npm install")),
        Decision::Ask,
        "npm install is not allowed"
    );
    assert_eq!(
        decide(&p, &cmd("git add . && ls | wc -l")),
        Decision::Allow,
        "read-only parts pass"
    );
    assert_eq!(
        decide(&p, &cmd("git status $(rm -rf ~)")),
        Decision::Ask,
        "opaque lines never pass"
    );
    assert_eq!(
        decide(&p, &cmd("echo $(git push --force)")),
        Decision::Deny,
        "deny sees inside"
    );
    assert_eq!(
        decide(&p, &call("mcp__exa__search", json!({}))),
        Decision::Allow
    );
    assert_eq!(
        decide(&p, &call("mcp__exa__crawl", json!({}))),
        Decision::Deny
    );
    assert_eq!(
        decide(&p, &call("read", json!({"path": "a"}))),
        Decision::Ask,
        "the default"
    );
}

#[test]
fn defaults_decide_what_rules_do_not() {
    let p = Policy::new()
        .rule(
            Rule::command("exec_command", ["cargo", "publish"]),
            Decision::Deny,
        )
        .when(
            |c, _| c.input["sandbox_permissions"] == "require_escalated",
            Decision::Ask,
        )
        .tool_default("exec_command", Decision::Allow)
        .tool_default("read", Decision::Allow)
        .default(Decision::Deny);
    assert_eq!(
        decide(&p, &cmd("npm install")),
        Decision::Allow,
        "sandboxed: allowed"
    );
    let escalated = call(
        "exec_command",
        json!({"cmd": "npm install", "sandbox_permissions": "require_escalated"}),
    );
    assert_eq!(decide(&p, &escalated), Decision::Ask);
    let escalated_ro = call(
        "exec_command",
        json!({"cmd": "ls ~", "sandbox_permissions": "require_escalated"}),
    );
    assert_eq!(
        decide(&p, &escalated_ro),
        Decision::Allow,
        "reading needs no approval"
    );
    assert_eq!(
        decide(&p, &cmd("cargo publish")),
        Decision::Deny,
        "rules come first"
    );
    assert_eq!(decide(&p, &call("read", json!({}))), Decision::Allow);
    assert_eq!(decide(&p, &call("other", json!({}))), Decision::Deny);
}

#[test]
fn asking_offers_rules_that_would_allow_the_whole_line() {
    let p = Policy::new();
    let v = p.decide(
        &cmd("git add -A && git commit -m 'x' && npm run build"),
        &SessionId::from("s"),
    );
    assert_eq!(v.decision, Decision::Ask);
    let offered: Vec<String> = v.remember.iter().map(Rule::to_string).collect();
    assert_eq!(
        offered,
        [
            "exec_command(git add)",
            "exec_command(git commit)",
            "exec_command(npm run build)"
        ]
    );
    for line in ["git add . && rm -rf build", "bash -c 'x'", "echo $(id)"] {
        assert!(
            p.decide(&cmd(line), &SessionId::from("s"))
                .remember
                .is_empty(),
            "{line}"
        );
    }
}

/// An agent whose `exec_command` only echoes, behind the policy; the provider calls it with
/// each line in turn, then answers.
async fn run(policy: &Policy, approvals: &Approvals, lines: &[&str]) -> Vec<(String, bool)> {
    let mut p = ScriptedProvider::new();
    for l in lines {
        p = p.call("exec_command", json!({ "cmd": l }));
    }
    let p = p.text("done");
    let tool = Tool::new("exec_command", "Runs a command.")
        .run_json(json!({"type": "object"}), |i, _| async move {
            Ok(format!("ran {}", i["cmd"]))
        });
    let rt = Runtime::builder().provider(p.clone()).model("m").build();
    let agent = Agent::new("t").tool(&tool).hooks(policy.hooks(approvals));
    let session = rt.session(&agent, SessionId::from("s"));
    session.prompt("go").await.unwrap().wait().await.unwrap();
    session
        .entries()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.record {
            manifatura_agent::Record::Message { message } if message.role == Role::Tool => {
                message.content.into_iter().find_map(|c| match c {
                    Content::ToolResult {
                        output, is_error, ..
                    } => Some((output, is_error)),
                    _ => None,
                })
            }
            _ => None,
        })
        .collect()
}

/// Answers every request with the next answer, and keeps what it was asked.
fn answer_with(
    approvals: &Approvals,
    answers: Vec<Answer>,
) -> Arc<Mutex<Vec<manifatura_kit::Request>>> {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let (a, seen, mut events) = (approvals.clone(), asked.clone(), approvals.subscribe());
    tokio::spawn(async move {
        let mut answers = answers.into_iter();
        while let Ok(e) = events.recv().await {
            if let ApprovalEvent::Requested(r) = e {
                seen.lock().unwrap().push(r.clone());
                a.answer(&r.id, answers.next().unwrap_or(Answer::Deny(None)));
            }
        }
    });
    asked
}

#[tokio::test(flavor = "multi_thread")]
async fn remembered_rules_allow_later_calls_and_reach_the_app() {
    let saved = Arc::new(Mutex::new(Vec::new()));
    let s = saved.clone();
    let policy = Policy::new().on_remember(move |r| s.lock().unwrap().push(r.to_string()));
    let approvals = Approvals::new();
    let asked = answer_with(
        &approvals,
        vec![
            Answer::AllowAndRemember,
            Answer::Deny(Some("not now".into())),
        ],
    );

    let results = run(
        &policy,
        &approvals,
        &["git commit -m a", "git commit -m b", "npm publish"],
    )
    .await;
    assert_eq!(results[0], ("ran \"git commit -m a\"".into(), false));
    assert_eq!(
        results[1],
        ("ran \"git commit -m b\"".into(), false),
        "no second question"
    );
    assert!(
        results[2].1 && results[2].0.contains("not now"),
        "{:?}",
        results[2]
    );

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert_eq!(
        asked[0].remember.as_deref(),
        Some("exec_command(git commit)")
    );
    assert_eq!(*saved.lock().unwrap(), ["exec_command(git commit)"]);
    assert!(approvals.pending().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn always_covers_the_same_command_for_the_session_and_deny_rules_refuse() {
    let policy = Policy::new().rule(Rule::command("exec_command", ["rm"]), Decision::Deny);
    let approvals = Approvals::new();
    let asked = answer_with(&approvals, vec![Answer::AllowAlways]);
    let results = run(&policy, &approvals, &["make", "make", "rm -rf /"]).await;
    assert!(!results[0].1 && !results[1].1);
    assert_eq!(asked.lock().unwrap().len(), 1, "the same command asks once");
    assert!(
        results[2].1 && results[2].0.contains("rule `exec_command(rm)`"),
        "{:?}",
        results[2]
    );
}
