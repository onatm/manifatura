//! The runtime, sessions, turns and the session worker. The loop itself is in `turn`.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, Weak};

use futures_util::{FutureExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{OnceCell, broadcast, mpsc, oneshot};

use crate::agent::Agent;
use crate::compact::{CompactReport, Compactor};
use crate::context::{Live, prompt_tokens};
use crate::env::{Env, no_env_arc};
use crate::event::Event;
use crate::hook::{self, HookEvent};
use crate::hooks::{HookCx, Hooks, Ops};
use crate::provider::{Abort, Provider};
use crate::record::{
    Entry, MemoryStore, RECORD_VERSION, Record, SessionId, Store, StoreError, new_id,
};
use crate::turn::{self, TurnJob};
use crate::types::{
    Effort, ErrorKind, Message, ModelRequest, ProviderError, Response, StreamEvent, Usage,
};

/// Why a turn or a session operation failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The store failed to read or write the session.
    Store(StoreError),
    /// The model call failed, and no [`ModelError`](crate::hook::ModelError) hook retried it.
    /// Its [`kind`](ProviderError::kind) tells a context overflow or a bad key from the rest.
    Model(ProviderError),
    /// The turn failed for another reason: a hook failed it, it ran out of steps, no model
    /// was configured.
    Failed(String),
    /// The turn was aborted.
    Aborted,
    /// The session worker is gone.
    Closed,
}

impl Error {
    /// The failure without a prefix, as `turn.completed` reports it.
    pub(crate) fn reason(&self) -> String {
        match self {
            Error::Model(e) => e.message.clone(),
            Error::Failed(m) => m.clone(),
            e => e.to_string(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Store(e) => e.fmt(f),
            Error::Model(e) => write!(f, "model call failed: {}", e.message),
            Error::Failed(m) => write!(f, "turn failed: {m}"),
            Error::Aborted => f.write_str("turn aborted"),
            Error::Closed => f.write_str("session closed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Error::Store(e)
    }
}

/// The final result of a completed turn.
#[derive(Clone, Debug)]
pub struct Reply {
    /// The turn's id, as in its events.
    pub turn: String,
    /// Text of the last assistant message.
    pub text: String,
    /// `None` when an [`Input`](crate::hook::Input) hook dropped the prompt.
    pub message: Option<Message>,
    /// Tokens used over the whole turn.
    pub usage: Usage,
}

impl std::fmt::Display for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Shared services for every session: provider, store, env, default model and app-wide hooks.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RtInner>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("model", &self.inner.model)
            .field("env", &self.inner.env.describe())
            .finish_non_exhaustive()
    }
}

pub(crate) struct RtInner {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) store: Arc<dyn Store>,
    model: Option<String>,
    hooks: Hooks,
    env: Arc<dyn Env>,
    sessions: Mutex<HashMap<SessionId, LiveRef>>,
}

struct LiveRef {
    tx: mpsc::WeakUnboundedSender<Job>,
    shared: Weak<Shared>,
}

/// Builds a [`Runtime`]. Only a provider is needed; everything else has a default.
#[derive(Default)]
pub struct RuntimeBuilder {
    provider: Option<Arc<dyn Provider>>,
    store: Option<Arc<dyn Store>>,
    model: Option<String>,
    hooks: Hooks,
    env: Option<Arc<dyn Env>>,
}

impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeBuilder")
            .field("model", &self.model)
            .field("hooks", &self.hooks)
            .finish_non_exhaustive()
    }
}

impl RuntimeBuilder {
    /// The model backend. An `Arc<dyn Provider>` works too.
    pub fn provider(mut self, provider: impl Provider + 'static) -> Self {
        self.provider = Some(Arc::new(provider));
        self
    }

    /// Defaults to a [`MemoryStore`]. An `Arc<dyn Store>` works too, to share one store.
    pub fn store(mut self, store: impl Store + 'static) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// The model used when an agent does not name one.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// The environment tools act in, unless an agent sets its own.
    pub fn env(mut self, env: impl Env + 'static) -> Self {
        self.env = Some(Arc::new(env));
        self
    }

    /// A hook for every session of this runtime, including sessions tools open. Runs before the
    /// agent's own hooks.
    pub fn on<E, F, Fut>(mut self, f: F) -> Self
    where
        E: HookEvent,
        F: Fn(E, HookCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = E::Out> + Send + 'static,
    {
        self.hooks.add(f);
        self
    }

    /// A bundle of hooks for every session (see [`on`](Self::on)).
    pub fn hooks(mut self, hooks: Hooks) -> Self {
        self.hooks.extend(hooks);
        self
    }

    /// The runtime. Without a provider, every model call fails with a clear error.
    pub fn build(self) -> Runtime {
        Runtime {
            inner: Arc::new(RtInner {
                provider: self.provider.unwrap_or_else(|| Arc::new(NoProvider)),
                store: self.store.unwrap_or_else(|| Arc::new(MemoryStore::new())),
                model: self.model,
                hooks: self.hooks,
                env: self.env.unwrap_or_else(no_env_arc),
                sessions: Mutex::default(),
            }),
        }
    }
}

impl Runtime {
    pub(crate) fn from_inner(inner: Arc<RtInner>) -> Self {
        Self { inner }
    }

    /// Starts building a runtime.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::default()
    }

    /// Where sessions are kept.
    pub fn store(&self) -> &Arc<dyn Store> {
        &self.inner.store
    }

    /// The model backend.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.inner.provider
    }

    /// Opens a session, creating it on first use. Opening an id that is already live in this
    /// runtime returns that session, with the agent it was opened with. Must be called inside a
    /// tokio runtime.
    pub fn session(&self, agent: &Agent, id: SessionId) -> Session {
        open(&self.inner, agent, id)
    }
}

fn open(rt: &Arc<RtInner>, agent: &Agent, id: SessionId) -> Session {
    let mut sessions = rt.sessions.lock().unwrap();
    sessions.retain(|_, r| r.shared.strong_count() > 0);
    if let Some(r) = sessions.get(&id)
        && let (Some(tx), Some(shared)) = (r.tx.upgrade(), r.shared.upgrade())
    {
        return Session { tx, shared };
    }
    let hooks = rt.hooks.clone().and(agent.hooks.clone());
    let shared = Arc::new(Shared {
        status: Mutex::default(),
        leaf: Mutex::default(),
        id: id.clone(),
        rt: rt.clone(),
        agent: agent.clone(),
        hooks,
        events: broadcast::channel(512).0,
        init: OnceCell::new(),
        initial: Mutex::new(None),
        queue: Mutex::default(),
        ops: Mutex::default(),
    });
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(worker(shared.clone(), rx));
    sessions.insert(
        id,
        LiveRef {
            tx: tx.downgrade(),
            shared: Arc::downgrade(&shared),
        },
    );
    Session { tx, shared }
}

struct NoProvider;

impl Provider for NoProvider {
    fn stream(
        &self,
        _req: ModelRequest,
        _abort: Abort,
    ) -> futures_util::stream::BoxStream<'static, StreamEvent> {
        futures_util::stream::iter([StreamEvent::Done(Response::error(
            ErrorKind::Other,
            "no provider configured (RuntimeBuilder::provider)",
        ))])
        .boxed()
    }
}

/// A snapshot of a session for status lines.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct SessionStatus {
    /// The model in use, after the session's own changes.
    pub model: Option<String>,
    /// The reasoning effort in use, if any is set.
    pub reasoning: Option<Effort>,
    /// The model the session itself last recorded ([`Session::set_model`]); `None` when it uses
    /// the agent's or the runtime's.
    pub session_model: Option<String>,
    /// The effort the session itself last recorded ([`Session::set_reasoning`]); `None` when it
    /// uses the agent's.
    pub session_reasoning: Option<Effort>,
    /// The session's label, if it has one.
    pub label: Option<String>,
    /// Estimated input tokens of the current context.
    pub context_tokens: u64,
    /// The model's context window, when known.
    pub context_window: Option<u32>,
}

pub(crate) struct Shared {
    status: Mutex<SessionStatus>,
    /// The entry the next message follows, as last published; `None` until the session is first
    /// projected.
    leaf: Mutex<Option<Option<String>>>,
    pub(crate) id: SessionId,
    pub(crate) rt: Arc<RtInner>,
    pub(crate) agent: Agent,
    pub(crate) hooks: Hooks,
    pub(crate) events: broadcast::Sender<Event>,
    init: OnceCell<Result<(), StoreError>>,
    /// The entries loaded at init, and whether the session existed before.
    initial: Mutex<Option<(Vec<Entry>, bool)>>,
    /// Steering and follow-up messages, under one lock with whether a turn is running, so a
    /// message is either taken by the turn or refused, never accepted and then lost.
    pub(crate) queue: Mutex<Queue>,
    pub(crate) ops: Mutex<Ops>,
}

/// Messages for the running turn.
#[derive(Default)]
pub(crate) struct Queue {
    pub(crate) running: bool,
    pub(crate) steer: Vec<String>,
    pub(crate) follow: Vec<String>,
}

impl Shared {
    /// Loads the session once and writes the header for a new session.
    async fn init(&self) -> Result<(), Error> {
        let r = self
            .init
            .get_or_init(|| async {
                let store = &self.rt.store;
                let mut entries = store.load(&self.id).await?;
                let resumed = !entries.is_empty();
                if !resumed {
                    let header = Entry::new(
                        None,
                        Record::Header {
                            v: RECORD_VERSION,
                            agent: self.agent.name.clone(),
                        },
                    );
                    store
                        .append(&self.id, std::slice::from_ref(&header))
                        .await?;
                    entries.push(header);
                }
                *self.initial.lock().unwrap() = Some((entries, resumed));
                Ok(())
            })
            .await;
        r.clone().map_err(Error::Store)
    }

    pub(crate) fn env(&self) -> &Arc<dyn Env> {
        self.agent.env.as_ref().unwrap_or(&self.rt.env)
    }

    pub(crate) fn status(&self) -> SessionStatus {
        self.status.lock().unwrap().clone()
    }

    pub(crate) fn model(&self, live: &Live) -> Option<String> {
        live.model
            .clone()
            .or_else(|| self.agent.model.clone())
            .or_else(|| self.rt.model.clone())
            .or_else(|| self.rt.provider.default_model())
    }

    pub(crate) fn window(&self, model: &str) -> Option<u32> {
        self.agent
            .context_window
            .or_else(|| self.rt.provider.model_info(model).map(|i| i.context_window))
    }

    pub(crate) fn publish_status(&self, live: &Live) {
        *self.leaf.lock().unwrap() = Some(live.leaf.clone());
        let model = self.model(live);
        let mut s = self.status.lock().unwrap();
        s.context_window = model.as_deref().and_then(|m| self.window(m));
        s.model = model;
        s.reasoning = live.reasoning.or(self.agent.reasoning);
        s.session_model = live.model.clone();
        s.session_reasoning = live.reasoning;
        s.label = live.label.clone();
        s.context_tokens = live.estimate(prompt_tokens(&self.agent.instructions, &[]));
    }

    pub(crate) fn cx(
        self: &Arc<Self>,
        live: &Live,
        turn: Option<&str>,
        step: Option<usize>,
        abort: &Abort,
    ) -> HookCx {
        HookCx {
            shared: self.clone(),
            turn: turn.map(str::to_string),
            step,
            abort: abort.clone(),
            state: live.state.clone(),
            tools: Arc::from([]),
        }
    }

    /// Runs the `Event` hooks, then sends the event to subscribers.
    pub(crate) async fn emit(&self, cx: &HookCx, event: Event) {
        if self.hooks.has::<Event>() {
            self.hooks.run(event.clone(), cx).await;
        }
        let _ = self.events.send(event);
    }
}

/// The context, read through the worker.
struct Snapshot {
    messages: Vec<Message>,
    state: BTreeMap<String, Value>,
    leaf: Option<String>,
}

/// Work for the session worker. Everything that touches the context runs here, in order.
enum Job {
    Turn(TurnJob),
    Compact {
        compactor: Arc<dyn Compactor>,
        focus: Option<String>,
        done: oneshot::Sender<Result<CompactReport, Error>>,
    },
    Branch {
        leaf: Option<String>,
        done: oneshot::Sender<Result<(), Error>>,
    },
    Record {
        record: Record,
        done: oneshot::Sender<Result<(), Error>>,
    },
    Snapshot {
        done: oneshot::Sender<Result<Snapshot, Error>>,
    },
}

/// A handle to one session. Cloning is cheap; the session's worker stops when the last handle
/// is dropped and the queue is empty.
#[derive(Clone)]
pub struct Session {
    tx: mpsc::UnboundedSender<Job>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.shared.id)
            .field("agent", &self.shared.agent.name)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// The session's id.
    pub fn id(&self) -> &SessionId {
        &self.shared.id
    }

    /// The agent the session was opened with.
    pub fn agent(&self) -> &Agent {
        &self.shared.agent
    }

    /// The runtime the session belongs to.
    pub fn runtime(&self) -> Runtime {
        Runtime::from_inner(self.shared.rt.clone())
    }

    /// Queues a turn for `input`. Turns of one session run in order.
    pub async fn prompt(&self, input: impl Into<String>) -> Result<Turn, Error> {
        self.shared.init().await?;
        let id = new_id();
        let events = self.shared.events.subscribe();
        let abort = Abort::new();
        let (done, done_rx) = oneshot::channel();
        self.tx
            .send(Job::Turn(TurnJob {
                id: id.clone(),
                input: input.into(),
                abort: abort.clone(),
                done,
            }))
            .map_err(|_| Error::Closed)?;
        Ok(Turn {
            id,
            done: done_rx,
            result: None,
            events,
            abort,
            finished: false,
        })
    }

    /// Adds a user message to the running turn at its next step. Returns `false` (and drops the
    /// text) when no turn is running; use [`prompt`](Self::prompt) then.
    pub fn steer(&self, text: impl Into<String>) -> bool {
        let mut queue = self.shared.queue.lock().unwrap();
        if queue.running {
            queue.steer.push(text.into());
        }
        queue.running
    }

    /// Adds a user message for when the running turn would otherwise finish. Returns `false`
    /// (and drops the text) when no turn is running.
    pub fn follow_up(&self, text: impl Into<String>) -> bool {
        let mut queue = self.shared.queue.lock().unwrap();
        if queue.running {
            queue.follow.push(text.into());
        }
        queue.running
    }

    /// Summarises older context with `compactor` (after any queued turns), optionally focused
    /// on a topic. Runs the [`BeforeCompact`](crate::hook::BeforeCompact) and
    /// [`Compacted`](crate::hook::Compacted) hooks.
    pub async fn compact(
        &self,
        compactor: Arc<dyn Compactor>,
        focus: Option<String>,
    ) -> Result<CompactReport, Error> {
        self.shared.init().await?;
        let (done, rx) = oneshot::channel();
        self.tx
            .send(Job::Compact {
                compactor,
                focus,
                done,
            })
            .map_err(|_| Error::Closed)?;
        rx.await.unwrap_or(Err(Error::Closed))
    }

    /// Continues from an earlier tree entry: the next message becomes its child, starting a
    /// branch. `None` starts over from an empty context in the same session file.
    pub async fn branch(&self, entry: Option<String>) -> Result<(), Error> {
        self.shared.init().await?;
        let (done, rx) = oneshot::channel();
        self.tx
            .send(Job::Branch { leaf: entry, done })
            .map_err(|_| Error::Closed)?;
        rx.await.unwrap_or(Err(Error::Closed))
    }

    /// Uses `model` for this session from the next model call on, and records the choice.
    pub async fn set_model(&self, model: impl Into<String>) -> Result<(), Error> {
        self.record(Record::ModelChange {
            model: Some(model.into()),
            reasoning: None,
        })
        .await
    }

    /// Records the reasoning effort for the session's next model calls.
    pub async fn set_reasoning(&self, effort: Effort) -> Result<(), Error> {
        self.record(Record::ModelChange {
            model: None,
            reasoning: Some(effort),
        })
        .await
    }

    /// Names the session.
    pub async fn set_label(&self, name: impl Into<String>) -> Result<(), Error> {
        self.record(Record::Label { name: name.into() }).await
    }

    /// Writes session state between turns (`null` removes the key). Queued behind running work.
    pub async fn set_state(
        &self,
        key: impl Into<String>,
        value: impl serde::Serialize,
    ) -> Result<(), Error> {
        let value = serde_json::to_value(value).unwrap_or(Value::Null);
        self.record(Record::State {
            key: key.into(),
            value,
        })
        .await
    }

    /// Adds a note to the context: the model sees it with its next request, after any running
    /// turn. Unlike [`prompt`](Self::prompt), it starts no turn.
    pub async fn note(&self, text: impl Into<String>) -> Result<(), Error> {
        self.record(Record::Message {
            message: Message::system_note(text),
        })
        .await
    }

    /// Records custom data in the session tree. It never enters the context.
    pub async fn append_custom(
        &self,
        kind: impl Into<String>,
        data: impl serde::Serialize,
    ) -> Result<(), Error> {
        self.record(Record::Custom {
            kind: kind.into(),
            data: serde_json::to_value(data).unwrap_or(Value::Null),
        })
        .await
    }

    async fn record(&self, record: Record) -> Result<(), Error> {
        self.shared.init().await?;
        let (done, rx) = oneshot::channel();
        self.tx
            .send(Job::Record { record, done })
            .map_err(|_| Error::Closed)?;
        rx.await.unwrap_or(Err(Error::Closed))
    }

    /// Whether a turn is running now. Reads of the context ([`history`](Self::history),
    /// [`state`](Self::state)) wait for it to finish.
    pub fn is_running(&self) -> bool {
        self.shared.queue.lock().unwrap().running
    }

    /// Model, context use and label, as of the last change.
    pub fn status(&self) -> SessionStatus {
        self.shared.status()
    }

    /// Every record in the session file.
    pub async fn entries(&self) -> Result<Vec<Entry>, Error> {
        Ok(self.shared.rt.store.load(&self.shared.id).await?)
    }

    /// The current branch: the tree entries from the first to the one the next message follows,
    /// without meta records. After [`branch`](Self::branch), the branch it moved to.
    pub async fn path(&self) -> Result<Vec<Entry>, Error> {
        let leaf = self.snapshot().await?.leaf;
        let entries = self.entries().await?;
        Ok(crate::context::path_to(&entries, leaf.as_deref())
            .into_iter()
            .cloned()
            .collect())
    }

    /// Every event of every turn in this session.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.shared.events.subscribe()
    }

    /// The current branch's messages as recorded so far, without waiting for queued work or a
    /// turn in progress: for showing a session while it runs (its latest message may still be
    /// streaming, and [`subscribe`](Self::subscribe) follows it from here).
    pub async fn history_so_far(&self) -> Result<Vec<Message>, Error> {
        let entries = self.entries().await?;
        let leaf = self.shared.leaf.lock().unwrap().clone();
        let live = match leaf {
            Some(leaf) => Live::project_at(&entries, leaf.as_deref()),
            None => Live::project(&entries),
        };
        // A call still running has no result recorded: the projection's stand-in for one is left out.
        Ok(live
            .messages
            .into_iter()
            .zip(live.ids)
            .filter(|(_, id)| !id.is_empty())
            .map(|(message, _)| message)
            .collect())
    }

    /// The current context, as the model would see it (after any queued work).
    pub async fn history(&self) -> Result<Vec<Message>, Error> {
        Ok(self.snapshot().await?.messages)
    }

    /// Reads a state key (after any queued work). `None` when it is unset or not a `T`.
    pub async fn state<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, Error> {
        Ok(self
            .snapshot()
            .await?
            .state
            .get(key)
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::from_value(v.clone()).ok()))
    }

    /// Every state key and its value (after any queued work).
    pub async fn state_map(&self) -> Result<BTreeMap<String, Value>, Error> {
        Ok(self.snapshot().await?.state)
    }

    async fn snapshot(&self) -> Result<Snapshot, Error> {
        self.shared.init().await?;
        let (done, rx) = oneshot::channel();
        self.tx
            .send(Job::Snapshot { done })
            .map_err(|_| Error::Closed)?;
        rx.await.unwrap_or(Err(Error::Closed))
    }
}

/// One queued or running turn.
pub struct Turn {
    id: String,
    done: oneshot::Receiver<Result<Reply, Error>>,
    /// The result, once `done` resolved while reading events.
    result: Option<Result<Reply, Error>>,
    events: broadcast::Receiver<Event>,
    abort: Abort,
    finished: bool,
}

impl std::fmt::Debug for Turn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turn")
            .field("id", &self.id)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl Turn {
    /// The turn's id, as in its events.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Aborts the turn. The running model call and tools are told; the turn ends as aborted.
    pub fn abort(&self) {
        self.abort.abort();
    }

    /// A handle that aborts this turn from elsewhere, such as a key handler.
    pub fn abort_handle(&self) -> Abort {
        self.abort.clone()
    }

    /// The next event of this turn; `None` after `turn.completed`.
    pub async fn next_event(&mut self) -> Option<Event> {
        if self.finished {
            return None;
        }
        loop {
            let received = if self.result.is_some() {
                // Finished: only events already buffered remain.
                match self.events.try_recv() {
                    Ok(e) => Ok(e),
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        Err(broadcast::error::RecvError::Lagged(n))
                    }
                    Err(_) => Err(broadcast::error::RecvError::Closed),
                }
            } else {
                tokio::select! {
                    biased;
                    e = self.events.recv() => e,
                    r = &mut self.done => {
                        // The worker finished (or died) without us seeing turn.completed yet.
                        self.result = Some(r.unwrap_or(Err(Error::Closed)));
                        continue;
                    }
                }
            };
            match received {
                Ok(e) if e.turn() == self.id => {
                    self.finished = matches!(e, Event::TurnCompleted { .. });
                    return Some(e);
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    self.finished = true;
                    return None;
                }
            }
        }
    }

    /// Waits for the turn to finish. Failed and aborted turns are errors.
    pub async fn wait(self) -> Result<Reply, Error> {
        match self.result {
            Some(r) => r,
            None => self.done.await.unwrap_or(Err(Error::Closed)),
        }
    }
}

async fn worker(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Job>) {
    let mut live: Option<Live> = None;
    while let Some(job) = rx.recv().await {
        if let Err(e) = shared.init().await {
            job.fail(e);
            continue;
        }
        if live.is_none() {
            let (entries, resumed) = shared.initial.lock().unwrap().take().unwrap_or_default();
            let mut l = Live::project(&entries);
            shared.publish_status(&l);
            let abort = Abort::new();
            let cx = shared.cx(&l, None, None, &abort);
            shared.hooks.run(hook::SessionStart { resumed }, &cx).await;
            // A failure here surfaces with the job's own store access.
            let _ = turn::flush(&shared, &mut l, None, &abort).await;
            live = Some(l);
        }
        let live = live.as_mut().unwrap();
        match job {
            Job::Turn(job) => {
                shared.queue.lock().unwrap().running = true;
                let result = AssertUnwindSafe(turn::run(&shared, live, &job))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        Err(Error::Failed("internal error: the turn panicked".into()))
                    });
                // A turn that ends normally has already stopped running with its queue empty;
                // one that failed or was aborted drops what was queued for it.
                *shared.queue.lock().unwrap() = Queue::default();
                let _ = job.done.send(result);
            }
            Job::Compact {
                compactor,
                focus,
                done,
            } => {
                let abort = Abort::new();
                let mut r = turn::compact(&shared, live, compactor, focus, None, &abort).await;
                if let Err(e) = turn::flush(&shared, live, None, &abort).await
                    && r.is_ok()
                {
                    r = Err(e);
                }
                let _ = done.send(r);
            }
            Job::Branch { leaf, done } => {
                let r = match shared.rt.store.load(&shared.id).await {
                    Ok(entries) => {
                        let known = leaf.as_deref().is_none_or(|l| {
                            entries.iter().any(|e| e.id == l && !e.record.is_meta())
                        });
                        if known {
                            *live = Live::project_at(&entries, leaf.as_deref());
                            shared.publish_status(live);
                            Ok(())
                        } else {
                            Err(Error::Failed(format!(
                                "no entry {}",
                                leaf.unwrap_or_default()
                            )))
                        }
                    }
                    Err(e) => Err(e.into()),
                };
                let _ = done.send(r);
            }
            Job::Snapshot { done } => {
                let _ = done.send(Ok(Snapshot {
                    messages: live.messages.clone(),
                    state: (*live.state).clone(),
                    leaf: live.leaf.clone(),
                }));
            }
            Job::Record { record, done } => {
                let _ = done.send(
                    turn::append(&shared, live, vec![record])
                        .await
                        .map_err(Error::from),
                );
            }
        }
    }
}

impl Job {
    fn fail(self, e: Error) {
        match self {
            Job::Turn(j) => {
                let _ = j.done.send(Err(e));
            }
            Job::Compact { done, .. } => {
                let _ = done.send(Err(e));
            }
            Job::Branch { done, .. } | Job::Record { done, .. } => {
                let _ = done.send(Err(e));
            }
            Job::Snapshot { done } => {
                let _ = done.send(Err(e));
            }
        }
    }
}
