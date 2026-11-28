//! Agent definitions: instructions, tools, model settings and hooks.

use std::future::Future;
use std::sync::Arc;

use crate::env::Env;
use crate::hook::HookEvent;
use crate::hooks::{HookCx, Hooks};
use crate::tool::Tool;
use crate::types::Effort;

/// What an agent is: its instructions and tools, model settings, and the hooks that make up
/// its behaviour. Everything that changes per step (dynamic tools, prompt changes) is a
/// [`Step`](crate::hook::Step) hook.
#[derive(Clone)]
pub struct Agent {
    pub(crate) name: String,
    pub(crate) instructions: String,
    pub(crate) tools: Vec<Tool>,
    pub(crate) model: Option<String>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) reasoning: Option<Effort>,
    pub(crate) max_steps: usize,
    pub(crate) hooks: Hooks,
    pub(crate) env: Option<Arc<dyn Env>>,
    pub(crate) context_window: Option<u32>,
}

impl Agent {
    /// An agent with these instructions (its system prompt), no tools and no hooks.
    pub fn new(instructions: impl Into<String>) -> Self {
        Self {
            name: "agent".into(),
            instructions: instructions.into(),
            tools: Vec::new(),
            model: None,
            max_tokens: None,
            reasoning: None,
            max_steps: 50,
            hooks: Hooks::new(),
            env: None,
            context_window: None,
        }
    }

    /// The name recorded in the session header. Defaults to `agent`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// A model specifier such as `llamacpp/gemma` or `openai/gpt-5.5`.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Offers `tool` to the model.
    pub fn tool(mut self, tool: impl AsRef<Tool>) -> Self {
        self.tools.push(tool.as_ref().clone());
        self
    }

    /// Offers each of `tools` to the model.
    pub fn tools<T: AsRef<Tool>>(mut self, tools: impl IntoIterator<Item = T>) -> Self {
        self.tools
            .extend(tools.into_iter().map(|t| t.as_ref().clone()));
        self
    }

    /// Adds a hook for the event type `E` (see [`hook`](crate::hook)).
    pub fn on<E, F, Fut>(mut self, f: F) -> Self
    where
        E: HookEvent,
        F: Fn(E, HookCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = E::Out> + Send + 'static,
    {
        self.hooks.add(f);
        self
    }

    /// Adds a bundle of hooks, after the ones already added.
    pub fn hooks(mut self, hooks: Hooks) -> Self {
        self.hooks.extend(hooks);
        self
    }

    /// Adds a bundle of hooks before the ones already added (still after the runtime's): for a
    /// policy that must decide first, such as refusing a call before an approval hook asks
    /// about it.
    pub fn hooks_first(mut self, hooks: Hooks) -> Self {
        self.hooks = hooks.and(std::mem::take(&mut self.hooks));
        self
    }

    /// The environment for this agent's tools, instead of the runtime's.
    pub fn env(mut self, env: impl Env + 'static) -> Self {
        self.env = Some(Arc::new(env));
        self
    }

    /// The model's context window, overriding what the provider reports. Only reported (in
    /// events and status); nothing acts on it unless a hook does.
    pub fn context_window(mut self, tokens: u32) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// The most output tokens per model call. Unset, the provider decides.
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    /// How much the model reasons. Unset, the provider's default; a session's own choice wins.
    pub fn reasoning(mut self, effort: Effort) -> Self {
        self.reasoning = Some(effort);
        self
    }

    /// Model calls allowed per turn before it fails (default 50).
    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = n.max(1);
        self
    }

    /// The name given with [`name`](Self::name).
    pub fn agent_name(&self) -> &str {
        &self.name
    }

    /// The system prompt the agent was built with. `Step` hooks may change it per call.
    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// The tools the agent was built with. `Step` hooks may change them per call.
    pub fn tool_list(&self) -> &[Tool] {
        &self.tools
    }

    /// The same agent with other instructions, e.g. rebuilt after the app's catalog changed.
    /// Sessions already open keep the agent they were opened with.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// The same agent with `tool` in place of the one of the same name, or added.
    pub fn with_tool(mut self, tool: impl AsRef<Tool>) -> Self {
        let tool = tool.as_ref().clone();
        match self.tools.iter_mut().find(|t| t.name() == tool.name()) {
            Some(have) => *have = tool,
            None => self.tools.push(tool),
        }
        self
    }

    /// The same agent without the tool called `name`.
    pub fn without_tool(mut self, name: &str) -> Self {
        self.tools.retain(|t| t.name() != name);
        self
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}
