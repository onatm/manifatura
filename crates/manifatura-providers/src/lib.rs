//! Model providers behind one `provider/model-id` registry: llama.cpp, built in, and any other
//! server speaking OpenAI-compatible Chat Completions, streamed over SSE. With the `responses`
//! feature, [`responses`] speaks OpenAI's Responses API.
//!
//! ```no_run
//! # fn demo() -> Result<(), manifatura_providers::ModelsError> {
//! // LLAMACPP_URL, MANIFATURA_MODEL
//! let models = manifatura_providers::from_env()?;
//! # Ok(()) }
//! ```

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod acc;
mod http;
#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "responses")]
pub mod responses;
mod sse;

use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use manifatura_agent::{
    Abort, ErrorKind, ModelInfo, ModelRequest, Provider, Response, StreamEvent,
};

#[cfg(feature = "openai")]
pub use openai::OpenAi;
#[cfg(feature = "responses")]
pub use responses::Responses;

/// A provider could not be set up: its key's variable is unset, its kind is not compiled in, or
/// `MANIFATURA_MODEL` names no registered provider.
#[derive(Debug)]
pub struct ModelsError(String);

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelsError {}

/// Routes `provider/model-id` specifiers to registered providers.
#[derive(Clone, Default)]
pub struct Models {
    providers: Vec<(String, Arc<dyn Provider>)>,
    default: Option<String>,
    /// Overrides by provider name (`llamacpp`) or full spec (`llamacpp/qwen3.5-9b`).
    info: Vec<(String, ModelInfo)>,
}

impl std::fmt::Debug for Models {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let providers: Vec<&str> = self.providers.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("Models")
            .field("providers", &providers)
            .field("default", &self.default)
            .finish_non_exhaustive()
    }
}

impl Models {
    /// No providers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `provider` under `name`; a later registration of the same name wins.
    pub fn with(mut self, name: impl Into<String>, provider: impl Provider + 'static) -> Self {
        let name = name.into();
        self.providers.retain(|(n, _)| *n != name);
        self.providers.push((name, Arc::new(provider)));
        self
    }

    /// The `provider/model-id` used when a request names no model.
    pub fn with_default(mut self, spec: impl Into<String>) -> Self {
        self.default = Some(spec.into());
        self
    }

    /// Sets the context window and output limit for a provider's models (`name`) or for one
    /// model (`name/model-id`), overriding the built-in catalog.
    pub fn with_info(mut self, target: impl Into<String>, info: ModelInfo) -> Self {
        let target = target.into();
        self.info.retain(|(t, _)| *t != target);
        self.info.push((target, info));
        self
    }

    /// The registered providers' names, in registration order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.providers.iter().map(|(n, _)| n.as_str())
    }

    fn resolve<'a>(&self, spec: &'a str) -> Option<(&Arc<dyn Provider>, &'a str)> {
        let (name, model) = spec.split_once('/')?;
        let p = self.providers.iter().find(|(n, _)| n == name)?;
        Some((&p.1, model))
    }
}

impl Provider for Models {
    fn stream(&self, mut req: ModelRequest, abort: Abort) -> BoxStream<'static, StreamEvent> {
        match self.resolve(&req.model) {
            Some((p, model)) => {
                req.model = model.to_string();
                p.stream(req, abort)
            }
            None => {
                let known: Vec<&str> = self.names().collect();
                let msg = format!(
                    "no provider for model `{}` (expected provider/model-id; providers: {})",
                    req.model,
                    known.join(", ")
                );
                futures_util::stream::iter([StreamEvent::Done(Response::error(
                    ErrorKind::InvalidRequest,
                    msg,
                ))])
                .boxed()
            }
        }
    }

    fn default_model(&self) -> Option<String> {
        self.default.clone()
    }

    fn model_info(&self, spec: &str) -> Option<ModelInfo> {
        let provider = spec.split_once('/').map(|(p, _)| p).unwrap_or(spec);
        self.info
            .iter()
            .find(|(t, _)| t == spec)
            .or_else(|| self.info.iter().find(|(t, _)| t == provider))
            .map(|(_, i)| *i)
            .or_else(|| catalog(spec))
    }
}

/// Default limits by provider: llama.cpp is assumed small (its `-c`), so set `context_window`
/// for it explicitly. Anything else gets `None` and the runtime's default.
pub fn catalog(spec: &str) -> Option<ModelInfo> {
    match spec.split_once('/')?.0 {
        "llamacpp" => Some(ModelInfo {
            context_window: 8_192,
            max_output: 4_096,
        }),
        _ => None,
    }
}

/// Which wire protocol a configured provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// OpenAI-compatible Chat Completions.
    OpenAi,
}

/// One entry of a providers file (e.g. `providers.toml`), registered under its table name:
///
/// ```toml
/// [groq]
/// kind = "openai"
/// base_url = "https://api.groq.com/openai/v1"
/// api_key_env = "GROQ_API_KEY"
/// ```
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// The API the provider speaks.
    pub kind: Kind,
    /// Where it is; the kind's default when `None`.
    pub base_url: Option<String>,
    /// A literal key. Prefer `api_key_env`; a file holding keys should be private (mode 0600).
    pub api_key: Option<String>,
    /// The environment variable holding the key.
    pub api_key_env: Option<String>,
    /// Context window of this provider's models, e.g. the `-c` of a llama.cpp server.
    pub context_window: Option<u32>,
    /// Output tokens a reply may use, with `context_window`; a quarter of the window (at least
    /// 1,024) when `None`.
    pub max_output: Option<u32>,
}

impl Models {
    /// Registers a configured provider, replacing any provider of the same name.
    pub fn configure(mut self, name: &str, cfg: &ProviderConfig) -> Result<Self, ModelsError> {
        if let Some(context_window) = cfg.context_window {
            let max_output = cfg.max_output.unwrap_or((context_window / 4).max(1_024));
            self = self.with_info(
                name,
                ModelInfo {
                    context_window,
                    max_output,
                },
            );
        }
        #[cfg_attr(not(feature = "openai"), allow(unused_variables))]
        let key = match (&cfg.api_key, &cfg.api_key_env) {
            (Some(k), _) => Some(k.clone()),
            (None, Some(var)) => Some(std::env::var(var).map_err(|_| {
                ModelsError(format!(
                    "provider `{name}`: environment variable {var} is not set"
                ))
            })?),
            (None, None) => None,
        };
        match cfg.kind {
            #[cfg(feature = "openai")]
            Kind::OpenAi => {
                let mut p =
                    OpenAi::compatible(cfg.base_url.as_deref().unwrap_or(openai::DEFAULT_BASE_URL));
                if let Some(k) = key {
                    p = p.key(k);
                }
                Ok(self.with(name, p))
            }
            #[allow(unreachable_patterns)]
            kind => Err(ModelsError(format!(
                "provider `{name}`: {kind:?} support is not compiled in"
            ))),
        }
    }

    /// Whether `spec` names a registered provider.
    pub fn knows(&self, spec: &str) -> bool {
        self.resolve(spec).is_some()
    }
}

/// The llama.cpp provider, `llamacpp`, at `LLAMACPP_URL` (default `http://localhost:8080`).
/// The default model is `MANIFATURA_MODEL`, else none. Other servers are added with
/// [`Models::with`] or [`Models::configure`].
pub fn from_env() -> Result<Models, ModelsError> {
    let mut m = Models::new();
    #[cfg(feature = "openai")]
    {
        let llama = env_or("LLAMACPP_URL", "http://localhost:8080");
        m = m.with(
            "llamacpp",
            OpenAi::compatible(format!("{}/v1", llama.trim_end_matches('/'))),
        );
    }
    if let Ok(spec) = std::env::var("MANIFATURA_MODEL")
        && !spec.is_empty()
    {
        if m.resolve(&spec).is_none() {
            let known: Vec<&str> = m.names().collect();
            return Err(ModelsError(format!(
                "MANIFATURA_MODEL={spec}: expected provider/model-id with one of: {}",
                known.join(", ")
            )));
        }
        m = m.with_default(spec);
    }
    Ok(m)
}

#[cfg(feature = "openai")]
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}
