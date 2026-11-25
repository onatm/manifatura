//! Tools: a cloneable [`Tool`] handle, typed construction without macros, and the
//! per-call [`ToolCx`].

use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::env::Env;
use crate::provider::Abort;
use crate::record::SessionId;
use crate::runtime::Runtime;
use crate::types::ToolSpec;

/// What a tool hands back to the model (flue's `{output, terminate}` envelope).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    /// What the model reads.
    pub output: String,
    /// Whether the call failed. The model sees the output either way.
    pub is_error: bool,
    /// End the turn after this tool batch instead of asking the model again.
    pub terminate: bool,
}

impl ToolResult {
    /// A successful result.
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
            terminate: false,
        }
    }

    /// A failed result; `output` says why.
    pub fn error(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: true,
            terminate: false,
        }
    }

    /// The result, ending the turn after this tool batch.
    pub fn terminate(mut self) -> Self {
        self.terminate = true;
        self
    }
}

/// A tool failure. Any `std::error::Error` converts into it, so `?` works inside tools.
#[derive(Debug)]
pub struct ToolError(String);

impl ToolError {
    /// A failure described by `message`.
    pub fn msg(message: impl std::fmt::Display) -> Self {
        Self(message.to_string())
    }
}

impl<E: std::error::Error> From<E> for ToolError {
    fn from(e: E) -> Self {
        Self(e.to_string())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Values a typed tool may return. Strings pass through verbatim; wrap structs in [`Json`].
pub trait IntoToolResult {
    /// The value as a tool result.
    fn into_tool_result(self) -> ToolResult;
}

impl IntoToolResult for ToolResult {
    fn into_tool_result(self) -> ToolResult {
        self
    }
}
impl IntoToolResult for String {
    fn into_tool_result(self) -> ToolResult {
        ToolResult::ok(self)
    }
}
impl IntoToolResult for &str {
    fn into_tool_result(self) -> ToolResult {
        ToolResult::ok(self)
    }
}
impl IntoToolResult for () {
    fn into_tool_result(self) -> ToolResult {
        ToolResult::ok("ok")
    }
}
impl IntoToolResult for Value {
    fn into_tool_result(self) -> ToolResult {
        match self {
            Value::String(s) => ToolResult::ok(s),
            v => ToolResult::ok(v.to_string()),
        }
    }
}

/// Serialises any value as the tool output.
#[derive(Clone, Debug)]
pub struct Json<T>(pub T);

impl<T: Serialize> IntoToolResult for Json<T> {
    fn into_tool_result(self) -> ToolResult {
        match serde_json::to_value(&self.0) {
            Ok(v) => v.into_tool_result(),
            Err(e) => ToolResult::error(format!("could not serialise tool output: {e}")),
        }
    }
}

/// The untyped tool interface. Most tools are built with [`Tool::new`] instead.
#[async_trait]
pub trait ToolHandler: Send + Sync + 'static {
    /// Runs the tool on `input`.
    async fn call(&self, input: Value, cx: ToolCx) -> ToolResult;
}

/// A cloneable tool handle: a spec plus a handler.
#[derive(Clone)]
pub struct Tool(Arc<ToolInner>);

struct ToolInner {
    spec: ToolSpec,
    flags: Flags,
    handler: Arc<dyn ToolHandler>,
}

#[derive(Clone, Debug, Default)]
struct Flags {
    sequential: bool,
    deferred: bool,
    /// Guidance for the system prompt on when and how to use the tool.
    guideline: Option<String>,
}

impl Tool {
    /// Starts a tool definition; finish it with [`ToolBuilder::run`] or [`ToolBuilder::run_json`].
    #[allow(clippy::new_ret_no_self)]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> ToolBuilder {
        ToolBuilder {
            name: name.into(),
            description: description.into(),
            flags: Flags::default(),
        }
    }

    /// A tool from a spec and an untyped handler, such as one an MCP server describes.
    pub fn from_handler(spec: ToolSpec, handler: impl ToolHandler) -> Self {
        Self(Arc::new(ToolInner {
            spec,
            flags: Flags::default(),
            handler: Arc::new(handler),
        }))
    }

    /// The same tool, with its flags changed by `f`.
    fn with_flags(&self, f: impl FnOnce(&mut Flags)) -> Tool {
        let mut flags = self.0.flags.clone();
        f(&mut flags);
        Tool(Arc::new(ToolInner {
            spec: self.0.spec.clone(),
            flags,
            handler: self.0.handler.clone(),
        }))
    }

    /// The same tool, sequential or not (see [`ToolBuilder::sequential`]).
    pub fn with_sequential(&self, sequential: bool) -> Tool {
        self.with_flags(|f| f.sequential = sequential)
    }

    /// The same tool, deferred or not (see [`ToolBuilder::deferred`]).
    pub fn with_deferred(&self, deferred: bool) -> Tool {
        self.with_flags(|f| f.deferred = deferred)
    }

    /// The same tool with other prompt guidance (see [`ToolBuilder::guideline`]).
    pub fn with_guideline(&self, text: impl Into<String>) -> Tool {
        let text = text.into();
        self.with_flags(|f| f.guideline = Some(text))
    }

    /// Whether the tool is left out of what the model is offered (see
    /// [`ToolBuilder::deferred`]).
    pub fn is_deferred(&self) -> bool {
        self.0.flags.deferred
    }

    /// Guidance for the system prompt, if the tool has any. The loop does not send it; an app
    /// that builds its prompt from the agent's tools does (`kit::context::SystemPrompt::tools`).
    pub fn guideline(&self) -> Option<&str> {
        self.0.flags.guideline.as_deref()
    }

    /// The name, description and input schema the model sees.
    pub fn spec(&self) -> &ToolSpec {
        &self.0.spec
    }

    /// The name the model calls the tool by.
    pub fn name(&self) -> &str {
        &self.0.spec.name
    }

    /// Sequential tools never run concurrently with other calls in the same batch.
    pub fn is_sequential(&self) -> bool {
        self.0.flags.sequential
    }

    /// Runs the tool on `input`, outside the loop: tests, or a tool that calls another.
    pub async fn call(&self, input: Value, cx: ToolCx) -> ToolResult {
        self.0.handler.call(input, cx).await
    }
}

impl AsRef<Tool> for Tool {
    fn as_ref(&self) -> &Tool {
        self
    }
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Tool").field(&self.0.spec.name).finish()
    }
}

/// A tool being defined: started with [`Tool::new`], finished with [`run`](Self::run) or
/// [`run_json`](Self::run_json).
#[derive(Debug)]
pub struct ToolBuilder {
    name: String,
    description: String,
    flags: Flags,
}

impl ToolBuilder {
    /// Never run alongside other calls of the same batch: the batch waits for it, and it for
    /// the batch.
    pub fn sequential(mut self) -> Self {
        self.flags.sequential = true;
        self
    }

    /// Not offered to the model, but runnable when a call names it (e.g. through
    /// `kit::catalog`, which adds tools to find and run deferred ones).
    pub fn deferred(mut self) -> Self {
        self.flags.deferred = true;
        self
    }

    /// A line for the system prompt on when and how to use the tool, beyond its description
    /// (e.g. "Read a file before you edit it.").
    pub fn guideline(mut self, text: impl Into<String>) -> Self {
        self.flags.guideline = Some(text.into());
        self
    }

    /// A typed tool: the input schema comes from `I`'s `JsonSchema`, and the input is
    /// deserialised before `f` runs (a mismatch becomes a tool error the model can fix).
    pub fn run<I, O, F, Fut>(self, f: F) -> Tool
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: IntoToolResult + 'static,
        F: Fn(I, ToolCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
    {
        let schema = schema_for::<I>();
        self.finish(schema, Typed { f, _p: PhantomData })
    }

    /// An untyped tool with a hand-written JSON Schema.
    pub fn run_json<O, F, Fut>(self, input_schema: Value, f: F) -> Tool
    where
        O: IntoToolResult + 'static,
        F: Fn(Value, ToolCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
    {
        self.finish(input_schema, Untyped { f, _p: PhantomData })
    }

    fn finish(self, input_schema: Value, handler: impl ToolHandler) -> Tool {
        Tool(Arc::new(ToolInner {
            spec: ToolSpec {
                name: self.name,
                description: self.description,
                input_schema,
            },
            flags: self.flags,
            handler: Arc::new(handler),
        }))
    }
}

/// A JSON Schema for `T` with subschemas inlined and metadata keys removed, which small and
/// local models handle better than `$ref`s.
pub fn schema_for<T: JsonSchema>() -> Value {
    let generator = schemars::generate::SchemaSettings::draft2020_12()
        .with(|s| s.inline_subschemas = true)
        .into_generator();
    let mut v = generator.into_root_schema_for::<T>().to_value();
    if let Some(o) = v.as_object_mut() {
        o.remove("$schema");
        o.remove("title");
    }
    v
}

struct Typed<I, O, F> {
    f: F,
    _p: PhantomData<fn(I) -> O>,
}

#[async_trait]
impl<I, O, F, Fut> ToolHandler for Typed<I, O, F>
where
    I: DeserializeOwned + Send + 'static,
    O: IntoToolResult + 'static,
    F: Fn(I, ToolCx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
{
    async fn call(&self, input: Value, cx: ToolCx) -> ToolResult {
        let input: I = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("invalid input: {e}")),
        };
        match (self.f)(input, cx).await {
            Ok(o) => o.into_tool_result(),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

struct Untyped<O, F> {
    f: F,
    _p: PhantomData<fn() -> O>,
}

#[async_trait]
impl<O, F, Fut> ToolHandler for Untyped<O, F>
where
    O: IntoToolResult + 'static,
    F: Fn(Value, ToolCx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
{
    async fn call(&self, input: Value, cx: ToolCx) -> ToolResult {
        match (self.f)(input, cx).await {
            Ok(o) => o.into_tool_result(),
            Err(e) => ToolResult::error(e.to_string()),
        }
    }
}

/// State writes buffered by one tool call.
pub(crate) type Writes = Arc<Mutex<Vec<(String, Value)>>>;

/// Per-call context handed to a tool.
#[derive(Clone)]
pub struct ToolCx {
    pub(crate) call_id: String,
    pub(crate) session: SessionId,
    pub(crate) abort: Abort,
    pub(crate) state: Arc<BTreeMap<String, Value>>,
    pub(crate) writes: Writes,
    pub(crate) log: Arc<dyn Fn(String) + Send + Sync>,
    pub(crate) env: Arc<dyn Env>,
    pub(crate) runtime: Runtime,
}

impl std::fmt::Debug for ToolCx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCx")
            .field("call_id", &self.call_id)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl ToolCx {
    /// A context for calling a tool outside a session, e.g. in unit tests.
    pub fn detached(call_id: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            session: SessionId::from("detached"),
            abort: Abort::new(),
            state: Arc::default(),
            writes: Arc::default(),
            log: Arc::new(|_| {}),
            env: crate::env::no_env_arc(),
            runtime: Runtime::builder().build(),
        }
    }

    /// Replaces the env, e.g. for calling a tool directly in tests.
    pub fn with_env(mut self, env: Arc<dyn Env>) -> Self {
        self.env = env;
        self
    }

    /// The environment this tool acts in.
    pub fn env(&self) -> &Arc<dyn Env> {
        &self.env
    }

    /// The id of the call being answered.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The session the call belongs to.
    pub fn session_id(&self) -> &SessionId {
        &self.session
    }

    /// The runtime the session belongs to, e.g. for a tool that opens a session of its own.
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Fires when the turn is aborted; long-running tools should stop early.
    pub fn abort(&self) -> &Abort {
        &self.abort
    }

    /// Reads session state. Sees this call's own writes, and otherwise the state as it was
    /// when the tool batch started.
    pub fn state<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let writes = self.writes.lock().unwrap();
        let v = match writes.iter().rev().find(|(k, _)| k == key) {
            Some((_, v)) => v.clone(),
            None => self.state.get(key)?.clone(),
        };
        drop(writes);
        if v.is_null() {
            return None;
        }
        serde_json::from_value(v).ok()
    }

    /// Writes session state. The write is recorded with the tool batch; parallel writes to one
    /// key resolve in tool-call order.
    pub fn set_state(&self, key: impl Into<String>, value: impl Serialize) {
        let v = serde_json::to_value(value).unwrap_or(Value::Null);
        self.writes.lock().unwrap().push((key.into(), v));
    }

    /// Removes a state key. Recorded like [`set_state`](Self::set_state).
    pub fn clear_state(&self, key: impl Into<String>) {
        self.writes.lock().unwrap().push((key.into(), Value::Null));
    }

    /// Emits an `action.log` event for this call.
    pub fn log(&self, text: impl Into<String>) {
        (self.log)(text.into())
    }
}
