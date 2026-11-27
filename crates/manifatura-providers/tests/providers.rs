use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use manifatura_agent::*;
use manifatura_providers::{Models, OpenAi};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A canned HTTP response: status, extra headers, and body chunks written with a flush between
/// each (so SSE events arrive split across reads).
struct Canned {
    status: u16,
    headers: Vec<(&'static str, String)>,
    chunks: Vec<String>,
}

fn sse(chunks: Vec<String>) -> Canned {
    Canned {
        status: 200,
        headers: vec![("content-type", "text/event-stream".into())],
        chunks,
    }
}

#[derive(Clone, Default)]
struct Mock {
    responses: Arc<Mutex<VecDeque<Canned>>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Mock {
    async fn start(responses: Vec<Canned>) -> (Self, String) {
        let mock = Mock {
            responses: Arc::new(Mutex::new(responses.into())),
            ..Default::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let m = mock.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let m = m.clone();
                tokio::spawn(async move {
                    let (head, body) = read_request(&mut sock).await;
                    m.requests
                        .lock()
                        .unwrap()
                        .push((head, serde_json::from_slice(&body).unwrap_or(Value::Null)));
                    let c = m
                        .responses
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected request");
                    let mut out = format!(
                        "HTTP/1.1 {} X\r\ntransfer-encoding: chunked\r\nconnection: close\r\n",
                        c.status
                    );
                    for (k, v) in &c.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    sock.write_all(out.as_bytes()).await.unwrap();
                    for chunk in c.chunks {
                        let framed = format!("{:x}\r\n{chunk}\r\n", chunk.len());
                        sock.write_all(framed.as_bytes()).await.unwrap();
                        sock.flush().await.unwrap();
                        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    }
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                });
            }
        });
        (mock, url)
    }

    fn requests(&self) -> Vec<(String, Value)> {
        self.requests.lock().unwrap().clone()
    }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = sock.read(&mut tmp).await.unwrap();
        buf.extend_from_slice(&tmp[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < end + 4 + len {
                let n = sock.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
            }
            return (head, buf[end + 4..end + 4 + len].to_vec());
        }
    }
}

fn request() -> ModelRequest {
    let messages = vec![
        Message::user("look up 42"),
        Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: "t0".into(),
                name: "lookup".into(),
                input: json!({"id": "1"}),
            }],
        ),
        Message::new(
            Role::Tool,
            vec![Content::ToolResult {
                call_id: "t0".into(),
                output: "nothing".into(),
                is_error: true,
            }],
        ),
        Message::system_note("Tools now available: refund."),
    ];
    ModelRequest::new("m", messages)
        .system("Be brief.")
        .tools(vec![ToolSpec::new(
            "lookup",
            "Look up.",
            json!({"type": "object"}),
        )])
        .max_tokens(100)
}

async fn collect(p: &dyn Provider, req: ModelRequest) -> (Vec<StreamEvent>, Response) {
    let mut s = p.stream(req, Abort::new());
    let mut events = Vec::new();
    while let Some(e) = s.next().await {
        if let StreamEvent::Done(r) = e {
            return (events, r);
        }
        events.push(e);
    }
    panic!("stream ended without Done");
}

#[tokio::test]
async fn openai_streams_text_and_split_tool_calls() {
    let d = |v: Value| format!("data: {v}\n\n");
    let chunks = vec![
        d(json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"On it."}}]})),
        d(
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"lookup","arguments":"{\"id\""}}]}}]}),
        ),
        d(
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"42\"}"}}]}}]}),
        ),
        d(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})),
        d(
            json!({"choices":[],"usage":{"prompt_tokens":30,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":10}}}),
        ),
        "data: [DONE]\n\n".into(),
    ];
    let (mock, url) = Mock::start(vec![sse(chunks)]).await;
    let p = OpenAi::compatible(format!("{url}/v1")).key("sk-test");
    let (_, r) = collect(&p, request()).await;

    assert_eq!(r.stop, StopReason::ToolUse);
    assert_eq!(
        r.usage,
        Usage {
            input: 20,
            output: 7,
            cache_read: 10,
            cache_write: 0
        }
    );
    assert_eq!(
        r.message.content[1],
        Content::ToolCall {
            id: "c1".into(),
            name: "lookup".into(),
            input: json!({"id": "42"})
        }
    );

    let (head, body) = &mock.requests()[0];
    assert!(head.starts_with("POST /v1/chat/completions"));
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer sk-test")
    );
    assert_eq!(body["max_tokens"], 100);
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{\"id\":\"1\"}"
    );
    assert_eq!(body["messages"][3]["content"], "Error: nothing");
}

#[tokio::test]
async fn retries_429_then_succeeds() {
    let busy = Canned {
        status: 429,
        headers: vec![("retry-after", "0".into())],
        chunks: vec!["{\"error\":\"slow down\"}".into()],
    };
    let ok = sse(vec![
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n".into(),
        "data: [DONE]\n\n".into(),
    ]);
    let (mock, url) = Mock::start(vec![busy, ok]).await;
    let (_, r) = collect(&OpenAi::compatible(url), request()).await;
    assert_eq!(r.stop, StopReason::Stop);
    assert_eq!(r.message.text(), "hi");
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn context_overflow_is_classified_and_not_retried() {
    let bad = Canned {
        status: 400,
        headers: vec![],
        chunks: vec![json!({"error":{"code":"context_length_exceeded","message":"This model's maximum context length is 8192 tokens"}}).to_string()],
    };
    let (mock, url) = Mock::start(vec![bad]).await;
    let (_, r) = collect(&OpenAi::compatible(url), request()).await;
    match r.stop {
        StopReason::Error(e) => assert_eq!(e.kind, ErrorKind::ContextOverflow),
        s => panic!("{s:?}"),
    }
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn truncated_stream_is_an_error() {
    let (_, url) = Mock::start(vec![sse(vec![
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\n".into(),
    ])])
    .await;
    let (_, r) = collect(&OpenAi::compatible(url), request()).await;
    assert!(matches!(
        r.stop,
        StopReason::Error(ProviderError {
            kind: ErrorKind::Network,
            ..
        })
    ));
    assert_eq!(r.message.text(), "par");
}

#[tokio::test]
async fn models_routes_by_prefix() {
    let ok = sse(vec![
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"local\"},\"finish_reason\":\"stop\"}]}\n\n".into(),
        "data: [DONE]\n\n".into(),
    ]);
    let (mock, url) = Mock::start(vec![ok]).await;
    let models = Models::new()
        .with("llamacpp", OpenAi::compatible(url))
        .with_default("llamacpp/qwen3");
    assert_eq!(models.default_model().as_deref(), Some("llamacpp/qwen3"));
    let mut req = request();
    req.model = "llamacpp/qwen3".into();
    let (_, r) = collect(&models, req).await;
    assert_eq!(r.message.text(), "local");
    assert_eq!(mock.requests()[0].1["model"], "qwen3");

    let mut req = request();
    req.model = "nope/x".into();
    let (_, r) = collect(&models, req).await;
    assert!(matches!(
        r.stop,
        StopReason::Error(ProviderError {
            kind: ErrorKind::InvalidRequest,
            ..
        })
    ));
}

#[test]
fn model_info_prefers_overrides_then_catalog() {
    let m = Models::new()
        .with("llamacpp", OpenAi::compatible("http://x"))
        .with_info(
            "llamacpp",
            ModelInfo {
                context_window: 32_768,
                max_output: 8_192,
            },
        )
        .with_info(
            "llamacpp/tiny",
            ModelInfo {
                context_window: 4_096,
                max_output: 1_024,
            },
        );
    assert_eq!(
        m.model_info("llamacpp/qwen").unwrap().context_window,
        32_768
    );
    assert_eq!(m.model_info("llamacpp/tiny").unwrap().context_window, 4_096);
    assert_eq!(
        Models::new()
            .model_info("llamacpp/x")
            .unwrap()
            .context_window,
        8_192
    );
    assert!(m.model_info("groq/x").is_none());
}

#[cfg(feature = "responses")]
mod responses {
    use super::*;
    use manifatura_providers::responses::{BearerFuture, Credentials, Responses};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Hands out `t0`, then `t1` after a refusal.
    #[derive(Clone, Default)]
    struct Rotating(Arc<AtomicUsize>);

    impl Credentials for Rotating {
        fn bearer(&self, refused: bool) -> BearerFuture<'_> {
            if refused {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            let n = self.0.load(Ordering::SeqCst);
            Box::pin(async move { Ok(format!("t{n}")) })
        }
    }

    #[tokio::test]
    async fn streams_reasoning_text_and_a_tool_call_after_a_refresh() {
        let d = |v: Value| format!("event: x\ndata: {v}\n\n");
        let chunks = vec![
            d(json!({"type":"response.reasoning_summary_text.delta","delta":"thinking"})),
            d(
                json!({"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"ENC"}}),
            ),
            d(json!({"type":"response.output_text.delta","delta":"Reading."})),
            d(
                json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc1","call_id":"c1","name":"read","arguments":""}}),
            ),
            d(
                json!({"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"{\"path\":"}),
            ),
            d(
                json!({"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"\"a\"}"}),
            ),
            d(
                json!({"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":5}}}),
            ),
        ];
        let refused = Canned {
            status: 401,
            headers: vec![],
            chunks: vec!["{\"detail\":\"expired\"}".into()],
        };
        let (mock, url) = Mock::start(vec![refused, sse(chunks)]).await;
        let creds = Rotating::default();
        let p = Responses::new(format!("{url}/v1"))
            .credentials(creds.clone())
            .namespace("workspace", "Workspace tools");
        let (events, r) = collect(&p, request()).await;

        assert_eq!(r.stop, StopReason::ToolUse);
        assert!(events.contains(&StreamEvent::TextDelta("Reading.".into())));
        assert_eq!(
            r.message.content[0],
            Content::Thinking {
                text: "thinking".into(),
                signature: Some("ENC".into())
            }
        );
        assert_eq!(
            r.message.content[2],
            Content::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "a"})
            }
        );
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs[0]
                .0
                .to_ascii_lowercase()
                .contains("authorization: bearer t0")
        );
        assert!(
            reqs[1]
                .0
                .to_ascii_lowercase()
                .contains("authorization: bearer t1")
        );
        assert!(reqs[1].0.starts_with("POST /v1/responses"));
        assert_eq!(reqs[1].1["tools"][0]["type"], "namespace");
    }

    #[tokio::test]
    async fn streams_a_free_form_call_as_text_input() {
        let d = |v: Value| format!("event: x\ndata: {v}\n\n");
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch";
        let chunks = vec![
            d(
                json!({"type":"response.output_item.added","item":{"type":"custom_tool_call","id":"ct1","call_id":"c1","name":"apply_patch","input":""}}),
            ),
            d(
                json!({"type":"response.custom_tool_call_input.delta","item_id":"ct1","delta":"*** Begin Patch\n"}),
            ),
            d(
                json!({"type":"response.output_item.done","item":{"type":"custom_tool_call","id":"ct1","call_id":"c1","name":"apply_patch","input":patch}}),
            ),
            d(
                json!({"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}}),
            ),
        ];
        let (_mock, url) = Mock::start(vec![sse(chunks)]).await;
        let p = Responses::new(format!("{url}/v1")).key("k");
        let req = ModelRequest::new("m", vec![Message::user("add a.txt")])
            .tools(vec![ToolSpec::text("apply_patch", "Patch files.", None)]);
        let (events, r) = collect(&p, req).await;

        assert_eq!(r.stop, StopReason::ToolUse);
        assert!(events.contains(&StreamEvent::ToolCallStarted {
            id: "c1".into(),
            name: "apply_patch".into()
        }));
        assert_eq!(
            r.message.content[0],
            Content::ToolCall {
                id: "c1".into(),
                name: "apply_patch".into(),
                input: json!({ "input": patch })
            }
        );
    }

    #[tokio::test]
    async fn plan_usage_failure_mid_stream() {
        let d = |v: Value| format!("data: {v}\n\n");
        let chunks = vec![
            d(json!({"type":"response.output_text.delta","delta":"Hi"})),
            d(
                json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit"}}}),
            ),
        ];
        let (_mock, url) = Mock::start(vec![sse(chunks)]).await;
        let p = Responses::new(format!("{url}/v1")).key("k");
        let (_, r) = collect(&p, request()).await;
        match r.stop {
            StopReason::Error(e) => assert_eq!(e.kind, ErrorKind::RateLimited),
            other => panic!("{other:?}"),
        }
    }
}
