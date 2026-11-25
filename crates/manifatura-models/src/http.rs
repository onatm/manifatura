//! Shared HTTP plumbing: one client, retries before the stream starts, error classification,
//! and the channel-backed event stream every provider returns.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use manifatura_agent::{Abort, ErrorKind, ProviderError, Response, StreamEvent};
use tokio::sync::mpsc;

const ATTEMPTS: u32 = 4;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

pub(crate) fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                // Per read, so long generations are fine; a stalled stream fails.
                .read_timeout(Duration::from_secs(180))
                .build()
                .expect("HTTP client")
        })
        .clone()
}

pub(crate) enum SendError {
    Aborted,
    Failed(ProviderError),
}

/// Sends a request, retrying network errors, 429 and 5xx with backoff (honouring
/// `retry-after`). Only a successful response is returned.
pub(crate) async fn send(
    build: impl Fn() -> reqwest::RequestBuilder,
    abort: &Abort,
) -> Result<reqwest::Response, SendError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let last = attempt == ATTEMPTS;
        let result = tokio::select! {
            r = build().send() => r,
            _ = abort.aborted() => return Err(SendError::Aborted),
        };
        let wait = match result {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .map(Duration::from_secs_f64);
                let retryable = matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529);
                let wait = retry_after.unwrap_or_else(|| backoff(attempt));
                if !retryable || last || wait > MAX_RETRY_WAIT {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(SendError::Failed(classify(status, &body)));
                }
                wait
            }
            Err(e) => {
                if last || e.is_builder() {
                    return Err(SendError::Failed(ProviderError {
                        kind: ErrorKind::Network,
                        message: error_chain(&e),
                    }));
                }
                backoff(attempt)
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = abort.aborted() => return Err(SendError::Aborted),
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt.min(6)))
}

pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s.push_str(": ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    s
}

pub(crate) fn classify(status: u16, body: &str) -> ProviderError {
    let lower = body.to_ascii_lowercase();
    let overflow = [
        "prompt is too long",
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "too many tokens",
        "exceeds the context",
        "request too large",
    ]
    .iter()
    .any(|m| lower.contains(m));
    let kind = match status {
        _ if overflow => ErrorKind::ContextOverflow,
        401 | 403 => ErrorKind::Auth,
        429 => ErrorKind::RateLimited,
        500..=599 => ErrorKind::Server,
        400..=499 => ErrorKind::InvalidRequest,
        _ => ErrorKind::Other,
    };
    let mut body = body.trim().to_string();
    if body.len() > 2000 {
        let cut = body.floor_char_boundary(2000);
        body.truncate(cut);
        body.push('…');
    }
    ProviderError {
        kind,
        message: format!("HTTP {status}: {body}"),
    }
}

/// Runs `f` on a task and exposes what it sends as the provider stream. Dropping the stream
/// stops the task at its next send.
pub(crate) fn spawn_stream<F, Fut>(f: F) -> BoxStream<'static, StreamEvent>
where
    F: FnOnce(mpsc::Sender<StreamEvent>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(f(tx));
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) })
        .boxed()
}

/// Sends the final response.
pub(crate) async fn finish(tx: &mpsc::Sender<StreamEvent>, r: Response) {
    let _ = tx.send(StreamEvent::Done(r)).await;
}
