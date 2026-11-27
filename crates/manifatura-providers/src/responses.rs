//! OpenAI's Responses API (`POST {base}/responses`), streamed and stateless: every request
//! carries the whole conversation, with `store: false`.
//!
//! The bearer token comes from [`Credentials`], so a sign-in that refreshes its tokens (such as
//! `manifatura-auth`) plugs in without this crate knowing how it signs in.
//!
//! ```no_run
//! use manifatura_providers::responses::Responses;
//!
//! let provider = Responses::new("https://api.openai.com/v1").key("sk-…");
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::stream::BoxStream;
use manifatura_agent::{
    Abort, Content, ErrorKind, Message, ModelRequest, Provider, ProviderError, Response, Role,
    StopReason, StreamEvent, system_note_text,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::acc::{Acc, Block};
use crate::http::{self, SendError};
use crate::sse::{self, ReadEnd};

/// A future returned by [`Credentials::bearer`].
pub type BearerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send + 'a>>;

/// Where a [`Responses`] provider gets its bearer token.
pub trait Credentials: Send + Sync {
    /// A token to send. `refused` is true when the previous token was answered with 401, so a
    /// sign-in should refresh before answering.
    fn bearer(&self, refused: bool) -> BearerFuture<'_>;
}

/// A fixed API key.
struct Key(String);

impl Credentials for Key {
    fn bearer(&self, _refused: bool) -> BearerFuture<'_> {
        let k = self.0.clone();
        Box::pin(async move { Ok(k) })
    }
}

/// A provider for the Responses API.
#[derive(Clone)]
pub struct Responses {
    base: String,
    credentials: Option<Arc<dyn Credentials>>,
    namespace: Option<(String, String)>,
    max_output_tokens: bool,
    headers: Vec<(String, String)>,
}

/// Leaves the credentials out.
impl std::fmt::Debug for Responses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Responses")
            .field("base", &self.base)
            .field("namespace", &self.namespace)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish_non_exhaustive()
    }
}

impl Responses {
    /// The API at `base_url`, e.g. `https://api.openai.com/v1`, without authentication.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base: base_url.into().trim_end_matches('/').to_string(),
            credentials: None,
            namespace: None,
            max_output_tokens: true,
            headers: Vec::new(),
        }
    }

    /// Sends `api_key` as the bearer token.
    pub fn key(self, api_key: impl Into<String>) -> Self {
        self.credentials(Key(api_key.into()))
    }

    /// Takes the bearer token from `credentials` before each request.
    pub fn credentials(mut self, credentials: impl Credentials + 'static) -> Self {
        self.credentials = Some(Arc::new(credentials));
        self
    }

    /// Offers the tools grouped in one namespace instead of at the top level, for routes that
    /// accept function tools only that way.
    pub fn namespace(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.namespace = Some((name.into(), description.into()));
        self
    }

    /// Whether to send `max_output_tokens` (default `true`); some routes reject it.
    pub fn max_output_tokens(mut self, send: bool) -> Self {
        self.max_output_tokens = send;
        self
    }

    /// Adds a header to every request.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The base URL.
    pub fn base_url(&self) -> &str {
        &self.base
    }
}

impl Provider for Responses {
    fn stream(&self, req: ModelRequest, abort: Abort) -> BoxStream<'static, StreamEvent> {
        let this = self.clone();
        http::spawn_stream(move |tx| async move { this.run(req, abort, tx).await })
    }
}

fn tool_json(t: &manifatura_agent::ToolSpec) -> Value {
    json!({ "type": "function", "name": t.name, "description": t.description,
        "parameters": t.input_schema, "strict": false })
}

impl Responses {
    /// The request body for `req`.
    pub(crate) fn body(&self, req: &ModelRequest) -> Value {
        let ns = self.namespace.as_ref().map(|(n, _)| n.as_str());
        let mut input = Vec::new();
        for m in &req.messages {
            push_items(&mut input, m, ns);
        }
        let mut b = json!({
            "model": req.model,
            "instructions": req.system,
            "input": input,
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
        });
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req.tools.iter().map(tool_json).collect();
            b["tools"] = match &self.namespace {
                Some((name, description)) => json!([{
                    "type": "namespace", "name": name, "description": description, "tools": tools
                }]),
                None => Value::Array(tools),
            };
            b["tool_choice"] = json!(req.tool_choice.as_str());
            b["parallel_tool_calls"] = json!(true);
        }
        if let Some(e) = req.reasoning {
            b["reasoning"] = json!({ "effort": e.as_str(), "summary": "auto" });
        }
        if self.max_output_tokens
            && let Some(n) = req.max_tokens
        {
            b["max_output_tokens"] = json!(n);
        }
        b
    }
}

fn push_items(out: &mut Vec<Value>, m: &Message, namespace: Option<&str>) {
    match m.role {
        Role::Tool => {
            for c in &m.content {
                if let Content::ToolResult {
                    call_id,
                    output,
                    is_error,
                } = c
                {
                    let output = if *is_error {
                        format!("Error: {output}")
                    } else {
                        output.clone()
                    };
                    out.push(json!({ "type": "function_call_output", "call_id": call_id, "output": output }));
                }
            }
        }
        Role::Assistant => {
            for c in &m.content {
                match c {
                    // Reasoning goes back only as the encrypted item the API gave us.
                    Content::Thinking {
                        text,
                        signature: Some(enc),
                    } => {
                        let summary: Vec<Value> = if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![json!({ "type": "summary_text", "text": text })]
                        };
                        out.push(json!({ "type": "reasoning", "summary": summary, "encrypted_content": enc }));
                    }
                    Content::Text { text } if !text.is_empty() => out.push(json!({
                        "type": "message", "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }]
                    })),
                    Content::ToolCall { id, name, input } => {
                        let mut item = json!({
                            "type": "function_call", "call_id": id, "name": name,
                            "arguments": input.to_string()
                        });
                        if let Some(ns) = namespace {
                            item["namespace"] = json!(ns);
                        }
                        out.push(item);
                    }
                    _ => {}
                }
            }
        }
        Role::User | Role::System => {
            // System notes go as user text: some routes reject system-role items.
            let parts: Vec<Value> = m
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } if m.role == Role::System => {
                        Some(json!({ "type": "input_text", "text": system_note_text(text) }))
                    }
                    Content::Text { text } => Some(json!({ "type": "input_text", "text": text })),
                    Content::Image { media_type, data } => Some(json!({
                        "type": "input_image", "image_url": format!("data:{media_type};base64,{data}")
                    })),
                    _ => None,
                })
                .collect();
            if !parts.is_empty() {
                out.push(json!({ "type": "message", "role": "user", "content": parts }));
            }
        }
    }
}

/// The kind of a failure the API reports by code.
fn kind_of(code: &str) -> ErrorKind {
    match code {
        "context_length_exceeded" => ErrorKind::ContextOverflow,
        "rate_limit_exceeded"
        | "usage_limit_reached"
        | "subscription_sharing_usage_limit_exceeded" => ErrorKind::RateLimited,
        "subscription_sharing_invalid_user"
        | "subscription_sharing_user_not_eligible"
        | "subscription_sharing_route_not_supported" => ErrorKind::Auth,
        "subscription_sharing_usage_unavailable" | "server_error" => ErrorKind::Server,
        "subscription_sharing_unsupported_capability" | "invalid_request_error" => {
            ErrorKind::InvalidRequest
        }
        _ => ErrorKind::Other,
    }
}

impl Responses {
    async fn run(self, req: ModelRequest, abort: Abort, tx: mpsc::Sender<StreamEvent>) {
        let body = self.body(&req).to_string();
        let url = format!("{}/responses", self.base);
        let mut refused = false;
        let resp = loop {
            let token = match &self.credentials {
                Some(c) => match c.bearer(refused).await {
                    Ok(t) => Some(t),
                    Err(e) => return http::finish(&tx, Response::error(e.kind, e.message)).await,
                },
                None => None,
            };
            let client = http::client();
            let sent = http::send(
                || {
                    let mut r = client
                        .post(&url)
                        .header("content-type", "application/json")
                        .header("accept", "text/event-stream")
                        .body(body.clone());
                    for (k, v) in &self.headers {
                        r = r.header(k, v);
                    }
                    if let Some(t) = &token {
                        r = r.bearer_auth(t);
                    }
                    r
                },
                &abort,
            )
            .await;
            match sent {
                Ok(r) => break r,
                // A refused token: ask for a fresh one once.
                Err(SendError::Failed(e))
                    if e.kind == ErrorKind::Auth
                        && !refused
                        && self.credentials.is_some()
                        && e.message.starts_with("HTTP 401") =>
                {
                    refused = true
                }
                Err(SendError::Aborted) => {
                    return http::finish(&tx, Acc::default().finish(Some(StopReason::Aborted)))
                        .await;
                }
                Err(SendError::Failed(e)) => {
                    return http::finish(&tx, Response::error(e.kind, e.message)).await;
                }
            }
        };

        let mut acc = Acc::default();
        // Output item id to block index, for argument deltas.
        let mut items: HashMap<String, usize> = HashMap::new();
        let mut failure = None;
        let end = sse::read(resp, &abort, &tx, |e, deltas| {
            let Ok(v) = serde_json::from_str::<Value>(&e.data) else {
                return true;
            };
            let ty = v["type"].as_str().unwrap_or_default().to_string();
            match ty.as_str() {
                "response.output_text.delta" => {
                    if let Some(t) = v["delta"].as_str().filter(|t| !t.is_empty()) {
                        acc.text(t);
                        deltas.push(StreamEvent::TextDelta(t.to_string()));
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    if let Some(t) = v["delta"].as_str().filter(|t| !t.is_empty()) {
                        acc.thinking(t);
                        deltas.push(StreamEvent::ReasoningDelta(t.to_string()));
                    }
                }
                "response.output_item.done" if v["item"]["type"] == "reasoning" => {
                    if let Some(enc) = v["item"]["encrypted_content"].as_str() {
                        match acc.blocks.last_mut() {
                            Some(Block::Thinking { signature, .. }) if signature.is_none() => {
                                *signature = Some(enc.to_string());
                            }
                            _ => acc.blocks.push(Block::Thinking {
                                text: String::new(),
                                signature: Some(enc.to_string()),
                            }),
                        }
                    }
                }
                "response.output_item.added" if v["item"]["type"] == "function_call" => {
                    let item = &v["item"];
                    let id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    deltas.push(StreamEvent::ToolCallStarted {
                        id: id.clone(),
                        name: name.clone(),
                    });
                    acc.blocks.push(Block::Tool {
                        id,
                        name,
                        args: item["arguments"].as_str().unwrap_or_default().to_string(),
                    });
                    if let Some(item_id) = item["id"].as_str() {
                        items.insert(item_id.to_string(), acc.blocks.len() - 1);
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(&i) = v["item_id"].as_str().and_then(|id| items.get(id))
                        && let Block::Tool { args, .. } = &mut acc.blocks[i]
                    {
                        args.push_str(v["delta"].as_str().unwrap_or_default());
                    }
                }
                "response.output_item.done" if v["item"]["type"] == "function_call" => {
                    // The finished item is authoritative.
                    if let Some(&i) = v["item"]["id"].as_str().and_then(|id| items.get(id))
                        && let Block::Tool { args, .. } = &mut acc.blocks[i]
                        && let Some(full) = v["item"]["arguments"].as_str()
                    {
                        *args = full.to_string();
                    }
                }
                "response.completed" | "response.incomplete" => {
                    let u = &v["response"]["usage"];
                    let cached = u["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    acc.usage.input = u["input_tokens"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(cached);
                    acc.usage.cache_read = cached;
                    acc.usage.output = u["output_tokens"].as_u64().unwrap_or(0);
                    acc.stop = Some(if ty == "response.incomplete" {
                        StopReason::Length
                    } else if acc.has_tools() {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    });
                    return false;
                }
                "response.failed" | "error" => {
                    let err = if ty == "error" {
                        if v["error"].is_object() {
                            &v["error"]
                        } else {
                            &v
                        }
                    } else {
                        &v["response"]["error"]
                    };
                    let message = err["message"]
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| err.to_string());
                    let code = err["code"].as_str().unwrap_or_default();
                    failure = Some(ProviderError {
                        kind: kind_of(code),
                        message: if code.is_empty() {
                            message
                        } else {
                            format!("{code}: {message}")
                        },
                    });
                    return false;
                }
                _ => {}
            }
            true
        })
        .await;
        let end = match (end, failure) {
            (ReadEnd::Closed, _) => return,
            (_, Some(e)) => Some(StopReason::Error(e)),
            (ReadEnd::Aborted, _) => Some(StopReason::Aborted),
            (ReadEnd::Network(m), _) => Some(StopReason::Error(ProviderError {
                kind: ErrorKind::Network,
                message: m,
            })),
            (ReadEnd::Eof | ReadEnd::Stopped, _) => None,
        };
        http::finish(&tx, acc.finish(end)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifatura_agent::{ToolChoice, ToolSpec};

    fn req() -> ModelRequest {
        ModelRequest::new(
            "m",
            vec![
                Message::user("hi"),
                Message::system_note("note"),
                Message::new(
                    Role::Assistant,
                    vec![
                        Content::Thinking {
                            text: "t".into(),
                            signature: Some("enc".into()),
                        },
                        Content::ToolCall {
                            id: "c1".into(),
                            name: "read".into(),
                            input: json!({"path": "a"}),
                        },
                    ],
                ),
                Message::new(
                    Role::Tool,
                    vec![Content::ToolResult {
                        call_id: "c1".into(),
                        output: "x".into(),
                        is_error: false,
                    }],
                ),
            ],
        )
        .system("sys")
        .tools(vec![ToolSpec {
                name: "read".into(),
                description: "Read.".into(),
                input_schema: json!({"type": "object"}),
            }])
        .max_tokens(100)
    }

    #[test]
    fn plain_body() {
        let b = Responses::new("http://x").body(&req());
        assert_eq!(b["store"], false);
        assert_eq!(b["stream"], true);
        assert_eq!(b["instructions"], "sys");
        assert_eq!(b["max_output_tokens"], 100);
        assert_eq!(b["tools"][0]["type"], "function");
        let input = b["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "user", "system notes go as user text");
        assert_eq!(input[2]["type"], "reasoning");
        assert_eq!(input[2]["encrypted_content"], "enc");
        assert_eq!(input[3]["type"], "function_call");
        assert!(input[3].get("namespace").is_none());
        assert_eq!(input[4]["type"], "function_call_output");
        assert!(b.get("reasoning").is_none());
        assert_eq!(b["tool_choice"], "auto");
    }

    #[test]
    fn tool_choice_off_keeps_the_tools() {
        let b = Responses::new("http://x").body(&req().tool_choice(ToolChoice::Off));
        assert_eq!(b["tool_choice"], "none");
        assert_eq!(b["tools"][0]["name"], "read");
    }

    #[test]
    fn namespaced_body_without_max_tokens() {
        let b = Responses::new("http://x")
            .namespace("ws", "Workspace tools")
            .max_output_tokens(false)
            .body(&req());
        assert!(b.get("max_output_tokens").is_none());
        assert_eq!(b["tools"][0]["type"], "namespace");
        assert_eq!(b["tools"][0]["name"], "ws");
        assert_eq!(b["tools"][0]["tools"][0]["name"], "read");
        assert_eq!(b["input"][3]["namespace"], "ws");
    }

    #[test]
    fn codes() {
        assert_eq!(
            kind_of("subscription_sharing_usage_limit_exceeded"),
            ErrorKind::RateLimited
        );
        assert_eq!(
            kind_of("subscription_sharing_invalid_user"),
            ErrorKind::Auth
        );
    }
}
