//! Session records and the [`Store`] trait.
//!
//! A session is an append-only list of [`Entry`]s. Tree entries (header, messages, state,
//! compactions, custom data) carry a `parent_id`; the context is the path from the last tree
//! entry back to the root, so branching is a new child of an earlier entry. Meta entries
//! (labels, model changes) have no parent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::Message;

/// The version written in each session's header record.
pub const RECORD_VERSION: u32 = 1;

/// A session's id: any string, or a new time-ordered one from [`SessionId::new`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// A new time-ordered id, e.g. `0199f3a2c1b4-5e1f9a07`.
    pub fn new() -> Self {
        Self(format!("{:012x}-{}", now_ms(), &new_id()[..8]))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Ids used as file names must be non-empty and use only `[A-Za-z0-9._-]`.
    pub fn is_path_safe(&self) -> bool {
        !self.0.is_empty()
            && !self.0.starts_with('.')
            && self
                .0
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One line of a session: a record and its place in the tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Unique within the session.
    pub id: String,
    /// The entry this one follows; `None` for a root. Meta records have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    /// What the entry holds.
    #[serde(flatten)]
    pub record: Record,
}

impl Entry {
    /// A new entry after `parent_id`, with a fresh id and the current time.
    pub fn new(parent_id: Option<String>, record: Record) -> Self {
        Self {
            id: new_id(),
            parent_id,
            ts: now_ms(),
            record,
        }
    }
}

/// What an entry holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Record {
    /// The first entry of every session.
    Header {
        /// The record format's version ([`RECORD_VERSION`]).
        v: u32,
        /// The name of the agent that started the session.
        agent: String,
    },
    /// A message of the conversation.
    Message {
        /// The message.
        message: Message,
    },
    /// `null` removes the key.
    State {
        /// The state key.
        key: String,
        /// Its new value.
        value: Value,
    },
    /// Messages before `first_kept` leave the context, replaced by `summary`.
    Compaction {
        /// What the summary says about the messages it replaces.
        summary: String,
        /// The id of the first entry kept.
        first_kept: String,
        /// Estimated input tokens before compacting.
        tokens_before: u64,
        /// Anything else the compactor wants to keep with the summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// Data of an app, extension or hook. Part of the tree, never part of the context.
    Custom {
        /// What the data is, chosen by whoever writes it.
        kind: String,
        /// The data.
        data: Value,
    },
    /// A human-readable session name.
    Label {
        /// The name.
        name: String,
    },
    /// Model or reasoning chosen for the session from now on. `None` fields are unchanged.
    ModelChange {
        /// The model now used; `None` leaves it unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// The reasoning effort now used; `None` leaves it unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<crate::types::Effort>,
    },
}

impl Record {
    /// Session settings (labels, model changes) stay outside the tree and apply wherever they
    /// sit in the log.
    pub fn is_meta(&self) -> bool {
        matches!(self, Record::Label { .. } | Record::ModelChange { .. })
    }
}

/// What a store lists about a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The session's id.
    pub id: SessionId,
    /// The agent named in its header, if it has one.
    pub agent: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub updated: u64,
}

/// A store failed. Keeps the error that caused it, when there is one, as its
/// [`source`](std::error::Error::source).
#[derive(Clone, Debug)]
pub struct StoreError {
    message: String,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
}

impl StoreError {
    /// A failure described by `message`, with no underlying error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// `message`, caused by `source`.
    pub fn with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Arc::new(source)),
        }
    }

    /// What went wrong, without the `store:` prefix.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The kind of the I/O error behind this one, if it came from I/O.
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        let source = self.source.as_deref()?;
        source
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind)
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store: {}", self.message)
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::with_source(e.to_string(), e)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::with_source(e.to_string(), e)
    }
}

/// Where sessions are kept, as append-only lists of entries.
#[async_trait]
pub trait Store: Send + Sync {
    /// Appends in order. Implementations choose their own flushing; nothing in the framework
    /// asks for `fsync`.
    async fn append(&self, session: &SessionId, entries: &[Entry]) -> Result<(), StoreError>;
    /// Every entry of a session, in the order appended; empty for a session never written.
    async fn load(&self, session: &SessionId) -> Result<Vec<Entry>, StoreError>;
    /// Every session in the store.
    async fn list(&self) -> Result<Vec<SessionMeta>, StoreError>;
}

#[async_trait]
impl<S: Store + ?Sized> Store for Arc<S> {
    async fn append(&self, session: &SessionId, entries: &[Entry]) -> Result<(), StoreError> {
        (**self).append(session, entries).await
    }
    async fn load(&self, session: &SessionId) -> Result<Vec<Entry>, StoreError> {
        (**self).load(session).await
    }
    async fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        (**self).list().await
    }
}

/// Keeps sessions in process memory. The default store; also for tests and evals.
#[derive(Default)]
pub struct MemoryStore {
    sessions: Mutex<HashMap<SessionId, Vec<Entry>>>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sessions = self.sessions.lock().unwrap().len();
        f.debug_struct("MemoryStore")
            .field("sessions", &sessions)
            .finish()
    }
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn append(&self, session: &SessionId, entries: &[Entry]) -> Result<(), StoreError> {
        let mut map = self.sessions.lock().unwrap();
        map.entry(session.clone())
            .or_default()
            .extend_from_slice(entries);
        Ok(())
    }

    async fn load(&self, session: &SessionId) -> Result<Vec<Entry>, StoreError> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(session)
            .cloned()
            .unwrap_or_default())
    }

    async fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        let map = self.sessions.lock().unwrap();
        let mut out: Vec<SessionMeta> = map
            .iter()
            .map(|(id, log)| session_meta(id.clone(), log))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
}

/// Builds [`SessionMeta`] from a session's header and last entry.
pub fn session_meta(id: SessionId, log: &[Entry]) -> SessionMeta {
    let agent = match log.first().map(|e| &e.record) {
        Some(Record::Header { agent, .. }) => Some(agent.clone()),
        _ => None,
    };
    SessionMeta {
        id,
        agent,
        updated: log.last().map(|e| e.ts).unwrap_or(0),
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 16 hex characters, unique enough for entry and turn ids.
pub fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("{:016x}", h.finish())
}
