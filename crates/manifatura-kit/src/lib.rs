//! Hook bundles for the Manifatura loop. Nothing here runs unless an app adds it; everything is
//! built only on the public API of `manifatura-agent`.
//!
//! | Item | What it is |
//! | --- | --- |
//! | [`Summarizer`] | A [`Compactor`](manifatura_agent::Compactor) that asks a model for a summary |
//! | [`compaction::trim_results`] | A `Context` hook: stubs old, long tool results in what is sent |
//! | [`compaction::on_overflow`] | A `ModelError` hook: compacts once and retries |
//! | [`compaction::auto`] | A `StepEnd` hook: compacts past a share of the window |
//! | [`context`] | `AGENTS.md` files from the repository root down, and a system prompt around them |
//! | [`approve`], [`approve_when`], [`approve_by`] | A `ToolCall` hook that asks someone first |
//! | [`Approvals`] | Pending approval requests with ids, answerable from any client |
//! | [`catalog`] | Deferred tools behind `find_tools` and `run_tool` |
//! | [`repair`] | Coerces tool inputs to the schema |
//! | [`system_delta`] | Keeps the first system prompt; later changes become notes |
//!
//! Order matters where bundles look at the same calls: add [`catalog`] before bundles that look
//! at tool names, such as [`approve`], so they see the real tool rather than `run_tool`; add
//! [`system_delta`] last, after every hook that changes the instructions or tools.
//!
//! ```
//! use manifatura_agent::Agent;
//! use manifatura_kit as kit;
//!
//! let agent = Agent::new("You help.")
//!     .hooks(kit::catalog())
//!     .hooks(kit::approve(["bash"], |_call| async { kit::Answer::Allow }))
//!     .hooks(kit::system_delta());
//! ```

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod approve;
mod catalog;
pub mod compaction;
pub mod context;
mod repair;
mod system_delta;

pub use approve::{
    Answer, ApprovalEvent, Approvals, Request, approval_key, approve, approve_by, approve_when,
    call_scope, denial, record,
};
pub use catalog::{CALL_TOOL, SEARCH_TOOL, catalog, unwrap_call};
pub use compaction::Summarizer;
pub use repair::{repair, repair_input};
pub use system_delta::system_delta;
