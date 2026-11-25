//! Approvals: a `ToolCall` hook that asks someone (a TUI prompt, a chat button, a policy
//! function) before a call runs.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use manifatura_agent::{Hooks, ToolCall, hook};

/// What the person said.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answer {
    /// Allow this call only.
    Allow,
    /// Allow this call, and later calls in the same scope for the rest of the session. The
    /// hook decides the scope: the tool for [`approve`], the exact call for [`approve_when`],
    /// whatever its `scope` function says for [`approve_by`].
    AllowAlways,
    /// Refuse the call; the reason, if any, goes back to the model.
    Deny(Option<String>),
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
                Answer::AllowAlways => {
                    cx.set_state(key, true);
                    hook::Decision::Run(e)
                }
                Answer::Deny(reason) => hook::Decision::Deny(match reason {
                    Some(r) if !r.trim().is_empty() => format!("the user denied this call: {r}"),
                    _ => "the user denied this call".into(),
                }),
            }
        }
    })
}

/// The scope of exactly this call: its tool and its input.
pub fn call_scope(call: &ToolCall) -> String {
    format!("{} {}", call.name, call.input)
}
