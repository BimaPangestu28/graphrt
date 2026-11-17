//! Shared HTTP transport for providers: client construction and SSE decoding.

use std::collections::VecDeque;
use std::time::Duration;

use futures::StreamExt;

use crate::error::LlmError;
use crate::sse::{SseEvent, SseParser};
use crate::types::{ChatDelta, ChatStream};

/// Build a reqwest client using rustls with the ring crypto provider.
pub(crate) fn build_client(connect_timeout: Duration) -> Result<reqwest::Client, LlmError> {
    // reqwest is built without a bundled crypto provider; install ring once per process.
    // An error only means a provider is already installed, which is fine.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|e| LlmError::Config(format!("cannot build HTTP client: {e}")))
}

/// Converts provider SSE events into deltas.
pub(crate) trait SseDecoder: Send + 'static {
    /// Decode one event. Returning `Ok(None)` signals the end of the stream.
    fn decode(&mut self, event: SseEvent) -> Result<Option<Vec<ChatDelta>>, LlmError>;
}

fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let raw = headers.get("retry-after-ms").or_else(|| headers.get(reqwest::header::RETRY_AFTER))?;
    let text = raw.to_str().ok()?.trim();
    if headers.contains_key("retry-after-ms") {
        return text.parse::<u64>().ok();
    }
    let seconds: f64 = text.parse().ok()?;
    if seconds.is_finite() && seconds >= 0.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((seconds * 1000.0) as u64)
    } else {
        None
    }
}

fn network_error(e: &reqwest::Error) -> LlmError {
    LlmError::Network(e.to_string())
}

/// Send a request and stream its SSE body through `decoder`.
pub(crate) async fn send_sse<D: SseDecoder>(
    request: reqwest::RequestBuilder,
    decoder: D,
) -> Result<ChatStream, LlmError> {
    let response = request.send().await.map_err(|e| network_error(&e))?;
    let status = response.status();
    if !status.is_success() {
        let retry = retry_after_ms(response.headers());
        let body = response.text().await.unwrap_or_default();
        return Err(LlmError::from_status(status.as_u16(), &body, retry));
    }

    struct State<D> {
        bytes: futures::stream::BoxStream<'static, Result<Vec<u8>, LlmError>>,
        parser: SseParser,
        decoder: D,
        pending: VecDeque<Result<ChatDelta, LlmError>>,
        finished: bool,
    }

    impl<D: SseDecoder> State<D> {
        /// Decode events; returns true when the decoder signalled the end.
        fn feed(&mut self, events: Vec<SseEvent>) -> bool {
            for event in events {
                match self.decoder.decode(event) {
                    Ok(Some(deltas)) => self.pending.extend(deltas.into_iter().map(Ok)),
                    Ok(None) => return true,
                    Err(e) => {
                        self.pending.push_back(Err(e));
                        return true;
                    }
                }
            }
            false
        }
    }

    let bytes = response.bytes_stream().map(|r| r.map(|b| b.to_vec()).map_err(|e| network_error(&e))).boxed();
    let state = State { bytes, parser: SseParser::new(), decoder, pending: VecDeque::new(), finished: false };

    let stream = futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.pending.pop_front() {
                return Some((item, st));
            }
            if st.finished {
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    let events = st.parser.push(&chunk);
                    st.finished = st.feed(events);
                }
                Some(Err(e)) => {
                    st.finished = true;
                    st.pending.push_back(Err(e));
                }
                None => {
                    let tail: Vec<SseEvent> = st.parser.finish().into_iter().collect();
                    st.feed(tail);
                    st.finished = true;
                }
            }
        }
    });
    Ok(Box::pin(stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_parsing() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, reqwest::header::HeaderValue::from_static("2"));
        assert_eq!(retry_after_ms(&h), Some(2000));
        h.insert("retry-after-ms", reqwest::header::HeaderValue::from_static("150"));
        assert_eq!(retry_after_ms(&h), Some(150));
    }
}
