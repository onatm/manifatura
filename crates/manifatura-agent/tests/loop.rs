use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use manifatura_agent::testing::{ScriptedProvider, reply};
use manifatura_agent::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

fn runtime(p: &ScriptedProvider, store: Arc<dyn Store>) -> Runtime {
    Runtime::builder().provider(p.clone()).store(store).build()
}

fn mem() -> Arc<dyn Store> {
    Arc::new(MemoryStore::new())
}

fn kinds(entries: &[Entry]) -> Vec<&'static str> {
    entries
        .iter()
        .map(|e| match &e.record {
            Record::Header { .. } => "header",
            Record::Message { message } => match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::System => "note",
            },
            Record::State { .. } => "state",
            Record::Compaction { .. } => "compaction",
            Record::Custom { .. } => "custom",
            Record::Label { .. } => "label",
            Record::ModelChange { .. } => "model",
            _ => "other",
        })
        .collect()
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages.iter().map(Message::text).collect()
}

fn last_result(r: &ModelRequest) -> (bool, String) {
    match &r.messages.last().unwrap().content[0] {
        Content::ToolResult {
            output, is_error, ..
        } => (*is_error, output.clone()),
        c => panic!("{c:?}"),
    }
}

#[derive(Deserialize, JsonSchema)]
struct Lookup {
    order_id: String,
}

fn lookup() -> Tool {
    Tool::new("lookup_order", "Look up an order.").run(|i: Lookup, cx: ToolCx| async move {
        cx.set_state("last_order", &i.order_id);
        Ok(Json(json!({ "id": i.order_id, "status": "shipped" })))
    })
}

fn echo(name: &str) -> Tool {
    let n = name.to_string();
    Tool::new(name, "Echoes.").run_json(json!({"type":"object"}), move |i, _| {
        let n = n.clone();
        async move { Ok(format!("{n}: {i}")) }
    })
}

/// A compactor that records the plans it sees and answers with a fixed summary.
#[derive(Default)]
struct Fixed {
    summary: String,
    plans: Mutex<Vec<CompactPlan>>,
}

impl Fixed {
    fn new(summary: &str) -> Arc<Self> {
        Arc::new(Self {
            summary: summary.into(),
            plans: Mutex::default(),
        })
    }
}

#[async_trait]
impl Compactor for Fixed {
    fn keep_recent_tokens(&self) -> u64 {
        1
    }
    async fn summarize(&self, plan: &CompactPlan, _cx: &CompactCx) -> Result<String, String> {
        self.plans.lock().unwrap().push(plan.clone());
        Ok(self.summary.clone())
    }
}

// --- The loop -----------------------------------------------------------------------------

#[tokio::test]
async fn text_reply_writes_only_tree_records() {
    let p = ScriptedProvider::new().text("hello");
    let store = mem();
    let rt = runtime(&p, store.clone());
    let s = rt.session(&Agent::new("Be brief."), SessionId::from("s1"));
    let reply = s.prompt("hi").await.unwrap().wait().await.unwrap();
    assert_eq!(reply.text, "hello");

    let entries = store.load(&SessionId::from("s1")).await.unwrap();
    assert_eq!(kinds(&entries), ["header", "user", "assistant"]);
    let req = &p.requests()[0];
    assert_eq!(req.system, "Be brief.");
    assert_eq!(req.model, "scripted");
}

#[tokio::test]
async fn typed_tool_round_trip_and_state() {
    let p = ScriptedProvider::new()
        .call("lookup_order", json!({ "order_id": "42" }))
        .text("Order 42 has shipped.");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("Support.").tool(lookup()), SessionId::from("s"));
    let mut turn = s.prompt("where is 42?").await.unwrap();
    let mut names = Vec::new();
    while let Some(e) = turn.next_event().await {
        names.push(
            serde_json::to_value(&e).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert!(names.contains(&"action.started".to_string()));
    assert_eq!(names.last().unwrap(), "turn.completed");
    assert_eq!(turn.wait().await.unwrap().text, "Order 42 has shipped.");

    assert_eq!(
        p.requests()[0].tools[0].input_schema["required"],
        json!(["order_id"])
    );
    let (err, out) = last_result(&p.requests()[1]);
    assert!(!err && out.contains("shipped"));
    assert_eq!(
        s.state::<String>("last_order").await.unwrap().as_deref(),
        Some("42")
    );
}

#[tokio::test]
async fn invalid_input_unknown_tool_and_non_object_input_are_tool_errors() {
    let p = ScriptedProvider::new()
        .calls(&[
            ("lookup_order", json!({ "wrong": 1 })),
            ("nope", json!({})),
            ("lookup_order", json!("42")),
        ])
        .text("sorry");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x").tool(lookup()), SessionId::new());
    s.prompt("go").await.unwrap().wait().await.unwrap();
    let errs: Vec<(bool, String)> = p.requests()[1]
        .messages
        .last()
        .unwrap()
        .content
        .iter()
        .map(|c| match c {
            Content::ToolResult {
                output, is_error, ..
            } => (*is_error, output.clone()),
            _ => unreachable!(),
        })
        .collect();
    assert!(errs[0].0 && errs[0].1.starts_with("invalid input"));
    assert!(errs[1].0 && errs[1].1.contains("unknown tool `nope`"));
    // The core does not repair inputs: that is a hook's job.
    assert!(errs[2].0 && errs[2].1.contains("not a JSON object"));
}

#[tokio::test]
async fn parallel_tools_run_concurrently_sequential_alone() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mk = |name: &str| {
        let b = barrier.clone();
        Tool::new(name, "waits for its sibling").run_json(json!({"type":"object"}), move |_, _| {
            let b = b.clone();
            async move {
                // Deadlocks (and times out) unless both calls run at the same time.
                b.wait().await;
                Ok("done")
            }
        })
    };
    let seq = Tool::new("seq", "sequential")
        .sequential()
        .run_json(json!({"type":"object"}), |_, _| async { Ok("seq") });
    let p = ScriptedProvider::new()
        .calls(&[("a", json!({})), ("b", json!({})), ("seq", json!({}))])
        .text("ok");
    let rt = runtime(&p, mem());
    let agent = Agent::new("x").tool(mk("a")).tool(mk("b")).tool(seq);
    let s = rt.session(&agent, SessionId::new());
    let r = tokio::time::timeout(Duration::from_secs(2), async {
        s.prompt("go").await.unwrap().wait().await
    })
    .await
    .expect("parallel tools deadlocked");
    assert_eq!(r.unwrap().text, "ok");
}

#[tokio::test]
async fn length_stop_fails_every_call_without_running() {
    let ran = Arc::new(AtomicBool::new(false));
    let r2 = ran.clone();
    let tool = Tool::new("t", "t").run_json(json!({"type":"object"}), move |_, _| {
        r2.store(true, Ordering::SeqCst);
        async { Ok("ran") }
    });
    let truncated = reply(
        vec![Content::ToolCall {
            id: "c1".into(),
            name: "t".into(),
            input: json!({}),
        }],
        StopReason::Length,
    );
    let p = ScriptedProvider::new().respond(truncated).text("later");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x").tool(tool), SessionId::new());
    s.prompt("go").await.unwrap().wait().await.unwrap();
    assert!(!ran.load(Ordering::SeqCst));
    let (err, out) = last_result(&p.requests()[1]);
    assert!(err && out.contains("token limit"));
}

#[tokio::test]
async fn deferred_tools_are_not_offered_but_can_be_called() {
    let p = ScriptedProvider::new().call("hidden", json!({})).text("ok");
    let rt = runtime(&p, mem());
    let agent = Agent::new("x")
        .tool(echo("shown"))
        .tool(echo("hidden").with_deferred(true));
    let s = rt.session(&agent, SessionId::new());
    s.prompt("go").await.unwrap().wait().await.unwrap();
    let reqs = p.requests();
    let offered: Vec<&str> = reqs[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(offered, ["shown"]);
    let (err, out) = last_result(&reqs[1]);
    assert!(!err && out.starts_with("hidden"));
}

#[tokio::test]
async fn reload_restores_history_and_state_and_repairs_dangling_calls() {
    let store = mem();
    let id = SessionId::from("resume");
    {
        let p = ScriptedProvider::new()
            .call("lookup_order", json!({"order_id": "9"}))
            .text("done");
        let rt = runtime(&p, store.clone());
        let s = rt.session(&Agent::new("x").tool(lookup()), id.clone());
        s.prompt("one").await.unwrap().wait().await.unwrap();
    }
    // Simulate a crash after an assistant tool call was recorded, before its results were.
    let entries = store.load(&id).await.unwrap();
    let leaf = entries.last().unwrap().id.clone();
    let user = Entry::new(
        Some(leaf),
        Record::Message {
            message: Message::user("two"),
        },
    );
    let call = Entry::new(
        Some(user.id.clone()),
        Record::Message {
            message: Message::new(
                Role::Assistant,
                vec![Content::ToolCall {
                    id: "c9".into(),
                    name: "lookup_order".into(),
                    input: json!({}),
                }],
            ),
        },
    );
    store.append(&id, &[user, call]).await.unwrap();

    let p = ScriptedProvider::new().text("back");
    let rt = runtime(&p, store.clone());
    let s = rt.session(&Agent::new("x").tool(lookup()), id.clone());
    assert_eq!(
        s.state::<String>("last_order").await.unwrap().as_deref(),
        Some("9")
    );
    s.prompt("three").await.unwrap().wait().await.unwrap();

    let msgs = &p.requests()[0].messages;
    let repaired = &msgs[msgs.len() - 2];
    assert_eq!(repaired.role, Role::Tool);
    assert!(matches!(
        &repaired.content[0],
        Content::ToolResult { is_error: true, output, .. } if output.contains("stopped")
    ));
    assert_eq!(msgs.last().unwrap().text(), "three");
}

#[tokio::test]
async fn abort_ends_the_turn_and_keeps_what_finished() {
    let slow =
        Tool::new("slow", "slow").run_json(json!({"type":"object"}), |_, cx: ToolCx| async move {
            cx.abort().aborted().await;
            Ok("stopped")
        });
    let p = ScriptedProvider::new()
        .call("slow", json!({}))
        .text("never");
    let store = mem();
    let rt = runtime(&p, store.clone());
    let s = rt.session(&Agent::new("x").tool(slow), SessionId::from("a"));
    let mut turn = s.prompt("go").await.unwrap();
    let mut outcome = None;
    while let Some(e) = turn.next_event().await {
        if matches!(e, Event::ActionStarted { .. }) {
            turn.abort();
        }
        if let Event::TurnCompleted { outcome: o, .. } = e {
            outcome = Some(o);
        }
    }
    assert_eq!(outcome, Some(Outcome::Aborted));
    assert!(matches!(turn.wait().await, Err(Error::Aborted)));
    assert_eq!(p.requests().len(), 1);
    let entries = store.load(&SessionId::from("a")).await.unwrap();
    assert_eq!(kinds(&entries), ["header", "user", "assistant", "tool"]);
}

#[tokio::test]
async fn turns_run_in_order() {
    let p = ScriptedProvider::new().text("1").text("2").text("3");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x"), SessionId::new());
    let t1 = s.prompt("a").await.unwrap();
    let t2 = s.prompt("b").await.unwrap();
    let t3 = s.prompt("c").await.unwrap();
    let (r3, r1, r2) = (t3.wait().await, t1.wait().await, t2.wait().await);
    assert_eq!(
        [r1.unwrap().text, r2.unwrap().text, r3.unwrap().text],
        ["1", "2", "3"]
    );
    assert_eq!(texts(&p.requests()[2].messages), ["a", "1", "b", "2", "c"]);
}

#[tokio::test]
async fn steering_reaches_the_next_step_and_follow_ups_the_end() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let g = gate.clone();
    let wait = Tool::new("wait", "waits").run_json(json!({"type":"object"}), move |_, _| {
        let g = g.clone();
        async move {
            g.notified().await;
            Ok("waited")
        }
    });
    let p = ScriptedProvider::new()
        .call("wait", json!({}))
        .text("first")
        .text("second");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x").tool(wait), SessionId::new());
    assert!(!s.steer("too early"));
    let mut turn = s.prompt("go").await.unwrap();
    while let Some(e) = turn.next_event().await {
        if matches!(e, Event::ActionStarted { .. }) {
            assert!(s.steer("also this"));
            assert!(s.follow_up("and then this"));
            gate.notify_one();
        }
    }
    assert_eq!(turn.wait().await.unwrap().text, "second");
    let reqs = p.requests();
    assert_eq!(reqs[1].messages.last().unwrap().text(), "also this");
    assert_eq!(reqs[2].messages.last().unwrap().text(), "and then this");
}

#[tokio::test]
async fn a_message_sent_as_the_turn_ends_is_refused_not_lost() {
    // `turn.completed` hooks run after the turn has taken its last messages: anything steered
    // from here on must be refused, so its sender knows to start a new turn.
    let session: Arc<std::sync::OnceLock<Session>> = Arc::default();
    let accepted: Arc<Mutex<Vec<bool>>> = Arc::default();
    let (s2, a2) = (session.clone(), accepted.clone());
    let agent = Agent::new("x").on(move |e: Event, _cx| {
        let (s, a) = (s2.clone(), a2.clone());
        async move {
            if matches!(e, Event::TurnCompleted { .. }) {
                let s = s.get().unwrap();
                a.lock()
                    .unwrap()
                    .extend([s.steer("late"), s.follow_up("later")]);
            }
        }
    });
    let p = ScriptedProvider::new().text("one").text("two");
    let rt = runtime(&p, mem());
    let s = session.get_or_init(|| rt.session(&agent, SessionId::new()));
    s.prompt("a").await.unwrap().wait().await.unwrap();
    assert_eq!(*accepted.lock().unwrap(), [false, false]);
    s.prompt("b").await.unwrap().wait().await.unwrap();
    assert_eq!(texts(&p.requests()[1].messages), ["a", "one", "b"]);
}

#[tokio::test]
async fn branch_continues_from_an_earlier_entry() {
    let p = ScriptedProvider::new().text("r1").text("r2").text("r3");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x"), SessionId::new());
    s.prompt("a").await.unwrap().wait().await.unwrap();
    s.prompt("b").await.unwrap().wait().await.unwrap();
    let entries = s.entries().await.unwrap();
    let first_reply = entries
        .iter()
        .find(|e| matches!(&e.record, Record::Message { message } if message.text() == "r1"))
        .unwrap()
        .id
        .clone();
    s.branch(Some(first_reply)).await.unwrap();
    s.prompt("c").await.unwrap().wait().await.unwrap();
    assert_eq!(texts(&p.requests()[2].messages), ["a", "r1", "c"]);
    assert_eq!(texts(&s.history().await.unwrap()), ["a", "r1", "c", "r3"]);
    assert!(s.branch(Some("nope".into())).await.is_err());
}

#[tokio::test]
async fn model_and_reasoning_changes_persist() {
    let store = mem();
    let p = ScriptedProvider::new().text("1").text("2");
    let rt = runtime(&p, store.clone());
    let s = rt.session(&Agent::new("x").model("a/one"), SessionId::from("m"));
    s.prompt("x").await.unwrap().wait().await.unwrap();
    s.set_model("b/two").await.unwrap();
    s.set_reasoning(Effort::High).await.unwrap();
    s.set_label("my session").await.unwrap();
    assert_eq!(s.status().model.as_deref(), Some("b/two"));
    s.prompt("y").await.unwrap().wait().await.unwrap();
    let reqs = p.requests();
    assert_eq!(
        (reqs[0].model.as_str(), reqs[1].model.as_str()),
        ("a/one", "b/two")
    );
    assert_eq!(reqs[1].reasoning, Some(Effort::High));

    let p2 = ScriptedProvider::new().text("3");
    let rt2 = runtime(&p2, store.clone());
    let s2 = rt2.session(&Agent::new("x").model("a/one"), SessionId::from("m"));
    s2.prompt("z").await.unwrap().wait().await.unwrap();
    assert_eq!(p2.requests()[0].model, "b/two");
    assert_eq!(s2.status().label.as_deref(), Some("my session"));
}

#[tokio::test]
async fn a_panicking_provider_fails_the_turn_instead_of_hanging() {
    let p = ScriptedProvider::new().with(|_| panic!("provider bug"));
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x"), SessionId::new());
    let mut turn = s.prompt("go").await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(5), async {
        while turn.next_event().await.is_some() {}
        turn.wait().await
    })
    .await
    .expect("turn hung after a panic");
    assert!(matches!(r, Err(Error::Failed(m)) if m.contains("panicked")));
}

// --- Hooks: one test per event and its composition rule ------------------------------------

#[tokio::test]
async fn session_start_says_whether_the_session_existed() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let store = mem();
    for _ in 0..2 {
        let p = ScriptedProvider::new().text("ok");
        let seen = seen.clone();
        let agent = Agent::new("x").on(move |e: hook::SessionStart, _cx| {
            let seen = seen.clone();
            async move { seen.lock().unwrap().push(e.resumed) }
        });
        let rt = runtime(&p, store.clone());
        let s = rt.session(&agent, SessionId::from("ss"));
        s.prompt("hi").await.unwrap().wait().await.unwrap();
    }
    assert_eq!(*seen.lock().unwrap(), [false, true]);
}

#[tokio::test]
async fn input_hooks_chain_and_none_drops_the_prompt() {
    let p = ScriptedProvider::new().text("ok");
    let agent = Agent::new("x")
        .on(|e: hook::Input, _cx| async move {
            (e.text != "/ignore").then(|| hook::Input {
                text: e.text.to_uppercase(),
            })
        })
        .on(|e: hook::Input, _cx| async move {
            Some(hook::Input {
                text: format!("{}!", e.text),
            })
        });
    let rt = runtime(&p, mem());
    let s = rt.session(&agent, SessionId::new());
    let dropped = s.prompt("/ignore").await.unwrap().wait().await.unwrap();
    assert!(dropped.message.is_none());
    assert!(p.requests().is_empty());
    s.prompt("hi").await.unwrap().wait().await.unwrap();
    assert_eq!(texts(&p.requests()[0].messages), ["HI!"]);
}

#[tokio::test]
async fn step_hook_adds_a_tool_once_state_is_set() {
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
                    e.tool(&refund)
                } else {
                    e
                }
            }
        });
    let p = ScriptedProvider::new()
        .call("escalate", json!({}))
        .call("refund", json!({}))
        .text("Refunded.");
    let rt = runtime(&p, mem());
    let s = rt.session(&agent, SessionId::new());
    s.prompt("refund me").await.unwrap().wait().await.unwrap();
    let names = |r: &ModelRequest| r.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    let reqs = p.requests();
    assert_eq!(names(&reqs[0]), ["escalate"]);
    assert_eq!(names(&reqs[1]), ["escalate", "refund"]);
    // No system-delta note: that is a hook bundle's job, not the loop's.
    assert!(reqs[1].messages.iter().all(|m| m.role != Role::System));
}

#[tokio::test]
async fn context_hook_changes_the_request_not_the_session() {
    let p = ScriptedProvider::new().text("1").text("2");
    let agent = Agent::new("x").on(|mut e: hook::Context, _cx| async move {
        e.messages.retain(|m| m.role == Role::User);
        e
    });
    let rt = runtime(&p, mem());
    let s = rt.session(&agent, SessionId::new());
    s.prompt("a").await.unwrap().wait().await.unwrap();
    s.prompt("b").await.unwrap().wait().await.unwrap();
    assert_eq!(texts(&p.requests()[1].messages), ["a", "b"]);
    assert_eq!(texts(&s.history().await.unwrap()), ["a", "1", "b", "2"]);
}

#[tokio::test]
async fn request_hooks_chain_in_order() {
    let p = ScriptedProvider::new().text("ok");
    let agent = Agent::new("x")
        .on(|mut e: hook::Request, _cx| async move {
            e.request.max_tokens = Some(100);
            e
        })
        .on(|mut e: hook::Request, _cx| async move {
            e.request.max_tokens = e.request.max_tokens.map(|n| n * 2);
            e.request.system.push_str(" More.");
            e
        });
    let rt = runtime(&p, mem());
    rt.session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let req = &p.requests()[0];
    assert_eq!(req.max_tokens, Some(200));
    assert_eq!(req.system, "x More.");
}

#[tokio::test]
async fn model_error_fails_without_a_hook_and_the_first_retry_wins() {
    let overflow = || Response::error(ErrorKind::ContextOverflow, "prompt is too long");

    // No hook: the turn fails with the provider's error, its kind intact, and
    // `turn.completed` carries the bare message.
    let p = ScriptedProvider::new().respond(overflow());
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x"), SessionId::new());
    let mut turn = s.prompt("go").await.unwrap();
    let mut reported = None;
    while let Some(e) = turn.next_event().await {
        if let Event::TurnCompleted { error, .. } = e {
            reported = error;
        }
    }
    let r = turn.wait().await;
    assert!(
        matches!(&r, Err(Error::Model(e)) if e.kind == ErrorKind::ContextOverflow && e.message == "prompt is too long"),
        "{r:?}"
    );
    assert_eq!(reported.as_deref(), Some("prompt is too long"));

    // Two hooks: the first passes, the second retries.
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let a = attempts.clone();
    let agent = Agent::new("x")
        .on(|_: hook::ModelError, _cx| async { hook::Recover::Fail })
        .on(move |e: hook::ModelError, _cx| {
            let a = a.clone();
            async move {
                a.lock().unwrap().push((e.error.kind, e.attempt));
                hook::Recover::Retry
            }
        });
    let p = ScriptedProvider::new().respond(overflow()).text("fine");
    let rt2 = runtime(&p, mem());
    let s = rt2.session(&agent, SessionId::new());
    assert_eq!(
        s.prompt("go").await.unwrap().wait().await.unwrap().text,
        "fine"
    );
    assert_eq!(*attempts.lock().unwrap(), [(ErrorKind::ContextOverflow, 1)]);
}

#[tokio::test]
async fn a_hook_that_always_retries_is_cut_off() {
    let p = ScriptedProvider::new().fallback(|_| Response::error(ErrorKind::Server, "down"));
    let agent = Agent::new("x").on(|_: hook::ModelError, _cx| async { hook::Recover::Retry });
    let rt = runtime(&p, mem());
    let s = rt.session(&agent, SessionId::new());
    let r = s.prompt("go").await.unwrap().wait().await;
    assert!(
        matches!(&r, Err(Error::Model(e)) if e.kind == ErrorKind::Server),
        "{r:?}"
    );
    assert_eq!(p.requests().len(), 9);
}

#[tokio::test]
async fn overflow_hook_compacts_then_retries() {
    let compactor = Fixed::new("S");
    let c: Arc<dyn Compactor> = compactor.clone();
    let agent = Agent::new("x").on(move |e: hook::ModelError, cx| {
        let c = c.clone();
        async move {
            if e.error.kind == ErrorKind::ContextOverflow && e.attempt == 1 {
                cx.compact(c, None);
                hook::Recover::Retry
            } else {
                hook::Recover::Fail
            }
        }
    });
    let p = ScriptedProvider::new()
        .text("one")
        .text("two")
        .respond(Response::error(ErrorKind::ContextOverflow, "too long"))
        .text("three");
    let rt = runtime(&p, mem());
    let s = rt.session(&agent, SessionId::new());
    for m in ["a", "b"] {
        s.prompt(m).await.unwrap().wait().await.unwrap();
    }
    let mut turn = s.prompt("c").await.unwrap();
    let mut compacted = false;
    while let Some(e) = turn.next_event().await {
        compacted |= matches!(e, Event::Compacted { .. });
    }
    assert_eq!(turn.wait().await.unwrap().text, "three");
    assert!(compacted);
    let last = p.requests().last().unwrap().clone();
    assert!(last.messages[0].text().ends_with("S"));
    assert_eq!(last.messages.last().unwrap().text(), "c");
    assert_eq!(compactor.plans.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn response_hook_changes_what_is_recorded() {
    let p = ScriptedProvider::new().text("raw");
    let agent = Agent::new("x").on(|mut e: hook::Response, _cx| async move {
        e.message = Message::assistant(format!("[{}]", e.message.text()));
        e
    });
    let rt = runtime(&p, mem());
    let s = rt.session(&agent, SessionId::new());
    assert_eq!(
        s.prompt("go").await.unwrap().wait().await.unwrap().text,
        "[raw]"
    );
    assert_eq!(texts(&s.history().await.unwrap()), ["go", "[raw]"]);
}

#[tokio::test]
async fn step_end_reports_usage_and_context() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let mut resp = reply(vec![Content::text("ok")], StopReason::Stop);
    resp.usage.input = 1_000;
    resp.usage.output = 10;
    let p = ScriptedProvider::new().respond(resp);
    let agent = Agent::new("x")
        .context_window(8_000)
        .on(move |e: hook::StepEnd, _cx| {
            let s2 = s2.clone();
            async move {
                s2.lock()
                    .unwrap()
                    .push((e.usage.input, e.context_tokens, e.context_window))
            }
        });
    let rt = runtime(&p, mem());
    rt.session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), [(1_000, 1_010, Some(8_000))]);
}

#[tokio::test]
async fn tool_call_hooks_chain_deny_stops_and_result_short_circuits() {
    let ran = Arc::new(AtomicUsize::new(0));
    let r2 = ran.clone();
    let counted =
        Tool::new("counted", "Counts.").run_json(json!({"type":"object"}), move |_, _| {
            r2.fetch_add(1, Ordering::SeqCst);
            async { Ok("ran") }
        });
    let later_saw = Arc::new(Mutex::new(Vec::new()));
    let ls = later_saw.clone();
    let agent = Agent::new("x")
        .tool(counted)
        .tool(echo("real"))
        .on(|e: hook::ToolCall, _cx| async move {
            match e.call.input["mode"].as_str() {
                Some("deny") => hook::Decision::Deny("not today".into()),
                Some("stub") => hook::Decision::Result(ToolResult::ok("stubbed")),
                Some("redirect") => {
                    let mut e = e;
                    e.call.name = "real".into();
                    e.call.id = "changed-id".into();
                    hook::Decision::Run(e)
                }
                _ => hook::Decision::Run(e),
            }
        })
        .on(move |e: hook::ToolCall, _cx| {
            let ls = ls.clone();
            async move {
                ls.lock().unwrap().push(e.call.name.clone());
                hook::Decision::Run(e)
            }
        });
    let p = ScriptedProvider::new()
        .calls(&[
            ("counted", json!({"mode": "deny"})),
            ("counted", json!({"mode": "stub"})),
            ("counted", json!({"mode": "redirect"})),
            ("counted", json!({})),
        ])
        .text("done");
    let rt = runtime(&p, mem());
    rt.session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let results: Vec<(String, String)> = p.requests()[1]
        .messages
        .last()
        .unwrap()
        .content
        .iter()
        .map(|c| match c {
            Content::ToolResult {
                call_id, output, ..
            } => (call_id.clone(), output.clone()),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(results[0].1, "denied: not today");
    assert_eq!(results[1].1, "stubbed");
    // Redirected to another tool; the call id stays the model's.
    assert_eq!(
        results[2],
        ("call_0_2".into(), r#"real: {"mode":"redirect"}"#.into())
    );
    assert_eq!(results[3].1, "ran");
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    // Deny and Result stop the chain; Run passes the (changed) call on.
    assert_eq!(*later_saw.lock().unwrap(), ["real", "counted"]);
}

#[tokio::test]
async fn tool_result_hooks_chain_for_every_result() {
    let agent = Agent::new("x")
        .tool(echo("a"))
        .on(|_: hook::ToolCall, _cx| async { hook::Decision::Deny("no".into()) })
        .on(|mut e: hook::ToolResult, _cx| async move {
            e.result.output = format!("<{}>", e.result.output);
            e
        })
        .on(|mut e: hook::ToolResult, _cx| async move {
            e.result.output.push('!');
            e
        });
    let p = ScriptedProvider::new().call("a", json!({})).text("ok");
    let rt = runtime(&p, mem());
    rt.session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(
        last_result(&p.requests()[1]),
        (true, "<denied: no>!".into())
    );
}

#[tokio::test]
async fn turn_end_first_follow_up_wins() {
    let agent = Agent::new("x")
        .on(|_: hook::TurnEnd, _cx| async { hook::Next::Done })
        .on(|e: hook::TurnEnd, _cx| async move {
            if e.message.text() == "first" {
                hook::Next::FollowUp("say second".into())
            } else {
                hook::Next::Done
            }
        })
        .on(|_: hook::TurnEnd, _cx| async { hook::Next::FollowUp("never".into()) });
    let p = ScriptedProvider::new()
        .text("first")
        .text("second")
        .text("x");
    let rt = runtime(&p, mem());
    let s = rt.session(&agent.max_steps(3), SessionId::new());
    // The third hook would follow up forever; max_steps stops it, which shows the second hook's
    // FollowUp won the first round.
    let r = s.prompt("go").await.unwrap().wait().await;
    assert!(r.is_err());
    assert_eq!(
        p.requests()[1].messages.last().unwrap().text(),
        "say second"
    );
    assert_eq!(p.requests()[2].messages.last().unwrap().text(), "never");
}

#[tokio::test]
async fn runtime_hooks_run_before_agent_hooks() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let (o1, o2) = (order.clone(), order.clone());
    let p = ScriptedProvider::new().text("ok");
    let rt = Runtime::builder()
        .provider(p.clone())
        .on(move |_: hook::TurnStart, _cx| {
            let o1 = o1.clone();
            async move { o1.lock().unwrap().push("runtime") }
        })
        .build();
    let agent = Agent::new("x").on(move |_: hook::TurnStart, _cx| {
        let o2 = o2.clone();
        async move { o2.lock().unwrap().push("agent") }
    });
    rt.session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(*order.lock().unwrap(), ["runtime", "agent"]);
}

#[tokio::test]
async fn event_hooks_see_every_event_in_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let agent = Agent::new("x").tool(echo("a")).on(move |e: Event, _cx| {
        let s2 = s2.clone();
        async move {
            let v = serde_json::to_value(&e).unwrap();
            s2.lock()
                .unwrap()
                .push(v["type"].as_str().unwrap().to_string());
        }
    });
    let p = ScriptedProvider::new().call("a", json!({})).text("ok");
    let rt = runtime(&p, mem());
    rt.session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "turn.started",
            "step.started",
            "message.appended",
            "action.started",
            "action.result",
            "message.appended",
            "step.started",
            "text.delta",
            "message.appended",
            "turn.completed"
        ]
    );
}

#[tokio::test]
async fn hook_cx_writes_state_notes_and_custom_records_at_safe_points() {
    let agent = Agent::new("x")
        .tool(echo("a"))
        .on(|e: hook::ToolCall, cx| async move {
            cx.set_state("calls", cx.state::<u32>("calls").unwrap_or(0) + 1);
            cx.note("remember the rules");
            cx.custom("audit", json!({ "tool": e.call.name }));
            hook::Decision::Run(e)
        });
    let p = ScriptedProvider::new()
        .calls(&[("a", json!({})), ("a", json!({}))])
        .text("ok");
    let store = mem();
    let rt = runtime(&p, store.clone());
    let s = rt.session(&agent, SessionId::from("cx"));
    s.prompt("go").await.unwrap().wait().await.unwrap();
    // The second hook saw the first one's pending write.
    assert_eq!(s.state::<u32>("calls").await.unwrap(), Some(2));
    // Notes wait until the batch is recorded, so a call and its results stay together.
    let entries = store.load(&SessionId::from("cx")).await.unwrap();
    assert_eq!(
        kinds(&entries),
        [
            "header",
            "user",
            "assistant",
            "tool",
            "state",
            "state",
            "custom",
            "custom",
            "note",
            "note",
            "assistant"
        ]
    );
    assert_eq!(
        p.requests()[1].messages.last().unwrap().text(),
        "remember the rules"
    );
    assert!(
        !p.requests()[1]
            .messages
            .iter()
            .any(|m| m.text().contains("audit"))
    );
}

#[tokio::test]
async fn runtime_hooks_apply_to_sessions_a_tool_opens() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let p = ScriptedProvider::new()
        .call("delegate", json!({}))
        .text("child")
        .text("done");
    let rt = Runtime::builder()
        .provider(p.clone())
        .on(move |_: hook::TurnStart, cx| {
            let s2 = s2.clone();
            async move { s2.lock().unwrap().push(cx.session_id().to_string()) }
        })
        .build();
    let child = Agent::new("Child.");
    let delegate = Tool::new("delegate", "Delegates.").run_json(
        json!({"type":"object"}),
        move |_, cx: ToolCx| {
            let child = child.clone();
            async move {
                let id = SessionId::from(format!("{}.c", cx.session_id()));
                let s = cx.runtime().session(&child, id);
                let turn = s.prompt("work").await.map_err(ToolError::msg)?;
                Ok(turn.wait().await.map_err(ToolError::msg)?.text)
            }
        },
    );
    let s = rt.session(&Agent::new("Lead.").tool(delegate), SessionId::from("p"));
    assert_eq!(
        s.prompt("go").await.unwrap().wait().await.unwrap().text,
        "done"
    );
    assert_eq!(*seen.lock().unwrap(), ["p", "p.c"]);
}

// --- Compaction ---------------------------------------------------------------------------

#[tokio::test]
async fn compaction_never_runs_by_itself() {
    let p = ScriptedProvider::new()
        .fallback(|_| reply(vec![Content::text("word ".repeat(2_000))], StopReason::Stop));
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x").context_window(2_000), SessionId::new());
    for i in 0..5 {
        s.prompt(format!("turn {i}"))
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
    }
    assert_eq!(s.history().await.unwrap().len(), 10);
    assert!(s.status().context_tokens > 2_000);
}

#[tokio::test]
async fn manual_compact_plans_with_pi_cut_rules_and_focus() {
    let long = "a long answer. ".repeat(50);
    let p = ScriptedProvider::new()
        .text(&long)
        .call("a", json!({}))
        .text(&long)
        .text("3");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x").tool(echo("a")), SessionId::new());
    s.prompt("first").await.unwrap().wait().await.unwrap();
    s.prompt("second").await.unwrap().wait().await.unwrap();
    let c = Fixed::new("focused summary");
    let r = s.compact(c.clone(), Some("the API".into())).await.unwrap();
    assert!(r.summarized && r.tokens_after < r.tokens_before);
    let plans = c.plans.lock().unwrap().clone();
    assert_eq!(plans[0].focus.as_deref(), Some("the API"));
    assert!(plans[0].previous_summary.is_none());
    // The cut never ends between a call and its result.
    assert_ne!(plans[0].messages.last().unwrap().role, Role::Assistant);
    let h = s.history().await.unwrap();
    assert!(h[0].text().ends_with("focused summary"));

    // A second compaction hands over the previous summary.
    s.prompt("third").await.unwrap().wait().await.unwrap();
    let c2 = Fixed::new("again");
    assert!(s.compact(c2.clone(), None).await.unwrap().summarized);
    let plans = c2.plans.lock().unwrap().clone();
    assert_eq!(
        plans[0].previous_summary.as_deref(),
        Some("focused summary")
    );
    assert!(plans[0].messages[0].text().ends_with("focused summary"));
}

#[tokio::test]
async fn before_compact_can_cancel_or_supply_the_summary() {
    let cancel = Arc::new(AtomicBool::new(true));
    let c2 = cancel.clone();
    let done = Arc::new(Mutex::new(Vec::new()));
    let d2 = done.clone();
    let agent = Agent::new("x")
        .on(move |e: hook::BeforeCompact, _cx| {
            let c2 = c2.clone();
            async move {
                if c2.load(Ordering::SeqCst) {
                    hook::CompactDecision::Cancel
                } else if e.plan.focus.as_deref() == Some("mine") {
                    hook::CompactDecision::Summary("from the hook".into())
                } else {
                    hook::CompactDecision::Proceed(e)
                }
            }
        })
        .on(move |e: hook::Compacted, _cx| {
            let d2 = d2.clone();
            async move { d2.lock().unwrap().push(e.summary) }
        });
    let p = ScriptedProvider::new().text("1").text("2");
    let rt = runtime(&p, mem());
    let s = rt.session(&agent, SessionId::new());
    s.prompt("a").await.unwrap().wait().await.unwrap();
    s.prompt("b").await.unwrap().wait().await.unwrap();

    let c = Fixed::new("compactor");
    let r = s.compact(c.clone(), None).await.unwrap();
    assert!(!r.summarized);
    cancel.store(false, Ordering::SeqCst);
    let r = s.compact(c.clone(), Some("mine".into())).await.unwrap();
    assert!(r.summarized);
    assert!(c.plans.lock().unwrap().is_empty());
    assert_eq!(*done.lock().unwrap(), ["from the hook"]);
    assert!(
        s.history().await.unwrap()[0]
            .text()
            .ends_with("from the hook")
    );
}

#[tokio::test]
async fn compacting_a_short_session_does_nothing() {
    let p = ScriptedProvider::new().text("1");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("x"), SessionId::new());
    s.prompt("a").await.unwrap().wait().await.unwrap();
    let r = s.compact(Fixed::new("S"), None).await.unwrap();
    assert!(!r.summarized);
}

#[tokio::test]
async fn a_hook_can_fail_the_turn_before_the_model_is_called() {
    let p = ScriptedProvider::new().text("never");
    let agent = Agent::new("x").on(|e: hook::Step, cx| async move {
        assert!(cx.state_map().is_empty());
        cx.fail("agent.lua: syntax error");
        e
    });
    let rt = runtime(&p, mem());
    let r = rt
        .session(&agent, SessionId::new())
        .prompt("go")
        .await
        .unwrap()
        .wait()
        .await;
    assert!(matches!(r, Err(Error::Failed(m)) if m == "agent.lua: syntax error"));
    assert!(p.requests().is_empty());
}

#[tokio::test]
async fn a_text_tool_gets_the_text_however_it_arrives() {
    let echo = Tool::new("note", "Keep a note.")
        .grammar(Grammar::regex("[a-z ]+"))
        .run_text(|text: String, _cx| async move { Ok(format!("noted: {text}")) });
    assert_eq!(
        echo.spec().format,
        ToolFormat::Text {
            grammar: Some(Grammar::regex("[a-z ]+"))
        }
    );
    assert_eq!(echo.spec().input_schema, text_input_schema());

    let cx = || ToolCx::detached("c1");
    let r = echo.call(json!({ "input": "buy milk" }), cx()).await;
    assert_eq!((r.output.as_str(), r.is_error), ("noted: buy milk", false));
    let r = echo.call(json!("buy bread"), cx()).await;
    assert_eq!(r.output, "noted: buy bread", "a bare string is the text");
    let r = echo.call(json!({ "text": "x" }), cx()).await;
    assert!(r.is_error);
    assert!(r.output.contains("`input`"));

    let json_tool =
        Tool::new("t", "d").run_json(json!({"type": "object"}), |_, _| async { Ok("") });
    assert!(json_tool.spec().format.is_json());
}

#[test]
fn efforts_read_their_api_names() {
    for e in Effort::ALL {
        assert_eq!(Effort::parse(e.as_str()), Some(e));
    }
    assert_eq!(Effort::parse("Off"), Some(Effort::Off));
    assert_eq!(Effort::parse("XHIGH"), Some(Effort::XHigh));
    assert_eq!(serde_json::to_value(Effort::XHigh).unwrap(), json!("xhigh"));
    assert_eq!(Effort::parse("extreme"), None);
}

#[tokio::test]
async fn the_path_follows_the_current_branch() {
    let p = ScriptedProvider::new()
        .text("one")
        .text("two")
        .text("three");
    let rt = runtime(&p, mem());
    let s = rt.session(&Agent::new("t").model("m"), SessionId::new());
    s.prompt("a").await.unwrap().wait().await.unwrap();
    s.prompt("b").await.unwrap().wait().await.unwrap();
    let texts = |path: &[Entry]| -> Vec<String> {
        path.iter()
            .filter_map(|e| match &e.record {
                Record::Message { message } => Some(message.text()),
                _ => None,
            })
            .collect()
    };
    let path = s.path().await.unwrap();
    assert_eq!(texts(&path), ["a", "one", "b", "two"]);
    // Back to before "b": the next prompt starts a branch beside it.
    let one = path
        .iter()
        .find(|e| texts(std::slice::from_ref(e)) == ["one"])
        .unwrap()
        .id
        .clone();
    s.branch(Some(one)).await.unwrap();
    assert_eq!(texts(&s.path().await.unwrap()), ["a", "one"]);
    let so_far: Vec<String> = s
        .history_so_far()
        .await
        .unwrap()
        .iter()
        .map(Message::text)
        .collect();
    assert_eq!(so_far, ["a", "one"]);
    s.prompt("c").await.unwrap().wait().await.unwrap();
    assert_eq!(texts(&s.path().await.unwrap()), ["a", "one", "c", "three"]);
    assert_eq!(
        texts(&s.entries().await.unwrap()).len(),
        6,
        "the other branch stays in the session"
    );
}

#[tokio::test]
async fn what_is_recorded_so_far_is_read_while_a_turn_runs() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let g = gate.clone();
    let wait = Tool::new("wait", "waits").run_json(json!({"type":"object"}), move |_, _| {
        let g = g.clone();
        async move {
            g.notified().await;
            Ok("waited")
        }
    });
    let p = ScriptedProvider::new().call("wait", json!({})).text("done");
    let store = mem();
    let rt = runtime(&p, store.clone());
    let s = rt.session(&Agent::new("x").tool(wait), SessionId::new());
    let mut turn = s.prompt("go").await.unwrap();
    while let Some(e) = turn.next_event().await {
        if matches!(e, Event::ActionStarted { .. }) {
            // `history` would wait for the turn; this reads what the store has now.
            let so_far = s.history_so_far().await.unwrap();
            assert_eq!(so_far[0].text(), "go");
            assert_eq!(so_far.len(), 2, "the prompt and the call");
            gate.notify_one();
        }
    }
    turn.wait().await.unwrap();
    // A handle on another runtime, never used, reads the session from its last entry.
    let other = runtime(&ScriptedProvider::new(), store);
    let texts: Vec<String> = other
        .session(&Agent::new("x"), s.id().clone())
        .history_so_far()
        .await
        .unwrap()
        .iter()
        .map(Message::text)
        .collect();
    assert_eq!(texts.last().unwrap(), "done");
}
