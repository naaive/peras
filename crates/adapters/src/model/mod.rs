//! Model ports: vendor adapters, encoders and composable layers.

pub mod anthropic;
pub mod layers;
pub mod openai_compat;
pub mod sse;

use agent_proto::ModelError;
use agent_runtime::Delta;
use futures::stream::{BoxStream, StreamExt};
use sse::{SseEvent, SseParser};
use std::collections::VecDeque;

pub use anthropic::{AnthropicEncoderV1, AnthropicStreamMapper, Claude};
pub use layers::{Meter, MeterTotals, Metered, ModelPortExt, Price, Quota, RateLimited, Retry, RetryPolicy};
pub use openai_compat::{OpenAiCompat, OpenAiEncoderV1, OpenAiStreamMapper};

/// Turns vendor SSE events into semantic deltas. Stateful per response.
///
/// Canonical ordering shared by every mapper: content deltas, then one
/// `Usage`, then `Stop` as the last item.
pub trait StreamMapper: Send {
    fn on_event(&mut self, ev: SseEvent) -> Vec<Result<Delta, ModelError>>;
    /// The byte stream ended. Returns trailing deltas (or an error if the
    /// response was cut before its stop).
    fn finish(&mut self) -> Vec<Result<Delta, ModelError>>;
}

/// Run a complete recorded byte stream through a parser + mapper. Used by the
/// fixture tests and by anyone replaying captured responses.
pub fn map_recorded(bytes: &[u8], mapper: &mut dyn StreamMapper) -> Vec<Result<Delta, ModelError>> {
    let mut p = SseParser::new();
    let mut out = Vec::new();
    for ev in p.push(bytes).into_iter().chain(p.finish()) {
        out.extend(mapper.on_event(ev));
    }
    out.extend(mapper.finish());
    truncate_after_terminal(out)
}

/// Stop after the first `Stop` or error.
fn truncate_after_terminal(v: Vec<Result<Delta, ModelError>>) -> Vec<Result<Delta, ModelError>> {
    let mut out = Vec::with_capacity(v.len());
    for d in v {
        let term = matches!(d, Err(_) | Ok(Delta::Stop(_)));
        out.push(d);
        if term {
            break;
        }
    }
    out
}

/// Stream a successful HTTP response through a mapper.
pub(crate) fn delta_stream<'a, M: StreamMapper + 'a>(
    resp: reqwest::Response,
    mapper: M,
) -> BoxStream<'a, Result<Delta, ModelError>> {
    struct St<M> {
        bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
        parser: SseParser,
        mapper: M,
        pending: VecDeque<Result<Delta, ModelError>>,
        ended: bool,
        done: bool,
    }
    let st = St {
        bytes: resp.bytes_stream().boxed(),
        parser: SseParser::new(),
        mapper,
        pending: VecDeque::new(),
        ended: false,
        done: false,
    };
    futures::stream::unfold(st, |mut st| async move {
        loop {
            if st.done {
                return None;
            }
            if let Some(d) = st.pending.pop_front() {
                if matches!(d, Err(_) | Ok(Delta::Stop(_))) {
                    st.done = true;
                }
                return Some((d, st));
            }
            if st.ended {
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    for ev in st.parser.push(&chunk) {
                        st.pending.extend(st.mapper.on_event(ev));
                    }
                }
                Some(Err(e)) => {
                    st.pending.push_back(Err(ModelError::Network { message: e.to_string() }));
                    st.ended = true;
                }
                None => {
                    for ev in st.parser.finish() {
                        st.pending.extend(st.mapper.on_event(ev));
                    }
                    st.pending.extend(st.mapper.finish());
                    st.ended = true;
                }
            }
        }
    })
    .boxed()
}

/// Parse a `retry-after` header (seconds, possibly fractional) or
/// `retry-after-ms`.
pub(crate) fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(ms) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<f64>().ok())
    {
        return Some(ms.max(0.0) as u64);
    }
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|s| (s.max(0.0) * 1000.0) as u64)
}

/// Heuristic: does an error message describe a context-length overflow?
pub(crate) fn is_overflow_message(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("prompt is too long")
        || m.contains("context length")
        || m.contains("context_length")
        || m.contains("context window")
        || m.contains("maximum context")
        || m.contains("too many tokens")
}

/// Vendor-neutral HTTP status mapping (vendors refine on top).
pub(crate) fn map_status(status: u16, retry_after: Option<u64>, message: String) -> ModelError {
    match status {
        429 => ModelError::RateLimited { retry_after_ms: retry_after },
        529 | 503 => ModelError::Overloaded,
        401 | 403 => ModelError::Auth,
        413 => ModelError::Overflow,
        400 | 422 if is_overflow_message(&message) => ModelError::Overflow,
        404 => ModelError::Unavailable { message },
        408 | 500 | 502 | 504 => ModelError::Network { message: format!("http {status}: {message}") },
        s if s >= 500 => ModelError::Overloaded,
        _ => ModelError::Invalid { message: format!("http {status}: {message}") },
    }
}

/// Minimal standard base64 (for inlining image blobs).
pub(crate) fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn status_mapping() {
        assert_eq!(map_status(429, Some(5), String::new()), ModelError::RateLimited { retry_after_ms: Some(5) });
        assert_eq!(map_status(529, None, String::new()), ModelError::Overloaded);
        assert_eq!(map_status(401, None, String::new()), ModelError::Auth);
        assert_eq!(map_status(400, None, "prompt is too long: 300000 tokens".into()), ModelError::Overflow);
        assert!(matches!(map_status(400, None, "bad".into()), ModelError::Invalid { .. }));
        assert!(matches!(map_status(404, None, "model".into()), ModelError::Unavailable { .. }));
    }
}
