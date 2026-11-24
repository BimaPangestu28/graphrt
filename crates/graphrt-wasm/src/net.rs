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
//!
//! Bodies are bytes. A guest sends text as `body` or bytes as `body_base64`,
//! and gets the response as UTF-8 `body` (a non-UTF-8 body is an error) unless
//! it asks for `"response_encoding": "base64"`, which returns `body_base64`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
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
    /// Request body bytes.
    pub body: Option<Vec<u8>>,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Header name (lower-case) and value pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
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
    /// or the body is too large.
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
    /// Binary body, base64 (standard alphabet, padded); exclusive with `body`.
    #[serde(default)]
    body_base64: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// `utf8` (default): the response is `body` text; `base64`: `body_base64`.
    #[serde(default)]
    response_encoding: ResponseEncoding,
}

/// How the response body is written back to the guest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ResponseEncoding {
    /// `body`, UTF-8 text; any other body is an `http_error`.
    #[default]
    Utf8,
    /// `body_base64`, any bytes.
    Base64,
}

/// A validated guest request and how its response goes back.
#[derive(Debug)]
pub(crate) struct GuestHttpCall {
    pub request: HttpRequest,
    pub response_encoding: ResponseEncoding,
}

fn default_method() -> String {
    "GET".to_owned()
}

/// Parse and validate the request JSON written by the guest.
///
/// @throws HttpError kind `invalid_request` - Malformed JSON, a method or header the
/// guest may not use, both `body` and `body_base64`, or base64 that does not decode.
/// @throws HttpError kind `too_large` - A body above `http_request_bytes`.
pub(crate) fn parse_request(bytes: &[u8], limits: &WasmLimits) -> Result<GuestHttpCall, HttpError> {
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
    let response_encoding = request.response_encoding;
    let body = request_body(request.body, request.body_base64)?;
    if let Some(body) = &body
        && body.len() > limits.http_request_bytes
    {
        return Err(HttpError::new(
            "too_large",
            format!("request body is {} bytes, above the {}-byte limit", body.len(), limits.http_request_bytes),
        ));
    }
    let cap = limits.http_timeout_ms.max(1);
    let timeout_ms = request.timeout_ms.unwrap_or(cap).clamp(1, cap);
    let request = HttpRequest {
        method,
        url: request.url,
        headers,
        body,
        timeout: Duration::from_millis(timeout_ms),
        max_response_bytes: limits.http_response_bytes,
    };
    Ok(GuestHttpCall { request, response_encoding })
}

fn request_body(text: Option<String>, base64: Option<String>) -> Result<Option<Vec<u8>>, HttpError> {
    match (text, base64) {
        (Some(_), Some(_)) => Err(HttpError::new("invalid_request", "set `body` or `body_base64`, not both")),
        (Some(text), None) => Ok(Some(text.into_bytes())),
        (None, Some(encoded)) => BASE64
            .decode(encoded.as_bytes())
            .map(Some)
            .map_err(|e| HttpError::new("invalid_request", format!("body_base64: {e}"))),
        (None, None) => Ok(None),
    }
}

/// The JSON written back to the guest: `{status, headers, body}` (or `body_base64`) or
/// `{error: {kind, message}}`.
pub(crate) fn encode_result(result: Result<HttpResponse, HttpError>, encoding: ResponseEncoding) -> Vec<u8> {
    let value = match result.and_then(|response| encode_response(response, encoding)) {
        Ok(value) => value,
        Err(error) => serde_json::json!({ "error": error }),
    };
    serde_json::to_vec(&value).unwrap_or_default()
}

fn encode_response(response: HttpResponse, encoding: ResponseEncoding) -> Result<serde_json::Value, HttpError> {
    let headers = response.headers.into_iter().collect::<BTreeMap<_, _>>();
    let (field, body) = match encoding {
        ResponseEncoding::Utf8 => {
            let text = String::from_utf8(response.body).map_err(|_| {
                HttpError::new(
                    "http_error",
                    "response body is not UTF-8 text; ask for \"response_encoding\": \"base64\"",
                )
            })?;
            ("body", text)
        }
        ResponseEncoding::Base64 => ("body_base64", BASE64.encode(&response.body)),
    };
    Ok(serde_json::json!({ "status": response.status, "headers": headers, field: body }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_call(json: &str) -> Result<GuestHttpCall, HttpError> {
        parse_request(json.as_bytes(), &WasmLimits::default())
    }

    fn parse(json: &str) -> Result<HttpRequest, HttpError> {
        parse_call(json).map(|call| call.request)
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
        let ok = encode_result(
            Ok(HttpResponse { status: 200, headers: vec![("a".into(), "b".into())], body: b"hi".to_vec() }),
            ResponseEncoding::Utf8,
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&ok).unwrap(),
            serde_json::json!({"status":200,"headers":{"a":"b"},"body":"hi"})
        );
        let err = encode_result(Err(HttpError::new("timeout", "slow")), ResponseEncoding::Utf8);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&err).unwrap()["error"]["kind"], "timeout");
    }

    #[test]
    fn binary_bodies_go_both_ways_as_base64() {
        let call = parse_call(
            r#"{"method":"PUT","url":"https://x.test/o","body_base64":"/wAB","response_encoding":"base64"}"#,
        )
        .unwrap();
        assert_eq!(call.request.body, Some(vec![0xff, 0x00, 0x01]));
        assert_eq!(call.response_encoding, ResponseEncoding::Base64);
        let binary = HttpResponse { status: 200, headers: Vec::new(), body: vec![0x89, b'P', b'N', b'G'] };
        let reply = encode_result(Ok(binary.clone()), ResponseEncoding::Base64);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply).unwrap(),
            serde_json::json!({"status":200,"headers":{},"body_base64":"iVBORw=="})
        );
        let refused = encode_result(Ok(binary), ResponseEncoding::Utf8);
        let refused = serde_json::from_slice::<serde_json::Value>(&refused).unwrap();
        assert_eq!(refused["error"]["kind"], "http_error");
        assert!(refused["error"]["message"].as_str().unwrap().contains("response_encoding"));
    }

    #[test]
    fn refuses_bad_binary_bodies() {
        assert_eq!(
            parse(r#"{"url":"https://x.test","body":"a","body_base64":"YQ=="}"#).unwrap_err().kind,
            "invalid_request"
        );
        assert_eq!(
            parse(r#"{"url":"https://x.test","body_base64":"not base64!"}"#).unwrap_err().kind,
            "invalid_request"
        );
        assert_eq!(parse(r#"{"url":"https://x.test","response_encoding":"hex"}"#).unwrap_err().kind, "invalid_request");
        // The limit counts decoded bytes.
        let limits = WasmLimits { http_request_bytes: 3, ..WasmLimits::default() };
        assert!(parse_request(br#"{"url":"https://x.test","body_base64":"YWJj"}"#, &limits).is_ok());
        let error = parse_request(br#"{"url":"https://x.test","body_base64":"YWJjZA=="}"#, &limits).unwrap_err();
        assert_eq!(error.kind, "too_large");
    }
}
