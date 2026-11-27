//! Foundation: the agent loop, typed hooks, sessions, the context, and the cross-crate traits.
//!
//! The loop decides nothing a hook could decide. Policy (approvals, compaction, input repair)
//! is registered as hooks by whoever builds on top.
//!
//! ```no_run
//! # async fn demo(provider: impl manifatura_agent::Provider + 'static) -> Result<(), manifatura_agent::Error> {
//! use manifatura_agent::{Agent, Runtime, SessionId, hook};
//!
//! let agent = Agent::new("You are concise.").on(|e: hook::ToolCall, _cx| async move {
//!     hook::Decision::Run(e)
//! });
//! let rt = Runtime::builder().provider(provider).build();
//! let reply = rt.session(&agent, SessionId::new()).prompt("Hi").await?.wait().await?;
//! println!("{reply}");
//! # Ok(()) }
//! ```

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod agent;
mod compact;
mod context;
mod env;
mod event;
pub mod hook;
mod hooks;
mod provider;
mod record;
mod runtime;
#[cfg(feature = "testing")]
pub mod testing;
mod tool;
mod turn;
mod types;

pub use agent::Agent;
pub use compact::{CompactCx, CompactPlan, CompactReport, Compactor};
pub use context::estimate_tokens;
pub use env::{DirEntry, Env, ExecSpec, FileKind, Process, ProcessInput, Stat, TtySize};
pub use event::{Event, Outcome};
pub use hook::HookEvent;
pub use hooks::{HookCx, Hooks};
pub use provider::{Abort, Provider};
pub use record::{
    Entry, MemoryStore, RECORD_VERSION, Record, SessionId, SessionMeta, Store, StoreError, new_id,
    session_meta,
};
pub use runtime::{Error, Reply, Runtime, RuntimeBuilder, Session, SessionStatus, Turn};
pub use tool::{
    IntoToolResult, Json, Tool, ToolBuilder, ToolCx, ToolError, ToolHandler, ToolResult, schema_for,
};
pub use types::{
    Content, Effort, ErrorKind, Grammar, GrammarSyntax, Message, ModelInfo, ModelRequest,
    ProviderError, Response, Role, StopReason, StreamEvent, TEXT_INPUT, ToolCall, ToolChoice,
    ToolFormat, ToolSpec, Usage, system_note_text, text_input_schema,
};

pub use async_trait::async_trait;
pub use futures_util::stream::BoxStream;
