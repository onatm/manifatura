//! The hook registry and the context every hook gets.

use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::compact::Compactor;
use crate::env::Env;
use crate::hook::HookEvent;
use crate::provider::Abort;
use crate::record::{Entry, SessionId};
use crate::runtime::{Error, SessionStatus, Shared};
use crate::tool::Tool;

type HookFn<E> = Arc<dyn Fn(E, HookCx) -> BoxFuture<'static, <E as HookEvent>::Out> + Send + Sync>;

/// An ordered set of hooks, of any event types. Cheap to clone; a bundle of behaviour is a
/// function returning `Hooks`.
///
/// ```
/// use manifatura_agent::{Hooks, hook};
///
/// fn no_shell() -> Hooks {
///     Hooks::new().on(|e: hook::ToolCall, _cx| async move {
///         match e.call.name.as_str() {
///             "bash" => hook::Decision::Deny("no shell here".into()),
///             _ => hook::Decision::Run(e),
///         }
///     })
/// }
/// ```
#[derive(Clone, Default)]
pub struct Hooks(Vec<(TypeId, Arc<dyn Any + Send + Sync>)>);

impl Hooks {
    /// No hooks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a hook for the event type `E` (see [`hook`](crate::hook)).
    pub fn on<E, F, Fut>(mut self, f: F) -> Self
    where
        E: HookEvent,
        F: Fn(E, HookCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = E::Out> + Send + 'static,
    {
        self.add(f);
        self
    }

    /// [`on`](Self::on), in place.
    pub fn add<E, F, Fut>(&mut self, f: F) -> &mut Self
    where
        E: HookEvent,
        F: Fn(E, HookCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = E::Out> + Send + 'static,
    {
        let f: HookFn<E> = Arc::new(move |e, cx| Box::pin(f(e, cx)));
        self.0.push((TypeId::of::<E>(), Arc::new(f)));
        self
    }

    /// Appends `other`'s hooks after these.
    pub fn and(mut self, other: Hooks) -> Self {
        self.extend(other);
        self
    }

    /// Appends `other`'s hooks after these, in place.
    pub fn extend(&mut self, other: Hooks) {
        self.0.extend(other.0);
    }

    /// Whether no hook is registered.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether any hook for `E` is registered.
    pub fn has<E: HookEvent>(&self) -> bool {
        self.0.iter().any(|(t, _)| *t == TypeId::of::<E>())
    }

    /// Runs the hooks for `E` in order, following the event's composition rule.
    pub(crate) async fn run<E: HookEvent>(&self, mut e: E, cx: &HookCx) -> E::Out {
        let mut hooks = Vec::new();
        for (t, f) in &self.0 {
            if *t == TypeId::of::<E>()
                && let Some(f) = f.downcast_ref::<HookFn<E>>()
            {
                hooks.push(f.clone());
            }
        }
        let last = hooks.len().saturating_sub(1);
        for (i, f) in hooks.into_iter().enumerate() {
            // The last hook's answer is final: no copy is needed for a next one.
            let kept = if i < last { e.keep() } else { None };
            match E::next(f(e, cx.clone()).await, kept) {
                ControlFlow::Continue(next) => e = next,
                ControlFlow::Break(out) => return out,
            }
        }
        e.finish()
    }
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Hooks").field(&self.0.len()).finish()
    }
}

/// Changes hooks asked for, applied by the loop at its next safe point: before each model call
/// (once before the `Step` hooks and once after them), when the turn ends, and after hooks
/// that run outside turns; never between a tool call and its result.
#[derive(Default)]
pub(crate) struct Ops {
    pub writes: Vec<(String, Value)>,
    pub custom: Vec<(String, Value)>,
    pub notes: Vec<String>,
    pub compact: Option<(Arc<dyn Compactor>, Option<String>)>,
    pub fail: Option<String>,
}

/// What a hook can see and do.
#[derive(Clone)]
pub struct HookCx {
    pub(crate) shared: Arc<Shared>,
    pub(crate) turn: Option<String>,
    pub(crate) step: Option<usize>,
    pub(crate) abort: Abort,
    pub(crate) state: Arc<BTreeMap<String, Value>>,
    pub(crate) tools: Arc<[Tool]>,
}

impl std::fmt::Debug for HookCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookCx")
            .field("session", &self.shared.id)
            .field("turn", &self.turn)
            .field("step", &self.step)
            .finish_non_exhaustive()
    }
}

impl HookCx {
    /// The session the hook runs in.
    pub fn session_id(&self) -> &SessionId {
        &self.shared.id
    }

    /// The running turn, if any.
    pub fn turn_id(&self) -> Option<&str> {
        self.turn.as_deref()
    }

    /// The 1-based model call within the turn, from the first `Step` on.
    pub fn step(&self) -> Option<usize> {
        self.step
    }

    /// The tools of this step, as the `Step` hooks left them (deferred ones included). Empty
    /// before the `Step` hooks have run and outside turns.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// One of [`tools`](Self::tools), by name.
    pub fn tool(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|t| t.name() == name)
    }

    /// The turn's abort signal (a fresh one outside turns).
    pub fn abort(&self) -> &Abort {
        &self.abort
    }

    /// The environment the agent's tools act in.
    pub fn env(&self) -> &Arc<dyn Env> {
        self.shared.env()
    }

    /// Model, reasoning, label and context use, as of the last change.
    pub fn status(&self) -> SessionStatus {
        self.shared.status()
    }

    /// Session state, including writes hooks made that are not applied yet.
    pub fn state<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let pending = self
            .shared
            .ops
            .lock()
            .unwrap()
            .writes
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone());
        let v = match pending {
            Some(v) => v,
            None => self.state.get(key)?.clone(),
        };
        if v.is_null() {
            return None;
        }
        serde_json::from_value(v).ok()
    }

    /// Every state key and its value, including writes not applied yet.
    pub fn state_map(&self) -> BTreeMap<String, Value> {
        let mut map = (*self.state).clone();
        for (k, v) in &self.shared.ops.lock().unwrap().writes {
            if v.is_null() {
                map.remove(k);
            } else {
                map.insert(k.clone(), v.clone());
            }
        }
        map
    }

    /// Writes session state (`null` removes the key).
    pub fn set_state(&self, key: impl Into<String>, value: impl Serialize) {
        let v = serde_json::to_value(value).unwrap_or(Value::Null);
        self.shared.ops.lock().unwrap().writes.push((key.into(), v));
    }

    /// Removes a state key. Applied like [`set_state`](Self::set_state).
    pub fn clear_state(&self, key: impl Into<String>) {
        self.set_state(key, Value::Null);
    }

    /// Records custom data in the session tree. It never enters the context.
    pub fn custom(&self, kind: impl Into<String>, data: impl Serialize) {
        let v = serde_json::to_value(data).unwrap_or(Value::Null);
        self.shared
            .ops
            .lock()
            .unwrap()
            .custom
            .push((kind.into(), v));
    }

    /// Adds a note the model sees from its next request on.
    pub fn note(&self, text: impl Into<String>) {
        self.shared.ops.lock().unwrap().notes.push(text.into());
    }

    /// Adds a user message to the running turn at its next step.
    pub fn steer(&self, text: impl Into<String>) {
        self.shared.queue.lock().unwrap().steer.push(text.into());
    }

    /// Adds a user message for when the model would otherwise finish the turn.
    pub fn follow_up(&self, text: impl Into<String>) {
        self.shared.queue.lock().unwrap().follow.push(text.into());
    }

    /// Compacts at the next safe point: before the next model call (so a
    /// [`Recover::Retry`](crate::hook::Recover::Retry) runs on the compacted context), or once
    /// this hook returns when no turn is running. A later call replaces an earlier one.
    pub fn compact(&self, compactor: Arc<dyn Compactor>, focus: Option<String>) {
        self.shared.ops.lock().unwrap().compact = Some((compactor, focus));
    }

    /// Fails the running turn with `reason` at the next safe point (before the next model call
    /// at the latest). Outside turns it does nothing.
    pub fn fail(&self, reason: impl Into<String>) {
        self.shared
            .ops
            .lock()
            .unwrap()
            .fail
            .get_or_insert(reason.into());
    }

    /// Every record in the session.
    pub async fn entries(&self) -> Result<Vec<Entry>, Error> {
        Ok(self.shared.rt.store.load(&self.shared.id).await?)
    }
}
