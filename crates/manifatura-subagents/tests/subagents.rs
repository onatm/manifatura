//! `manifatura-subagents` with a scripted model: what the child is sent, what comes back, and what
//! the parent and child share.

use std::sync::{Arc, Mutex};

use manifatura_agent::testing::ScriptedProvider;
use manifatura_agent::*;
use manifatura_subagents::{
    AGENT_KEY, CUSTOM_KIND, DEPTH_KEY, Definition, Subagent, Subagents, TASK_TOOL, parent,
};
use serde_json::{Value, json};

/// A tool that marks `approval.grep` as always allowed, as an approval hook would.
fn grant() -> Tool {
    Tool::new("grant", "Grants grep.").run_json(
        json!({"type": "object"}),
        |_input: Value, cx: ToolCx| async move {
            cx.set_state("approval.grep", true);
            Ok::<_, ToolError>("granted".to_string())
        },
    )
}

fn explore() -> Subagent {
    Subagent::like_parent("explore", "Reads code to answer one question.")
        .brief("You explore for another agent.")
}

/// The output of every tool result in `messages`, joined.
fn results(messages: &[Message]) -> String {
    let mut out = Vec::new();
    for m in messages {
        for c in &m.content {
            if let Content::ToolResult { output, .. } = c {
                out.push(output.clone());
            }
        }
    }
    out.join("\n")
}

async fn child_ids(rt: &Runtime) -> Vec<SessionId> {
    let mut ids: Vec<SessionId> = rt
        .store()
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    ids.retain(|id| id.as_str().contains(".explore-"));
    ids
}

#[tokio::test]
async fn a_child_like_its_parent_sends_the_same_prompt_and_reports_back() {
    let provider = ScriptedProvider::new()
        .call(
            TASK_TOOL,
            json!({"agent": "explore", "description": "find main", "prompt": "Where is main?"}),
        )
        .text("main is in src/main.rs:1")
        .text("It is in src/main.rs.");
    let tools = vec![grant(), Subagents::new([explore()]).tool()];
    let agent = Agent::new("You are a coding agent.")
        .name("app")
        .tools(tools);
    let rt = Runtime::builder().provider(provider.clone()).build();
    let session = rt.session(&agent, SessionId::from("top"));
    let reply = session
        .prompt("Find main")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(reply.text, "It is in src/main.rs.");

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let (parent_req, child_req) = (&requests[0], &requests[1]);
    // The same system prompt and tool declarations, so a prompt cache serves the child too.
    assert_eq!(child_req.system, parent_req.system);
    assert_eq!(child_req.tools, parent_req.tools);
    // None of the parent's conversation: only the brief and the task.
    assert_eq!(child_req.messages.len(), 1);
    assert_eq!(
        child_req.messages[0].text(),
        "You explore for another agent.\n\nWhere is main?"
    );

    // The report is the task's result.
    let history = session.history().await.unwrap();
    let result = results(&history);
    assert_eq!(result, "main is in src/main.rs:1");

    // The child is a session of its own, named after the subagent and linked to its parent.
    let ids = child_ids(&rt).await;
    assert_eq!(ids.len(), 1, "{ids:?}");
    assert!(ids[0].as_str().starts_with("top.explore-"));
    let entries = rt.store().load(&ids[0]).await.unwrap();
    assert_eq!(parent(&entries), Some(SessionId::from("top")));
    assert!(
        entries
            .iter()
            .any(|e| matches!(&e.record, Record::Header { agent, .. } if agent == "explore"))
    );
    assert!(
        entries
            .iter()
            .any(|e| matches!(&e.record, Record::Custom { kind, .. } if kind == CUSTOM_KIND))
    );
    assert_eq!(
        parent(&rt.store().load(&SessionId::from("top")).await.unwrap()),
        None
    );
}

#[tokio::test]
async fn a_child_cannot_start_a_subagent_past_the_depth_limit() {
    let provider = ScriptedProvider::new()
        .call(
            TASK_TOOL,
            json!({"agent": "explore", "prompt": "Look around"}),
        )
        .call(
            TASK_TOOL,
            json!({"agent": "explore", "prompt": "Look deeper"}),
        )
        .text("I looked myself.")
        .text("Done.");
    let agent = Agent::new("x").tools([Subagents::new([explore()]).tool()]);
    let rt = Runtime::builder().provider(provider.clone()).build();
    let session = rt.session(&agent, SessionId::from("top"));
    session.prompt("Go").await.unwrap().wait().await.unwrap();

    // The child's own task call was refused, and it went on by itself.
    let child_turn = &provider.requests()[2];
    let refused = results(&child_turn.messages);
    assert!(
        refused.contains("cannot start another subagent"),
        "{refused}"
    );
    assert_eq!(child_ids(&rt).await.len(), 1);
    let child = rt.session(&agent, child_ids(&rt).await.remove(0));
    assert_eq!(child.state::<usize>(DEPTH_KEY).await.unwrap(), Some(1));
    assert_eq!(
        child.state::<String>(AGENT_KEY).await.unwrap().as_deref(),
        Some("explore")
    );
}

#[tokio::test]
async fn approvals_are_shared_both_ways() {
    let provider = ScriptedProvider::new()
        .call(
            TASK_TOOL,
            json!({"agent": "explore", "prompt": "Grant grep"}),
        )
        .call("grant", json!({}))
        .text("Granted.")
        .text("Done.");
    let agent = Agent::new("x").tools([grant(), Subagents::new([explore()]).tool()]);
    let rt = Runtime::builder().provider(provider).build();
    let session = rt.session(&agent, SessionId::from("top"));
    session.set_state("approval.read", true).await.unwrap();
    session.set_state("other", 1).await.unwrap();
    session.prompt("Go").await.unwrap().wait().await.unwrap();

    // What the child granted holds in the parent.
    assert_eq!(
        session.state::<bool>("approval.grep").await.unwrap(),
        Some(true)
    );
    // The parent's approvals reached the child, and nothing else did.
    let child = rt.session(&agent, child_ids(&rt).await.remove(0));
    assert_eq!(
        child.state::<bool>("approval.read").await.unwrap(),
        Some(true)
    );
    assert_eq!(child.state::<u32>("other").await.unwrap(), None);
}

#[tokio::test]
async fn a_child_that_runs_out_of_steps_reports_what_it_had() {
    let provider = ScriptedProvider::new()
        .call(
            TASK_TOOL,
            json!({"agent": "explore", "prompt": "Grant grep"}),
        )
        .respond(testing::reply(
            vec![
                Content::Text {
                    text: "Halfway there.".into(),
                },
                Content::ToolCall {
                    id: "c".into(),
                    name: "grant".into(),
                    input: json!({}),
                },
            ],
            StopReason::ToolUse,
        ))
        .text("Done.");
    let sub = explore().max_steps(1);
    let agent = Agent::new("x").tools([grant(), Subagents::new([sub]).tool()]);
    let rt = Runtime::builder().provider(provider).build();
    let session = rt.session(&agent, SessionId::from("top"));
    session.prompt("Go").await.unwrap().wait().await.unwrap();
    let history = session.history().await.unwrap();
    let text = results(&history);
    assert!(
        text.contains("stopped before finishing") && text.contains("Halfway there."),
        "{text}"
    );
}

#[tokio::test]
async fn a_subagent_of_its_own_runs_its_agent_and_events_are_observed() {
    let provider = ScriptedProvider::new()
        .call(
            TASK_TOOL,
            json!({"agent": "reviewer", "prompt": "Review it"}),
        )
        .call("grant", json!({}))
        .text("Looks fine.")
        .text("Done.");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let reviewer = Subagent::new(
        "reviewer",
        "Reviews code.",
        Agent::new("You review code.").tools([grant()]),
    );
    let task = Subagents::new([reviewer]).on_event({
        let seen = seen.clone();
        move |p| {
            if let Event::ActionStarted { name, .. } = &p.event {
                seen.lock().unwrap().push(format!("{}:{name}", p.agent));
            }
        }
    });
    let agent = Agent::new("You lead.").tools([task.tool()]);
    let rt = Runtime::builder().provider(provider.clone()).build();
    let session = rt.session(&agent, SessionId::from("top"));
    let mut turn = session.prompt("Go").await.unwrap();
    let mut logs = Vec::new();
    while let Some(event) = turn.next_event().await {
        if let Event::ActionLog { text, .. } = event {
            logs.push(text);
        }
    }
    assert_eq!(provider.requests()[1].system, "You review code.");
    assert_eq!(*seen.lock().unwrap(), ["reviewer:grant"]);
    assert_eq!(logs, ["grant {}"]);
}

#[tokio::test]
async fn bad_calls_are_errors_the_model_can_fix() {
    let provider = ScriptedProvider::new()
        .calls(&[
            (TASK_TOOL, json!({"agent": "nobody", "prompt": "x"})),
            (TASK_TOOL, json!({"agent": "explore"})),
        ])
        .text("Done.");
    let agent = Agent::new("x").tools([Subagents::new([explore()]).tool()]);
    let rt = Runtime::builder().provider(provider).build();
    let session = rt.session(&agent, SessionId::from("top"));
    session.prompt("Go").await.unwrap().wait().await.unwrap();
    let history = session.history().await.unwrap();
    let text = results(&history);
    assert!(
        text.contains("unknown subagent `nobody`; the subagents are: explore"),
        "{text}"
    );
    assert!(text.contains("missing `prompt`"), "{text}");
    assert!(child_ids(&rt).await.is_empty());
}

#[tokio::test]
async fn long_reports_are_cut() {
    let provider = ScriptedProvider::new()
        .call(TASK_TOOL, json!({"agent": "explore", "prompt": "x"}))
        .text(&"é".repeat(100))
        .text("Done.");
    let agent = Agent::new("x").tools([Subagents::new([explore()]).max_result_bytes(51).tool()]);
    let rt = Runtime::builder().provider(provider).build();
    let session = rt.session(&agent, SessionId::from("top"));
    session.prompt("Go").await.unwrap().wait().await.unwrap();
    let history = session.history().await.unwrap();
    let text = results(&history);
    assert!(
        text.starts_with(&"é".repeat(25)) && text.contains("150 more bytes"),
        "{text}"
    );
}

#[test]
fn definitions_parse_and_become_subagents() {
    let def = Definition::parse("---\nname: explore\ndescription: Reads code.\nmax_steps: 12\n---\nReport with path:line.\n").unwrap();
    assert_eq!(def.max_steps, Some(12));
    let sub = Subagent::from_definition(def);
    assert_eq!((sub.name(), sub.description()), ("explore", "Reads code."));
    assert!(
        Definition::parse("---\nname: Explore\ndescription: x\n---\n")
            .unwrap_err()
            .contains("lowercase")
    );
    assert!(
        Definition::parse("---\nname: explore\n---\n")
            .unwrap_err()
            .contains("description")
    );
    assert!(Definition::parse("no frontmatter").is_err());
}

#[tokio::test]
async fn aborting_the_parent_aborts_the_child() {
    let provider = ScriptedProvider::new()
        .call(TASK_TOOL, json!({"agent": "explore", "prompt": "Wait"}))
        .call("wait", json!({}))
        .fallback(|_| {
            testing::reply(
                vec![Content::Text {
                    text: "late".into(),
                }],
                StopReason::Stop,
            )
        });
    let wait = Tool::new("wait", "Waits until aborted.").run_json(
        json!({"type": "object"}),
        |_input: Value, cx: ToolCx| async move {
            cx.abort().aborted().await;
            Ok::<_, ToolError>("stopped".to_string())
        },
    );
    let agent = Agent::new("x").tools([wait, Subagents::new([explore()]).tool()]);
    let rt = Runtime::builder().provider(provider.clone()).build();
    let session = rt.session(&agent, SessionId::from("top"));
    let mut turn = session.prompt("Go").await.unwrap();
    while let Some(event) = turn.next_event().await {
        if matches!(&event, Event::ActionLog { text, .. } if text.starts_with("wait")) {
            turn.abort();
        }
    }
    assert!(matches!(turn.wait().await, Err(Error::Aborted)));
    // Neither the parent nor the child asked the model again.
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn an_app_can_run_a_subagent_itself() {
    let provider = ScriptedProvider::new()
        .call("grant", json!({}))
        .text("main is in src/main.rs:1");
    let task = Subagents::new([explore()]);
    let agent = Agent::new("You are a coding agent.").tools([grant(), task.tool()]);
    let rt = Runtime::builder().provider(provider.clone()).build();
    let session = rt.session(&agent, SessionId::from("top"));
    session.set_state("approval.read", true).await.unwrap();

    let lines = Arc::new(Mutex::new(Vec::new()));
    let log = {
        let lines = lines.clone();
        move |line| lines.lock().unwrap().push(line)
    };
    let result = task
        .run(&session, "explore", "Where is main?", &Abort::new(), log)
        .await;
    assert_eq!(result.output, "main is in src/main.rs:1");
    assert!(!result.is_error);
    assert_eq!(*lines.lock().unwrap(), ["grant {}"]);

    // The child is the session's own agent, with the brief before the task.
    let child_req = &provider.requests()[0];
    assert_eq!(child_req.system, "You are a coding agent.");
    assert_eq!(
        child_req.messages[0].text(),
        "You explore for another agent.\n\nWhere is main?"
    );
    // Its approvals came back, and the parent's conversation is untouched.
    assert_eq!(
        session.state::<bool>("approval.grep").await.unwrap(),
        Some(true)
    );
    assert!(session.history().await.unwrap().is_empty());
    let child = rt.session(&agent, child_ids(&rt).await.remove(0));
    assert_eq!(
        child.state::<bool>("approval.read").await.unwrap(),
        Some(true)
    );

    let unknown = task
        .run(&session, "nobody", "x", &Abort::new(), |_| {})
        .await;
    assert!(unknown.is_error && unknown.output.contains("unknown subagent"));
}

/// The text of the system notes at the end of a request's messages.
fn notes(req: &ModelRequest) -> Vec<String> {
    req.messages
        .iter()
        .rev()
        .take_while(|m| m.role == Role::System)
        .map(|m| m.text())
        .collect()
}

#[tokio::test]
async fn a_child_counts_its_steps_and_writes_its_report_on_the_last() {
    let provider = ScriptedProvider::new()
        .call(
            TASK_TOOL,
            json!({"agent": "explore", "prompt": "Look around"}),
        )
        .call("grant", json!({}))
        .call("grant", json!({}))
        .call("grant", json!({}))
        .text("Here is what I found.")
        .text("Done.");
    let sub = explore().max_steps(4);
    let agent = Agent::new("x").tools([grant(), Subagents::new([sub]).tool()]);
    let rt = Runtime::builder().provider(provider.clone()).build();
    let session = rt.session(&agent, SessionId::from("top"));
    session.prompt("Go").await.unwrap().wait().await.unwrap();

    let child: Vec<ModelRequest> = provider.requests()[1..5].to_vec();
    assert!(notes(&child[0]).is_empty() && notes(&child[1]).is_empty());
    assert!(
        notes(&child[2]).iter().any(|n| n.contains("Step 3 of 4")),
        "{:?}",
        notes(&child[2])
    );
    let last = notes(&child[3]);
    assert!(last.iter().any(|n| n.contains("last step")), "{last:?}");
    // Tools stay declared on the last step; only calling them is off.
    assert_eq!(child[3].tool_choice, ToolChoice::Off);
    assert_eq!(child[3].tools, child[0].tools);
    assert!(child[..3].iter().all(|r| r.tool_choice == ToolChoice::Auto));
    // The parent never sees the notes.
    assert!(
        provider.requests()[5]
            .messages
            .iter()
            .all(|m| !m.text().contains("last step"))
    );
    let history = session.history().await.unwrap();
    assert_eq!(results(&history), "Here is what I found.");
}

#[tokio::test]
async fn a_child_may_call_only_its_tools_and_is_not_asked_about_others() {
    let provider = ScriptedProvider::new()
        .call(TASK_TOOL, json!({"agent": "explore", "prompt": "Look"}))
        .calls(&[("grant", json!({})), ("look", json!({}))])
        .text("Looked.")
        .text("Done.");
    let look = Tool::new("look", "Looks.").run_json(
        json!({"type": "object"}),
        |_input: Value, _cx: ToolCx| async move { Ok::<_, ToolError>("seen".to_string()) },
    );
    let asked = Arc::new(Mutex::new(Vec::new()));
    let sub = explore().tools(["look"]);
    let agent = Agent::new("x")
        .tools([grant(), look, Subagents::new([sub]).tool()])
        .on({
            let asked = asked.clone();
            move |e: hook::ToolCall, _cx| {
                asked.lock().unwrap().push(e.call.name.clone());
                async move { hook::Decision::Run(e) }
            }
        });
    let rt = Runtime::builder().provider(provider.clone()).build();
    let session = rt.session(&agent, SessionId::from("top"));
    session.prompt("Go").await.unwrap().wait().await.unwrap();

    let child_turn = &provider.requests()[2];
    let out = results(&child_turn.messages);
    assert!(
        out.contains("explore may use only look") && out.contains("seen"),
        "{out}"
    );
    // The parent's own call and the child's allowed one reached the other hooks; the refused one did not.
    assert_eq!(*asked.lock().unwrap(), ["task", "look"]);
    // The parent is not limited, and grant is still declared to the child.
    assert!(child_turn.tools.iter().any(|t| t.name == "grant"));
}

#[test]
fn tools_are_read_as_a_list_or_as_text() {
    let def = Definition::parse("---\nname: a\ndescription: x\ntools:\n  - read\n  - grep\n---\n")
        .unwrap();
    assert_eq!(def.tools.unwrap(), ["read", "grep"]);
    let def =
        Definition::parse("---\nname: a\ndescription: x\ntools: read,grep glob\n---\n").unwrap();
    assert_eq!(def.tools.unwrap(), ["read", "grep", "glob"]);
    assert_eq!(
        Definition::parse("---\nname: a\ndescription: x\n---\n")
            .unwrap()
            .tools,
        None
    );
}
