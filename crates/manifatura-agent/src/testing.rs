//! A scripted provider for tests, examples and evals.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::Value;

use crate::provider::{Abort, Provider};
use crate::types::{
    Content, ErrorKind, Message, ModelRequest, Response, Role, StopReason, StreamEvent, Usage,
};

type Script = Box<dyn FnMut(&ModelRequest) -> Response + Send>;

/// Answers each model call with the next scripted response and records every request.
#[derive(Clone, Default)]
pub struct ScriptedProvider {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for ScriptedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().unwrap();
        f.debug_struct("ScriptedProvider")
            .field("scripted", &inner.script.len())
            .field("requests", &inner.requests.len())
            .finish()
    }
}

#[derive(Default)]
struct Inner {
    script: VecDeque<Script>,
    fallback: Option<Script>,
    requests: Vec<ModelRequest>,
}

impl ScriptedProvider {
    /// A provider with nothing scripted yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replies with text and stops.
    pub fn text(self, text: &str) -> Self {
        self.respond(reply(vec![Content::text(text)], StopReason::Stop))
    }

    /// Calls one tool.
    pub fn call(self, name: &str, input: Value) -> Self {
        self.calls(&[(name, input)])
    }

    /// Calls several tools in one message.
    pub fn calls(self, calls: &[(&str, Value)]) -> Self {
        let n = self.inner.lock().unwrap().script.len();
        let content = calls
            .iter()
            .enumerate()
            .map(|(i, (name, input))| Content::ToolCall {
                id: format!("call_{n}_{i}"),
                name: name.to_string(),
                input: input.clone(),
            })
            .collect();
        self.respond(reply(content, StopReason::ToolUse))
    }

    /// Replies with `response`.
    pub fn respond(self, response: Response) -> Self {
        self.with(move |_| response.clone())
    }

    /// A response computed from the request.
    pub fn with(self, f: impl FnMut(&ModelRequest) -> Response + Send + 'static) -> Self {
        self.inner.lock().unwrap().script.push_back(Box::new(f));
        self
    }

    /// Answers every request once the script is used up.
    pub fn fallback(self, f: impl FnMut(&ModelRequest) -> Response + Send + 'static) -> Self {
        self.inner.lock().unwrap().fallback = Some(Box::new(f));
        self
    }

    /// Every request the provider was sent, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.inner.lock().unwrap().requests.clone()
    }
}

/// An assistant response with the given content and stop reason, reporting no usage.
pub fn reply(content: Vec<Content>, stop: StopReason) -> Response {
    Response {
        message: Message::new(Role::Assistant, content),
        stop,
        // Zero usage: the runtime then estimates context size from the messages themselves.
        usage: Usage::default(),
    }
}

impl Provider for ScriptedProvider {
    fn stream(&self, req: ModelRequest, _abort: Abort) -> BoxStream<'static, StreamEvent> {
        let mut inner = self.inner.lock().unwrap();
        let resp = match (inner.script.pop_front(), &mut inner.fallback) {
            (Some(mut f), _) => f(&req),
            (None, Some(f)) => f(&req),
            (None, None) => Response::error(ErrorKind::Other, "script exhausted"),
        };
        inner.requests.push(req);
        let mut events: Vec<StreamEvent> = resp
            .message
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text, .. } => Some(StreamEvent::TextDelta(text.clone())),
                _ => None,
            })
            .collect();
        events.push(StreamEvent::Done(resp));
        futures_util::stream::iter(events).boxed()
    }

    fn default_model(&self) -> Option<String> {
        Some("scripted".into())
    }
}
