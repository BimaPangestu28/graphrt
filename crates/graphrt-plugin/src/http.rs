//! HTTP plugins: each request is a JSON-RPC `POST`. Responses are either a
//! single `application/json` message or an `application/x-ndjson` stream of
//! notifications followed by the final response.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::node::Clock;

use crate::conn::NotifySink;
use crate::error::CallError;
use crate::manifest::{HttpRuntime, LoadedManifest};
use crate::protocol::{self, Incoming};
use crate::transport::{INITIALIZE_TIMEOUT, Transport, check_initialize, initialize_params, with_timeout};

/// A plugin served over HTTP.
pub struct HttpTransport {
    name: String,
    runtime: HttpRuntime,
    client: reqwest::Client,
    max_message_bytes: usize,
    next_id: AtomicU64,
    initialized: tokio::sync::OnceCell<serde_json::Value>,
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("plugin", &self.name)
            .field("url", &self.runtime.url)
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    pub fn new(plugin: &LoadedManifest, runtime: HttpRuntime) -> Result<Self, CallError> {
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| CallError::Transport(format!("cannot build HTTP client: {}", e.without_url())))?;
        Ok(Self {
            name: plugin.manifest.name.clone(),
            runtime,
            client,
            max_message_bytes: plugin.manifest.limits.max_message_bytes,
            next_id: AtomicU64::new(1),
            initialized: tokio::sync::OnceCell::new(),
        })
    }

    async fn post(
        &self,
        id: &str,
        method: &str,
        params: &serde_json::Value,
        sink: Option<&NotifySink>,
    ) -> Result<serde_json::Value, CallError> {
        let mut req = self
            .client
            .post(&self.runtime.url)
            .header("content-type", "application/json")
            .header("accept", "application/x-ndjson, application/json")
            .body(protocol::request(id, method, params));
        for (k, v) in &self.runtime.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(var) = &self.runtime.bearer_token_env {
            match std::env::var(var) {
                Ok(token) if !token.is_empty() => req = req.bearer_auth(token),
                _ => return Err(CallError::Start(format!("environment variable `{var}` (bearer token) is not set"))),
            }
        }
        let resp = req.send().await.map_err(|e| CallError::Transport(e.without_url().to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(error_from_status(status.as_u16(), resp).await);
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut final_result: Option<Result<serde_json::Value, CallError>> = None;
        loop {
            // Process complete lines already buffered.
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                if let Some(r) = self.handle_line(&line, id, sink)? {
                    final_result = Some(r);
                }
            }
            if final_result.is_some() {
                break;
            }
            match stream.next().await {
                Some(Ok(bytes)) => {
                    buf.extend_from_slice(&bytes);
                    if buf.len() > self.max_message_bytes {
                        return Err(CallError::Protocol(format!(
                            "plugin message exceeds the {}-byte limit",
                            self.max_message_bytes
                        )));
                    }
                }
                Some(Err(e)) => return Err(CallError::Transport(e.without_url().to_string())),
                None => {
                    // A single JSON body without a trailing newline.
                    let rest = std::mem::take(&mut buf);
                    if let Some(r) = self.handle_line(&rest, id, sink)? {
                        final_result = Some(r);
                    }
                    break;
                }
            }
        }
        final_result.unwrap_or_else(|| Err(CallError::Protocol("response ended without a JSON-RPC result".into())))
    }

    fn handle_line(
        &self,
        line: &[u8],
        id: &str,
        sink: Option<&NotifySink>,
    ) -> Result<Option<Result<serde_json::Value, CallError>>, CallError> {
        let text = std::str::from_utf8(line).map_err(|_| CallError::Protocol("invalid UTF-8 in response".into()))?;
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        match protocol::parse(text)? {
            Incoming::Response { id: rid, result } if rid == id => Ok(Some(result)),
            Incoming::Response { .. } => Err(CallError::Protocol("response id does not match the request".into())),
            Incoming::Notification(n) => {
                if n.method == "log" {
                    let level = n.params.get("level").and_then(|v| v.as_str()).unwrap_or("info");
                    let message = n.params.get("message").and_then(|v| v.as_str()).unwrap_or_default();
                    crate::conn::forward_log(&self.name, level, message);
                }
                if n.request_id() == Some(id)
                    && let Some(sink) = sink
                {
                    sink(n);
                }
                Ok(None)
            }
            Incoming::Request { method, .. } => {
                Err(CallError::Protocol(format!("plugin sent a `{method}` request over HTTP, which is not supported")))
            }
        }
    }

    async fn ensure_initialized(&self, clock: &Arc<dyn Clock>) -> Result<serde_json::Value, CallError> {
        let result = self
            .initialized
            .get_or_try_init(|| async {
                let id = self.next_id();
                let params = initialize_params(self.max_message_bytes);
                let result =
                    with_timeout(clock, INITIALIZE_TIMEOUT, self.post(&id, "initialize", &params, None)).await?;
                check_initialize(&result)?;
                Ok::<_, CallError>(result)
            })
            .await?;
        Ok(result.clone())
    }
}

/// Largest error body read to explain a failed request.
const ERROR_BODY_BYTES: usize = 4 * 1024;

/// Turn a non-2xx response into a call error, keeping a JSON-RPC error the
/// plugin sent (its `kind` / `retryable`) or a snippet of the body.
async fn error_from_status(status: u16, mut resp: reqwest::Response) -> CallError {
    let mut body = Vec::new();
    while body.len() < ERROR_BODY_BYTES {
        match resp.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            _ => break,
        }
    }
    body.truncate(ERROR_BODY_BYTES);
    let text = String::from_utf8_lossy(&body).trim().to_owned();
    if let Ok(Incoming::Response { result: Err(err @ CallError::Remote { .. }), .. }) = protocol::parse(&text) {
        return err;
    }
    let snippet = if text.is_empty() { String::new() } else { format!(": {text}") };
    let message = format!("plugin endpoint answered HTTP {status}{snippet}");
    // Client errors (other than timeouts and rate limits) will not succeed on retry.
    let retryable = !(400..500).contains(&status) || status == 408 || status == 429;
    if retryable {
        CallError::Transport(message)
    } else {
        CallError::Remote {
            code: -32000,
            message,
            data: Some(serde_json::json!({"kind": "plugin_http_status", "retryable": false})),
        }
    }
}

impl Transport for HttpTransport {
    fn next_id(&self) -> String {
        format!("r-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn call<'a>(
        &'a self,
        id: String,
        method: &'a str,
        params: serde_json::Value,
        sink: Option<NotifySink>,
        _cancel_on_drop: bool,
        clock: &'a Arc<dyn Clock>,
    ) -> BoxFuture<'a, Result<serde_json::Value, CallError>> {
        // Cancellation over HTTP is dropping the request (the connection closes).
        Box::pin(async move {
            self.ensure_initialized(clock).await?;
            self.post(&id, method, &params, sink.as_ref()).await
        })
    }

    fn initialize<'a>(&'a self, clock: &'a Arc<dyn Clock>) -> BoxFuture<'a, Result<serde_json::Value, CallError>> {
        Box::pin(self.ensure_initialized(clock))
    }
}
