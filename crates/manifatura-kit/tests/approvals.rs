use manifatura_agent::{Abort, SessionId, ToolCall};
use manifatura_kit::{Answer, ApprovalEvent, Approvals, Request};
use serde_json::json;

fn request() -> Request {
    Request::new(
        SessionId::from("s"),
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: json!({"command": "make"}),
        },
    )
    .reason("why")
}

#[tokio::test]
async fn a_request_waits_until_any_client_answers_it() {
    let approvals = Approvals::new();
    let mut events = approvals.subscribe();
    let asking = {
        let a = approvals.clone();
        tokio::spawn(async move { a.ask(request(), &Abort::new()).await })
    };
    let ApprovalEvent::Requested(r) = events.recv().await.unwrap() else {
        panic!("expected a request")
    };
    assert_eq!(r.reason.as_deref(), Some("why"));
    assert_eq!(r.scope, "bash {\"command\":\"make\"}");
    assert_eq!(approvals.pending(), vec![r.clone()]);

    assert!(approvals.answer(&r.id, Answer::AllowAlways));
    assert!(
        !approvals.answer(&r.id, Answer::Deny(None)),
        "the first answer wins"
    );
    assert_eq!(asking.await.unwrap(), Answer::AllowAlways);
    assert!(matches!(
        events.recv().await.unwrap(),
        ApprovalEvent::Answered {
            answer: Answer::AllowAlways,
            ..
        }
    ));
    assert!(approvals.pending().is_empty());
}

#[tokio::test]
async fn an_abort_cancels_the_request_and_denies() {
    let approvals = Approvals::new();
    let mut events = approvals.subscribe();
    let abort = Abort::new();
    let asking = {
        let (a, abort) = (approvals.clone(), abort.clone());
        tokio::spawn(async move { a.ask(request(), &abort).await })
    };
    let ApprovalEvent::Requested(r) = events.recv().await.unwrap() else {
        panic!("expected a request")
    };
    abort.abort();
    assert!(matches!(asking.await.unwrap(), Answer::Deny(Some(_))));
    assert!(
        matches!(events.recv().await.unwrap(), ApprovalEvent::Cancelled { id, .. } if id == r.id)
    );
    assert!(!approvals.answer(&r.id, Answer::Allow));
}
