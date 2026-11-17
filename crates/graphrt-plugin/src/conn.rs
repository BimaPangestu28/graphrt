//! A multiplexed JSON-RPC connection over any byte stream (child process stdio,
//! in-process duplex pipes...).
//!
//! One reader task routes responses to waiting callers by id and notifications
//! to the in-flight request named by their `request_id`; one writer task
//! serializes outgoing lines.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::error::CallError;
use crate::protocol::{self, Incoming, Notification};

/// Receives notifications for one in-flight request.
pub type NotifySink = Arc<dyn Fn(Notification) + Send + Sync>;

/// Produces extra detail (e.g. the plugin's stderr tail) when the connection breaks.
pub(crate) type CrashDetail = Arc<dyn Fn() -> String + Send + Sync>;

struct Pending {
    reply: oneshot::Sender<Result<serde_json::Value, CallError>>,
    sink: Option<NotifySink>,
}

#[derive(Default)]
struct Shared {
    /// Pending requests. `None` once the connection is closed: the closed state and
    /// the pending map change together under this one lock, so a request can never
    /// be registered after the drain and then wait forever.
    pending: Mutex<Option<HashMap<String, Pending>>>,
    closed: AtomicBool,
    close_reason: Mutex<Option<String>>,
    logs: Mutex<LogBuffer>,
    /// Plugin name used in forwarded log events.
    label: Mutex<String>,
}

/// Most recent `log` notifications, bounded by line count and total bytes.
#[derive(Default)]
struct LogBuffer {
    lines: std::collections::VecDeque<String>,
    bytes: usize,
}

const MAX_LOG_LINES: usize = 200;
/// Longest log line kept (longer lines are truncated).
const MAX_LOG_LINE_BYTES: usize = 4 * 1024;
/// Total bytes of retained log lines.
const MAX_LOG_BYTES: usize = 256 * 1024;

impl Shared {
    fn new() -> Self {
        Self { pending: Mutex::new(Some(HashMap::new())), ..Self::default() }
    }

    fn take_pending(&self, id: &str) -> Option<Pending> {
        self.pending.lock().ok().and_then(|mut p| p.as_mut().and_then(|m| m.remove(id)))
    }

    fn sink_for(&self, request_id: &str) -> Option<NotifySink> {
        self.pending.lock().ok().and_then(|p| p.as_ref().and_then(|m| m.get(request_id).and_then(|e| e.sink.clone())))
    }

    /// Register a pending request, or fail if the connection already closed.
    fn register(&self, id: String, pending: Pending) -> Result<(), CallError> {
        let mut guard = self.pending.lock().map_err(|_| CallError::Protocol("connection state poisoned".into()))?;
        match guard.as_mut() {
            Some(map) => {
                map.insert(id, pending);
                Ok(())
            }
            None => Err(CallError::Crashed(self.reason())),
        }
    }

    fn close(&self, reason: String) {
        if let Ok(mut r) = self.close_reason.lock() {
            if r.is_none() {
                *r = Some(reason.clone());
            }
        }
        // Mark closed and take every pending request under the same lock.
        let drained: Vec<Pending> = match self.pending.lock() {
            Ok(mut p) => {
                self.closed.store(true, Ordering::SeqCst);
                p.take().map(|m| m.into_values().collect()).unwrap_or_default()
            }
            Err(_) => {
                self.closed.store(true, Ordering::SeqCst);
                Vec::new()
            }
        };
        let reason = self.reason();
        for pending in drained {
            let _receiver_gone = pending.reply.send(Err(CallError::Crashed(reason.clone())));
        }
    }

    fn reason(&self) -> String {
        self.close_reason.lock().ok().and_then(|r| r.clone()).unwrap_or_else(|| "connection closed".into())
    }

    fn label(&self) -> String {
        self.label.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn push_log(&self, mut line: String) {
        truncate_utf8(&mut line, MAX_LOG_LINE_BYTES);
        if let Ok(mut logs) = self.logs.lock() {
            logs.bytes = logs.bytes.saturating_add(line.len());
            logs.lines.push_back(line);
            while logs.lines.len() > MAX_LOG_LINES || logs.bytes > MAX_LOG_BYTES {
                match logs.lines.pop_front() {
                    Some(old) => logs.bytes = logs.bytes.saturating_sub(old.len()),
                    None => break,
                }
            }
        }
    }
}

/// Cut `s` to at most `max` bytes on a character boundary, marking the cut.
pub(crate) fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("…[truncated]");
}

/// Forward a plugin log line to the operator (stderr), tagged with the plugin name.
pub(crate) fn forward_log(plugin: &str, level: &str, message: &str) {
    let mut message = message.to_owned();
    truncate_utf8(&mut message, MAX_LOG_LINE_BYTES);
    let level = match level {
        "error" | "warn" | "warning" | "info" | "debug" | "trace" => level,
        _ => "info",
    };
    // Plugin authors are told to log through the protocol instead of writing to
    // stdout; surface those lines where operators look. Debug/trace stay quiet
    // unless `GRAPHRT_PLUGIN_LOG=debug`.
    let verbose = std::env::var("GRAPHRT_PLUGIN_LOG").is_ok_and(|v| v == "debug" || v == "trace");
    if matches!(level, "debug" | "trace") && !verbose {
        return;
    }
    let plugin = if plugin.is_empty() { "plugin" } else { plugin };
    #[allow(clippy::print_stderr)]
    {
        eprintln!("[plugin {plugin}] {level}: {message}");
    }
}

/// A live connection to a plugin.
pub struct Connection {
    shared: Arc<Shared>,
    writer: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    max_message_bytes: usize,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Resource kept alive with the connection (e.g. the child-process guard).
    _guard: Option<Box<dyn std::any::Any + Send + Sync>>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").field("closed", &self.is_closed()).finish_non_exhaustive()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Connection {
    /// Connect over a reader/writer pair. Must be called inside a tokio runtime.
    pub fn new<R, W>(reader: R, writer: W, max_message_bytes: usize) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::with_guard(reader, writer, max_message_bytes, None, None)
    }

    pub(crate) fn with_guard<R, W>(
        reader: R,
        writer: W,
        max_message_bytes: usize,
        guard: Option<Box<dyn std::any::Any + Send + Sync>>,
        crash_detail: Option<CrashDetail>,
    ) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let shared = Arc::new(Shared::new());
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let writer_task = tokio::spawn(write_loop(writer, rx, shared.clone()));
        let reader_task = tokio::spawn(read_loop(
            BufReader::new(reader),
            shared.clone(),
            tx.clone(),
            max_message_bytes,
            crash_detail,
        ));
        Self {
            shared,
            writer: tx,
            next_id: AtomicU64::new(1),
            max_message_bytes,
            tasks: vec![reader_task, writer_task],
            _guard: guard,
        }
    }

    /// Whether the connection is closed (the plugin exited or broke the protocol).
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// A fresh request id, unique within this connection.
    pub fn next_id(&self) -> String {
        format!("r-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Recent `log` notifications from the plugin (`level: message`).
    pub fn recent_logs(&self) -> Vec<String> {
        self.shared.logs.lock().map(|l| l.lines.iter().cloned().collect()).unwrap_or_default()
    }

    /// Name used when forwarding the plugin's log lines to the operator.
    pub fn set_label(&self, label: &str) {
        if let Ok(mut l) = self.shared.label.lock() {
            label.clone_into(&mut l);
        }
    }

    /// Close the connection (pending requests fail with `reason`) and drop the
    /// guarded resource on the next drop of this connection.
    pub fn close(&self, reason: &str) {
        self.shared.close(reason.to_owned());
    }

    /// Send a notification (fire and forget).
    pub fn notify(&self, method: &str, params: &serde_json::Value) {
        let _closed = self.writer.send(protocol::notification(method, params));
    }

    /// Send a request with id `id` and wait for its response. Notifications whose
    /// `request_id` equals `id` go to `sink`. When the returned future is dropped
    /// before completion and `cancel_on_drop` is set, a `node.cancel` notification
    /// is sent.
    pub async fn call(
        &self,
        id: String,
        method: &str,
        params: serde_json::Value,
        sink: Option<NotifySink>,
        cancel_on_drop: bool,
    ) -> Result<serde_json::Value, CallError> {
        if self.is_closed() {
            return Err(CallError::Crashed(self.shared.reason()));
        }
        let line = protocol::request(&id, method, &params);
        if line.len() > self.max_message_bytes {
            return Err(CallError::Protocol(format!(
                "request is {} bytes, above the {}-byte message limit",
                line.len(),
                self.max_message_bytes
            )));
        }
        let (reply, rx) = oneshot::channel();
        self.shared.register(id.clone(), Pending { reply, sink })?;
        let mut guard = CancelGuard {
            id: id.clone(),
            shared: self.shared.clone(),
            writer: self.writer.clone(),
            armed: cancel_on_drop,
        };
        if self.writer.send(line).is_err() {
            guard.armed = false;
            self.shared.take_pending(&id);
            return Err(CallError::Crashed(self.shared.reason()));
        }
        let result = rx.await.unwrap_or_else(|_| Err(CallError::Crashed(self.shared.reason())));
        guard.armed = false;
        result
    }
}

/// Sends `node.cancel` if a call is abandoned before its response arrives.
struct CancelGuard {
    id: String,
    shared: Arc<Shared>,
    writer: mpsc::UnboundedSender<String>,
    armed: bool,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.shared.take_pending(&self.id).is_some() && self.armed {
            let params = serde_json::json!({"request_id": self.id});
            let _closed = self.writer.send(protocol::notification("node.cancel", &params));
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<String>,
    shared: Arc<Shared>,
) {
    while let Some(mut line) = rx.recv().await {
        line.push('\n');
        let written = async {
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await
        };
        if let Err(e) = written.await {
            shared.close(format!("cannot write to plugin: {e}"));
            return;
        }
    }
}

async fn read_loop<R: AsyncBufRead + Unpin>(
    mut reader: R,
    shared: Arc<Shared>,
    writer: mpsc::UnboundedSender<String>,
    max_message_bytes: usize,
    crash_detail: Option<CrashDetail>,
) {
    let mut buf = Vec::new();
    let reason = loop {
        buf.clear();
        match read_line_capped(&mut reader, &mut buf, max_message_bytes).await {
            Ok(0) => break "plugin closed its output (process exited?)".to_owned(),
            Ok(_) => {}
            Err(e) => break e,
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            break "plugin sent invalid UTF-8".to_owned();
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match protocol::parse(line) {
            Ok(Incoming::Response { id, result }) => match shared.take_pending(&id) {
                Some(pending) => {
                    let _receiver_gone = pending.reply.send(result);
                }
                // Usually a late answer to a request that timed out or was cancelled.
                None => forward_log(&shared.label(), "debug", &format!("ignored response to unknown request `{id}`")),
            },
            Ok(Incoming::Notification(n)) => route_notification(&shared, n),
            Ok(Incoming::Request { id, method }) => {
                let _closed = writer.send(protocol::method_not_found(&id, &method));
            }
            Err(e) => break e.to_string(),
        }
    };
    let detail = match crash_detail {
        Some(f) => {
            // Let the stderr reader and the exit-status watcher catch up so the
            // report says why the plugin died.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Some(f()).filter(|d| !d.trim().is_empty())
        }
        None => None,
    };
    let reason = match detail {
        Some(d) => format!("{reason} ({d})"),
        None => reason,
    };
    shared.close(reason);
}

fn route_notification(shared: &Shared, n: Notification) {
    if n.method == "log" {
        let level = n.params.get("level").and_then(|v| v.as_str()).unwrap_or("info");
        let message = n.params.get("message").and_then(|v| v.as_str()).unwrap_or_default();
        forward_log(&shared.label(), level, message);
        shared.push_log(format!("{level}: {message}"));
        // A log tied to a request also reaches that node's event stream.
        if let Some(request_id) = n.request_id().map(str::to_owned) {
            if let Some(sink) = shared.sink_for(&request_id) {
                sink(n);
            }
        }
        return;
    }
    let Some(request_id) = n.request_id().map(str::to_owned) else { return };
    if let Some(sink) = shared.sink_for(&request_id) {
        sink(n);
    }
}

/// Read one `\n`-terminated line into `buf`, failing past `max` bytes.
/// Returns the number of bytes read (0 at EOF).
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> Result<usize, String> {
    loop {
        let available = reader.fill_buf().await.map_err(|e| format!("cannot read from plugin: {e}"))?;
        if available.is_empty() {
            return Ok(buf.len());
        }
        let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
            Some(pos) => (available.get(..=pos).unwrap_or(available), true),
            None => (available, false),
        };
        if buf.len().saturating_add(chunk.len()) > max.saturating_add(1) {
            return Err(format!("plugin message exceeds the {max}-byte limit"));
        }
        buf.extend_from_slice(chunk);
        let used = chunk.len();
        reader.consume(used);
        if done {
            return Ok(buf.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn call_after_close_fails_immediately() {
        let (client, _server) = tokio::io::duplex(1024);
        let (r, w) = tokio::io::split(client);
        let conn = Connection::new(r, w, 1 << 20);
        conn.close("gone");
        // Even a caller that passed the `is_closed` check must not register a request
        // after the drain; `register` refuses it under the same lock.
        let err = conn.shared.register("x".into(), Pending { reply: oneshot::channel().0, sink: None });
        assert!(matches!(err, Err(CallError::Crashed(m)) if m == "gone"));
        let res = conn.call("r-1".into(), "node.run", serde_json::json!({}), None, false).await;
        assert!(matches!(res, Err(CallError::Crashed(_))));
    }

    #[test]
    fn retained_logs_are_bounded() {
        let shared = Shared::new();
        for _ in 0..500 {
            shared.push_log("x".repeat(64 * 1024));
        }
        let logs = shared.logs.lock().unwrap();
        assert!(logs.bytes <= MAX_LOG_BYTES);
        assert!(logs.lines.iter().all(|l| l.len() <= MAX_LOG_LINE_BYTES + 16));
    }
}
