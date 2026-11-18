//! Shared HTTP transport for providers: client construction and SSE decoding.

// Which helpers are used depends on the enabled providers.
#![cfg_attr(
    not(all(feature = "openai", feature = "anthropic", feature = "gemini", feature = "bedrock")),
    allow(dead_code)
)]

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::StreamExt;

use crate::error::LlmError;
use crate::sse::{SseEvent, SseParser};
use crate::types::{ChatDelta, ChatStream};

/// Maximum bytes of an error response body that are read (the rest is ignored).
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Build a reqwest client using rustls with the ring crypto provider.
pub(crate) fn build_client(connect_timeout: Duration) -> Result<reqwest::Client, LlmError> {
    // reqwest is built without a bundled crypto provider; install ring once per process.
    // An error only means a provider is already installed, which is fine.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        // Never follow redirects: reqwest strips only `Authorization`/`Cookie` on
        // cross-host hops, so key headers (`x-api-key`, `x-goog-api-key`, `api-key`)
        // and the prompt body would follow a redirect to another host.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| LlmError::Config(format!("cannot build HTTP client: {e}")))
}

/// An HTTP client built on first use.
///
/// Building a client loads the system trust store (hundreds of certificate
/// files on Linux, Security.framework on macOS), so providers that a run never
/// calls should not pay for it at startup. Clones share the same client.
#[derive(Clone, Debug)]
pub(crate) struct LazyClient {
    connect_timeout: Duration,
    cell: Arc<OnceLock<Result<reqwest::Client, String>>>,
}

impl LazyClient {
    pub(crate) fn new(connect_timeout: Duration) -> Self {
        Self { connect_timeout, cell: Arc::new(OnceLock::new()) }
    }

    /// The client, built on the first call.
    ///
    /// @throws `LlmError::Config` when the TLS backend cannot be initialised.
    pub(crate) fn get(&self) -> Result<&reqwest::Client, LlmError> {
        self.cell
            .get_or_init(|| build_client(self.connect_timeout).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|message| LlmError::Config(message.clone()))
    }
}

use crate::credentials::is_secure_endpoint;

/// `scheme://host[:port]` of a URL, lowercased, for same-origin checks.
fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{}://{}", scheme.to_ascii_lowercase(), authority.to_ascii_lowercase())
}

/// The endpoint for one request, honoring a credential-supplied base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub base_url: String,
    /// Static config headers / query parameters may be attached: false when the
    /// credential moved the request to a different origin than the configured one,
    /// so operator secrets in config headers never follow a credential override.
    pub send_static: bool,
}

/// Resolve the base URL for a request: a credential's `base_url` wins (it must be
/// a secure endpoint); otherwise the configured one.
pub(crate) fn resolve_endpoint(configured: &str, credential_base: Option<&str>) -> Result<Endpoint, LlmError> {
    match credential_base.filter(|u| !u.is_empty()) {
        None => Ok(Endpoint { base_url: configured.to_owned(), send_static: true }),
        Some(url) => {
            if !is_secure_endpoint(url) {
                return Err(LlmError::Auth(
                    "credential supplied a non-https base_url; refusing to send credentials there".into(),
                ));
            }
            let send_static = origin(url) == origin(configured);
            Ok(Endpoint { base_url: url.to_owned(), send_static })
        }
    }
}

/// Result of decoding one SSE event.
#[derive(Debug, Default)]
pub(crate) struct Decoded {
    pub deltas: Vec<ChatDelta>,
    /// The provider signalled the end of the response.
    pub done: bool,
}

impl Decoded {
    pub fn deltas(deltas: Vec<ChatDelta>) -> Self {
        Self { deltas, done: false }
    }

    pub fn done(deltas: Vec<ChatDelta>) -> Self {
        Self { deltas, done: true }
    }
}

/// Converts provider SSE events into deltas.
pub(crate) trait SseDecoder: Send + 'static {
    /// Decode one event.
    fn decode(&mut self, event: SseEvent) -> Result<Decoded, LlmError>;

    /// Called when the body ends without the decoder having reported `done`.
    /// Returns the deltas to emit, or an error when the response is incomplete.
    fn end_of_stream(&mut self) -> Result<Vec<ChatDelta>, LlmError> {
        Err(truncated())
    }
}

/// The error used for bodies that end before the provider's end-of-stream marker.
pub(crate) fn truncated() -> LlmError {
    LlmError::Network("stream ended unexpectedly before the response was complete".into())
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

fn network_error(e: reqwest::Error) -> LlmError {
    // `without_url` keeps credentials passed in query strings out of error messages.
    LlmError::Network(e.without_url().to_string())
}

/// Read at most [`MAX_ERROR_BODY_BYTES`] of an error response body.
async fn read_error_body(response: reqwest::Response) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                let room = MAX_ERROR_BODY_BYTES.saturating_sub(bytes.len());
                bytes.extend(chunk.iter().take(room));
                if bytes.len() >= MAX_ERROR_BODY_BYTES {
                    break;
                }
            }
            Err(e) => {
                let partial = String::from_utf8_lossy(&bytes).into_owned();
                return format!("{partial} (error body could not be read completely: {})", e.without_url());
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Send a request and stream its SSE body through `decoder`.
pub(crate) async fn send_sse<D: SseDecoder>(
    request: reqwest::RequestBuilder,
    decoder: D,
) -> Result<ChatStream, LlmError> {
    let response = send_request(request, LlmError::from_status).await?;
    Ok(sse_stream(response, decoder))
}

/// Send a request; non-success responses are read (capped) and turned into an
/// error by `classify(status, body, retry_after_ms)`.
pub(crate) async fn send_request(
    request: reqwest::RequestBuilder,
    classify: impl FnOnce(u16, &str, Option<u64>) -> LlmError,
) -> Result<reqwest::Response, LlmError> {
    let response = request.send().await.map_err(network_error)?;
    let status = response.status();
    if status.is_redirection() {
        return Err(LlmError::BadRequest(format!(
            "provider endpoint answered with a redirect (HTTP {}); redirects are not followed so credentials \
             cannot leak to another host — point `base_url` at the final endpoint",
            status.as_u16()
        )));
    }
    if !status.is_success() {
        let retry = retry_after_ms(response.headers());
        let body = read_error_body(response).await;
        return Err(classify(status.as_u16(), &body, retry));
    }
    Ok(response)
}

/// Body of a successful response as a stream of byte chunks.
pub(crate) fn byte_stream(
    response: reqwest::Response,
) -> futures::stream::BoxStream<'static, Result<Vec<u8>, LlmError>> {
    response.bytes_stream().map(|r| r.map(|b| b.to_vec()).map_err(network_error)).boxed()
}

/// Stream an SSE response body through `decoder`.
pub(crate) fn sse_stream<D: SseDecoder>(response: reqwest::Response, decoder: D) -> ChatStream {
    struct State<D> {
        bytes: futures::stream::BoxStream<'static, Result<Vec<u8>, LlmError>>,
        parser: SseParser,
        decoder: D,
        pending: VecDeque<Result<ChatDelta, LlmError>>,
        finished: bool,
    }

    impl<D: SseDecoder> State<D> {
        /// Decode events; returns true when the decoder signalled the end or failed.
        fn feed(&mut self, events: Vec<SseEvent>) -> bool {
            for event in events {
                match self.decoder.decode(event) {
                    Ok(decoded) => {
                        self.pending.extend(decoded.deltas.into_iter().map(Ok));
                        if decoded.done {
                            return true;
                        }
                    }
                    Err(e) => {
                        self.pending.push_back(Err(e));
                        return true;
                    }
                }
            }
            false
        }

        fn fail(&mut self, error: LlmError) {
            self.pending.push_back(Err(error));
            self.finished = true;
        }
    }

    let bytes = byte_stream(response);
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
                Some(Ok(chunk)) => match st.parser.push(&chunk) {
                    Ok(events) => st.finished = st.feed(events),
                    Err(e) => st.fail(LlmError::Decode(e.to_string())),
                },
                Some(Err(e)) => st.fail(e),
                None => {
                    let tail: Vec<SseEvent> = match st.parser.finish() {
                        Ok(tail) => tail.into_iter().collect(),
                        Err(e) => {
                            st.fail(LlmError::Decode(e.to_string()));
                            continue;
                        }
                    };
                    if st.feed(tail) {
                        st.finished = true;
                        continue;
                    }
                    match st.decoder.end_of_stream() {
                        Ok(deltas) => {
                            st.pending.extend(deltas.into_iter().map(Ok));
                            st.finished = true;
                        }
                        Err(e) => st.fail(e),
                    }
                }
            }
        }
    });
    Box::pin(stream)
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

    #[test]
    fn secure_endpoints() {
        assert!(is_secure_endpoint("https://api.example.com/v1"));
        assert!(is_secure_endpoint("http://localhost:8080/v1"));
        assert!(is_secure_endpoint("http://127.0.0.1:9/x"));
        assert!(is_secure_endpoint("http://[::1]:9/x"));
        assert!(is_secure_endpoint("http://10.0.2.2:8787"));
        assert!(!is_secure_endpoint("http://api.example.com"));
        assert!(!is_secure_endpoint("http://localhost.evil.com"));
        assert!(!is_secure_endpoint("ftp://x"));
    }

    #[test]
    fn endpoint_override_drops_static_headers_cross_origin() {
        let e = resolve_endpoint("https://api.openai.com/v1", None).unwrap();
        assert!(e.send_static);
        let e = resolve_endpoint("https://api.openai.com/v1", Some("https://proxy.example.com/v1")).unwrap();
        assert_eq!(e.base_url, "https://proxy.example.com/v1");
        assert!(!e.send_static);
        let e = resolve_endpoint("https://proxy.example.com/v1", Some("https://PROXY.example.com/v2")).unwrap();
        assert!(e.send_static);
        assert!(resolve_endpoint("https://api.openai.com/v1", Some("http://attacker.example")).is_err());
    }

    #[test]
    fn truncation_is_retryable() {
        assert!(truncated().is_retryable());
    }
}
