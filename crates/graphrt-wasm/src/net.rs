//! Outbound HTTP for guests (`graphrt::http`) and the host services a plugin gets.
//!
//! graphrt-wasm does not link an HTTP client. The embedder hands one in as an
//! [`HttpEgress`] (the graphrt CLI uses graphrt-http's SSRF-safe client, the one
//! the `http-request` node uses), and only to plugins allowed to use the
//! network. A module importing `graphrt::http` without that grant fails at load.
//!
//! Every request a guest makes is checked here before it reaches the egress:
//! the method, the header names, the per-call request count, the timeout (capped
//! by [`crate::WasmLimits::http_timeout_ms`]) and the body sizes. The egress then
//! applies its destination policy (scheme, hosts, public addresses only, no
//! redirects).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::limits::WasmLimits;

/// Methods a guest may use.
const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// Headers the host owns: framing, routing and hop-by-hop headers.
const FORBIDDEN_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-authorization",
    "proxy-connection",
];

/// One outbound request, validated by the host.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Upper-case method, one of GET, HEAD, POST, PUT, PATCH, DELETE.
    pub method: String,
    pub url: String,
    /// Header name (lower-case) and value pairs.
    pub headers: Vec<(String, String)>,
    /// UTF-8 request body.
    pub body: Option<String>,
    /// Total deadline of the request.
    pub timeout: Duration,
    /// Largest response body accepted, in bytes.
    pub max_response_bytes: usize,
}

impl fmt::Debug for HttpRequest {
    /// Header values are credentials more often than not: only names are shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &names)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// A response handed back to the guest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HttpResponse {
    pub status: u16,
    /// Header name (lower-case) and value pairs.
    pub headers: Vec<(String, String)>,
    /// UTF-8 response body.
    pub body: String,
}

/// Why a request failed. The guest receives `kind` and `message`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HttpError {
    /// `invalid_request`, `egress_denied`, `timeout`, `too_large` or `http_error`.
    pub kind: String,
    pub message: String,
}

impl HttpError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        Self { kind: kind.to_owned(), message: message.into() }
    }
}

/// Sends guest HTTP requests. Implementations enforce the destination policy.
///
/// `send` is called from the thread running the guest, which is a Tokio
/// blocking-pool thread when the `tokio` feature is on, so an implementation
/// may block on a future with `tokio::runtime::Handle::block_on`.
pub trait HttpEgress: Send + Sync + fmt::Debug {
    /// Send `request` and wait for the response.
    ///
    /// @throws HttpError - The destination is refused, the request timed out or failed,
    /// or the body is too large or not UTF-8.
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// Host services granted to one plugin, next to its [`WasmLimits`].
#[derive(Clone, Default)]
pub struct WasmHost {
    /// Outbound HTTP (`graphrt::http`). `None`: a module importing it fails at load.
    pub egress: Option<Arc<dyn HttpEgress>>,
    /// Host variables the plugin receives in every request (`env`), e.g. the
    /// ones its manifest declares in `permissions.env`.
    pub env: BTreeMap<String, String>,
}

impl fmt::Debug for WasmHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmHost")
            .field("egress", &self.egress.is_some())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl WasmHost {
    /// Grant outbound HTTP through `egress`.
    pub fn with_egress(mut self, egress: Arc<dyn HttpEgress>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Hand `env` to the guest in every request.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }
}

/// The request JSON a guest passes to `graphrt::http`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestRequest {
    #[serde(default = "default_method")]
    method: String,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn default_method() -> String {
    "GET".to_owned()
}

/// Parse and validate the request JSON written by the guest.
///
/// @throws HttpError kind `invalid_request` - Malformed JSON, a method or header the
/// guest may not use, or a body above the limit.
pub(crate) fn parse_request(bytes: &[u8], limits: &WasmLimits) -> Result<HttpRequest, HttpError> {
    let request: GuestRequest =
        serde_json::from_slice(bytes).map_err(|e| HttpError::new("invalid_request", format!("request JSON: {e}")))?;
    let method = request.method.to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) {
        return Err(HttpError::new("invalid_request", format!("method `{method}` is not allowed")));
    }
    let mut headers = Vec::with_capacity(request.headers.len());
    for (name, value) in request.headers {
        let name = name.to_ascii_lowercase();
        if FORBIDDEN_HEADERS.contains(&name.as_str()) {
            return Err(HttpError::new("invalid_request", format!("header `{name}` is set by the host")));
        }
        headers.push((name, value));
    }
    if let Some(body) = &request.body
        && body.len() > limits.http_request_bytes
    {
        return Err(HttpError::new(
            "too_large",
            format!("request body is {} bytes, above the {}-byte limit", body.len(), limits.http_request_bytes),
        ));
    }
    let cap = limits.http_timeout_ms.max(1);
    let timeout_ms = request.timeout_ms.unwrap_or(cap).clamp(1, cap);
    Ok(HttpRequest {
        method,
        url: request.url,
        headers,
        body: request.body,
        timeout: Duration::from_millis(timeout_ms),
        max_response_bytes: limits.http_response_bytes,
    })
}

/// The JSON written back to the guest: `{status, headers, body}` or `{error: {kind, message}}`.
pub(crate) fn encode_result(result: &Result<HttpResponse, HttpError>) -> Vec<u8> {
    let value = match result {
        Ok(response) => serde_json::json!({
            "status": response.status,
            "headers": response.headers.iter().cloned().collect::<BTreeMap<_, _>>(),
            "body": response.body,
        }),
        Err(error) => serde_json::json!({ "error": error }),
    };
    serde_json::to_vec(&value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<HttpRequest, HttpError> {
        parse_request(json.as_bytes(), &WasmLimits::default())
    }

    #[test]
    fn parses_and_normalises() {
        let request = parse(
            r#"{"method":"post","url":"https://x.test/a","headers":{"ApiKey":"k"},"body":"{}","timeout_ms":5000}"#,
        )
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.headers, vec![("apikey".to_owned(), "k".to_owned())]);
        assert_eq!(request.timeout, Duration::from_secs(5));
        let debug = format!("{request:?}");
        assert!(debug.contains("apikey") && !debug.contains("\"k\""), "{debug}");
    }

    #[test]
    fn timeout_is_capped_by_the_operator() {
        let limits = WasmLimits::default();
        let request = parse(r#"{"url":"https://x.test","timeout_ms":999999999}"#).unwrap();
        assert_eq!(request.timeout, Duration::from_millis(limits.http_timeout_ms));
        assert_eq!(
            parse(r#"{"url":"https://x.test"}"#).unwrap().timeout,
            Duration::from_millis(limits.http_timeout_ms)
        );
        assert_eq!(parse(r#"{"url":"https://x.test","timeout_ms":0}"#).unwrap().timeout, Duration::from_millis(1));
    }

    #[test]
    fn refuses_what_the_host_owns() {
        assert_eq!(parse(r#"{"method":"CONNECT","url":"https://x.test"}"#).unwrap_err().kind, "invalid_request");
        assert_eq!(parse(r#"{"url":"https://x.test","headers":{"Host":"evil"}}"#).unwrap_err().kind, "invalid_request");
        assert_eq!(parse(r#"{"url":"https://x.test","extra":1}"#).unwrap_err().kind, "invalid_request");
        let limits = WasmLimits { http_request_bytes: 4, ..WasmLimits::default() };
        let error = parse_request(br#"{"url":"https://x.test","body":"12345"}"#, &limits).unwrap_err();
        assert_eq!(error.kind, "too_large");
    }

    #[test]
    fn encodes_both_shapes() {
        let ok = encode_result(&Ok(HttpResponse {
            status: 200,
            headers: vec![("a".into(), "b".into())],
            body: "hi".into(),
        }));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&ok).unwrap(),
            serde_json::json!({"status":200,"headers":{"a":"b"},"body":"hi"})
        );
        let err = encode_result(&Err(HttpError::new("timeout", "slow")));
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&err).unwrap()["error"]["kind"], "timeout");
    }
}
