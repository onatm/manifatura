//! Compaction as mechanism: the plan (what would be summarised, cut by pi's rules), the
//! [`Compactor`] that writes a summary, and the report. Nothing here runs by itself; call
//! [`Session::compact`](crate::Session::compact), or [`HookCx::compact`](crate::HookCx::compact)
//! from a hook.

use std::sync::Arc;

use async_trait::async_trait;

use crate::provider::{Abort, Provider};
use crate::types::{Message, ModelInfo};

/// What a compaction would summarise.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactPlan {
    /// The context before the cut, oldest first. When the session was compacted before, the
    /// first message is the previous summary.
    pub messages: Vec<Message>,
    /// The previous summary, if any (also the first of `messages`).
    pub previous_summary: Option<String>,
    /// The entry the kept context starts at. The cut never separates a tool result from its
    /// call.
    pub first_kept: String,
    /// Estimated input tokens before compacting.
    pub tokens_before: u64,
    /// What the summary should pay attention to.
    pub focus: Option<String>,
}

/// What a [`Compactor`] gets besides the plan.
pub struct CompactCx {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) model: String,
    pub(crate) abort: Abort,
}

impl std::fmt::Debug for CompactCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactCx")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl CompactCx {
    /// The runtime's provider, for compactors that ask a model.
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    /// The session's current model.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Context window and output limit of the session's model, if the provider knows them.
    pub fn model_info(&self) -> Option<ModelInfo> {
        self.provider.model_info(&self.model)
    }

    /// Fires when the compaction is cancelled; a compactor waiting on a model should stop.
    pub fn abort(&self) -> &Abort {
        &self.abort
    }
}

/// Writes the summary for a [`CompactPlan`].
#[async_trait]
pub trait Compactor: Send + Sync {
    /// Recent context kept verbatim. A compaction keeps at most the last quarter of the
    /// context, so it acts on short sessions too.
    fn keep_recent_tokens(&self) -> u64 {
        20_000
    }

    /// The summary of `plan.messages`, or why there is none.
    async fn summarize(&self, plan: &CompactPlan, cx: &CompactCx) -> Result<String, String>;
}

/// What a compaction did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// False when there was nothing worth summarising, or a hook cancelled it.
    pub summarized: bool,
    /// Estimated input tokens before compacting.
    pub tokens_before: u64,
    /// Estimated input tokens after; the same as `tokens_before` when nothing was summarised.
    pub tokens_after: u64,
}
