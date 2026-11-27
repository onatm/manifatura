//! Incremental server-sent events decoder (M0 finding 6: no crate needed).

use futures_util::StreamExt;
use manifatura_agent::{Abort, StreamEvent};
use tokio::sync::mpsc;

#[derive(Default)]
pub(crate) struct Sse {
    buf: Vec<u8>,
}

/// One decoded event's `data:` lines. Event names are ignored.
pub(crate) struct SseEvent {
    pub data: String,
}

impl Sse {
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) {
        self.buf.extend_from_slice(chunk);
        let mut start = 0;
        while let Some((end, delim)) = find_event_end(&self.buf[start..]) {
            let raw = &self.buf[start..start + end];
            start += end + delim;
            let text = String::from_utf8_lossy(raw);
            let mut data = String::new();
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(v.strip_prefix(' ').unwrap_or(v));
                }
            }
            if !data.is_empty() {
                out.push(SseEvent { data });
            }
        }
        self.buf.drain(..start);
    }
}

fn find_event_end(b: &[u8]) -> Option<(usize, usize)> {
    (0..b.len()).find_map(|i| {
        if b[i..].starts_with(b"\n\n") {
            Some((i, 2))
        } else if b[i..].starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else {
            None
        }
    })
}

pub(crate) enum ReadEnd {
    /// The body ended.
    Eof,
    Aborted,
    /// The handler asked to stop (e.g. `[DONE]`).
    Stopped,
    Network(String),
    /// The consumer dropped the stream.
    Closed,
}

/// Feeds a response body through the decoder, calling `on_event` for each event until it
/// returns `false`, the body ends, or the turn is aborted. Events the handler pushes are
/// forwarded to `tx` after every network chunk.
pub(crate) async fn read(
    resp: reqwest::Response,
    abort: &Abort,
    tx: &mpsc::Sender<StreamEvent>,
    mut on_event: impl FnMut(SseEvent, &mut Vec<StreamEvent>) -> bool,
) -> ReadEnd {
    let mut forward = Vec::new();
    let mut body = resp.bytes_stream();
    let mut sse = Sse::default();
    let mut events = Vec::new();
    loop {
        let chunk = tokio::select! {
            c = body.next() => c,
            _ = abort.aborted() => return ReadEnd::Aborted,
        };
        match chunk {
            None => return ReadEnd::Eof,
            Some(Err(e)) => return ReadEnd::Network(crate::http::error_chain(&e)),
            Some(Ok(bytes)) => {
                sse.push(&bytes, &mut events);
                let mut stop = false;
                for e in events.drain(..) {
                    if !on_event(e, &mut forward) {
                        stop = true;
                        break;
                    }
                }
                for e in forward.drain(..) {
                    if tx.send(e).await.is_err() {
                        return ReadEnd::Closed;
                    }
                }
                if stop {
                    return ReadEnd::Stopped;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_any_byte_and_both_delimiters() {
        let input = "event: a\ndata: {\"x\"\ndata: :1}\n\ndata: [DONE]\r\n\r\n";
        let mut sse = Sse::default();
        let mut out = Vec::new();
        for b in input.bytes() {
            sse.push(&[b], &mut out);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data, "{\"x\"\n:1}");
        assert_eq!(out[1].data, "[DONE]");
    }
}
