//! HTTP client for a remote code sandbox.
//!
//! Protocol: `POST <url>` with JSON body
//! `{"language": "...", "code": "...", "inputs": {...}, "limits": {"time_ms": ..., "memory_bytes": ...}}`.
//! The service answers `{"outputs": {...}}` on success or `{"error": "..."}` on failure.
//! When configured, the API key is sent as `Authorization: Bearer <key>`.

use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;

use graphrt_core::node::NodeError;
use graphrt_http::HttpClient;

use crate::{CodeLimits, RemoteSandbox};

/// A [`RemoteSandbox`] reached over HTTP. The endpoint is operator-configured, so it
/// uses a trusted egress policy (private addresses allowed).
#[derive(Clone)]
pub struct HttpSandbox {
    url: reqwest::Url,
    api_key: Option<String>,
    client: HttpClient,
}

impl std::fmt::Debug for HttpSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSandbox")
            .field("url", &self.url.as_str())
            .field("api_key", &self.api_key.as_ref().map(|_| "******"))
            .finish()
    }
}

#[derive(Deserialize)]
struct SandboxResponse {
    #[serde(default)]
    outputs: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<String>,
}

impl HttpSandbox {
    /// Create a client for the sandbox at `url`. No API key is assumed by default.
    pub fn new(url: &str, api_key: Option<String>) -> Result<Self, NodeError> {
        let url = reqwest::Url::parse(url)
            .map_err(|e| NodeError::fatal("invalid_config", format!("invalid sandbox URL `{url}`: {e}")))?;
        let client = HttpClient::trusted().map_err(|e| NodeError::fatal("http_client", e.to_string()))?;
        Ok(Self { url, api_key, client })
    }
}

impl RemoteSandbox for HttpSandbox {
    fn execute<'a>(
        &'a self,
        language: &'a str,
        code: &'a str,
        inputs: serde_json::Value,
        limits: CodeLimits,
    ) -> BoxFuture<'a, Result<serde_json::Value, NodeError>> {
        Box::pin(async move {
            let payload = serde_json::json!({
                "language": language,
                "code": code,
                "inputs": inputs,
                "limits": { "time_ms": limits.time_ms, "memory_bytes": limits.memory_bytes },
            });
            let body = serde_json::to_vec(&payload)
                .map_err(|e| NodeError::fatal("invalid_input", format!("cannot encode sandbox request: {e}")))?;
            let http = self.client.client(true).map_err(|e| NodeError::fatal("sandbox", e.to_string()))?;
            let mut request = http
                .post(self.url.clone())
                .header("content-type", "application/json")
                .timeout(Duration::from_millis(limits.time_ms.saturating_add(5_000)))
                .body(body);
            if let Some(key) = &self.api_key {
                request = request.bearer_auth(key);
            }
            let mut response = request
                .send()
                .await
                .map_err(|e| NodeError::new("sandbox", format!("sandbox request failed: {}", e.without_url())))?;
            let status = response.status();
            let cap = limits.max_output_bytes.saturating_add(64 * 1024);
            let mut bytes: Vec<u8> = Vec::new();
            let mut truncated = false;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| NodeError::new("sandbox", format!("cannot read sandbox response: {}", e.without_url())))?
            {
                if bytes.len().saturating_add(chunk.len()) > cap {
                    truncated = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            classify_response(status, &bytes, truncated)
        })
    }
}

/// Turn a sandbox reply into outputs or an error.
///
/// Transport-level status decides retryability first: 401/403 are configuration
/// problems (fatal), 408/429/5xx are transient (retryable). An `error` field is a
/// code error only on a successful or other 4xx protocol reply.
fn classify_response(
    status: reqwest::StatusCode,
    bytes: &[u8],
    truncated: bool,
) -> Result<serde_json::Value, NodeError> {
    let code = status.as_u16();
    let error_text = || {
        serde_json::from_slice::<SandboxResponse>(bytes)
            .ok()
            .and_then(|r| r.error)
            .unwrap_or_else(|| String::from_utf8_lossy(bytes).chars().take(300).collect())
    };
    if matches!(code, 401 | 403) {
        return Err(NodeError::fatal("sandbox_auth", format!("sandbox rejected the credentials (HTTP {status})")));
    }
    if status.is_server_error() || matches!(code, 408 | 429) {
        return Err(NodeError::new("sandbox", format!("sandbox returned HTTP {status}: {}", error_text())));
    }
    if truncated {
        return Err(NodeError::fatal("invalid_output", "sandbox response exceeds the output size limit"));
    }
    let parsed: SandboxResponse = serde_json::from_slice(bytes).map_err(|e| {
        NodeError::fatal("sandbox", format!("sandbox returned HTTP {status} with an invalid body: {e}"))
    })?;
    if let Some(error) = parsed.error {
        return Err(NodeError::fatal("code_error", error));
    }
    if !status.is_success() {
        return Err(NodeError::fatal("sandbox", format!("sandbox returned HTTP {status}")));
    }
    parsed.outputs.ok_or_else(|| NodeError::fatal("invalid_output", "sandbox response has no `outputs`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_key() {
        let s = HttpSandbox::new("http://127.0.0.1:8194/run", Some("secret-key".into())).unwrap();
        assert!(!format!("{s:?}").contains("secret-key"));
        assert!(HttpSandbox::new("not a url", None).is_err());
    }

    #[test]
    fn response_classification() {
        use reqwest::StatusCode;
        let ok = classify_response(StatusCode::OK, br#"{"outputs":{"a":1}}"#, false).unwrap();
        assert_eq!(ok, serde_json::json!({"a": 1}));

        let err = classify_response(StatusCode::OK, br#"{"error":"NameError: x"}"#, false).unwrap_err();
        assert_eq!(err.kind, "code_error");
        assert!(!err.retryable);

        let err = classify_response(StatusCode::INTERNAL_SERVER_ERROR, br#"{"error":"boom"}"#, false).unwrap_err();
        assert_eq!(err.kind, "sandbox");
        assert!(err.retryable);

        let err = classify_response(StatusCode::UNAUTHORIZED, b"", false).unwrap_err();
        assert_eq!(err.kind, "sandbox_auth");
        assert!(!err.retryable);

        let err = classify_response(StatusCode::OK, b"{", true).unwrap_err();
        assert_eq!(err.kind, "invalid_output");
    }
}
