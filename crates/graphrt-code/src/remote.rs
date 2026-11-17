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
            let mut request = self
                .client
                .client(true)
                .post(self.url.clone())
                .header("content-type", "application/json")
                .timeout(Duration::from_millis(limits.time_ms.saturating_add(5_000)))
                .body(body);
            if let Some(key) = &self.api_key {
                request = request.bearer_auth(key);
            }
            let response =
                request.send().await.map_err(|e| NodeError::new("sandbox", format!("sandbox request failed: {e}")))?;
            let status = response.status();
            let bytes = response
                .bytes()
                .await
                .map_err(|e| NodeError::new("sandbox", format!("cannot read sandbox response: {e}")))?;
            if bytes.len() > limits.max_output_bytes.saturating_add(64 * 1024) {
                return Err(NodeError::fatal("invalid_output", "sandbox response exceeds the output size limit"));
            }
            let parsed: SandboxResponse = serde_json::from_slice(&bytes).map_err(|e| {
                NodeError::new("sandbox", format!("sandbox returned HTTP {status} with an invalid body: {e}"))
            })?;
            if let Some(error) = parsed.error {
                return Err(NodeError::fatal("code_error", error));
            }
            if !status.is_success() {
                return Err(NodeError::new("sandbox", format!("sandbox returned HTTP {status}")));
            }
            parsed.outputs.ok_or_else(|| NodeError::fatal("invalid_output", "sandbox response has no `outputs`"))
        })
    }
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
}
