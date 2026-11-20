//! Transports carrying the plugin protocol.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use graphrt_core::node::Clock;

use crate::conn::{Connection, NotifySink};
use crate::error::CallError;
use crate::manifest::PROTOCOL_VERSION;

/// How long the `initialize` handshake may take.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);

/// A channel to one plugin.
pub trait Transport: Send + Sync + fmt::Debug {
    /// A fresh request id.
    fn next_id(&self) -> String;

    /// Send request `id` and wait for the response. Notifications carrying
    /// `request_id == id` are passed to `sink`. Dropping the future abandons the
    /// request (and cancels it when `cancel_on_drop` is set).
    fn call<'a>(
        &'a self,
        id: String,
        method: &'a str,
        params: serde_json::Value,
        sink: Option<NotifySink>,
        cancel_on_drop: bool,
        clock: &'a Arc<dyn Clock>,
    ) -> BoxFuture<'a, Result<serde_json::Value, CallError>>;

    /// Run the `initialize` handshake (starting the plugin if needed) and return its result.
    fn initialize<'a>(&'a self, clock: &'a Arc<dyn Clock>) -> BoxFuture<'a, Result<serde_json::Value, CallError>>;

    /// A request was abandoned after its timeout. Transports supervising a process
    /// use this to restart a plugin that stopped answering. Default: no-op.
    fn note_timeout(&self) {}

    /// A request completed (successfully or with a plugin error). Default: no-op.
    fn note_answer(&self) {}
}

/// Optional protocol features this host supports, announced in `initialize`
/// (`tool_images`: `tool.call` results may carry `images`; `tool_context`:
/// `tool.call` requests may carry a `context` with per-call secrets).
pub const HOST_FEATURES: &[&str] = &["tool_images", "tool_context"];

/// Parameters of `initialize`. `max_message_bytes` tells the plugin how large a
/// line the host may send, so SDKs size their read buffers to match.
pub(crate) fn initialize_params(max_message_bytes: usize) -> serde_json::Value {
    serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "host": {"name": "graphrt", "version": env!("CARGO_PKG_VERSION")},
        "max_message_bytes": max_message_bytes,
        "features": HOST_FEATURES,
    })
}

/// Validate an `initialize` result.
pub(crate) fn check_initialize(result: &serde_json::Value) -> Result<(), CallError> {
    match result.get("protocol_version").and_then(serde_json::Value::as_u64) {
        None => Ok(()),
        Some(v) if v == u64::from(PROTOCOL_VERSION) => Ok(()),
        Some(v) => {
            Err(CallError::Protocol(format!("plugin speaks protocol version {v}, this host speaks {PROTOCOL_VERSION}")))
        }
    }
}

/// Race `fut` against `timeout` on `clock` (no timeout when the clock has no timers).
pub(crate) async fn with_timeout<T>(
    clock: &Arc<dyn Clock>,
    timeout: Duration,
    fut: impl std::future::Future<Output = Result<T, CallError>>,
) -> Result<T, CallError> {
    if !clock.supports_timers() {
        return fut.await;
    }
    let sleep = clock.sleep(timeout);
    futures::pin_mut!(fut);
    match futures::future::select(fut, sleep).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(((), _)) => {
            Err(CallError::Timeout(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)))
        }
    }
}

/// A transport over an already-connected byte stream, e.g. an in-process
/// plugin or a socket. The connection is initialized on first use.
#[derive(Debug)]
pub struct IoTransport {
    conn: Connection,
    max_message_bytes: usize,
    initialized: tokio::sync::OnceCell<serde_json::Value>,
}

impl IoTransport {
    /// Wrap a reader/writer pair. Must be called inside a tokio runtime.
    pub fn new<R, W>(reader: R, writer: W, max_message_bytes: usize) -> Self
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            conn: Connection::new(reader, writer, max_message_bytes),
            max_message_bytes,
            initialized: tokio::sync::OnceCell::new(),
        }
    }

    async fn ensure_initialized(&self, clock: &Arc<dyn Clock>) -> Result<serde_json::Value, CallError> {
        let result = self
            .initialized
            .get_or_try_init(|| async {
                let id = self.conn.next_id();
                let fut = self.conn.call(id, "initialize", initialize_params(self.max_message_bytes), None, false);
                let result = with_timeout(clock, INITIALIZE_TIMEOUT, fut).await?;
                check_initialize(&result)?;
                Ok::<_, CallError>(result)
            })
            .await?;
        Ok(result.clone())
    }
}

impl Transport for IoTransport {
    fn next_id(&self) -> String {
        self.conn.next_id()
    }

    fn call<'a>(
        &'a self,
        id: String,
        method: &'a str,
        params: serde_json::Value,
        sink: Option<NotifySink>,
        cancel_on_drop: bool,
        clock: &'a Arc<dyn Clock>,
    ) -> BoxFuture<'a, Result<serde_json::Value, CallError>> {
        Box::pin(async move {
            self.ensure_initialized(clock).await?;
            self.conn.call(id, method, params, sink, cancel_on_drop).await
        })
    }

    fn initialize<'a>(&'a self, clock: &'a Arc<dyn Clock>) -> BoxFuture<'a, Result<serde_json::Value, CallError>> {
        Box::pin(self.ensure_initialized(clock))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_announces_features() {
        let params = initialize_params(1024);
        assert_eq!(params["max_message_bytes"], 1024);
        assert_eq!(params["features"], serde_json::json!(["tool_images", "tool_context"]));
    }
}
