//! OpenAI-compatible Chat Completions: llama.cpp's `llama-server` and other compatible servers.

use std::collections::HashMap;

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

/// The OpenAI API's base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// A provider for the OpenAI chat completions API, and servers compatible with it (llama.cpp,
/// vLLM, hosted APIs).
#[derive(Clone)]
pub struct OpenAi {
    key: Option<String>,
    base: String,
}

/// Leaves the key out.
impl std::fmt::Debug for OpenAi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAi")
            .field("base", &self.base)
            .field("key", &self.key.as_ref().map(|_| "…"))
            .finish()
    }
}

impl OpenAi {
    /// The OpenAI API.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            key: Some(api_key.into()),
            base: DEFAULT_BASE_URL.into(),
        }
    }

    /// A compatible server without authentication, e.g. `http://localhost:8080/v1`.
    pub fn compatible(base_url: impl Into<String>) -> Self {
        Self {
            key: None,
            base: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Sends `api_key` as a bearer token.
    pub fn key(mut self, api_key: impl Into<String>) -> Self {
        self.key = Some(api_key.into());
        self
    }

    /// Another base URL, e.g. `http://localhost:8080/v1`.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base = url.into().trim_end_matches('/').to_string();
        self
    }

    fn is_openai(&self) -> bool {
        self.base.starts_with("https://api.openai.com")
    }
}

impl Provider for OpenAi {
    fn stream(&self, req: ModelRequest, abort: Abort) -> BoxStream<'static, StreamEvent> {
        let this = self.clone();
        http::spawn_stream(move |tx| async move { this.run(req, abort, tx).await })
    }
}

pub(crate) fn body(req: &ModelRequest, openai: bool) -> Value {
    let mut msgs = Vec::new();
    if !req.system.is_empty() {
        msgs.push(json!({ "role": "system", "content": req.system }));
    }
    for m in &req.messages {
        push_message(&mut msgs, m);
    }
    let mut b = json!({
        "model": req.model,
        "messages": msgs,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !req.tools.is_empty() {
        b["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({ "type": "function", "function": {
                    "name": t.name, "description": t.description, "parameters": t.input_schema } })
            })
            .collect();
        if !req.tool_choice.is_auto() {
            b["tool_choice"] = json!(req.tool_choice.as_str());
        }
    }
    if let Some(n) = req.max_tokens {
        // api.openai.com rejects `max_tokens` for reasoning models; other servers expect it.
        b[if openai {
            "max_completion_tokens"
        } else {
            "max_tokens"
        }] = json!(n);
    }
    if let Some(e) = req.reasoning {
        b["reasoning_effort"] = json!(e.as_str());
    }
    b
}

fn push_message(out: &mut Vec<Value>, m: &Message) {
    match m.role {
        Role::Tool => {
            for c in &m.content {
                if let Content::ToolResult {
                    call_id,
                    output,
                    is_error,
                } = c
                {
                    let content = if *is_error {
                        format!("Error: {output}")
                    } else {
                        output.clone()
                    };
                    out.push(
                        json!({ "role": "tool", "tool_call_id": call_id, "content": content }),
                    );
                }
            }
        }
        Role::Assistant => {
            let text = m.text();
            let calls: Vec<Value> = m
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::ToolCall { id, name, input } => Some(json!({
                        "id": id, "type": "function",
                        "function": { "name": name, "arguments": input.to_string() }
                    })),
                    _ => None,
                })
                .collect();
            if text.is_empty() && calls.is_empty() {
                return;
            }
            let mut v = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
            if !calls.is_empty() {
                v["tool_calls"] = json!(calls);
            }
            out.push(v);
        }
        // System notes go in as user text: many local chat templates allow only one leading
        // system message.
        Role::User | Role::System => {
            let parts: Vec<Value> = m
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } if m.role == Role::System => {
                        Some(json!({ "type": "text", "text": system_note_text(text) }))
                    }
                    Content::Text { text } => Some(json!({ "type": "text", "text": text })),
                    Content::Image { media_type, data } => Some(json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{media_type};base64,{data}") }
                    })),
                    _ => None,
                })
                .collect();
            if parts.is_empty() {
                return;
            }
            // Plain strings are the most widely supported form.
            let content = if parts.iter().all(|p| p["type"] == "text") {
                json!(
                    parts
                        .iter()
                        .map(|p| p["text"].as_str().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                )
            } else {
                json!(parts)
            };
            out.push(json!({ "role": "user", "content": content }));
        }
    }
}

impl OpenAi {
    async fn run(self, req: ModelRequest, abort: Abort, tx: mpsc::Sender<StreamEvent>) {
        let body = body(&req, self.is_openai()).to_string();
        let url = format!("{}/chat/completions", self.base);
        let client = http::client();
        let resp = http::send(
            || {
                let mut r = client
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(body.clone());
                if let Some(k) = &self.key {
                    r = r.bearer_auth(k);
                }
                r
            },
            &abort,
        )
        .await;
        let resp = match resp {
            Ok(r) => r,
            Err(SendError::Aborted) => {
                return http::finish(&tx, Acc::default().finish(Some(StopReason::Aborted))).await;
            }
            Err(SendError::Failed(e)) => {
                return http::finish(&tx, Response::error(e.kind, e.message)).await;
            }
        };

        let mut acc = Acc::default();
        let mut tools: HashMap<u64, usize> = HashMap::new();
        let mut failure = None;
        let mut done = false;
        let end = sse::read(resp, &abort, &tx, |e, deltas| {
            if e.data.trim() == "[DONE]" {
                done = true;
                return false;
            }
            let Ok(v) = serde_json::from_str::<Value>(&e.data) else {
                return true;
            };
            if let Some(err) = v.get("error") {
                failure = Some(ProviderError {
                    kind: ErrorKind::Other,
                    message: err["message"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| err.to_string()),
                });
                return false;
            }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                let cached = u["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                acc.usage.input = u["prompt_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(cached);
                acc.usage.cache_read = cached;
                acc.usage.output = u["completion_tokens"].as_u64().unwrap_or(0);
            }
            let c = &v["choices"][0];
            let d = &c["delta"];
            if let Some(t) = d["reasoning_content"].as_str().or(d["reasoning"].as_str())
                && !t.is_empty()
            {
                acc.thinking(t);
                deltas.push(StreamEvent::ReasoningDelta(t.to_string()));
            }
            if let Some(t) = d["content"].as_str()
                && !t.is_empty()
            {
                acc.text(t);
                deltas.push(StreamEvent::TextDelta(t.to_string()));
            }
            if let Some(calls) = d["tool_calls"].as_array() {
                for (pos, tc) in calls.iter().enumerate() {
                    let key = tc["index"].as_u64().unwrap_or(pos as u64);
                    let f = &tc["function"];
                    let i = *tools.entry(key).or_insert_with(|| {
                        let id = tc["id"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("call_{key}"));
                        let name = f["name"].as_str().unwrap_or_default().to_string();
                        deltas.push(StreamEvent::ToolCallStarted {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        acc.blocks.push(Block::Tool {
                            id,
                            name,
                            args: String::new(),
                            text: false,
                        });
                        acc.blocks.len() - 1
                    });
                    if let Block::Tool { args, name, .. } = &mut acc.blocks[i] {
                        if name.is_empty() {
                            name.push_str(f["name"].as_str().unwrap_or_default());
                        }
                        args.push_str(f["arguments"].as_str().unwrap_or_default());
                    }
                }
            }
            if let Some(r) = c["finish_reason"].as_str() {
                acc.stop = Some(match r {
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "length" => StopReason::Length,
                    _ => StopReason::Stop,
                });
            }
            true
        })
        .await;
        // Some local servers report `stop` even when they emitted tool calls.
        if acc.stop == Some(StopReason::Stop) && acc.has_tools() {
            acc.stop = Some(StopReason::ToolUse);
        }
        if done && acc.stop.is_none() {
            acc.stop = Some(if acc.has_tools() {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            });
        }
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

    #[test]
    fn tool_choice_is_sent_only_with_tools() {
        let mut req =
            ModelRequest::new("m", vec![Message::user("hi")]).tool_choice(ToolChoice::Off);
        assert!(body(&req, false).get("tool_choice").is_none());
        req.tools
            .push(ToolSpec::new("t", "d", json!({"type": "object"})));
        assert_eq!(body(&req, false)["tool_choice"], "none");
        req.tool_choice = ToolChoice::Auto;
        assert!(body(&req, false).get("tool_choice").is_none());
    }
}
