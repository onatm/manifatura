//! manifatura: a small library for building agents into your own programs: a loop, typed hooks,
//! and sessions kept as a tree.
//!
//! Guides: <https://manifatura.dev>.
//!
//! The foundation (agent loop, typed hooks, sessions, tools, records) is re-exported at the root.
//! Building blocks sit behind cargo features:
//!
//! | Feature | Module |
//! | --- | --- |
//! | `kit` (default) | [`kit`]: hook bundles (compaction, approvals, catalog, input repair, system delta) |
//! | `models-openai` (default) | [`models`]: llama.cpp and other OpenAI-compatible servers |
//! | `store-jsonl` (default) | [`store`] |
//! | `tools` (default) | [`tools`]: `read`, `edit`, `grep`, `glob`, `bash` and [`tools::Local`], the env on this machine |
//! | `mcp` | [`mcp`]: MCP servers' tools, over stdio or HTTP |
//! | `testing` | `testing`: a scripted provider for tests |

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub use manifatura_agent::*;

#[cfg(feature = "kit")]
pub use manifatura_kit as kit;

#[cfg(feature = "models-openai")]
pub use manifatura_models as models;

#[cfg(feature = "store-jsonl")]
pub use manifatura_store as store;

#[cfg(feature = "tools")]
pub use manifatura_tools as tools;

#[cfg(feature = "mcp")]
pub use manifatura_mcp as mcp;

pub use schemars;
pub use serde_json;
