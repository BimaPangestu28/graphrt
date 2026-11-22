//! A scripted HTTP/1.1 server that stands in for a provider API (Kubernetes,
//! Cloud Run admin, ARM, ECS JSON 1.1, Fly Machines...) in conformance tests.
//!
//! Each provider crate supplies a handler that emulates the few endpoints it
//! uses; the harness injects faults (401, 429, 5xx, capacity) in front of it
//! and inspects the recorded requests (C8, C9, C10). One request per
//! connection (`Connection: close`) keeps the parser tiny.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Largest request accepted (headers + body).
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// A request the fake server received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// `GET`, `POST`...
    pub method: String,
    /// Path with query string.
    pub path: String,
    /// Headers with lowercase names, in arrival order.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// First header value named `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers.iter().find(|(header, _)| *header == name).map(|(_, value)| value.as_str())
    }

    /// Body as UTF-8 (lossy).
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A scripted response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeResponse {
    /// HTTP status.
    pub status: u16,
    /// Extra headers.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl FakeResponse {
    /// A JSON response.
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: value.to_string().into_bytes(),
        }
    }

    /// A plain response.
    pub fn status(status: u16) -> Self {
        Self { status, headers: Vec::new(), body: Vec::new() }
    }

    /// Adds a header.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

/// A fault served instead of the handler for the next `count` requests whose
/// path starts with `path_prefix`.
#[derive(Debug, Clone)]
pub struct Fault {
    /// Path prefix the fault applies to (`""` = all).
    pub path_prefix: String,
    /// What to answer.
    pub response: FakeResponse,
    /// How many requests it applies to.
    pub count: u32,
}

/// Handler of the emulated API.
pub type FakeHandler = Arc<dyn Fn(&RecordedRequest) -> FakeResponse + Send + Sync>;

#[derive(Default)]
struct SharedState {
    requests: Vec<RecordedRequest>,
    faults: VecDeque<Fault>,
}

/// A running fake server; stops when dropped.
pub struct FakeServer {
    address: SocketAddr,
    state: Arc<Mutex<SharedState>>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for FakeServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("FakeServer").field("address", &self.address).finish_non_exhaustive()
    }
}

impl FakeServer {
    /// Binds `127.0.0.1:0` and serves `handler`.
    ///
    /// # Errors
    /// When the port cannot be bound.
    pub async fn start(handler: FakeHandler) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(Mutex::new(SharedState::default()));
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = task_state.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let _connection_result = serve_connection(stream, state, handler).await;
                });
            }
        });
        Ok(Self { address, state, task })
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Queues a fault.
    pub fn inject(&self, fault: Fault) {
        lock(&self.state).faults.push_back(fault);
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.state).requests.clone()
    }

    /// Drops every queued fault.
    pub fn clear_faults(&self) {
        lock(&self.state).faults.clear();
    }

    /// Forgets recorded requests.
    pub fn clear_requests(&self) {
        lock(&self.state).requests.clear();
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock(state: &Mutex<SharedState>) -> std::sync::MutexGuard<'_, SharedState> {
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn take_fault(state: &Mutex<SharedState>, path: &str) -> Option<FakeResponse> {
    let mut state = lock(state);
    let position = state.faults.iter().position(|fault| path.starts_with(&fault.path_prefix))?;
    let fault = state.faults.get_mut(position)?;
    let response = fault.response.clone();
    fault.count = fault.count.saturating_sub(1);
    if fault.count == 0 {
        state.faults.remove(position);
    }
    Some(response)
}

async fn serve_connection(
    mut stream: TcpStream,
    state: Arc<Mutex<SharedState>>,
    handler: FakeHandler,
) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream).await? else { return Ok(()) };
    lock(&state).requests.push(request.clone());
    let response = take_fault(&state, &request.path).unwrap_or_else(|| handler(&request));
    stream.write_all(&encode_response(&response)).await?;
    stream.shutdown().await
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<RecordedRequest>> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 || buffer.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
        buffer.extend_from_slice(chunk.get(..read).unwrap_or_default());
    };
    let head = String::from_utf8_lossy(buffer.get(..header_end).unwrap_or_default()).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer.get(header_end + 4..).unwrap_or_default().to_vec();
    while body.len() < content_length.min(MAX_REQUEST_BYTES) {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(chunk.get(..read).unwrap_or_default());
    }
    Ok(Some(RecordedRequest { method, path, headers, body }))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn encode_response(response: &FakeResponse) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {} Fake\r\ncontent-length: {}\r\nconnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&response.body);
    bytes
}
