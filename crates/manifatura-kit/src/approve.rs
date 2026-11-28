//! Approvals: a `ToolCall` hook that asks someone (a TUI prompt, a chat button, a policy
//! function) before a call runs. [`approve`] and its siblings ask through a callback;
//! [`Approvals`] keeps each question as a pending request with an id, which any client can
//! list and answer.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use manifatura_agent::{Abort, Hooks, SessionId, ToolCall, hook};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

/// What the person said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Answer {
    /// Allow this call only.
    Allow,
    /// Allow this call, and later calls in the same scope for the rest of the session. The
    /// hook decides the scope: the tool for [`approve`], the exact call for [`approve_when`],
    /// whatever its `scope` function says for [`approve_by`].
    AllowAlways,
    /// Allow this call, and keep what the request offers to remember
    /// ([`Request::remember`], such as a rule) beyond this session. Where nothing is offered,
    /// it is [`Answer::AllowAlways`].
    AllowAndRemember,
    /// Refuse the call; the reason, if any, goes back to the model.
    Deny(Option<String>),
}

impl Answer {
    /// Whether the call may run.
    pub fn allows(&self) -> bool {
        !matches!(self, Answer::Deny(_))
    }
}

/// The session state key that remembers an [`Answer::AllowAlways`] for `scope`.
pub fn approval_key(scope: &str) -> String {
    format!("approval.{scope}")
}

/// Asks `asker` before any call of the named tools. "Always" allows the tool.
pub fn approve<S, F, Fut>(tools: impl IntoIterator<Item = S>, asker: F) -> Hooks
where
    S: Into<String>,
    F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Answer> + Send + 'static,
{
    let names: HashSet<String> = tools.into_iter().map(Into::into).collect();
    approve_by(
        move |call: &ToolCall| names.contains(&call.name),
        |call: &ToolCall| call.name.clone(),
        asker,
    )
}

/// Asks `asker` before calls for which `when` is true (e.g. a `bash` command matching a
/// pattern). "Always" allows only the same call again: the same tool with the same input.
pub fn approve_when<P, F, Fut>(when: P, asker: F) -> Hooks
where
    P: Fn(&ToolCall) -> bool + Send + Sync + 'static,
    F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Answer> + Send + 'static,
{
    approve_by(when, call_scope, asker)
}

/// Asks `asker` before calls for which `when` is true; "always" allows later calls whose
/// `scope` is the same. The scope is what the person agreed to, so a UI should show it:
///
/// ```
/// # use manifatura_agent::ToolCall;
/// // Edits are allowed per tool, commands one at a time.
/// let scope = |call: &ToolCall| match call.name.as_str() {
///     "bash" => manifatura_kit::call_scope(call),
///     tool => tool.to_string(),
/// };
/// # let _ = scope;
/// ```
///
/// Each scope is kept in session state under [`approval_key`], so hooks with different
/// scopes do not answer for each other. While waiting, an aborted turn denies the call.
pub fn approve_by<P, K, F, Fut>(when: P, scope: K, asker: F) -> Hooks
where
    P: Fn(&ToolCall) -> bool + Send + Sync + 'static,
    K: Fn(&ToolCall) -> String + Send + Sync + 'static,
    F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Answer> + Send + 'static,
{
    let when = Arc::new(when);
    let scope = Arc::new(scope);
    let asker = Arc::new(asker);
    Hooks::new().on(move |e: hook::ToolCall, cx| {
        let (when, scope, asker) = (when.clone(), scope.clone(), asker.clone());
        async move {
            if !when(&e.call) {
                return hook::Decision::Run(e);
            }
            let key = approval_key(&scope(&e.call));
            if cx.state::<bool>(&key) == Some(true) {
                return hook::Decision::Run(e);
            }
            let answer = tokio::select! {
                a = asker(e.call.clone()) => a,
                _ = cx.abort().aborted() => {
                    return hook::Decision::Deny("aborted while waiting for approval".into());
                }
            };
            match answer {
                Answer::Allow => hook::Decision::Run(e),
                Answer::AllowAlways | Answer::AllowAndRemember => {
                    cx.set_state(key, true);
                    hook::Decision::Run(e)
                }
                Answer::Deny(reason) => hook::Decision::Deny(denial(reason.as_deref())),
            }
        }
    })
}

/// The scope of exactly this call: its tool and its input.
pub fn call_scope(call: &ToolCall) -> String {
    format!("{} {}", call.name, call.input)
}

/// The message a denial sends back to the model.
pub fn denial(reason: Option<&str>) -> String {
    match reason {
        Some(r) if !r.trim().is_empty() => format!("the user denied this call: {r}"),
        _ => "the user denied this call".into(),
    }
}

/// A question waiting for an answer: which call, in which session, and what an answer covers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Request {
    /// Its id, for [`Approvals::answer`].
    pub id: String,
    /// The session the call is in.
    pub session: SessionId,
    /// The call.
    pub call: ToolCall,
    /// What [`Answer::AllowAlways`] allows for the rest of the session; show it to the person.
    pub scope: String,
    /// Why it asks, if there is more to say than the call: a rule that asks, the model's
    /// justification for running a command outside the sandbox.
    pub reason: Option<String>,
    /// What [`Answer::AllowAndRemember`] keeps beyond the session (such as a rule like
    /// `exec_command(git commit)`); `None` when there is nothing to offer.
    pub remember: Option<String>,
}

impl Request {
    /// A request about `call` in `session`, whose "always" covers the same call again.
    pub fn new(session: SessionId, call: ToolCall) -> Self {
        Self {
            id: String::new(),
            session,
            scope: call_scope(&call),
            call,
            reason: None,
            remember: None,
        }
    }

    /// What "always" allows for the rest of the session.
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }

    /// Why it asks.
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// What "allow and remember" keeps beyond the session.
    pub fn remember(mut self, what: impl Into<String>) -> Self {
        self.remember = Some(what.into());
        self
    }
}

/// What happens to requests, for the clients that show them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApprovalEvent {
    /// A request is waiting.
    Requested(Request),
    /// A request was answered.
    Answered {
        /// The request's id.
        id: String,
        /// Its session.
        session: SessionId,
        /// The answer.
        answer: Answer,
    },
    /// A request is no longer waiting, unanswered: its turn was aborted.
    Cancelled {
        /// The request's id.
        id: String,
        /// Its session.
        session: SessionId,
    },
}

/// Pending approval requests, answerable from anywhere: a terminal prompt, a web page, another
/// person's client. A hook asks through it and waits; clients see each request through
/// [`subscribe`](Self::subscribe) or [`pending`](Self::pending) and answer it by id with
/// [`answer`](Self::answer). Cheap to clone; one is usually shared by a runtime's sessions.
///
/// ```no_run
/// # use manifatura_agent::{Agent, ToolCall};
/// use manifatura_kit::{Answer, ApprovalEvent, Approvals};
///
/// let approvals = Approvals::new();
/// let agent = Agent::new("…").hooks(approvals.gate(|call: &ToolCall| call.name == "bash", |c: &ToolCall| c.name.clone()));
///
/// let mut events = approvals.subscribe();
/// # async {
/// while let Ok(ApprovalEvent::Requested(r)) = events.recv().await {
///     approvals.answer(&r.id, Answer::Allow); // after asking someone
/// }
/// # };
/// ```
///
/// Requests live in memory: a process that stops leaves none behind, and the turns that
/// waited on them are interrupted.
#[derive(Clone)]
pub struct Approvals(Arc<Inner>);

struct Inner {
    pending: Mutex<BTreeMap<String, (Request, oneshot::Sender<Answer>)>>,
    events: broadcast::Sender<ApprovalEvent>,
}

impl std::fmt::Debug for Approvals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Approvals")
            .field("pending", &self.0.pending.lock().unwrap().len())
            .finish()
    }
}

impl Default for Approvals {
    fn default() -> Self {
        Self::new()
    }
}

impl Approvals {
    /// No requests yet.
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            pending: Mutex::new(BTreeMap::new()),
            events: broadcast::channel(256).0,
        }))
    }

    /// Requests as they come and go. A client that starts late reads
    /// [`pending`](Self::pending) first.
    pub fn subscribe(&self) -> broadcast::Receiver<ApprovalEvent> {
        self.0.events.subscribe()
    }

    /// The requests waiting now, oldest first.
    pub fn pending(&self) -> Vec<Request> {
        self.0
            .pending
            .lock()
            .unwrap()
            .values()
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// Answers the request `id`. `false` when no request by that id waits (answered already,
    /// or cancelled): the first answer wins.
    pub fn answer(&self, id: &str, answer: Answer) -> bool {
        let Some((request, reply)) = self.0.pending.lock().unwrap().remove(id) else {
            return false;
        };
        let _ = self.0.events.send(ApprovalEvent::Answered {
            id: id.to_string(),
            session: request.session,
            answer: answer.clone(),
        });
        reply.send(answer).is_ok()
    }

    /// Asks: adds `request` (given a fresh id) to the pending ones, announces it, and waits
    /// for an answer. When `abort` fires first, the request is cancelled and the answer is a
    /// denial.
    pub async fn ask(&self, mut request: Request, abort: &Abort) -> Answer {
        request.id = manifatura_agent::new_id();
        let id = request.id.clone();
        let session = request.session.clone();
        let (tx, rx) = oneshot::channel();
        self.0
            .pending
            .lock()
            .unwrap()
            .insert(id.clone(), (request.clone(), tx));
        let _ = self.0.events.send(ApprovalEvent::Requested(request));
        tokio::select! {
            a = rx => a.unwrap_or(Answer::Deny(Some("the approval was dropped".into()))),
            _ = abort.aborted() => {
                if self.0.pending.lock().unwrap().remove(&id).is_some() {
                    let _ = self.0.events.send(ApprovalEvent::Cancelled { id, session });
                }
                Answer::Deny(Some("aborted while waiting for approval".into()))
            }
        }
    }

    /// A `ToolCall` hook that asks through these approvals before calls for which `when` is
    /// true; "always" allows later calls whose `scope` is the same, for the rest of the session
    /// (as [`approve_by`]). Each answer is recorded in the session as a custom `approval`
    /// record.
    pub fn gate<P, K>(&self, when: P, scope: K) -> Hooks
    where
        P: Fn(&ToolCall) -> bool + Send + Sync + 'static,
        K: Fn(&ToolCall) -> String + Send + Sync + 'static,
    {
        let approvals = self.clone();
        let when = Arc::new(when);
        let scope = Arc::new(scope);
        Hooks::new().on(move |e: hook::ToolCall, cx| {
            let (approvals, when, scope) = (approvals.clone(), when.clone(), scope.clone());
            async move {
                if !when(&e.call) {
                    return hook::Decision::Run(e);
                }
                let scope = scope(&e.call);
                let key = approval_key(&scope);
                if cx.state::<bool>(&key) == Some(true) {
                    return hook::Decision::Run(e);
                }
                let request = Request::new(cx.session_id().clone(), e.call.clone()).scope(scope);
                let answer = approvals.ask(request, cx.abort()).await;
                record(&cx, &e.call, &answer);
                match answer {
                    Answer::Deny(reason) => hook::Decision::Deny(denial(reason.as_deref())),
                    Answer::AllowAlways | Answer::AllowAndRemember => {
                        cx.set_state(key, true);
                        hook::Decision::Run(e)
                    }
                    _ => hook::Decision::Run(e),
                }
            }
        })
    }
}

/// Records an answer in the session, as a custom `approval` record the model never sees.
pub fn record(cx: &manifatura_agent::HookCx, call: &ToolCall, answer: &Answer) {
    cx.custom(
        "approval",
        serde_json::json!({ "call_id": call.id, "tool": call.name, "answer": answer }),
    );
}
