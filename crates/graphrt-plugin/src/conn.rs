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
    pending: Mutex<HashMap<String, Pending>>,
    closed: AtomicBool,
    close_reason: Mutex<Option<String>>,
    logs: Mutex<std::collections::VecDeque<String>>,
}

const MAX_LOG_LINES: usize = 200;

impl Shared {
    fn take_pending(&self, id: &str) -> Option<Pending> {
        self.pending.lock().ok().and_then(|mut p| p.remove(id))
    }

    fn sink_for(&self, request_id: &str) -> Option<NotifySink> {
        self.pending.lock().ok().and_then(|p| p.get(request_id).and_then(|e| e.sink.clone()))
    }

    fn close(&self, reason: String) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut r) = self.close_reason.lock() {
            *r = Some(reason.clone());
        }
        let drained: Vec<Pending> = match self.pending.lock() {
            Ok(mut p) => p.drain().map(|(_, v)| v).collect(),
            Err(_) => Vec::new(),
        };
        for pending in drained {
            let _receiver_gone = pending.reply.send(Err(CallError::Crashed(reason.clone())));
        }
    }

    fn reason(&self) -> String {
        self.close_reason.lock().ok().and_then(|r| r.clone()).unwrap_or_else(|| "connection closed".into())
    }

    fn push_log(&self, line: String) {
        if let Ok(mut logs) = self.logs.lock() {
            if logs.len() >= MAX_LOG_LINES {
                logs.pop_front();
            }
            logs.push_back(line);
        }
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
        let shared = Arc::new(Shared::default());
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
        self.shared.logs.lock().map(|l| l.iter().cloned().collect()).unwrap_or_default()
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
        match self.shared.pending.lock() {
            Ok(mut p) => {
                p.insert(id.clone(), Pending { reply, sink });
            }
            Err(_) => return Err(CallError::Protocol("connection state poisoned".into())),
        }
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
            Ok(Incoming::Response { id, result }) => {
                if let Some(pending) = shared.take_pending(&id) {
                    let _receiver_gone = pending.reply.send(result);
                }
            }
            Ok(Incoming::Notification(n)) => route_notification(&shared, n),
            Ok(Incoming::Request { id, method }) => {
                let _closed = writer.send(protocol::method_not_found(&id, &method));
            }
            Err(e) => break e.to_string(),
        }
    };
    let detail = crash_detail.map(|f| f()).filter(|d| !d.trim().is_empty());
    let reason = match detail {
        Some(d) => format!("{reason}; stderr: {d}"),
        None => reason,
    };
    shared.close(reason);
}

fn route_notification(shared: &Shared, n: Notification) {
    if n.method == "log" {
        let level = n.params.get("level").and_then(|v| v.as_str()).unwrap_or("info");
        let message = n.params.get("message").and_then(|v| v.as_str()).unwrap_or_default();
        shared.push_log(format!("{level}: {message}"));
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
