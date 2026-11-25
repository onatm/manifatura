use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::stream::BoxStream;
use tokio::sync::Notify;

use crate::types::{ModelInfo, ModelRequest, StreamEvent};

/// A model backend. `stream` never fails: errors arrive as the final
/// [`StreamEvent::Done`] with [`StopReason::Error`](crate::StopReason::Error).
pub trait Provider: Send + Sync {
    /// Streams the reply to `req`. The stream ends with exactly one [`StreamEvent::Done`]; when
    /// `abort` fires it should end soon, with [`StopReason::Aborted`](crate::StopReason::Aborted).
    fn stream(&self, req: ModelRequest, abort: Abort) -> BoxStream<'static, StreamEvent>;

    /// The model to use when neither the agent nor the runtime names one.
    fn default_model(&self) -> Option<String> {
        None
    }

    /// Context window and output limit for `model`, if known.
    fn model_info(&self, _model: &str) -> Option<ModelInfo> {
        None
    }
}

impl<P: Provider + ?Sized> Provider for Arc<P> {
    fn stream(&self, req: ModelRequest, abort: Abort) -> BoxStream<'static, StreamEvent> {
        (**self).stream(req, abort)
    }
    fn default_model(&self) -> Option<String> {
        (**self).default_model()
    }
    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        (**self).model_info(model)
    }
}

/// A cloneable cancellation signal shared by a turn, its model stream and its tool calls.
#[derive(Clone, Default)]
pub struct Abort(Arc<AbortInner>);

#[derive(Default)]
struct AbortInner {
    set: AtomicBool,
    notify: Notify,
}

impl Abort {
    /// A signal not yet fired.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fires the signal. Firing it again does nothing.
    pub fn abort(&self) {
        self.0.set.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Whether the signal has fired.
    pub fn is_aborted(&self) -> bool {
        self.0.set.load(Ordering::SeqCst)
    }

    /// Resolves once [`abort`](Self::abort) has been called.
    pub async fn aborted(&self) {
        loop {
            let mut notified = std::pin::pin!(self.0.notify.notified());
            notified.as_mut().enable();
            if self.is_aborted() {
                return;
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Abort").field(&self.is_aborted()).finish()
    }
}
