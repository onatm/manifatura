use std::sync::{Arc, Mutex};

use manifatura_agent::testing::{ScriptedProvider, reply};
use manifatura_agent::*;
use manifatura_kit::{self as kit, Answer, compaction};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

fn runtime(p: &ScriptedProvider) -> Runtime {
    Runtime::builder().provider(p.clone()).build()
}

fn text(t: &str) -> Response {
    reply(vec![Content::Text { text: t.into() }], StopReason::Stop)
}

fn is_summary_request(r: &ModelRequest) -> bool {
    r.system == compaction::SUMMARY_PROMPT
}

fn results(r: &ModelRequest) -> Vec<(bool, String)> {
    r.messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            Content::ToolResult {
                output, is_error, ..
            } => Some((*is_error, output.clone())),
            _ => None,
        })
        .collect()
}

fn echo(name: &str) -> Tool {
    let n = name.to_string();
    Tool::new(name, format!("Echoes for {name}.")).run_json(
        json!({"type":"object", "properties": {"x": {"type": "string"}}}),
        move |i, _| {
            let n = n.clone();
            async move { Ok(format!("{n}: {i}")) }
        },
    )
}

#[derive(Deserialize, JsonSchema)]
struct Lookup {
    order_id: String,
}

fn lookup() -> Tool {
    Tool::new("lookup_order", "Look up an order.").run(|i: Lookup, _cx: ToolCx| async move {
        Ok(Json(json!({ "id": i.order_id, "status": "shipped" })))
    })
}

// --- Compaction ---------------------------------------------------------------------------

#[tokio::test]
async fn summarizer_sends_a_transcript_with_the_focus() {
    let p = ScriptedProvider::new().text("1").text("2").with(|req| {
        assert!(is_summary_request(req));
        assert_eq!(req.reasoning, Some(Effort::Off));
        let ask = req.messages[0].text();
        assert!(ask.contains("<transcript>\n[user]\na"));
        assert!(ask.contains("Pay particular attention to: the API"));
        text("focused")
    });
    let rt = runtime(&p);
    let s = rt.session(&Agent::new("x"), SessionId::new());
    s.prompt("a").await.unwrap().wait().await.unwrap();
    s.prompt("b").await.unwrap().wait().await.unwrap();
    let c = Arc::new(kit::Summarizer::default().keep_recent(1));
    let r = s.compact(c, Some("the API".into())).await.unwrap();
    assert!(r.summarized);
    assert!(s.history().await.unwrap()[0].text().ends_with("focused"));
}

#[tokio::test]
async fn trim_results_shrinks_old_results_in_the_request_only() {
    let big = Tool::new("big", "Returns a lot.").run_json(json!({"type":"object"}), |_, _| async {
        Ok("z".repeat(3_000))
    });
    let p = ScriptedProvider::new()
        .call("big", json!({}))
        .call("big", json!({}))
        .text("done");
    let rt = runtime(&p);
    let agent = Agent::new("x")
        .tool(big)
        .hooks(compaction::trim_results(500, 600));
    let s = rt.session(&agent, SessionId::new());
    s.prompt("go").await.unwrap().wait().await.unwrap();
    let last = results(p.requests().last().unwrap());
    assert!(last[0].1.contains("[tool output trimmed") && last[0].1.len() < 400);
    assert_eq!(last[1].1.len(), 3_000);
    let kept = s.history().await.unwrap();
    assert!(kept.iter().all(|m| !m.text().contains("trimmed")));
}

#[tokio::test]
async fn trim_results_leaves_tools_that_keep_their_results_whole() {
    let tool = |name: &str, keep: bool| {
        let t = Tool::new(name, "Returns a lot.");
        let t = if keep { t.keep_results() } else { t };
        t.run_json(json!({"type":"object"}), |_, _| async {
            Ok("z".repeat(3_000))
        })
    };
    let p = ScriptedProvider::new()
        .call("plan", json!({}))
        .call("big", json!({}))
        .call("big", json!({}))
        .text("done");
    let rt = runtime(&p);
    let agent = Agent::new("x")
        .tool(tool("plan", true))
        .tool(tool("big", false))
        .hooks(compaction::trim_results(500, 600));
    let s = rt.session(&agent, SessionId::new());
    s.prompt("go").await.unwrap().wait().await.unwrap();
    let last = results(p.requests().last().unwrap());
    assert_eq!(last[0].1.len(), 3_000, "the plan is kept");
    assert!(last[1].1.contains("[tool output trimmed"));
}

#[tokio::test]
async fn on_overflow_compacts_once_per_turn() {
    let overflow = || Response::error(ErrorKind::ContextOverflow, "too long");
    let p = ScriptedProvider::new()
        .text("one")
        .text("two")
        .respond(overflow())
        .with(|req| {
            assert!(is_summary_request(req));
            text("S")
        })
        .text("three")
        .respond(overflow())
        .with(|_| text("S2"))
        .respond(overflow());
    let rt = runtime(&p);
    let c = Arc::new(kit::Summarizer::default().keep_recent(1));
    let agent = Agent::new("x").hooks(compaction::on_overflow(c));
    let s = rt.session(&agent, SessionId::new());
    for m in ["a", "b"] {
        s.prompt(m).await.unwrap().wait().await.unwrap();
    }
    assert_eq!(
        s.prompt("c").await.unwrap().wait().await.unwrap().text,
        "three"
    );
    let last = p.requests()[4].clone();
    assert!(last.messages[0].text().ends_with("S"));
    // A second overflow in the same turn fails it rather than compacting again.
    let r = s.prompt("d").await.unwrap().wait().await;
    assert!(
        matches!(&r, Err(Error::Model(e)) if e.message == "too long"),
        "{r:?}"
    );
    assert_eq!(p.requests().len(), 8);
}

/// Opt-in automatic compaction (the M3 exit test): 200 tool-using turns against a 16k window
/// with the small summariser stay within it.
#[tokio::test]
async fn two_hundred_turns_stay_within_the_window_with_auto() {
    const WINDOW: u32 = 16_000;
    let out = Tool::new("out", "Prints output.").run_json(json!({"type":"object"}), |_, _| async {
        Ok("log line\n".repeat(300))
    });
    let p = ScriptedProvider::new().fallback(|req| {
        if is_summary_request(req) {
            return text(&"Summary of earlier work. ".repeat(40));
        }
        match req.messages.last().unwrap().role {
            Role::Tool => text(&"Done with that step. ".repeat(20)),
            _ => reply(
                vec![Content::ToolCall {
                    id: new_id(),
                    name: "out".into(),
                    input: json!({}),
                }],
                StopReason::ToolUse,
            ),
        }
    });
    let rt = runtime(&p);
    let agent = Agent::new("x")
        .tool(out)
        .context_window(WINDOW)
        .hooks(compaction::trim_results(6_000, 600))
        .hooks(compaction::auto(0.7, Arc::new(kit::Summarizer::small())));
    let s = rt.session(&agent, SessionId::new());
    let mut summaries = 0;
    for i in 0..200 {
        let mut turn = s
            .prompt(format!("Task {i}: run the tool and report."))
            .await
            .unwrap();
        while let Some(e) = turn.next_event().await {
            if let Event::StepStarted { context_tokens, .. } = e {
                assert!(
                    context_tokens < u64::from(WINDOW),
                    "turn {i}: {context_tokens}"
                );
            }
            if let Event::Compacted { .. } = e {
                summaries += 1;
            }
        }
        turn.wait().await.unwrap();
    }
    assert!(summaries > 0);
    let biggest = p
        .requests()
        .iter()
        .map(|r| {
            r.system.len()
                + r.messages
                    .iter()
                    .map(|m| serde_json::to_string(m).unwrap().len())
                    .sum::<usize>()
        })
        .max()
        .unwrap();
    // Even counting raw JSON bytes (more than the model sees), requests stay well inside.
    assert!(biggest / 3 < WINDOW as usize, "{biggest} bytes");
}

// --- Approvals ----------------------------------------------------------------------------

#[tokio::test]
async fn approve_asks_only_for_named_tools_and_remembers_always() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let a2 = asked.clone();
    let answers = Arc::new(Mutex::new(vec![
        Answer::AllowAlways,
        Answer::Deny(Some("not that".into())),
        Answer::Allow,
    ]));
    let agent = Agent::new("x")
        .tools([echo("free"), echo("guarded"), echo("other")])
        .hooks(kit::approve(["guarded", "other"], move |call: ToolCall| {
            let (a2, answers) = (a2.clone(), answers.clone());
            async move {
                a2.lock().unwrap().push(call.name.clone());
                answers.lock().unwrap().remove(0)
            }
        }));
    let p = ScriptedProvider::new()
        .calls(&[
            ("free", json!({})),
            ("guarded", json!({})),
            ("guarded", json!({})),
            ("other", json!({})),
        ])
        .call("other", json!({}))
        .text("ok");
    let rt = runtime(&p);
    let s = rt.session(&agent, SessionId::new());
    s.prompt("go").await.unwrap().wait().await.unwrap();
    // `guarded` was asked once (always), `other` twice.
    assert_eq!(*asked.lock().unwrap(), ["guarded", "other", "other"]);
    let r = results(&p.requests()[1]);
    assert!(!r[0].0 && !r[1].0 && !r[2].0);
    assert_eq!(
        r[3],
        (true, "denied: the user denied this call: not that".into())
    );
    assert!(!results(&p.requests()[2]).last().unwrap().0);
    assert_eq!(
        s.state::<bool>(&kit::approval_key("guarded"))
            .await
            .unwrap(),
        Some(true)
    );
}

#[tokio::test]
async fn always_covers_only_the_scope_it_was_given() {
    // Two hooks on one tool: any `guarded` call is asked about per tool, and dangerous ones
    // again, per exact call. "Always" in the first must not answer for the second.
    let asked = Arc::new(Mutex::new(Vec::new()));
    let asker = |hook: &'static str, asked: Arc<Mutex<Vec<String>>>| {
        move |call: ToolCall| {
            let asked = asked.clone();
            async move {
                asked
                    .lock()
                    .unwrap()
                    .push(format!("{hook} {}", call.input["cmd"]));
                Answer::AllowAlways
            }
        }
    };
    let agent = Agent::new("x")
        .tool(echo("guarded"))
        .hooks(kit::approve(["guarded"], asker("tool", asked.clone())))
        .hooks(kit::approve_when(
            |c: &ToolCall| c.input["cmd"].as_str().is_some_and(|c| c.starts_with("rm")),
            asker("call", asked.clone()),
        ));
    let p = ScriptedProvider::new()
        .calls(&[
            ("guarded", json!({"cmd": "ls"})),
            ("guarded", json!({"cmd": "rm a"})),
            ("guarded", json!({"cmd": "rm a"})),
            ("guarded", json!({"cmd": "rm b"})),
        ])
        .text("ok");
    let rt = runtime(&p);
    let s = rt.session(&agent, SessionId::new());
    s.prompt("go").await.unwrap().wait().await.unwrap();
    assert_eq!(
        *asked.lock().unwrap(),
        ["tool \"ls\"", "call \"rm a\"", "call \"rm b\""]
    );
    let scope = kit::call_scope(&ToolCall {
        id: "c".into(),
        name: "guarded".into(),
        input: json!({"cmd": "rm a"}),
    });
    assert_eq!(scope, r#"guarded {"cmd":"rm a"}"#);
    assert_eq!(
        s.state::<bool>(&kit::approval_key(&scope)).await.unwrap(),
        Some(true)
    );
}

#[tokio::test]
async fn an_abort_while_waiting_for_approval_denies_the_call() {
    let waiting = Arc::new(tokio::sync::Notify::new());
    let w2 = waiting.clone();
    let agent = Agent::new("x")
        .tool(echo("guarded"))
        .hooks(kit::approve_when(
            |c: &ToolCall| c.name == "guarded",
            move |_| {
                let w2 = w2.clone();
                async move {
                    w2.notify_one();
                    std::future::pending::<Answer>().await
                }
            },
        ));
    let p = ScriptedProvider::new().call("guarded", json!({})).text("x");
    let rt = runtime(&p);
    let s = rt.session(&agent, SessionId::new());
    let turn = s.prompt("go").await.unwrap();
    waiting.notified().await;
    turn.abort();
    assert!(matches!(turn.wait().await, Err(Error::Aborted)));
    let h = s.history().await.unwrap();
    match &h.last().unwrap().content[0] {
        Content::ToolResult { output, .. } => assert!(output.contains("aborted while waiting")),
        c => panic!("{c:?}"),
    }
}

// --- Catalog ------------------------------------------------------------------------------

#[tokio::test]
async fn catalog_searches_and_unwraps_calls_so_approvals_see_the_real_tool() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let a2 = asked.clone();
    let agent = Agent::new("x")
        .tool(echo("github__create_issue").with_deferred(true))
        .tool(echo("calendar__add_event").with_deferred(true))
        .tool(echo("shown"))
        .hooks(kit::catalog())
        .hooks(kit::approve(
            ["github__create_issue"],
            move |c: ToolCall| {
                let a2 = a2.clone();
                async move {
                    a2.lock().unwrap().push(c.name);
                    Answer::Allow
                }
            },
        ));
    let p = ScriptedProvider::new()
        .call(kit::SEARCH_TOOL, json!({ "query": "create an issue" }))
        .call(
            kit::CALL_TOOL,
            json!({ "name": "github__create_issue", "input": { "x": "bug" } }),
        )
        .call(kit::CALL_TOOL, json!({ "name": kit::CALL_TOOL }))
        .text("filed");
    let rt = runtime(&p);
    rt.session(&agent, SessionId::new())
        .prompt("file a bug")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let reqs = p.requests();
    let offered: Vec<&str> = reqs[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(offered, ["shown", kit::SEARCH_TOOL, kit::CALL_TOOL]);
    let search = &reqs[0].tools[1].description;
    assert!(search.contains("calendar (1 tool)") && search.contains("github (1 tool)"));
    let r = results(reqs.last().unwrap());
    assert!(r[0].1.starts_with("github__create_issue"), "{}", r[0].1);
    assert_eq!(r[1], (false, r#"github__create_issue: {"x":"bug"}"#.into()));
    assert!(r[2].0 && r[2].1.contains("cannot run"));
    assert_eq!(*asked.lock().unwrap(), ["github__create_issue"]);
}

#[tokio::test]
async fn without_deferred_tools_the_catalog_adds_nothing() {
    let p = ScriptedProvider::new().text("ok");
    let agent = Agent::new("x")
        .tool(echo("a"))
        .hooks(kit::catalog())
        .hooks(kit::catalog());
    runtime(&p)
        .session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(p.requests()[0].tools.len(), 1);
}

#[tokio::test]
async fn repair_wraps_a_bare_value_for_a_one_field_tool() {
    let p = ScriptedProvider::new()
        .call("lookup_order", json!(42))
        .text("ok");
    let agent = Agent::new("x").tool(lookup()).hooks(kit::repair());
    runtime(&p)
        .session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let (err, out) = results(&p.requests()[1]).pop().unwrap();
    assert!(!err && out.contains("\"42\""), "{out}");
}

// --- System delta -------------------------------------------------------------------------

#[tokio::test]
async fn system_delta_keeps_the_prompt_and_notes_changes() {
    let escalate = Tool::new("escalate", "Escalate.").run_json(
        json!({"type":"object"}),
        |_, cx: ToolCx| async move {
            cx.set_state("escalated", true);
            Ok("escalated")
        },
    );
    let refund = echo("refund");
    let agent = Agent::new("Support.")
        .tool(escalate)
        .on(move |e: hook::Step, cx| {
            let refund = refund.clone();
            async move {
                if cx.state::<bool>("escalated").unwrap_or(false) {
                    let mut e = e.tool(&refund);
                    e.instructions = "Support, escalated.".into();
                    e
                } else {
                    e
                }
            }
        })
        .hooks(kit::system_delta());
    let p = ScriptedProvider::new()
        .call("escalate", json!({}))
        .call("refund", json!({}))
        .text("Refunded.");
    let rt = runtime(&p);
    let s = rt.session(&agent, SessionId::from("d"));
    s.prompt("refund me").await.unwrap().wait().await.unwrap();

    let reqs = p.requests();
    assert!(reqs.iter().all(|r| r.system == "Support."));
    let note = reqs[1].messages.last().unwrap();
    assert_eq!(note.role, Role::System);
    assert!(
        note.text()
            .contains("They now read:\n\nSupport, escalated.")
    );
    assert!(note.text().contains("Tools now available: refund."));
    // No change on the third step, so no further note.
    let notes = |r: &ModelRequest| r.messages.iter().filter(|m| m.role == Role::System).count();
    assert_eq!(notes(&reqs[2]), 1);

    // A reopened session continues from what was declared.
    let p2 = ScriptedProvider::new().text("ok");
    let rt2 = Runtime::builder()
        .provider(p2.clone())
        .store(rt.store().clone())
        .build();
    rt2.session(&agent, SessionId::from("d"))
        .prompt("again")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(p2.requests()[0].system, "Support.");
    assert_eq!(notes(&p2.requests()[0]), 1);
}
